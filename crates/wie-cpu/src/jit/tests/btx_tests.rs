// `LOCK BTS`/`BTR`/`BTC` against memory must be a real read-modify-write.
//
// `e09e22c` made `XCHG` and `CMPXCHG` on memory atomic, and the JIT's
// `lock_rmw_mem_is_lowerable` deliberately *rejects* locked memory `BTX` so those
// blocks fall back to the interpreter. That left the interpreter as the only path
// for a locked memory `BTX` — and `exec_bit` ignored the `LOCK` prefix, doing a
// plain `mem.read` followed by `write_mem_value`. So the one remaining atomicity
// hole for those instructions sat on the single path nothing else covered.
//
// The fix reuses `atomic_rmw` (the striped-mutex / `host_span` machinery added with
// XCHG/CMPXCHG) rather than adding a mechanism, which is what keeps the software
// page-permission oracle on every path.
//
// ## Operand widths, because it decides which paths are reachable
//
// `0F BA /5 ib` is ambiguous in the ISA and resolves on the operand size: with no
// `0x66` it is the **32-bit** form, with `0x66` the 16-bit form. The 8-bit memory
// form is not reachable with an immediate at all (it takes its index from CL), so
// there is no "always one byte" case to lean on:
//
// * a locked **32-bit** `BTX` on a 4-byte-aligned address *is* served by
//   `atomic_rmw`'s host SeqCst `AtomicI32`;
// * a locked **16-bit** `BTX`, or an unaligned one, always takes the striped host
//   mutex.
//
// Both are covered below, and the permission tests assert `host_span(.., true)` is
// `None` before running so a read-only page proves the fallback — not a host
// pointer — is what is being exercised.
//
// Split out of `atomic_tests.rs` (file-size policy, ADR-002); it reuses that
// file's dual-engine harness rather than duplicating it.
use super::atomic_tests::{
    BACKENDS, DATA_BASE, Engine, LOCK_WORD, UNALIGNED_WORD, ctx, open, open_worker, plant,
    read_word, write_word,
};
use super::*;
use crate::exec::{AccessType, StepResult as StepResultAlias};
use crate::regs::Rflags;
use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};
use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, Ordering};

/// `lock bts dword ptr [rbx], 0` — F0 0F BA /5 ib. ModRM 0x2B is mod=00,
/// reg=101 (/5), rm=011 (RBX); the trailing byte is the bit index.
const LOCK_BTS32: [u8; 5] = [0xF0, 0x0F, 0xBA, 0x2B, 0x00];
/// `lock btr dword ptr [rbx], 0` — F0 0F BA /6, ModRM 0x33.
const LOCK_BTR32: [u8; 5] = [0xF0, 0x0F, 0xBA, 0x33, 0x00];
/// `lock btc dword ptr [rbx], 0` — F0 0F BA /7, ModRM 0x3B.
const LOCK_BTC32: [u8; 5] = [0xF0, 0x0F, 0xBA, 0x3B, 0x00];
/// `lock bts word ptr [rbx], 0` — F0 66 0F BA /5. A sub-word operand has no host
/// atomic, so this always takes the striped fallback.
const LOCK_BTS16: [u8; 6] = [0xF0, 0x66, 0x0F, 0xBA, 0x2B, 0x00];
/// The same three 32-bit forms without the `F0` prefix.
const BTS32: [u8; 4] = [0x0F, 0xBA, 0x2B, 0x00];
const BTR32: [u8; 4] = [0x0F, 0xBA, 0x33, 0x00];
const BTC32: [u8; 4] = [0x0F, 0xBA, 0x3B, 0x00];
/// `bts word ptr [rbx], 0` — 66 0F BA /5.
const BTS16: [u8; 5] = [0x66, 0x0F, 0xBA, 0x2B, 0x00];
/// `bt dword ptr [rbx], 0` — 0F BA /4 ib, ModRM 0x23. Reads CF, never writes.
const BT32: [u8; 4] = [0x0F, 0xBA, 0x23, 0x00];

/// Base of the per-encoding code pages, and one page per distinct byte sequence.
///
/// The interpreter's decode cache is process-wide and thread-local, keyed by
/// (rip, mem_generation), and every test CPU here starts at generation 0 — so two
/// *different* byte sequences that share a VA make the second case silently
/// execute the first case's instruction. The bit index is an immediate in the
/// encoding, so it is part of the byte sequence: every index used below gets its
/// own id. Pages are 64 KiB apart because `plant` reserves with `MEM_RESERVE`,
/// which rounds the base up to the allocation granularity.
const CODE_BASE: u64 = 0x4040_0000;
const fn code_va(id: u64) -> u64 {
    CODE_BASE + id * 0x1_0000
}

const ID_LOCK_BTS32: u64 = 0;
const ID_LOCK_BTR32: u64 = 1;
const ID_LOCK_BTC32: u64 = 2;
const ID_LOCK_BTS16: u64 = 3;
const ID_BTS32: u64 = 4;
const ID_BTR32: u64 = 5;
const ID_BTC32: u64 = 6;
const ID_BTS16: u64 = 7;
const ID_BT32: u64 = 8;
const ID_BTS32_8: u64 = 9;
const ID_BTS32_31: u64 = 10;
const ID_BTS32_32: u64 = 11;
const ID_BTR32_8: u64 = 12;
const ID_BTC32_8: u64 = 13;
const ID_LOCK_BTS32_8: u64 = 14;
const ID_LOCK_BTC32_8: u64 = 15;
const ID_BTC32_31: u64 = 16;
/// Decode-only (never executed), so it only has to differ from the above.
const CODE_DECODE: u64 = 0x4040_0000 - 0x1_0000;

/// `op` with its trailing `imm8` bit index replaced.
fn with_index(op: &[u8], index: u8) -> Vec<u8> {
    let mut code = op.to_vec();
    let last = code.len().saturating_sub(1);
    if let Some(slot) = code.get_mut(last) {
        *slot = index;
    }
    code
}

/// CF after the BTX, read the way a guest would.
fn cf(primary: &mut Engine) -> bool {
    let rflags = primary.cpu().snapshot_thread_context().rflags;
    (rflags & Rflags::CF) == Rflags::CF
}

fn read_byte(primary: &mut Engine, addr: u64) -> u8 {
    let mut buf = [0_u8; 1];
    primary.cpu().mem_read(addr, &mut buf).expect("read byte");
    buf[0]
}

/// Plant `op` with bit `index`, run it once, and return the resulting CF.
///
/// `id` must identify the exact `(op, index)` pair — see [`code_va`]. `initial`
/// seeds the operand under test; every byte of the `plant` word matters for a
/// wider `BTX`, so the cases below set all of them.
fn run_btx(primary: &mut Engine, op: &[u8], id: u64, word: u64, index: u8, initial: u32) -> bool {
    let code_base = code_va(id);
    plant(primary, code_base, op, word, initial);
    let code = with_index(op, index);
    primary.cpu().mem_write(code_base, &code).expect("write op");
    primary.set_state(&ctx(code_base, word, 0, 0));
    assert!(
        matches!(primary.run_rmw(), StepResultAlias::Continue),
        "BTX must retire"
    );
    cf(primary)
}

// ── Encoding sanity (deterministic) ─────────────────────────────────────

/// The bytes above must still be the instructions these tests claim, with the
/// `LOCK` prefix present on exactly the ones that carry it and the operand width
/// these tests depend on. The width assertions matter: they are what decides
/// whether a locked case can reach `atomic_rmw`'s host atomic or is forced onto the
/// striped fallback.
#[test]
fn btx_test_encodings_are_what_we_think() {
    for (bytes, mnemonic, locked, size) in [
        (
            &LOCK_BTS32[..],
            Mnemonic::Bts,
            true,
            iced_x86::MemorySize::UInt32,
        ),
        (
            &LOCK_BTR32[..],
            Mnemonic::Btr,
            true,
            iced_x86::MemorySize::UInt32,
        ),
        (
            &LOCK_BTC32[..],
            Mnemonic::Btc,
            true,
            iced_x86::MemorySize::UInt32,
        ),
        (
            &LOCK_BTS16[..],
            Mnemonic::Bts,
            true,
            iced_x86::MemorySize::UInt16,
        ),
        (
            &BTS32[..],
            Mnemonic::Bts,
            false,
            iced_x86::MemorySize::UInt32,
        ),
        (
            &BTR32[..],
            Mnemonic::Btr,
            false,
            iced_x86::MemorySize::UInt32,
        ),
        (
            &BTC32[..],
            Mnemonic::Btc,
            false,
            iced_x86::MemorySize::UInt32,
        ),
        (
            &BTS16[..],
            Mnemonic::Bts,
            false,
            iced_x86::MemorySize::UInt16,
        ),
        (&BT32[..], Mnemonic::Bt, false, iced_x86::MemorySize::UInt32),
    ] {
        let mut dec = Decoder::with_ip(64, bytes, CODE_DECODE, DecoderOptions::NONE);
        let insn = dec.decode();
        assert_eq!(insn.mnemonic(), mnemonic, "{bytes:02x?}");
        assert_eq!(
            insn.has_lock_prefix(),
            locked,
            "{bytes:02x?}: LOCK prefix presence"
        );
        assert_eq!(insn.op_kind(0), OpKind::Memory, "{bytes:02x?}");
        assert_eq!(insn.op_kind(1), OpKind::Immediate8, "{bytes:02x?}");
        assert_eq!(
            insn.memory_size(),
            size,
            "{bytes:02x?}: 0F BA /5 ib is the 32-bit form without an override and \
             the 16-bit form with one — there is no 8-bit immediate form"
        );
        assert_eq!(insn.memory_base(), Register::RBX, "{bytes:02x?}");
    }
}

// ── Single-threaded semantics (deterministic) ───────────────────────────

/// Locked and unlocked `BTX` set/clear/flip the bit, report CF from the bit's value
/// *before* the update, and touch nothing else — on both backends, at both
/// operand widths.
///
/// Two properties are under test here. First, CF-from-the-old-value: the locked
/// path must take the old operand from the RMW itself, not from a separate
/// pre-load that could observe a different value. Second, the bit index is modulo
/// the **operand width**, not byte-granular: index 8 in a dword is bit 8 of the
/// dword at the EA, not bit 0 of the byte at `EA + 1`. That second one is a real
/// interpreter bug the JIT never had, so it is what makes the dword cases with an
/// index ≥ 8 differential rather than merely self-consistent.
#[test]
fn btx_mem_updates_the_bit_and_reports_cf_on_both_backends() {
    for (label, op, id, word, index, initial, want, want_cf) in [
        (
            "bts32-set",
            &BTS32[..],
            ID_BTS32,
            LOCK_WORD,
            0_u8,
            0x0000_0000_u32,
            0x0000_0001,
            false,
        ),
        (
            "bts32-set-already",
            &BTS32[..],
            ID_BTS32,
            LOCK_WORD,
            0,
            0x0000_0001,
            0x0000_0001,
            true,
        ),
        (
            "bts32-neighbour",
            &BTS32[..],
            ID_BTS32,
            LOCK_WORD,
            0,
            0xffff_fff0,
            0xffff_fff1,
            false,
        ),
        // Index 8: byte-granular addressing would have written EA+1 instead.
        (
            "bts32-index8",
            &BTS32[..],
            ID_BTS32_8,
            LOCK_WORD,
            8,
            0x0000_0000,
            0x0000_0100,
            false,
        ),
        (
            "bts32-index31",
            &BTS32[..],
            ID_BTS32_31,
            LOCK_WORD,
            31,
            0x0000_0000,
            0x8000_0000,
            false,
        ),
        // The index is masked to 5 bits for a 32-bit operand, so 32 wraps to bit 0
        // of the same dword.
        (
            "bts32-index32-wraps",
            &BTS32[..],
            ID_BTS32_32,
            LOCK_WORD,
            32,
            0x0000_0000,
            0x0000_0001,
            false,
        ),
        (
            "btr32-clear",
            &BTR32[..],
            ID_BTR32,
            LOCK_WORD,
            0,
            0x0000_0001,
            0x0000_0000,
            true,
        ),
        (
            "btr32-clear-already",
            &BTR32[..],
            ID_BTR32,
            LOCK_WORD,
            0,
            0x0000_0000,
            0x0000_0000,
            false,
        ),
        (
            "btr32-index8",
            &BTR32[..],
            ID_BTR32_8,
            LOCK_WORD,
            8,
            0x0000_0100,
            0x0000_0000,
            true,
        ),
        (
            "btc32-flip-on",
            &BTC32[..],
            ID_BTC32,
            LOCK_WORD,
            0,
            0x0000_0000,
            0x0000_0001,
            false,
        ),
        (
            "btc32-flip-off",
            &BTC32[..],
            ID_BTC32,
            LOCK_WORD,
            0,
            0x0000_0001,
            0x0000_0000,
            true,
        ),
        (
            "btc32-index8",
            &BTC32[..],
            ID_BTC32_8,
            LOCK_WORD,
            8,
            0x0000_0100,
            0x0000_0000,
            true,
        ),
        (
            "btc32-index31",
            &BTC32[..],
            ID_BTC32_31,
            LOCK_WORD,
            31,
            0x8000_0000,
            0x0000_0000,
            true,
        ),
        // Locked, same answers — these are the ones that must go through the RMW.
        (
            "lock-bts32-set",
            &LOCK_BTS32[..],
            ID_LOCK_BTS32,
            LOCK_WORD,
            0,
            0x0000_0000,
            0x0000_0001,
            false,
        ),
        (
            "lock-bts32-set-already",
            &LOCK_BTS32[..],
            ID_LOCK_BTS32,
            LOCK_WORD,
            0,
            0x0000_0001,
            0x0000_0001,
            true,
        ),
        (
            "lock-bts32-neighbour",
            &LOCK_BTS32[..],
            ID_LOCK_BTS32,
            LOCK_WORD,
            0,
            0xffff_fff0,
            0xffff_fff1,
            false,
        ),
        (
            "lock-bts32-index8",
            &LOCK_BTS32[..],
            ID_LOCK_BTS32_8,
            LOCK_WORD,
            8,
            0x0000_0000,
            0x0000_0100,
            false,
        ),
        (
            "lock-btr32-clear",
            &LOCK_BTR32[..],
            ID_LOCK_BTR32,
            LOCK_WORD,
            0,
            0x0000_0001,
            0x0000_0000,
            true,
        ),
        (
            "lock-btc32-flip-on",
            &LOCK_BTC32[..],
            ID_LOCK_BTC32,
            LOCK_WORD,
            0,
            0x0000_0000,
            0x0000_0001,
            false,
        ),
        (
            "lock-btc32-index8",
            &LOCK_BTC32[..],
            ID_LOCK_BTC32_8,
            LOCK_WORD,
            8,
            0x0000_0100,
            0x0000_0000,
            true,
        ),
        // 16-bit: sub-word, so the locked arm is forced onto the striped fallback.
        (
            "bts16-set",
            &BTS16[..],
            ID_BTS16,
            LOCK_WORD,
            0,
            0x0000_0000,
            0x0000_0001,
            false,
        ),
        (
            "bts16-neighbour",
            &BTS16[..],
            ID_BTS16,
            LOCK_WORD,
            0,
            0x0000_fff0,
            0x0000_fff1,
            false,
        ),
        (
            "lock-bts16-set",
            &LOCK_BTS16[..],
            ID_LOCK_BTS16,
            LOCK_WORD,
            0,
            0x0000_0000,
            0x0000_0001,
            false,
        ),
        (
            "lock-bts16-neighbour",
            &LOCK_BTS16[..],
            ID_LOCK_BTS16,
            LOCK_WORD,
            0,
            0x0000_fff0,
            0x0000_fff1,
            false,
        ),
    ] {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            let got_cf = run_btx(&mut cpu, op, id, word, index, initial);
            assert_eq!(
                read_word(&mut cpu, word),
                want,
                "{backend:?} {label}: operand"
            );
            assert_eq!(got_cf, want_cf, "{backend:?} {label}: CF");
        }
    }
}

/// The engines must agree, and the JIT must be doing the work exactly where it is
/// allowed to.
///
/// Unlocked memory `BTX` is `alu_is_lowerable`, so it compiles and this really
/// exercises the JIT's BTX lowering. Locked memory `BTX` is rejected by
/// `lock_rmw_mem_is_lowerable`, so the JIT arm must fall back to iced. Asserting
/// that split here is what stops the locked cases from silently "passing" on a
/// block some future change might admit *without* a native atomic lowering — the
/// exact regression `e09e22c` was written to prevent.
#[test]
fn btx_mem_engines_agree_and_the_jit_split_is_the_intended_one() {
    for (label, op, id, locked, index) in [
        ("bts32", &BTS32[..], ID_BTS32, false, 0_u8),
        ("bts32-index8", &BTS32[..], ID_BTS32_8, false, 8),
        ("btr32", &BTR32[..], ID_BTR32, false, 0),
        ("btc32", &BTC32[..], ID_BTC32, false, 0),
        ("btc32-index8", &BTC32[..], ID_BTC32_8, false, 8),
        ("lock-bts32", &LOCK_BTS32[..], ID_LOCK_BTS32, true, 0),
        (
            "lock-bts32-index8",
            &LOCK_BTS32[..],
            ID_LOCK_BTS32_8,
            true,
            8,
        ),
        ("lock-btr32", &LOCK_BTR32[..], ID_LOCK_BTR32, true, 0),
        ("lock-btc32", &LOCK_BTC32[..], ID_LOCK_BTC32, true, 0),
    ] {
        let code_base = code_va(id);
        let mut results = Vec::with_capacity(2);
        let mut jit_fell_back = Vec::with_capacity(1);
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_base, op, LOCK_WORD, 0x5a_0f_0f_5a);
            let code = with_index(op, index);
            cpu.cpu().mem_write(code_base, &code).expect("write op");
            cpu.set_state(&ctx(code_base, LOCK_WORD, 0, 0));
            assert!(
                matches!(cpu.run_rmw(), StepResultAlias::Continue),
                "{backend:?} {label}: must retire"
            );
            let out = cpu.cpu().snapshot_thread_context();
            results.push((u64::from(out.rflags), read_word(&mut cpu, LOCK_WORD)));
            if let Engine::Jit(ref j) = cpu {
                jit_fell_back.push(!j.has_ready_at(code_base));
            }
        }
        assert_eq!(
            results[0], results[1],
            "{label}: engines disagree on (rflags, operand) — iced vs jit"
        );
        if let Some(&fell_back) = jit_fell_back.first() {
            assert_eq!(
                fell_back, locked,
                "{label}: the JIT must compile the unlocked form and reject the \
                 locked one (observed fell_back={fell_back}, locked={locked})"
            );
        }
    }
}

/// Plain `BT` reads CF and writes nothing. `LOCK` is invalid on `BT`
/// architecturally, so only the unlocked form is exercised — but the point is that
/// routing BTX through an RMW did not turn `BT` into a store.
#[test]
fn bt_mem_is_read_only_on_both_backends() {
    for backend in BACKENDS {
        let mut cpu = open(backend);
        let got_cf = run_btx(&mut cpu, &BT32, ID_BT32, LOCK_WORD, 0, 0b1010_0001);
        assert_eq!(
            read_word(&mut cpu, LOCK_WORD),
            0b1010_0001,
            "{backend:?}: BT must not write"
        );
        assert!(got_cf, "{backend:?}: bit 0 was set");
    }
}

/// A sub-word `BTX` must leave the bytes outside its operand alone, and a locked one
/// must do so on the striped-fallback path — where `atomic_rmw` is reading and
/// writing exactly the operand width, not the whole word.
#[test]
fn btx_mem_touches_exactly_its_operand() {
    // High half of the plant word, untouched by a 16-bit BTX at `LOCK_WORD`.
    const HIGH_HALF: u32 = 0xabcd_0000;
    for (label, op, id, initial, want) in [
        (
            "lock-bts16",
            &LOCK_BTS16[..],
            ID_LOCK_BTS16,
            0x0000_0000_u32,
            0x0000_0001,
        ),
        (
            "lock-bts16-neighbour",
            &LOCK_BTS16[..],
            ID_LOCK_BTS16,
            0x0000_fff0,
            0x0000_fff1,
        ),
    ] {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            run_btx(&mut cpu, op, id, LOCK_WORD, 0, initial | HIGH_HALF);
            let got = read_word(&mut cpu, LOCK_WORD);
            assert_eq!(got & 0xffff, want & 0xffff, "{backend:?} {label}: low half");
            assert_eq!(
                got & 0xffff_0000,
                HIGH_HALF,
                "{backend:?} {label}: the bytes above the 16-bit operand must be \
                 untouched"
            );
        }
    }
}

// ── Permission oracle (deterministic — the load-bearing argument) ───────

/// A locked `BTX` on a read-only page must fault as a guest **write** and leave the
/// word byte-for-byte unchanged.
///
/// This is the case that proves the atomic path did not become an escape hatch. A
/// read-only page removes the host pointer outright — asserted — so the RMW can
/// only have gone through the striped fallback, and a fallback that skipped the
/// software check would store the word instead of faulting.
#[test]
fn locked_btx_mem_faults_on_readonly_page_without_bypassing_the_write_check() {
    for (label, op, code_base, size) in [
        (
            "lock-bts32",
            &LOCK_BTS32[..],
            code_va(ID_LOCK_BTS32),
            4_usize,
        ),
        ("lock-btr32", &LOCK_BTR32[..], code_va(ID_LOCK_BTR32), 4),
        ("lock-btc32", &LOCK_BTC32[..], code_va(ID_LOCK_BTC32), 4),
        // The sub-word case, which can only ever be the striped fallback.
        ("lock-bts16", &LOCK_BTS16[..], code_va(ID_LOCK_BTS16), 2),
    ] {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_base, op, LOCK_WORD, 0x0bad_f00d);
            cpu.cpu()
                .virtual_protect(DATA_BASE, 0x1000, protect::PAGE_READONLY)
                .expect("protect ro");
            assert!(
                cpu.cpu().host_span(LOCK_WORD, size, true).is_none(),
                "{backend:?} {label}: no host pointer for a write onto a read-only \
                 page, so this case can only be served by the striped fallback"
            );
            cpu.set_state(&ctx(code_base, LOCK_WORD, 0, 0));
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
                "{backend:?} {label}: a faulting locked BTX must not write"
            );
        }
    }
}

/// The unlocked form must not lose its write-permission check either: it shares
/// the checked `read_mem_value`/`write_mem_value` pair, and an unlocked `BTX` is
/// still a store.
#[test]
fn unlocked_btx_mem_faults_on_readonly_page() {
    for (label, op, code_base) in [
        ("bts32", &BTS32[..], code_va(ID_BTS32)),
        ("btr32", &BTR32[..], code_va(ID_BTR32)),
        ("btc32", &BTC32[..], code_va(ID_BTC32)),
        ("bts16", &BTS16[..], code_va(ID_BTS16)),
    ] {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_base, op, LOCK_WORD, 0x0000_0000);
            cpu.cpu()
                .virtual_protect(DATA_BASE, 0x1000, protect::PAGE_READONLY)
                .expect("protect ro");
            cpu.set_state(&ctx(code_base, LOCK_WORD, 0, 0));
            match cpu.run_rmw() {
                StepResultAlias::InvalidMemory(inv) => {
                    assert_eq!(
                        inv.access_type,
                        AccessType::Write,
                        "{backend:?} {label}: must fault as a write"
                    );
                }
                other => panic!("{backend:?} {label}: expected a write fault, got {other:?}"),
            }
            assert_eq!(
                read_word(&mut cpu, LOCK_WORD),
                0x0000_0000,
                "{backend:?} {label}: a faulting BTX must not write"
            );
        }
    }
}

/// An unmapped word faults and, again, writes nothing.
#[test]
fn locked_btx_mem_faults_on_unmapped_word() {
    // Far outside the reservation (MEM_RESERVE rounds the span up, so a nearby
    // address can still land inside the arena).
    const UNMAPPED: u64 = DATA_BASE + 0x0040_0000;
    for backend in BACKENDS {
        let mut cpu = open(backend);
        plant(
            &mut cpu,
            code_va(ID_LOCK_BTS32),
            &LOCK_BTS32,
            LOCK_WORD,
            0x0000_0000,
        );
        cpu.set_state(&ctx(code_va(ID_LOCK_BTS32), UNMAPPED, 0, 0));
        match cpu.run_rmw() {
            // Read or Write: on an unmapped word the RMW's *load* is the first
            // access to touch the page, so either tag is defensible. What matters is
            // that it faults at all and writes nothing — the write-specific proof is
            // the read-only page case above.
            StepResultAlias::InvalidMemory(inv) => {
                assert!(
                    matches!(inv.access_type, AccessType::Read | AccessType::Write),
                    "{backend:?}: unexpected access type {:?}",
                    inv.access_type
                );
                assert_eq!(inv.address, UNMAPPED, "{backend:?}");
            }
            other => panic!("{backend:?}: expected a fault, got {other:?}"),
        }
        assert_eq!(read_word(&mut cpu, LOCK_WORD), 0x0000_0000, "{backend:?}");
    }
}

// ── Mutual exclusion (high-signal race, not a proof) ────────────────────

/// Threads hammering one locked `BTS` against one word, one attempt each, no
/// release — exactly a guest spinlock's acquisition attempt against a word nobody
/// holds.
///
/// The invariant a load-then-store RMW violates loudly: the word starts clear, so
/// **at most one** thread can observe CF=0 (bit 0 still clear). Every other thread
/// must have read a word that already had the bit set, which is only true if the
/// read is ordered against the write. A non-atomic version lets several threads
/// read the same old word and all believe they were first.
///
/// A pass is evidence of atomicity, not a proof of it — see `atomic_tests.rs`.
fn run_locked_bts_race(word: u64, op: &[u8], id: u64, what: &str) {
    let code_base = code_va(id);
    const THREADS: usize = 32;
    const TRIALS: usize = 300;

    for backend in BACKENDS {
        let mut primary = open(backend);
        plant(&mut primary, code_base, op, word, 0);

        // Atomic slots: each thread owns one, the main thread reads them all after
        // the `fire` barrier, so no borrow of the array outlives a spawn.
        let observed: Vec<AtomicU64> = (0..THREADS).map(|_| AtomicU64::new(0)).collect();
        let arm = Barrier::new(THREADS + 1);
        let fire = Barrier::new(THREADS + 1);
        // Built here and moved in: the shared primary is not `Sync` (per-thread JIT
        // state holds raw host pointers), and the runtime builds worker engines on
        // the spawning thread for the same reason.
        let workers: Vec<Engine> = (0..THREADS).map(|_| open_worker(&primary)).collect();
        std::thread::scope(|scope| {
            for (tid, mut cpu) in (1..=THREADS).zip(workers) {
                let arm = &arm;
                let fire = &fire;
                let slot = &observed[tid - 1];
                scope.spawn(move || {
                    let state = ctx(code_base, word, 0, 0);
                    for _ in 0..TRIALS {
                        arm.wait();
                        cpu.set_state(&state);
                        assert!(
                            matches!(cpu.run_rmw(), StepResultAlias::Continue),
                            "lock bts must retire"
                        );
                        // CF=0 means this thread saw the bit still clear.
                        slot.store(u64::from(!cf(&mut cpu)), Ordering::Relaxed);
                        fire.wait();
                    }
                });
            }

            // Recorded, not asserted, inside the scope: a panic here would strand
            // every worker on the next `fire.wait()` and the failure would read as a
            // hang instead of as the race it is.
            let mut double_first: Option<(usize, usize)> = None;
            for trial in 0..TRIALS {
                write_word(&mut primary, word, 0);
                arm.wait();
                fire.wait();

                let first = observed
                    .iter()
                    .filter(|slot| slot.load(Ordering::Relaxed) == 1)
                    .count();
                if first != 1 && double_first.is_none() {
                    double_first = Some((trial, first));
                }
            }
            assert!(
                double_first.is_none(),
                "{backend:?} {what}: {THREADS} threads x {TRIALS} trials of a \
                 single `lock bts` against one word found {double_first:?} (trial, \
                 threads that all saw bit 0 clear and 'set it first' — only one may). \
                 The BTX is not atomic."
            );
        });
    }
}

/// The aligned 32-bit case: `atomic_rmw` can serve this one with a host
/// `AtomicI32`, so this is the host-atomic arm under test.
#[test]
fn locked_bts32_is_mutually_exclusive_across_engines() {
    run_locked_bts_race(LOCK_WORD, &LOCK_BTS32, ID_LOCK_BTS32, "lock-bts32");
}

/// The unaligned case, which `host_atomic_rmw` refuses on alignment grounds, so
/// the striped host mutex is the only thing supplying mutual exclusion. Asserted up
/// front: this address must really be off the 4-byte grid, or the case would
/// silently stop covering the fallback.
#[test]
fn locked_bts32_unaligned_fallback_is_mutually_exclusive() {
    assert_eq!(
        UNALIGNED_WORD % 4,
        1,
        "the unaligned variant must not be 4-byte aligned"
    );
    run_locked_bts_race(
        UNALIGNED_WORD,
        &LOCK_BTS32,
        ID_LOCK_BTS32,
        "lock-bts32-unaligned",
    );
}

/// The sub-word case, which has no host atomic at any alignment.
#[test]
fn locked_bts16_is_mutually_exclusive_across_engines() {
    run_locked_bts_race(LOCK_WORD, &LOCK_BTS16, ID_LOCK_BTS16, "lock-bts16");
}

/// Sanity-check the harness the race invariant rests on: a single thread must see
/// CF=0 on a clear word. Without this, a build that broke the BTX decode would make
/// the race tests pass vacuously.
#[test]
fn a_single_locked_bts_on_a_clear_word_reports_cf_clear() {
    for backend in BACKENDS {
        let mut cpu = open(backend);
        plant(&mut cpu, code_va(ID_LOCK_BTS32), &LOCK_BTS32, LOCK_WORD, 0);
        cpu.set_state(&ctx(code_va(ID_LOCK_BTS32), LOCK_WORD, 0, 0));
        assert!(matches!(cpu.run_rmw(), StepResultAlias::Continue));
        assert!(
            !cf(&mut cpu),
            "{backend:?}: bit was clear, so CF must be clear"
        );
        assert_eq!(read_word(&mut cpu, LOCK_WORD), 1, "{backend:?}: bit set");
    }
}

/// A BTX must never reach outside the word it was told about. `LOCK_WORD + 4` is the
/// next word on the same page, so a width mistake (reading or writing 8 bytes, say)
/// would show up here.
#[test]
fn locked_btx_mem_stays_inside_its_word() {
    const NEXT_WORD: u64 = LOCK_WORD + 4;
    for (label, op, code_base) in [
        ("lock-bts32", &LOCK_BTS32[..], code_va(ID_LOCK_BTS32)),
        ("lock-bts16", &LOCK_BTS16[..], code_va(ID_LOCK_BTS16)),
    ] {
        for backend in BACKENDS {
            let mut cpu = open(backend);
            plant(&mut cpu, code_base, op, LOCK_WORD, 0x0000_0000);
            // Mark the following word so an over-wide store is visible.
            let marker = [0_u8; 4];
            cpu.cpu()
                .mem_write(NEXT_WORD, &marker)
                .expect("write marker");
            cpu.set_state(&ctx(code_base, LOCK_WORD, 0, 0));
            assert!(matches!(cpu.run_rmw(), StepResultAlias::Continue));
            let mut buf = [0_u8; 4];
            cpu.cpu()
                .mem_read(NEXT_WORD, &mut buf)
                .expect("read marker");
            assert_eq!(
                u32::from_le_bytes(buf),
                0,
                "{backend:?} {label}: the following word must be untouched"
            );
            let _ = read_byte(&mut cpu, LOCK_WORD);
        }
    }
}
