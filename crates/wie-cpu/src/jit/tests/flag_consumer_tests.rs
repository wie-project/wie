// Flag **consumers** read a flag a previous instruction in the same block set.
//
// The SSA `FlagState` (`lower::flags`) exists so a consumer reads an i1 instead
// of decoding the packed RFLAGS word, and the pending/written split
// (`PendingFlags` + `flush_pending`) exists so a consumer sitting between a
// flag-setting instruction and the next flag write sees the pending arithmetic
// rather than the stale packed word. Both properties are invisible in the
// instruction results unless the *condition* is wrong, and a wrong condition
// shows up as a mis-executed branch on a real guest long after the fact — so
// every case here is run twice, once on the iced interpreter (the reference) and
// once through a compiled Cranelift block, and the whole register file is
// compared (`assert_same_regs`, including RFLAGS and RIP).
//
// The consumer shapes are the ones the lowering special-cases: `cmovcc` and
// `setcc` (FLAGS→GPR inside the body), `jcc` (FLAGS→branch at the terminator),
// and `adc`/`sbb` (FLAGS→GPR *after a flush*, which is the pending-flag path).
// `imul` is here because it is an *eager* writer: it rewrites only CF/OF in the
// carrier, and a consumer after it must see those two without the state having
// been re-derived from the packed word.
//
// Split out of `simd_parity_tests` (which is the SSE family) rather than
// appended to it: this file is about integer flags, and it reuses that file's
// dual-engine harness instead of duplicating it.
use super::chain_tests::chain_dual;
use super::simd_parity_tests::{assert_same_regs, simd_dual};
use crate::regs::RegFile;

/// Condition codes in encoding order: `0x70 | cc` is `jcc`, `0x40 | cc` is
/// `cmovcc`, `0x90 | cc` is `setcc`. Names are the iced mnemonics.
const COND_NAMES: [&str; 16] = [
    "o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g",
];

/// `cmp eax, ebx` (39 /r) — the flag writer that heads every case. It sets
/// ZF/SF/CF/OF/AF/PF, so every condition below is a live consumer of state that
/// was produced *in this block*.
const CMP_EAX_EBX: [u8; 2] = [0x39, 0xD8];

/// `(name, eax, ebx)` — the `cmp eax, ebx` flag combinations the cases below
/// cycle through, chosen so each condition is exercised both taken and not
/// taken:
///
/// * equal               → ZF=1
/// * unsigned-less      → CF=1, SF=1, ZF=0
/// * signed-overflow    → OF=1, SF=0 (0x8000_0000 - 1)
/// * greater            → CF=0, SF=0, ZF=0
/// * negative-no-borrow → SF=1, CF=0, OF=0
/// * pf-odd             → result 1, PF=0
const CMP_CASES: [(&str, u64, u64); 6] = [
    ("equal", 5, 5),
    ("unsigned-less", 1, 2),
    ("signed-overflow", 0x8000_0000, 1),
    ("greater", 2, 1),
    ("negative-no-borrow", 0xffff_ffff, 0),
    ("pf-odd", 2, 1),
];

const INIT_ECX: u64 = 0x1111_2222;
const INIT_EDX: u64 = 0x3333_4444;

/// Guest state shared by every case: operands in EAX/EBX, `cmov`/`setcc`
/// destination and source in ECX/EDX.
///
/// `Copy` because [`chain_dual`] runs the closure on both engines.
fn setup(eax: u64, ebx: u64) -> impl Fn(&mut RegFile) + Copy {
    move |r: &mut RegFile| {
        r.set_gpr_public(0, eax);
        r.set_gpr_public(3, ebx);
        r.set_gpr_public(1, INIT_ECX);
        r.set_gpr_public(2, INIT_EDX);
    }
}

/// `cmovcc ecx, edx` — 0F 4x /r, ModRM 0xCA = ecx ← (edx if cc).
fn cmov_ecx_edx(cc: u8) -> [u8; 3] {
    [0x0F, 0x40 | cc, 0xCA]
}

/// `setcc cl` — 0F 9x /0, ModRM 0xC1.
fn setcc_cl(cc: u8) -> [u8; 3] {
    [0x0F, 0x90 | cc, 0xC1]
}

/// `cmovcc ecx, edx` reading the `cmp`'s flags, for all 16 conditions and all
/// six flag combinations.
///
/// The observable is ECX: it is `edx` exactly when the condition holds, so a
/// condition that reads the wrong flag (or a stale one) changes ECX instead of
/// silently taking the same branch.
#[test]
fn cmovcc_reads_flags_set_earlier_in_the_block() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in COND_NAMES.iter().enumerate() {
            let cc = u8::try_from(cc).expect("0..16");
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&cmov_ecx_edx(cc));
            let (iced, jit) = simd_dual(&code, &[], setup(eax, ebx));
            assert_same_regs(&iced, &jit, &format!("cmov{name} after cmp, {case}"));
        }
    }
}

/// `setcc cl` — the same table, one byte wide. CL is the low byte of RCX, so
/// this checks the zero/sign/carry/overflow/parity reads independently of the
/// operand-width handling that `cmov` exercises.
#[test]
fn setcc_reads_flags_set_earlier_in_the_block() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in COND_NAMES.iter().enumerate() {
            let cc = u8::try_from(cc).expect("0..16");
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&setcc_cl(cc));
            let (iced, jit) = simd_dual(&code, &[], setup(eax, ebx));
            assert_same_regs(&iced, &jit, &format!("set{name} after cmp, {case}"));
        }
    }
}

/// `jcc` as the block terminator, reading the `cmp` one instruction earlier.
///
/// Layout: `39 D8` (cmp) · `7x 01` (jcc +1) · `90` (nop) · `0F 0B` (ud2). Taken
/// leaves RIP at the ud2, not-taken at the nop, so RIP *is* the condition and
/// the comparison covers the branch as well as RFLAGS.
#[test]
fn jcc_reads_flags_set_earlier_in_the_block() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in COND_NAMES.iter().enumerate() {
            let cc = u8::try_from(cc).expect("0..16");
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&[0x70 | cc, 0x01, 0x90, 0x0F, 0x0B]);
            // Stop on the first RIP at or past the `nop`: reached directly when
            // the branch is not taken, and skipped over when it is.
            let stop = super::SIMD_BASE + 6;
            let (iced, jit) = chain_dual(&code, stop, setup(eax, ebx), |_| {});
            assert_same_regs(&iced, &jit, &format!("j{name} after cmp, {case}"));
            assert!(
                iced.rip == super::SIMD_BASE + 6 || iced.rip == super::SIMD_BASE + 7,
                "j{name}, {case}: rip {:#x} is neither the nop nor the ud2",
                iced.rip
            );
        }
    }
}

/// A block whose **successor** reads the flags it leaves behind.
///
/// Layout: `39 D8` (cmp) · `EB 00` (jmp +0) · `7x 01` (jcc +1) · `90` (nop) ·
/// `0F 0B` (ud2). The first block ends at the `jmp` without reading any flag;
/// the `jcc` that consumes them is the *next* block, reached through a chain hop
/// (or the dispatcher, if the successor is not compiled yet — both engines only
/// have to agree on where they end up). This is the path that keeps the packed
/// carrier alive across a block boundary, and the reason `rflags` is still
/// stored at block exit.
#[test]
fn successor_block_reads_flags_from_the_previous_blocks_cmp() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in COND_NAMES.iter().enumerate() {
            let cc = u8::try_from(cc).expect("0..16");
            let mut code = Vec::from(CMP_EAX_EBX);
            // jmp +0 lands on the jcc; the jcc's +1 skips the nop to the ud2.
            code.extend_from_slice(&[0xEB, 0x00]);
            code.extend_from_slice(&[0x70 | cc, 0x01, 0x90, 0x0F, 0x0B]);
            let stop = super::SIMD_BASE + 6;
            let (iced, jit) = chain_dual(&code, stop, setup(eax, ebx), |_| {});
            assert_same_regs(&iced, &jit, &format!("j{name} after a jmp block, {case}"));
        }
    }
}

/// `adc`/`sbb` read the carry *after a flush*: they are the consumers that go
/// through `flush_pending`, so they see the pending `cmp` arithmetic and not the
/// packed word as it stood before it. ECX carries the result
/// (`ecx + ecx + CF` for `adc`, `ecx - ecx - CF` for `sbb`).
#[test]
fn adc_and_sbb_read_the_pending_carry() {
    for (case, eax, ebx) in CMP_CASES {
        for (name, op) in [("adc", [0x11_u8, 0xC9]), ("sbb", [0x19_u8, 0xC9])] {
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&op);
            let (iced, jit) = simd_dual(&code, &[], setup(eax, ebx));
            assert_same_regs(&iced, &jit, &format!("{name} after cmp, {case}"));
        }
    }
}

/// `imul` is an *eager* writer that defines only CF/OF. Its SSA state is now
/// updated from the overflow predicate directly, so these cases pin the two
/// flags it does define reaching a consumer in the same block, in both the
/// overflow and no-overflow direction.
///
/// `0x4000_0000 * 4` overflows 32 bits; `3 * 5` does not.
#[test]
fn consumer_reads_imuls_cf_and_of() {
    for (name, a, b) in [("overflow", 0x4000_0000_u64, 4_u64), ("no-overflow", 3, 5)] {
        for (cc, cname) in [(2_u8, "b"), (0_u8, "o"), (3_u8, "ae"), (1_u8, "no")] {
            let mut code = vec![0x0F, 0xAF, 0xC3]; // imul eax, ebx
            code.extend_from_slice(&cmov_ecx_edx(cc));
            let (iced, jit) = simd_dual(&code, &[], setup(a, b));
            assert_same_regs(&iced, &jit, &format!("cmov{cname} after imul, {name}"));

            let mut code = vec![0x0F, 0xAF, 0xC3];
            code.extend_from_slice(&setcc_cl(cc));
            let (iced, jit) = simd_dual(&code, &[], setup(a, b));
            assert_same_regs(&iced, &jit, &format!("set{cname} after imul, {name}"));
        }
    }
}

/// A `cmp`, then an `imul` (which flushes the pending `cmp` and rewrites only
/// CF/OF), then a `jne` that reads the *`cmp`'s* ZF.
///
/// This is the case where an eager writer must not disturb the flags it does not
/// define: ZF/SF/PF/AF still describe the `cmp`, so `jne` must branch on that.
/// Layout: `39 D8` (cmp) · `0F AF C9` (imul ecx, ecx) · `75 01` (jne +1) ·
/// `90` (nop) · `0F 0B` (ud2).
#[test]
fn jcc_after_an_imul_reads_the_earlier_cmp() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in [(5_u8, "ne"), (4_u8, "e")] {
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&[0x0F, 0xAF, 0xC9]);
            code.extend_from_slice(&[0x70 | cc, 0x01, 0x90, 0x0F, 0x0B]);
            let stop = super::SIMD_BASE + 8;
            let (iced, jit) = chain_dual(&code, stop, setup(eax, ebx), |_| {});
            assert_same_regs(&iced, &jit, &format!("j{name} after cmp+imul, {case}"));
        }
    }
}
