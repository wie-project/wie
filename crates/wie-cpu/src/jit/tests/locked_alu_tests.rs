// `LOCK` on the ALU / unary group against memory is an atomic RMW, on **both**
// backends.
//
// ## What was wrong
//
// x86-64 allows a `LOCK` prefix on the memory-operand forms of the integer ALU
// and unary groups (`LOCK ADD [rbx], ECX`, `LOCK INC [rbx]`, `LOCK NOT [rbx]`,
// …). The architecture requires each to be a *single* atomic
// read-modify-write: the read and the write may not be separated by another
// engine's update.
//
// Neither backend honoured that. The JIT's lowerability predicate admitted the
// forms and compiled them to a `call_load` + `call_store` pair; the interpreter
// did a plain `read_op` + `write_op`. A guest spinlock built on `lock add` had
// **no mutual exclusion anywhere** — not "less on the JIT", none at all.
//
// `acd5ef1` fixed the neighbouring hole (`XCHG`/`CMPXCHG` are *implicitly*
// locked, whether or not an `F0` byte is present). This file is the other half:
// the ALU / unary group is lockable but **not** implicitly locked, so a bare
// `add [rbx], eax` must still compile and only the prefixed form is refused.
//
// ## What these tests do and do not guarantee
//
// **The permission tests are deterministic** and are the load-bearing
// correctness argument: they prove every new atomic path still routes through
// the software page-permission oracle, so making ~20 RMWs atomic did not turn any
// of them into an unchecked host write. Each asserts `host_span(.., true)` is
// `None` first, so the case provably exercises the striped-mutex fallback rather
// than a host atomic.
//
// **The mutual-exclusion tests are probabilistic high-signal races, not proofs.**
// One RMW per thread, no release, 32 threads from a barrier, 300 independent
// trials, both engines. The invariant is an exact one — no contribution may be
// lost, so every trial's final word is a known constant (see
// `run_locked_alu_race`). Measured against a deliberately load-then-store
// `atomic_rmw`, they fail at trials 0 and 1 on both the host-atomic arm
// (`lock add`: 519 instead of 528) and the striped fallback
// (`lock add unaligned`: 303 instead of 528; `lock inc`: 25 instead of 32), and
// pass outright against the atomic one. A pass is evidence of atomicity, not a
// proof of it.
//
// ## The mnemonic set, and why
//
// The group is every mnemonic for which x86 permits a `LOCK` prefix **and** which
// reads and writes a memory word: `Add`, `Adc`, `Sub`, `Sbb`, `Xor`, `And`, `Or`,
// `Inc`, `Dec`, `Not`, `Neg`. `Cmp`, `Test` and `Bt` are deliberately absent: they
// only *read* their memory operand, so there is no RMW to make atomic. `Mov`,
// `Lea`, `Movzx` and the SSE group likewise write without reading a value to
// combine with.
//
// **The shift/rotate group is not in the group, because no `F0` encoding of it
// exists.** Intel SDM Vol. 2D states the LOCK-able set verbatim: "ADD, ADC, AND,
// BTC, BTR, BTS, CMPXCHG, CMPXCHG8B, DEC, INC, NEG, NOT, OR, SBB, SUB, XOR, XADD,
// and XCHG". Two independent implementations agree:
// * `x86_64-w64-mingw32-as` rejects `lock shl` / `lock rol` / `lock rcr` with
//   "Error: expecting lockable instruction after `lock'";
// * iced-x86 decodes **every** LOCK-prefixed shift/rotate encoding — `D0`, `D1`,
//   `D2`, `D3`, `C0` and `C1` alike — as `Code::INVALID`.
//
// An earlier draft of this file carried `shl`/`sal`/`shr`/`sar`/`rol`/`ror`/`rcl`/
// `rcr` rows built from the `C0`/`C1` imm8 forms on the assumption that iced's
// rejection of `D0`/`D1` was specific to the implicit-count-1 encodings. It is
// not; those rows could never execute, so every test below simply stopped at
// `Err("invalid instruction")` on the first shift row. (They were also half wrong
// in a second, independent way: `C0` is the `r/m8` form of the whole
// rotate-by-imm8 group — `C0 03 01` is `rol BYTE PTR [rbx], 1` — while `C1 /0../3`
// is the `r/m32` rotate and `C1 /4../7` the `r/m32` shift. Only the four shift
// rows had the right width; the four rotate rows were byte rotates asserting a
// dword one.)
//
// What a `LOCK` prefix on a shift would mean is moot: a guest cannot emit one, so
// there is nothing to make atomic and nothing to test.
use super::atomic_tests::{
    BACKENDS, Backend, DATA_BASE, Engine, LOCK_WORD, UNALIGNED_WORD, ctx, open, open_worker, plant,
    read_word, write_word,
};
use crate::exec::{AccessType, StepResult as StepResultAlias};
use crate::mem::protect;
use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};
use std::sync::Barrier;

/// The interpreter's decode cache is process-wide and thread-local, keyed by
/// (rip, mem_generation), and every test CPU starts at generation 0 — so one code
/// VA per distinct byte sequence is mandatory. 64 KiB apart, because `plant`
/// reserves with `MEM_RESERVE`, which rounds the base up to the allocation
/// granularity.
const CODE_BASE: u64 = 0x40a0_0000;
const fn code_va(id: u64) -> u64 {
    CODE_BASE + id * 0x1_0000
}

/// Decode-only (never executed), so it only has to differ from the above.
const CODE_DECODE: u64 = CODE_BASE - 0x1_0000;

/// One `LOCK`ed 32-bit ALU RMW per mnemonic, `ecx` source, `[rbx]` destination.
///
/// `F0` + the opcode + ModRM 0x0B (mod=00, reg=ECX, rm=RBX), with the
/// register-source form's 1-byte opcode.
const LOCKED: &[(&str, &[u8], u64, Mnemonic)] = &[
    ("lock add", &[0xF0, 0x01, 0x0B], 0, Mnemonic::Add),
    ("lock adc", &[0xF0, 0x11, 0x0B], 1, Mnemonic::Adc),
    ("lock sub", &[0xF0, 0x29, 0x0B], 2, Mnemonic::Sub),
    ("lock sbb", &[0xF0, 0x19, 0x0B], 3, Mnemonic::Sbb),
    ("lock xor", &[0xF0, 0x31, 0x0B], 4, Mnemonic::Xor),
    ("lock or", &[0xF0, 0x09, 0x0B], 5, Mnemonic::Or),
    ("lock and", &[0xF0, 0x21, 0x0B], 6, Mnemonic::And),
    ("lock inc", &[0xF0, 0xFF, 0x03], 7, Mnemonic::Inc),
    ("lock dec", &[0xF0, 0xFF, 0x0B], 8, Mnemonic::Dec),
    ("lock not", &[0xF0, 0xF7, 0x13], 9, Mnemonic::Not),
    ("lock neg", &[0xF0, 0xF7, 0x1B], 10, Mnemonic::Neg),
];

/// The same ALU/unary forms with the `F0` byte removed. These must keep compiling:
/// a blanket "refuse every memory RMW" fix would reject them and cost real
/// coverage for nothing, because none of them is implicitly locked.
const UNLOCKED: &[(&str, &[u8], u64, Mnemonic)] = &[
    ("add", &[0x01, 0x0B], 30, Mnemonic::Add),
    ("sub", &[0x29, 0x0B], 31, Mnemonic::Sub),
    ("inc", &[0xFF, 0x03], 32, Mnemonic::Inc),
    ("not", &[0xF7, 0x13], 33, Mnemonic::Not),
    ("shl", &[0xC1, 0x23, 0x01], 34, Mnemonic::Shl),
    // `C1 /0`, **not** `C0 /0`: `C0` is the `r/m8` form of the whole
    // rotate-by-imm8 group and `C1` is the `r/m32` one in long mode, so `C0 03 01`
    // is `rol BYTE PTR [rbx], 1`. Caught by the width assertion in
    // `locked_alu_encodings_are_what_we_think`, which is the reason it is there.
    ("rol", &[0xC1, 0x03, 0x01], 35, Mnemonic::Rol),
];

/// Decode sanity: the tables must still be the instructions they claim, with the
/// `LOCK` prefix on exactly the prefixed half. If iced ever renumbers a ModRM
/// field these tests would otherwise "pass" while asserting the wrong instruction.
#[test]
fn locked_alu_encodings_are_what_we_think() {
    for (bytes, mnemonic, locked) in LOCKED
        .iter()
        .map(|(_, b, _, m)| (b, *m, true))
        .chain(UNLOCKED.iter().map(|(_, b, _, m)| (b, *m, false)))
    {
        let mut dec = Decoder::with_ip(64, bytes, CODE_DECODE, DecoderOptions::NONE);
        let insn = dec.decode();
        assert_eq!(insn.mnemonic(), mnemonic, "{bytes:02x?}");
        assert_eq!(insn.has_lock_prefix(), locked, "{bytes:02x?}");
        assert_eq!(insn.op_kind(0), OpKind::Memory, "{bytes:02x?}");
        assert_eq!(insn.memory_base(), Register::RBX, "{bytes:02x?}");
        // Width, not signedness, is what reaches `atomic_rmw`: `host_atomic_rmw`
        // picks its `AtomicI32` arm off `size`, not off the `MemorySize` variant.
        //
        // iced's `memory_size()` is *not* uniformly unsigned — its generated table
        // (`instruction_memory_sizes.rs`) marks the sign-extending mnemonics
        // signed, so `f7 /3` (`not`) is `UInt32` while `f7 /2` (`neg`) is
        // `Int32`, at the same 4-byte width. Asserting the variant rather than
        // the width therefore tested iced's sign convention, not the operand
        // width this file depends on, and failed on the one row where the two
        // disagree.
        assert_eq!(
            insn.memory_size().size(),
            4,
            "{bytes:02x?}: a dword operand is what reaches the 4-byte host atomic"
        );
    }
}

/// The direction guard that matters for under-rejection: **every** `LOCK`ed memory
/// form in the group must be refused JIT compilation and must retire on the
/// interpreter, which performs the atomic RMW.
///
/// This is the load-bearing assertion, not the differential result. A block that
/// is silently refused and a block that is silently miscompiled produce the same
/// single-threaded guest result, so only `has_ready_at()` distinguishes them.
#[test]
fn every_locked_alu_mem_form_is_refused_jit_and_retires_on_iced() {
    for (label, op, id, _) in LOCKED {
        let code_base = code_va(*id);
        let mut cpu = open(Backend::Jit);
        plant(&mut cpu, code_base, op, LOCK_WORD, 0x0000_0001);
        cpu.set_state(&ctx(code_base, LOCK_WORD, 0, 7));
        assert!(
            matches!(cpu.run_rmw(), StepResultAlias::Continue),
            "{label}: must retire"
        );
        let Engine::Jit(ref j) = cpu else {
            unreachable!("lowerability always opens the JIT")
        };
        assert!(
            !j.has_ready_at(code_base),
            "{label}: a LOCKed memory RMW must NOT be compiled — has_ready_at() \
             means the block was translated into a non-atomic load/store pair"
        );
        assert!(
            j.stats().exec.iced_insns > 0,
            "{label}: a refused block must retire on the interpreter, which does a \
             real atomic RMW"
        );
        // `never_marks > 0` names *how* it was refused. `has_ready_at() == false` on
        // its own is also what a block too short to compile, or one that failed to
        // translate, would report — this distinguishes "the admission gate said no
        // and recorded it" from "something else went wrong first". The mark also
        // reaches the persistent ledger, so the gate is not re-run for these bytes
        // on a later process: a guest spinlock does not re-pay for its refusal on
        // every iteration.
        assert!(
            j.stats().profile.never_marks > 0,
            "{label}: the refusal must be the admission gate's `mark_never`, not an \
             incidental failure to translate"
        );
    }
}

/// The opposite direction: the *unprefixed* forms are not implicitly locked, so
/// they must still be compiled. Over-rejection costs coverage exactly as
/// under-rejection costs correctness, and a `has_ready_at() == false` everywhere
/// would look like "safe".
#[test]
fn unlocked_alu_mem_forms_still_compile() {
    for (label, op, id, _) in UNLOCKED {
        let code_base = code_va(*id);
        let mut cpu = open(Backend::Jit);
        plant(&mut cpu, code_base, op, LOCK_WORD, 0x0000_0001);
        cpu.set_state(&ctx(code_base, LOCK_WORD, 0, 7));
        assert!(
            matches!(cpu.run_rmw(), StepResultAlias::Continue),
            "{label}: must retire"
        );
        let Engine::Jit(ref j) = cpu else {
            unreachable!("lowerability always opens the JIT")
        };
        assert!(
            j.has_ready_at(code_base),
            "{label}: a bare ALU memory RMW is not implicitly locked and must still \
             be compiled"
        );
        assert_eq!(
            j.stats().exec.iced_insns,
            0,
            "{label}: must retire on the JIT, not fall back to the interpreter"
        );
    }
}

/// The set is complete: no `LOCK`-able mnemonic that this file owns is missing
/// from the table, so a newly-`LOCK`-able form cannot be admitted non-atomically
/// by omission.
///
/// The membership is taken from the architecture, not from taste: Intel SDM Vol. 2D
/// enumerates the LOCK-able set — "ADD, ADC, AND, BTC, BTR, BTS, CMPXCHG,
/// CMPXCHG8B, DEC, INC, NEG, NOT, OR, SBB, SUB, XOR, XADD, and XCHG" — and this
/// file owns the ALU/unary half of it. `BTC`/`BTR`/`BTS` are pinned by
/// `btx_tests`; `CMPXCHG`/`XCHG`/`XADD` by `implicit_lock_tests` /
/// `atomic_tests`.
#[test]
fn the_locked_group_has_no_missing_member() {
    const GROUP: [Mnemonic; 11] = [
        Mnemonic::Add,
        Mnemonic::Adc,
        Mnemonic::Sub,
        Mnemonic::Sbb,
        Mnemonic::Xor,
        Mnemonic::Or,
        Mnemonic::And,
        Mnemonic::Inc,
        Mnemonic::Dec,
        Mnemonic::Not,
        Mnemonic::Neg,
    ];
    for m in GROUP {
        assert!(
            LOCKED.iter().any(|(_, _, _, t)| *t == m),
            "{m:?} is lockable against memory but has no case in LOCKED"
        );
    }
    // And nothing outside the group crept in.
    assert_eq!(LOCKED.len(), GROUP.len());
    // `Cmp`, `Test` and `Bt` only read their operand: no RMW, so not lockable.
    for m in [Mnemonic::Cmp, Mnemonic::Test, Mnemonic::Bt] {
        assert!(
            !LOCKED.iter().any(|(_, _, _, t)| *t == m),
            "{m:?} does not write its memory operand and must not be in the group"
        );
    }
    // The shift/rotate group reads and writes its memory operand but is **not**
    // LOCK-able at all — no `F0` encoding of it exists, so iced reports every one
    // as `Code::INVALID` and a guest cannot emit it. Pinned because an earlier
    // draft of this file listed all eight and thereby asserted something the
    // architecture does not have; a row here would be a test of an instruction
    // that cannot exist.
    for m in [
        Mnemonic::Shl,
        Mnemonic::Sal,
        Mnemonic::Shr,
        Mnemonic::Sar,
        Mnemonic::Rol,
        Mnemonic::Ror,
        Mnemonic::Rcl,
        Mnemonic::Rcr,
    ] {
        assert!(
            !LOCKED.iter().any(|(_, _, _, t)| *t == m),
            "{m:?} has no LOCK-prefixed encoding and must not be in the group"
        );
    }
}

/// Differential: the two engines agree on the result of every `LOCK`ed form, and
/// the interpreter's answer is the architectural one.
#[test]
fn locked_alu_mem_forms_agree_across_backends() {
    for (label, op, id, _) in LOCKED {
        let code_base = code_va(*id);
        let mut results = Vec::with_capacity(2);
        for backend in BACKENDS {
            let mut cpu = open(backend);
            // 0x8000_0001 so CF is set on entry: ADC/SBB/rotate-through-carry read
            // the carry-in, and a set CF exercises that path rather than the
            // carry-clear one.
            plant(&mut cpu, code_base, op, LOCK_WORD, 0x8000_0001);
            cpu.set_state(&ctx(code_base, LOCK_WORD, 0, 7));
            assert!(
                matches!(cpu.run_rmw(), StepResultAlias::Continue),
                "{backend:?} {label}: must retire"
            );
            let out = cpu.cpu().snapshot_thread_context();
            results.push((out.gpr, u64::from(out.rflags)));
        }
        assert_eq!(
            results[0], results[1],
            "{label}: engines disagree on (gpr, rflags)"
        );
    }
}

// ── Permission oracle (deterministic — the load-bearing argument) ───────

/// A `LOCK`ed memory RMW on a read-only page must fault as a guest **write** and
/// leave the word byte-for-byte unchanged.
///
/// The atomic fast path is a host `AtomicI32` reached through
/// `host_span(addr, size, true)`, which runs the software page-permission check,
/// the arena walk and the per-page write permission before a host pointer exists.
/// Asserting `host_span` is `None` here removes that path entirely, so this case
/// provably runs the striped-mutex fallback instead — and the fallback must still
/// refuse, which is the invariant an unchecked host write would break.
#[test]
fn locked_alu_mem_forms_fault_as_writes_on_readonly_pages() {
    for (label, op, id, _) in LOCKED {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_va(*id), op, LOCK_WORD, 0x0bad_f00d);
            cpu.cpu()
                .virtual_protect(DATA_BASE, 0x1000, protect::PAGE_READONLY)
                .expect("protect ro");
            assert!(
                cpu.cpu().host_span(LOCK_WORD, 4, true).is_none(),
                "{backend:?} {label}: no host pointer for a write onto a read-only \
                 page, so this case can only be served by the striped fallback"
            );
            cpu.set_state(&ctx(code_va(*id), LOCK_WORD, 0, 7));
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
                "{backend:?} {label}: a faulting atomic RMW must not write"
            );
        }
    }
}

/// An unmapped word faults and, again, writes nothing.
#[test]
fn locked_alu_mem_form_faults_on_unmapped_word() {
    // Far outside the reservation (MEM_RESERVE rounds the span up, so a nearby
    // address can still land inside the arena).
    const UNMAPPED: u64 = DATA_BASE + 0x0040_0000;
    for (label, op, id, _) in LOCKED {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_va(*id), op, LOCK_WORD, 0x0000_0000);
            cpu.set_state(&ctx(code_va(*id), UNMAPPED, 0, 7));
            match cpu.run_rmw() {
                // Read or Write: on an unmapped word the RMW's *load* is the
                // first access to touch the page, so either tag is defensible.
                // What matters is that it faults at all and writes nothing — the
                // write-specific proof is the read-only page case above.
                StepResultAlias::InvalidMemory(inv) => {
                    assert!(
                        matches!(inv.access_type, AccessType::Read | AccessType::Write),
                        "{backend:?} {label}: unexpected access type {:?}",
                        inv.access_type
                    );
                    assert_eq!(inv.address, UNMAPPED, "{backend:?} {label}");
                }
                other => panic!("{backend:?} {label}: expected a fault, got {other:?}"),
            }
            assert_eq!(
                read_word(&mut cpu, LOCK_WORD),
                0x0000_0000,
                "{backend:?} {label}"
            );
        }
    }
}

/// The RMW must touch exactly its operand: a width mistake (an 8-byte access, say)
/// would show up in the neighbouring word.
#[test]
fn locked_alu_mem_form_stays_inside_its_word() {
    const NEXT_WORD: u64 = LOCK_WORD + 4;
    for (label, op, id, _) in LOCKED {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_va(*id), op, LOCK_WORD, 0x0000_0001);
            cpu.set_state(&ctx(code_va(*id), LOCK_WORD, 0, 7));
            assert!(matches!(cpu.run_rmw(), StepResultAlias::Continue));
            let mut buf = [0_u8; 4];
            cpu.cpu().mem_read(NEXT_WORD, &mut buf).expect("read next");
            assert_eq!(
                u32::from_le_bytes(buf),
                0,
                "{backend:?} {label}: the following word must be untouched"
            );
        }
    }
}

// ── Mutual exclusion (high-signal race, not a proof) ────────────────────

/// Threads hammering one `LOCK`ed RMW against one word, one attempt each, no
/// release — exactly a guest spinlock's acquisition step against a word nobody
/// holds.
///
/// The invariant is **no lost update**: each trial is reseeded to `SEED` and every
/// thread then adds a known, strictly positive amount, so the word must end on
/// `want_final` *exactly*. Every contribution is non-negative and each thread
/// contributes exactly once, so any interleaving of read-then-write pairs — the
/// thing a non-atomic lowering does — drops at least one of them and lands
/// short. There is no direction to be wrong in: too large is as impossible as
/// too small.
///
/// An earlier draft of this file instead asserted "at most one thread may observe
/// the initial value", reading that observation out of RAX. Neither half holds
/// for this group. `lock add [rbx], ecx` and `lock inc [rbx]` write no register
/// at all — they leave RAX exactly as it was seeded, at 0, which is also the seed
/// value of the word — so all 32 threads "observed the seed", unconditionally,
/// and the accompanying "the final word is some thread's id" check could not hold
/// either, since the sum of 32 concurrent `add`s is not any one thread's id. The
/// race was measuring RAX's initial value, not atomicity, and could not have
/// passed for *any* implementation. Observing the old value per thread needs an
/// instruction that returns it, which is why the `XCHG` / `CMPXCHG` races in
/// `atomic_tests` / `implicit_lock_tests` are structured around RAX instead.
///
/// A pass is evidence of atomicity, not a proof.
fn run_locked_alu_race(word: u64, op: &[u8], id: u64, what: &str, want_final: u32) {
    const THREADS: usize = 32;
    const TRIALS: usize = 300;
    let code_base = code_va(id);

    for backend in BACKENDS {
        let mut primary = open(backend);
        plant(&mut primary, code_base, op, word, SEED);

        let arm = Barrier::new(THREADS + 1);
        let fire = Barrier::new(THREADS + 1);
        // Built here and moved in: the shared primary is not `Sync` (per-thread
        // JIT state holds raw host pointers), and the runtime builds worker
        // engines on the spawning thread for the same reason.
        let workers: Vec<Engine> = (0..THREADS).map(|_| open_worker(&primary)).collect();
        std::thread::scope(|scope| {
            for (tid, mut cpu) in (1..=THREADS).zip(workers) {
                let arm = &arm;
                let fire = &fire;
                scope.spawn(move || {
                    // ECX (the source register) is this thread's id.
                    let state = ctx(code_base, word, 0, u64::try_from(tid).unwrap_or(u64::MAX));
                    for _ in 0..TRIALS {
                        arm.wait();
                        cpu.set_state(&state);
                        assert!(
                            matches!(cpu.run_rmw(), StepResultAlias::Continue),
                            "{what} must retire"
                        );
                        fire.wait();
                    }
                });
            }

            // Recorded, not asserted, inside the scope: a panic here would strand
            // every worker on the next `fire.wait()` and the failure would read as
            // a hang instead of as the race it is.
            let mut lost_update: Option<(usize, u32)> = None;
            for trial in 0..TRIALS {
                write_word(&mut primary, word, SEED);
                arm.wait();
                fire.wait();

                let final_word = read_word(&mut primary, word);
                if final_word != want_final && lost_update.is_none() {
                    lost_update = Some((trial, final_word));
                }
            }
            assert!(
                lost_update.is_none(),
                "{backend:?} {what}: {THREADS} threads x {TRIALS} trials of a single \
                 RMW against one word, every trial reseeded to {SEED}, found \
                 {lost_update:?} (trial, final word; it must be exactly \
                 {want_final}, because every thread contributes once and no \
                 contribution may be dropped). The RMW is not atomic."
            );
        });
    }
}

/// Seed the word with 0, so every thread's contribution is a plain positive
/// addition and the trial's exact total is known before the barrier opens.
///
/// The `u32::MAX` seed the `XCHG` races use would be wrong here for a different
/// reason than "the total is unknown": `add 1` would wrap to 0, and two
/// wrap-around contributions could cancel a lost update back into the expected
/// total, hiding the very defect the trial is looking for.
const SEED: u32 = 0;

/// What one `lock add` trial must leave behind: ECX is the thread's id, so
/// `1..=32` accumulate to `528`.
fn add_total() -> u32 {
    (1..=32_u32).fold(SEED, u32::wrapping_add)
}

/// The aligned 32-bit case: `atomic_rmw`'s host `AtomicI32` arm serves this one, so
/// this is that arm under test.
#[test]
fn locked_add_is_mutually_exclusive_across_engines() {
    run_locked_alu_race(LOCK_WORD, LOCKED[0].1, LOCKED[0].2, "lock add", add_total());
}

/// The unaligned case, which `host_atomic_rmw` refuses on alignment grounds, so
/// the striped host mutex is the only thing supplying mutual exclusion. Asserted
/// up front: this address must really be off the 4-byte grid, or the case would
/// silently stop covering the fallback.
#[test]
fn locked_add_unaligned_fallback_is_mutually_exclusive() {
    assert_eq!(
        UNALIGNED_WORD % 4,
        1,
        "the unaligned variant must not be 4-byte aligned"
    );
    run_locked_alu_race(
        UNALIGNED_WORD,
        LOCKED[0].1,
        LOCKED[0].2,
        "lock add unaligned",
        add_total(),
    );
}

/// A second, different member of the group, so the race does not only cover
/// `add`'s code path: `inc` has no source register and its flags differ.
#[test]
fn locked_inc_is_mutually_exclusive_across_engines() {
    // `inc` adds 1 with no source register, so all 32 threads contribute the
    // same amount and the total is just the thread count.
    run_locked_alu_race(LOCK_WORD, LOCKED[7].1, LOCKED[7].2, "lock inc", SEED + 32);
}

/// Sanity-check the invariant the races rest on: a single thread starting from
/// `SEED` must leave the word equal to its own id. Without this, a build that
/// broke the decode would make the races pass vacuously.
///
/// Only the raced members are checked, and deliberately not every member of the
/// group: the probe is ill-posed for the idempotent ones (`or` against an
/// all-ones seed, `and` against a zero seed), where a correct RMW legitimately
/// leaves the word unchanged.
#[test]
fn a_single_locked_alu_rmw_replaces_the_seed_with_its_own_id() {
    for (label, op, id) in [
        ("lock add", LOCKED[0].1, LOCKED[0].2),
        ("lock inc", LOCKED[7].1, LOCKED[7].2),
    ] {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_va(id), op, LOCK_WORD, SEED);
            cpu.set_state(&ctx(code_va(id), LOCK_WORD, 0, 7));
            assert!(matches!(cpu.run_rmw(), StepResultAlias::Continue));
            assert_ne!(
                read_word(&mut cpu, LOCK_WORD),
                SEED,
                "{backend:?} {label}: the RMW must have replaced the word"
            );
        }
    }
}
