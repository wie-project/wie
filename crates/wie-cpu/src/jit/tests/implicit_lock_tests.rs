// The JIT must refuse to compile an **implicitly locked** memory read-modify-write.
//
// ## The bug this file pins
//
// On x86-64 the memory forms of `XCHG` and `CMPXCHG` are locked by the
// architecture whether or not a `F0` prefix byte is present, and compilers emit
// them **without** one as the norm. The JIT's lowerability predicate keyed on
// `Instruction::has_lock_prefix()`, which reports only the prefix byte, so it
// refused `F0 0F B1 /r` and admitted the bare `0F B1 /r` — compiling the standard
// 64-bit CAS (every `std::atomic` CAS, every `InterlockedCompareExchange64`) into a
// plain load plus store while the interpreter performed a real atomic CAS. Guest
// atomics therefore had no mutual exclusion on the default backend.
//
// ## Why a differential test alone is NOT sufficient
//
// The guest-visible result of `cmpxchg [rbx], ecx` is identical whether the block
// was correctly refused (interpreter does a real atomic CAS) or incorrectly
// compiled (JIT does load-then-store): single-threaded, both store the same value
// and set the same flags. A JIT-vs-iced differential assertion therefore passes
// on the broken code — which is exactly why the defect survived.
//
// What distinguishes them is *lowerability*, so that is the load-bearing
// assertion here: `has_ready_at()` must be false, and the retired count must show
// the work went to the interpreter. `cmpxchg_mem_bare_encoding_is_refused_jit_but_correct_via_iced`
// is that test. The differential assertion is kept alongside it as a guard on the
// guest-visible result, not as the primary defence.
//
// Split out of `cmpxchg_tests.rs` (file-size policy, ADR-002); it reuses that
// file's helpers where the shape is the same and adds the memory-operand cases.
use super::atomic_tests::{
    BACKENDS, Backend, DATA_BASE, Engine, LOCK_WORD, UNALIGNED_WORD, ctx, open, open_worker, plant,
    read_word, write_word,
};
use super::*;
use crate::exec::{AccessType, StepResult as StepResultAlias};
use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};
use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, Ordering};

/// `cmpxchg dword ptr [rbx], ecx` — 0F B1 /r, ModRM 0x0B. **No `F0`.** This is
/// the encoding that was missed, and the one compilers actually emit.
const CAS_MEM_BARE32: [u8; 3] = [0x0F, 0xB1, 0x0B];
/// The same instruction with the prefix byte present.
const CAS_MEM_LOCK32: [u8; 4] = [0xF0, 0x0F, 0xB1, 0x0B];
/// `cmpxchg qword ptr [rbx], rcx` — REX.W 0F B1 /r, still no `F0`. The 64-bit CAS
/// is what `std::atomic<u64>` and `InterlockedCompareExchange64` compile to.
const CAS_MEM_BARE64: [u8; 4] = [0x48, 0x0F, 0xB1, 0x0B];
/// `cmpxchg word ptr [rbx], cx` — 66 0F B1 /r.
const CAS_MEM_BARE16: [u8; 4] = [0x66, 0x0F, 0xB1, 0x0B];
/// `xchg dword ptr [rbx], eax` — 87 /r, ModRM 0x03. Implicitly locked, no `F0`.
const XCHG_MEM_BARE32: [u8; 2] = [0x87, 0x03];
/// `xchg dword ptr [rbx], ecx` — 87 /r, ModRM 0x0B.
///
/// `xchg` is symmetric, so the register may sit in either ModRM field; 0x03 puts
/// EAX in the reg field and 0x0B puts ECX. Kept as a separate case because a
/// predicate that hard-codes "the register is RAX" or inspects only one operand
/// slot would pass on 0x03 and fail here. Note that iced *canonicalises* the
/// operand order — both encodings decode with the memory in op0 — so this is not a
/// case where an `op0_kind()` check fails; it is a case where the *register*
/// involved differs, and the swap must move ECX rather than RAX. See
/// `has_memory_operand` for why the predicate still scans every operand.
const XCHG_MEM_RM_REG: [u8; 2] = [0x87, 0x0B];
/// `xchg eax, ecx` — 0x90 is `xchg eax, eax`; the register/register form we want
/// is `91` (`xchg ecx, eax` with ModRM-style reg fields). Kept for the positive
/// case: a register-only XCHG must still compile.
const XCHG_REG_REG: [u8; 1] = [0x91];
/// `xadd dword ptr [rbx], ecx` — 0F C1 /r, ModRM 0x0B. RMW but **not**
/// implicitly locked: without `F0` it is a plain RMW in hardware too, so the JIT
/// is right to keep compiling it.
const XADD_MEM_BARE32: [u8; 3] = [0x0F, 0xC1, 0x0B];
/// `inc dword ptr [rbx]` — FF /0, ModRM 0x03. An unlocked memory RMW that is NOT
/// implicitly locked. Pinned because a blanket "refuse every memory RMW" fix would
/// reject this and throw away coverage for nothing.
const INC_MEM_BARE32: [u8; 2] = [0xFF, 0x03];

// The interpreter's decode cache is process-wide and thread-local, keyed by
// (rip, mem_generation), and every test CPU here starts at generation 0, so one
// code VA per distinct byte sequence is mandatory. 64 KiB apart, because `plant`
// reserves with `MEM_RESERVE`, which rounds the base up to the allocation
// granularity.
const CODE_BASE: u64 = 0x4070_0000;
const fn code_va(id: u64) -> u64 {
    CODE_BASE + id * 0x1_0000
}

const ID_CAS_BARE32: u64 = 0;
const ID_CAS_LOCK32: u64 = 1;
const ID_CAS_BARE64: u64 = 2;
const ID_CAS_BARE16: u64 = 3;
const ID_XCHG_BARE32: u64 = 4;
const ID_XCHG_RM_REG: u64 = 5;
const ID_XCHG_REG_REG: u64 = 6;
const ID_XADD_BARE32: u64 = 7;
const ID_INC_BARE32: u64 = 8;
/// Decode-only (never executed), so it only has to differ from the above.
const CODE_DECODE: u64 = CODE_BASE - 0x1_0000;

/// Is `op` the 16-bit memory-operand form? Kept as a decode rather than a length
/// comparison so it cannot drift from the encodings above.
fn insn_is_16bit(op: &[u8]) -> bool {
    let mut dec = Decoder::with_ip(64, op, CODE_DECODE, DecoderOptions::NONE);
    dec.decode().memory_size() == iced_x86::MemorySize::UInt16
}

/// Does the JIT refuse to compile `op`, and did the work go to the interpreter?
///
/// Returns `(has_compiled_block, iced_insns_retired)`.
fn lowerability(op: &[u8], id: u64, word: u64) -> (bool, u64) {
    let code_base = code_va(id);
    let mut cpu = open(Backend::Jit);
    plant(&mut cpu, code_base, op, word, 0);
    cpu.set_state(&ctx(code_base, word, 5, 9));
    assert!(
        matches!(cpu.run_rmw(), StepResultAlias::Continue),
        "{op:02x?} must retire"
    );
    match &cpu {
        Engine::Jit(j) => (j.has_ready_at(code_base), j.stats().exec.iced_insns),
        Engine::Iced(_) => unreachable!("lowerability always opens the JIT"),
    }
}

// ── Encoding sanity (deterministic) ─────────────────────────────────────

/// The bytes must be what these tests claim, and — critically — the *bare*
/// `CMPXCHG`/`XCHG` memory forms must report **no** LOCK prefix. That is the whole
/// point: the prefix byte is absent from the encoding, so any predicate consulting
/// `has_lock_prefix()` is consulting a property that is false for the instruction
/// that most needs to be treated as atomic.
#[test]
fn implicit_lock_test_encodings_are_what_we_think() {
    // `Some(n)` = the memory operand is op`n`; `None` = no memory operand at all
    // (a register-only form), which the old boolean column could not express.
    for (bytes, mnemonic, lock_reported, mem_op) in [
        (&CAS_MEM_BARE32[..], Mnemonic::Cmpxchg, false, Some(0)),
        (&CAS_MEM_LOCK32[..], Mnemonic::Cmpxchg, true, Some(0)),
        (&CAS_MEM_BARE64[..], Mnemonic::Cmpxchg, false, Some(0)),
        (&CAS_MEM_BARE16[..], Mnemonic::Cmpxchg, false, Some(0)),
        (&XCHG_MEM_BARE32[..], Mnemonic::Xchg, false, Some(0)),
        (&XCHG_MEM_RM_REG[..], Mnemonic::Xchg, false, Some(0)),
        (&XCHG_REG_REG[..], Mnemonic::Xchg, false, None),
        (&XADD_MEM_BARE32[..], Mnemonic::Xadd, false, Some(0)),
        (&INC_MEM_BARE32[..], Mnemonic::Inc, false, Some(0)),
    ] {
        let mut dec = Decoder::with_ip(64, bytes, CODE_DECODE, DecoderOptions::NONE);
        let insn = dec.decode();
        assert_eq!(insn.mnemonic(), mnemonic, "{bytes:02x?}");
        assert_eq!(
            insn.has_lock_prefix(),
            lock_reported,
            "{bytes:02x?}: LOCK prefix presence — implicit locking is NOT visible \
             here, which is the trap this file exists for"
        );
        match mem_op {
            Some(n) => {
                assert_eq!(
                    insn.op_kind(n),
                    OpKind::Memory,
                    "{bytes:02x?}: op{n} is memory"
                );
                assert_eq!(
                    (0..insn.op_count())
                        .filter(|i| insn.op_kind(*i) == OpKind::Memory)
                        .count(),
                    1,
                    "{bytes:02x?}: exactly one memory operand"
                );
            }
            None => assert!(
                (0..insn.op_count()).all(|i| insn.op_kind(i) != OpKind::Memory),
                "{bytes:02x?}: no memory operand"
            ),
        }
        if mem_op.is_some() {
            assert_eq!(insn.memory_base(), Register::RBX, "{bytes:02x?}");
        }
    }
}

/// The load-bearing assertion of this file.
///
/// The bare `CMPXCHG` memory form must be **refused** JIT compilation and must
/// retire on the interpreter. This is the case the prefix-byte predicate got
/// backwards; the differential result alone would pass either way.
#[test]
fn cmpxchg_mem_bare_encoding_is_refused_jit_but_correct_via_iced() {
    for (label, op, id) in [
        ("cmpxchg-bare32", &CAS_MEM_BARE32[..], ID_CAS_BARE32),
        ("cmpxchg-bare64", &CAS_MEM_BARE64[..], ID_CAS_BARE64),
        ("cmpxchg-bare16", &CAS_MEM_BARE16[..], ID_CAS_BARE16),
    ] {
        let (compiled, iced_insns) = lowerability(op, id, LOCK_WORD);
        assert!(
            !compiled,
            "{label}: the memory form of CMPXCHG is implicitly locked on x86-64, \
             so the JIT must NOT compile it — has_ready_at() means the block was \
             translated, i.e. a non-atomic load/store pair"
        );
        assert!(
            iced_insns > 0,
            "{label}: a refused block must retire on the interpreter, which does a \
             real atomic CAS (saw iced_insns={iced_insns})"
        );
    }
}

/// The `F0`-prefixed form was already refused; keep it pinned so the fix cannot be
/// "achieved" by dropping the prefix check instead of adding the implicit one.
#[test]
fn cmpxchg_mem_lock_prefixed_encoding_is_refused_jit() {
    let (compiled, iced_insns) = lowerability(&CAS_MEM_LOCK32, ID_CAS_LOCK32, LOCK_WORD);
    assert!(!compiled, "lock cmpxchg mem must not be compiled");
    assert!(
        iced_insns > 0,
        "lock cmpxchg mem must retire on the interpreter"
    );
}

/// `XCHG`'s memory form is implicitly locked too, in both encodings of the
/// symmetric operand pair.
#[test]
fn xchg_mem_both_operand_orders_are_refused_jit() {
    for (label, op, id) in [
        ("xchg-mem-r32", &XCHG_MEM_BARE32[..], ID_XCHG_BARE32),
        ("xchg-mem-rm-reg", &XCHG_MEM_RM_REG[..], ID_XCHG_RM_REG),
    ] {
        let (compiled, iced_insns) = lowerability(op, id, LOCK_WORD);
        assert!(
            !compiled,
            "{label}: XCHG's memory form is implicitly locked"
        );
        assert!(iced_insns > 0, "{label}: must retire on the interpreter");
    }
}

/// The positive cases: forms with no implicit lock and no `F0` must still compile.
/// These guard the fix against over-rejecting, which would cost JIT coverage
/// silently — a `has_ready_at() == false` everywhere would look like "safe".
#[test]
fn non_implicitly_locked_memory_rmws_still_compile() {
    for (label, op, id, what) in [
        (
            "xadd-mem-bare",
            &XADD_MEM_BARE32[..],
            ID_XADD_BARE32,
            "XADD's memory form is a plain RMW without F0, exactly like an \
             unlocked ADD",
        ),
        (
            "inc-mem-bare",
            &INC_MEM_BARE32[..],
            ID_INC_BARE32,
            "INC on memory is NOT implicitly locked on x86-64",
        ),
    ] {
        let (compiled, iced_insns) = lowerability(op, id, LOCK_WORD);
        assert!(
            compiled,
            "{label}: {what} — refusing it would cost coverage"
        );
        assert_eq!(
            iced_insns, 0,
            "{label}: must retire on the JIT, not fall back to the interpreter"
        );
    }
}

/// The 64-bit bare encoding, which is the one `std::atomic<u64>` and
/// `InterlockedCompareExchange64` actually emit. Separate from the table above
/// because `plant` seeds a `u32` and this needs a full qword.
#[test]
fn bare_cmpxchg64_stays_correct_on_both_backends() {
    const ACC: u64 = 0x1122_3344_5566_7788;
    const SRC: u64 = 0xdead_beef_cafe_f00d;
    for (label, initial, acc, src, want_word, want_acc, want_zf) in [
        ("cas64-equal", ACC, ACC, SRC, SRC, ACC, true),
        ("cas64-unequal", ACC, 1, SRC, ACC, ACC, false),
    ] {
        let mut results = Vec::with_capacity(2);
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(
                &mut cpu,
                code_va(ID_CAS_BARE64),
                &CAS_MEM_BARE64,
                LOCK_WORD,
                0,
            );
            cpu.cpu()
                .mem_write(LOCK_WORD, &initial.to_le_bytes())
                .expect("seed qword");
            cpu.set_state(&ctx(code_va(ID_CAS_BARE64), LOCK_WORD, acc, src));
            assert!(
                matches!(cpu.run_rmw(), StepResultAlias::Continue),
                "{backend:?} {label}: must retire"
            );
            let mut buf = [0_u8; 8];
            cpu.cpu().mem_read(LOCK_WORD, &mut buf).expect("read qword");
            assert_eq!(
                u64::from_le_bytes(buf),
                want_word,
                "{backend:?} {label}: memory operand"
            );
            let out = cpu.cpu().snapshot_thread_context();
            assert_eq!(out.gpr[0], want_acc, "{backend:?} {label}: accumulator");
            assert_eq!(
                (u64::from(out.rflags) & 0x40 != 0),
                want_zf,
                "{backend:?} {label}: ZF"
            );
            results.push((out.gpr, u64::from(out.rflags)));
        }
        assert_eq!(results[0], results[1], "{label}: engines disagree");
    }
}

/// A register-only `XCHG` needs no memory ordering and must stay compiled.
#[test]
fn xchg_reg_reg_still_compiles() {
    let (compiled, iced_insns) = lowerability(&XCHG_REG_REG, ID_XCHG_REG_REG, LOCK_WORD);
    assert!(compiled, "xchg reg,reg must compile");
    assert_eq!(iced_insns, 0, "xchg reg,reg must retire on the JIT");
}

/// Guest-visible correctness of the refused forms, on **both** backends.
///
/// This is the guard on the result, not the defence against the bug: the refused
/// and the miscompiled cases produce the same single-threaded result. It is here
/// because "refuse it" must not degrade into "stop supporting it" — the
/// interpreter path has to still be correct, at every width, in both directions.
#[test]
fn implicitly_locked_mem_forms_stay_correct_on_both_backends() {
    for (label, op, code_base, word, initial, acc, src, want_word, want_acc, want_zf) in [
        // 32-bit equal: word takes RCX, RAX untouched, ZF set.
        (
            "cas32-equal",
            &CAS_MEM_BARE32[..],
            code_va(ID_CAS_BARE32),
            LOCK_WORD,
            5_u32,
            5,
            9,
            9,
            5,
            true,
        ),
        // 32-bit unequal: no store, RAX takes the word, ZF clear.
        (
            "cas32-unequal",
            &CAS_MEM_BARE32[..],
            code_va(ID_CAS_BARE32),
            LOCK_WORD,
            5,
            7,
            3,
            5,
            5,
            false,
        ),
        // 16-bit: only the low half is the operand, so `want_word` is the low half
        // (the high half is asserted separately by `btx`-style containment checks).
        (
            "cas16-equal",
            &CAS_MEM_BARE16[..],
            code_va(ID_CAS_BARE16),
            LOCK_WORD,
            0xabcd_0005,
            0x0005,
            0x0009,
            0x0009,
            0x0005,
            true,
        ),
        // XCHG both operand orders, and the F0 form.
        (
            "xchg-mem-r32",
            &XCHG_MEM_BARE32[..],
            code_va(ID_XCHG_BARE32),
            LOCK_WORD,
            0x1111_1111,
            0x2222_2222,
            0,
            0x2222_2222,
            0x1111_1111,
            false,
        ),
        // 87 0B has ECX in the reg field, so the value that lands in memory is
        // `src` (RCX) and the word comes back in RCX — not RAX as in the 0x03 case.
        (
            "xchg-mem-rm-reg",
            &XCHG_MEM_RM_REG[..],
            code_va(ID_XCHG_RM_REG),
            LOCK_WORD,
            0x1111_1111,
            0x2222_2222,
            0x3333_3333,
            0x3333_3333,
            0x1111_1111,
            false,
        ),
        (
            "xchg-mem-f0",
            &CAS_MEM_LOCK32[..],
            code_va(ID_CAS_LOCK32),
            LOCK_WORD,
            0x0000_0005,
            5,
            9,
            9,
            5,
            true,
        ),
    ] {
        let mut results = Vec::with_capacity(2);
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_base, op, word, initial);
            cpu.set_state(&ctx(code_base, word, acc, src));
            assert!(
                matches!(cpu.run_rmw(), StepResultAlias::Continue),
                "{backend:?} {label}: must retire"
            );
            let out = cpu.cpu().snapshot_thread_context();
            results.push((out.gpr, u64::from(out.rflags)));
            let got = read_word(&mut cpu, word);
            let seeded = initial;
            if insn_is_16bit(op) {
                // A 16-bit CAS may only touch the low half.
                assert_eq!(
                    got & 0xffff,
                    want_word & 0xffff,
                    "{backend:?} {label}: low half"
                );
                assert_eq!(
                    got & 0xffff_0000,
                    seeded & 0xffff_0000,
                    "{backend:?} {label}: the bytes above the 16-bit operand must be \
                     untouched"
                );
            } else {
                assert_eq!(got, want_word, "{backend:?} {label}: memory operand");
            }
            if label.starts_with("cas") {
                assert_eq!(out.gpr[0], want_acc, "{backend:?} {label}: accumulator");
                assert_eq!(
                    (u64::from(out.rflags) & 0x40 != 0),
                    want_zf,
                    "{backend:?} {label}: ZF"
                );
            }
        }
        assert_eq!(
            results[0], results[1],
            "{label}: engines disagree on (gpr, rflags)"
        );
    }
}

// ── Permission oracle (deterministic) ───────────────────────────────────

/// A refused-but-correct block must still be permission-checked: an implicitly
/// locked `CMPXCHG` against a read-only page faults as a **write** and changes
/// nothing. This is the same oracle the compiled-away path would have had to
/// honour, so it is worth asserting on the path that actually runs.
#[test]
fn implicitly_locked_mem_forms_fault_as_writes_on_readonly_pages() {
    for (label, op, id, size) in [
        (
            "cmpxchg-bare32",
            &CAS_MEM_BARE32[..],
            ID_CAS_BARE32,
            4_usize,
        ),
        ("cmpxchg-bare64", &CAS_MEM_BARE64[..], ID_CAS_BARE64, 8),
        ("xchg-mem-r32", &XCHG_MEM_BARE32[..], ID_XCHG_BARE32, 4),
        ("xchg-mem-rm-reg", &XCHG_MEM_RM_REG[..], ID_XCHG_RM_REG, 4),
    ] {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_va(id), op, LOCK_WORD, 0x0bad_f00d);
            cpu.cpu()
                .virtual_protect(DATA_BASE, 0x1000, protect::PAGE_READONLY)
                .expect("protect ro");
            assert!(
                cpu.cpu().host_span(LOCK_WORD, size, true).is_none(),
                "{backend:?} {label}: no host pointer for a write onto a read-only page"
            );
            // CMPXCHG stores only on the *equal* path, so the accumulator has to
            // match the word for there to be a write to fault on. XCHG always
            // writes, and passing a matching accumulator exercises that too.
            cpu.set_state(&ctx(code_va(id), LOCK_WORD, 0x0bad_f00d, 9));
            match cpu.run_rmw() {
                StepResultAlias::InvalidMemory(inv) => {
                    assert_eq!(
                        inv.access_type,
                        AccessType::Write,
                        "{backend:?} {label}: must fault as a write"
                    );
                    assert_eq!(inv.address, LOCK_WORD, "{backend:?} {label}: address");
                }
                other => panic!("{backend:?} {label}: expected a write fault, got {other:?}"),
            }
            assert_eq!(
                read_word(&mut cpu, LOCK_WORD),
                0x0bad_f00d,
                "{backend:?} {label}: a faulting CAS must not write"
            );
        }
    }
}

// ── Mutual exclusion across engines (high-signal race, not a proof) ─────

/// Threads racing the **bare** encoding — the one that used to be compiled — so a
/// regression re-admits it and this test notices.
///
/// The invariant: the word starts at `0`, the accumulator is `0`, so exactly one
/// thread can win the exchange. Every other thread must fail the compare and
/// leave the word alone. A load-then-store CAS lets several threads all observe
/// `expected` and all believe they exchanged.
fn run_bare_cmpxchg_race(word: u64, op: &[u8], id: u64, what: &str) {
    const THREADS: usize = 32;
    const TRIALS: usize = 300;

    for backend in BACKENDS {
        let mut primary = open(backend);
        plant(&mut primary, code_va(id), op, word, 0);

        // Each thread owns one slot; the main thread reads them all after `fire`,
        // so no borrow of the array outlives a spawn.
        let observed: Vec<AtomicU64> = (0..THREADS).map(|_| AtomicU64::new(0)).collect();
        let arm = Barrier::new(THREADS + 1);
        let fire = Barrier::new(THREADS + 1);
        let workers: Vec<Engine> = (0..THREADS).map(|_| open_worker(&primary)).collect();
        std::thread::scope(|scope| {
            for (tid, mut cpu) in (1..=THREADS).zip(workers) {
                let arm = &arm;
                let fire = &fire;
                let slot = &observed[tid - 1];
                scope.spawn(move || {
                    let state = ctx(code_va(id), word, 0, u64::try_from(tid).unwrap_or(u64::MAX));
                    for _ in 0..TRIALS {
                        arm.wait();
                        cpu.set_state(&state);
                        assert!(
                            matches!(cpu.run_rmw(), StepResultAlias::Continue),
                            "cmpxchg must retire"
                        );
                        // RAX keeps `expected` (0) only if this thread won.
                        slot.store(cpu.rax(), Ordering::Relaxed);
                        fire.wait();
                    }
                });
            }

            // Recorded, not asserted, inside the scope: a panic here would strand
            // every worker on the next `fire.wait()` and the failure would read as
            // a hang instead of as the race it is.
            let mut double_winner: Option<(usize, usize)> = None;
            let mut lost_write: Option<(usize, u32)> = None;
            for trial in 0..TRIALS {
                write_word(&mut primary, word, 0);
                arm.wait();
                fire.wait();

                let won = observed
                    .iter()
                    .filter(|s| s.load(Ordering::Relaxed) == 0)
                    .count();
                let final_word = read_word(&mut primary, word);
                if won != 1 && double_winner.is_none() {
                    double_winner = Some((trial, won));
                }
                if !(1..=THREADS).contains(&(final_word as usize)) && lost_write.is_none() {
                    lost_write = Some((trial, final_word));
                }
            }
            assert!(
                double_winner.is_none() && lost_write.is_none(),
                "{backend:?} {what}: {THREADS} threads x {TRIALS} trials of a single \
                 `cmpxchg` against one word found {double_winner:?} (trial, threads \
                 that all exchanged — only one may) and {lost_write:?} (trial, final \
                 word that is not any thread's id — a lost write). The CAS is not atomic."
            );
        });
    }
}

/// The bare 32-bit encoding, on an aligned word: `atomic_cas`'s host `AtomicI32`
/// arm serves this one.
#[test]
fn bare_cmpxchg32_is_mutually_exclusive_across_engines() {
    run_bare_cmpxchg_race(LOCK_WORD, &CAS_MEM_BARE32, ID_CAS_BARE32, "cmpxchg-bare32");
}

/// The bare 64-bit encoding, which is what `InterlockedCompareExchange64` and
/// `std::atomic<u64>` actually emit.
#[test]
fn bare_cmpxchg64_is_mutually_exclusive_across_engines() {
    run_bare_cmpxchg_race(LOCK_WORD, &CAS_MEM_BARE64, ID_CAS_BARE64, "cmpxchg-bare64");
}

/// The unaligned case, where `atomic_cas` can only use the striped host lock.
#[test]
fn bare_cmpxchg_unaligned_fallback_is_mutually_exclusive() {
    assert_eq!(
        UNALIGNED_WORD % 4,
        1,
        "the unaligned variant must not be 4-byte aligned"
    );
    run_bare_cmpxchg_race(
        UNALIGNED_WORD,
        &CAS_MEM_BARE32,
        ID_CAS_BARE32,
        "cmpxchg-bare32-unaligned",
    );
}

/// Harness sanity: a single thread on a matching word *must* win, otherwise the
/// "exactly one winner" invariant above is vacuous.
#[test]
fn a_single_bare_cmpxchg_on_a_matching_word_wins() {
    for backend in BACKENDS {
        let mut cpu = open(backend);
        plant(
            &mut cpu,
            code_va(ID_CAS_BARE32),
            &CAS_MEM_BARE32,
            LOCK_WORD,
            0,
        );
        cpu.set_state(&ctx(code_va(ID_CAS_BARE32), LOCK_WORD, 0, 42));
        assert!(matches!(cpu.run_rmw(), StepResultAlias::Continue));
        assert_eq!(cpu.rax(), 0, "{backend:?}: winner keeps the accumulator");
        assert_eq!(
            read_word(&mut cpu, LOCK_WORD),
            42,
            "{backend:?}: the word takes RCX"
        );
    }
}

/// `XCHG`'s memory form races the same way. The word starts at all-ones so the
/// invariant is "at most one thread sees the initial value" — the shape the
/// existing `atomic_tests` xchg race uses.
#[test]
fn bare_xchg_mem_is_mutually_exclusive_across_engines() {
    const THREADS: usize = 32;
    const TRIALS: usize = 300;
    let code_base = code_va(ID_XCHG_BARE32);

    for backend in BACKENDS {
        let mut primary = open(backend);
        plant(
            &mut primary,
            code_base,
            &XCHG_MEM_BARE32,
            LOCK_WORD,
            u32::MAX,
        );

        let observed: Vec<AtomicU64> = (0..THREADS).map(|_| AtomicU64::new(0)).collect();
        let arm = Barrier::new(THREADS + 1);
        let fire = Barrier::new(THREADS + 1);
        let workers: Vec<Engine> = (0..THREADS).map(|_| open_worker(&primary)).collect();
        std::thread::scope(|scope| {
            for (tid, mut cpu) in (1..=THREADS).zip(workers) {
                let arm = &arm;
                let fire = &fire;
                let slot = &observed[tid - 1];
                scope.spawn(move || {
                    let state = ctx(
                        code_base,
                        LOCK_WORD,
                        u64::try_from(tid).unwrap_or(u64::MAX),
                        0,
                    );
                    for _ in 0..TRIALS {
                        arm.wait();
                        cpu.set_state(&state);
                        assert!(
                            matches!(cpu.run_rmw(), StepResultAlias::Continue),
                            "xchg must retire"
                        );
                        slot.store(cpu.rax(), Ordering::Relaxed);
                        fire.wait();
                    }
                });
            }

            let mut stale_reads: Option<(usize, usize)> = None;
            for trial in 0..TRIALS {
                write_word(&mut primary, LOCK_WORD, u32::MAX);
                arm.wait();
                fire.wait();
                let first = observed
                    .iter()
                    .filter(|s| s.load(Ordering::Relaxed) == u64::from(u32::MAX))
                    .count();
                if first != 1 && stale_reads.is_none() {
                    stale_reads = Some((trial, first));
                }
            }
            assert!(
                stale_reads.is_none(),
                "{backend:?}: {THREADS} threads x {TRIALS} trials of `xchg [rbx], eax` \
                 found {stale_reads:?} (trial, threads that read the initial value — \
                 only one may). The XCHG is not atomic."
            );
        });
    }
}
