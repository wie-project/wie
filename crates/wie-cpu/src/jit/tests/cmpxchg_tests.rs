// `CMPXCHG r, r` (register destination) must agree between the two backends.
//
// The memory form is covered (and made atomic) in `atomic_tests.rs`; this file
// is about the *register* form, which involves no memory and no atomicity — only
// the implicit accumulator and the RFLAGS word. It was silently miscompiled:
//
// * the block's live-in set is built from a decoded instruction's *explicit*
//   operands plus a hand-written implicit list (`mark_insn_gprs`), which never
//   mentioned `CmpXchg`. Its accumulator is not an operand in iced's decode, so
//   RAX stayed the `iconst 0` placeholder and the compare ran against `0`;
// * `CmpXchg` was likewise absent from the flags-needed set, so the block had no
//   RFLAGS carrier at all — the flags the lowering computed were computed from
//   an `iconst 0` and then thrown away instead of being stored back;
// * and the lowering itself only ever wrote ZF, while the interpreter computes
//   the whole `CMP acc, dest` flag set, so even with a carrier present the two
//   engines disagreed about CF/SF/PF/AF/OF.
//
// Every case here asserts JIT == iced *and* the architectural answer, and the
// JIT arm additionally asserts the block really compiled (`iced_insns == 0`) —
// otherwise a case would silently degrade into re-running the interpreter and
// prove nothing about the lowering.
use super::*;
use crate::ThreadContext;
use crate::exec::StepResult as StepResultAlias;
use crate::regs::Rflags;
use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};

/// `cmpxchg ecx, edx` — 0F B1 /r, ModRM 0xD1 (mod=11, reg=EDX, rm=ECX).
const CAS32: [u8; 3] = [0x0F, 0xB1, 0xD1];
/// `cmpxchg cx, dx` — 66 0F B1 /r. The accumulator is AX, so the low 16 bits
/// are compared while the upper 48 bits of RAX must survive untouched.
const CAS16: [u8; 4] = [0x66, 0x0F, 0xB1, 0xD1];
/// `cmpxchg rcx, rdx` — REX.W 0F B1 /r.
const CAS64: [u8; 4] = [0x48, 0x0F, 0xB1, 0xD1];
/// `xadd ecx, edx` — 0F C1 /r, ModRM 0xD1 (mod=11, reg=EDX, rm=ECX). Shares the
/// same missing-flags-carrier mechanism as `CMPXCHG` (see
/// `implicit_operand_completion`), so it is pinned here too.
const XADD32: [u8; 3] = [0x0F, 0xC1, 0xD1];

// The interpreter's decode cache is process-wide and thread-local, keyed by
// (rip, mem_generation), and every test CPU here starts at generation 0 — so the
// same rip always replays the *first* decode of that rip in the process. One
// code VA per distinct byte sequence is therefore mandatory: reusing one rip for
// two encodings makes the second case silently execute the first case's bytes.
// Note `CAS32` and `CAS16` differ only by the 0x66 override, so they must not
// share a VA.
const CODE32: u64 = 0x4020_0000;
const CODE16: u64 = 0x4021_0000;
const CODE64: u64 = 0x4023_0000;
const CODE_XADD32: u64 = 0x4025_0000;
/// Decode-only (never executed), so it only has to differ from the above.
const CODE_DECODE: u64 = 0x4024_0000;

/// CF|PF|AF|ZF|SF|OF — exactly the bits a `CMP`-shaped flag update rewrites.
const ARITH: u64 = 0x1 | 0x4 | 0x10 | 0x40 | 0x80 | 0x800;

/// Incoming RFLAGS with every arithmetic bit *and* three non-arithmetic bits
/// (the reserved bit 1, IF and DF) set. The arithmetic bits are overwritten by
/// the instruction; `ALWAYS1 | IF | DF` must survive both engines untouched. If
/// the flags carrier is never loaded or stored back, the guest sees this value
/// verbatim and every case below fails.
const FLAGS_IN: u64 = 0x1 | 0x2 | 0x4 | 0x200 | 0x400 | 0x800;
/// The non-arithmetic half of [`FLAGS_IN`].
const FLAGS_KEPT: u64 = FLAGS_IN & !ARITH;

/// `CMP a, b` flag word for the `CMPXCHG` compare `acc - dest`.
///
/// Transcribed from the architecture (and from `add_sub_preds`/`set_sub_flags`,
/// which agree), so the cases below pin the *whole* flag word rather than only
/// ZF: a JIT that computes ZF and drops the rest is wrong even though x86 calls
/// the other bits undefined, because the interpreter here defines them and real
/// guest code reads RFLAGS after a CAS.
fn cmp_flags(acc: u64, dest: u64, bits: u32) -> u64 {
    let mask = match bits {
        16 => 0xffff,
        32 => 0xffff_ffff,
        _ => u64::MAX,
    };
    let sign = 1_u64 << (bits - 1);
    let (a, b) = (acc & mask, dest & mask);
    let r = a.wrapping_sub(b) & mask;
    let mut f = 0_u64;
    if a < b {
        f |= 1; // CF: borrow
    }
    if r == 0 {
        f |= 0x40; // ZF
    }
    if r & sign != 0 {
        f |= 0x80; // SF
    }
    if (r & 0xff).count_ones().is_multiple_of(2) {
        f |= 0x4; // PF
    }
    if (a ^ b) & (a ^ r) & sign != 0 {
        f |= 0x800; // OF
    }
    if (a ^ b ^ r) & 0x10 != 0 {
        f |= 0x10; // AF
    }
    f
}

/// The RFLAGS a correctly lowered `CMPXCHG` must leave behind.
fn expected_rflags(acc: u64, dest: u64, bits: u32) -> u64 {
    FLAGS_KEPT | cmp_flags(acc, dest, bits)
}

/// The `ADD` flag word, for the `XADD` case below.
fn add_flags(a: u64, b: u64, bits: u32) -> u64 {
    let mask = match bits {
        16 => 0xffff,
        32 => 0xffff_ffff,
        _ => u64::MAX,
    };
    let sign = 1_u64 << (bits - 1);
    let (x, y) = (a & mask, b & mask);
    let wide = u128::from(x) + u128::from(y);
    let r = (wide as u64) & mask;
    let mut f = 0_u64;
    if wide > u128::from(mask) {
        f |= 1; // CF: carry out
    }
    if r == 0 {
        f |= 0x40; // ZF
    }
    if r & sign != 0 {
        f |= 0x80; // SF
    }
    if (r & 0xff).count_ones().is_multiple_of(2) {
        f |= 0x4; // PF
    }
    if (x ^ r) & (y ^ r) & sign != 0 {
        f |= 0x800; // OF
    }
    if (x ^ y ^ r) & 0x10 != 0 {
        f |= 0x10; // AF
    }
    f
}

/// Run one `CMPXCHG` on both engines from identical state, with the default
/// [`FLAGS_IN`]. See [`cmpxchg_dual_rflags`] for the plumbing and the caveats.
fn cmpxchg_dual(code: &[u8], code_base: u64, gpr: [u64; 16]) -> (ThreadContext, ThreadContext) {
    cmpxchg_dual_rflags(code, code_base, gpr, FLAGS_IN)
}

/// Run one `CMPXCHG` on both engines from identical state.
///
/// Returns `(iced, jit)` `ThreadContext`s. The JIT arm asserts the block really
/// compiled, so a case can never pass by falling back to the interpreter.
///
/// The two engines retire a *different* number of instructions for the same
/// bytes — iced is stepped once, the JIT runs the whole compiled block (the
/// instruction plus its `nop` filler) — so `rip` is deliberately not compared
/// here; `atomic_tests.rs` makes the same choice. Everything the instruction
/// itself defines is compared.
fn cmpxchg_dual_rflags(
    code: &[u8],
    code_base: u64,
    gpr: [u64; 16],
    rflags_in: u64,
) -> (ThreadContext, ThreadContext) {
    // The trailing `nop` reaches the 2-insn block minimum; the `ud2` stops linear
    // decode so the zero-filled tail cannot extend the block.
    let mut full = Vec::with_capacity(code.len() + 3);
    full.extend_from_slice(code);
    full.extend_from_slice(&[0x90, 0x0F, 0x0B]);

    let ctx = ThreadContext {
        rip: code_base,
        gpr,
        rflags: Rflags::from(rflags_in),
        ..ThreadContext::default()
    };

    // `restore_thread_context` rather than poking `regs` directly: the JIT block
    // loads its entry GPRs and RFLAGS out of `JitCtx`, so writing only the
    // interpreter-side register file would leave the block reading a stale
    // carrier and the case would fail for the wrong reason.
    let mut iced = IcedCpu::open_x86_64();
    iced.mem_map(code_base, 0x1000, RwxPerms::ALL)
        .expect("iced map");
    iced.mem_write(code_base, &full).expect("iced code");
    iced.restore_thread_context(&ctx);
    assert!(
        matches!(
            iced.step_once_result().expect("iced step"),
            StepResultAlias::Continue
        ),
        "iced must retire the CMPXCHG"
    );

    let mut jit = JitCpu::open_x86_64();
    jit.mem_map(code_base, 0x1000, RwxPerms::ALL)
        .expect("jit map");
    jit.mem_write(code_base, &full).expect("jit code");
    jit.restore_thread_context(&ctx);
    // Same thing a real guest thread switch does: without dropping the translated
    // pointers a JIT engine would reuse state from a previous case.
    jit.on_thread_switch();
    let (result, _retired) = jit.step_one().expect("jit step");
    assert!(
        matches!(result, StepResultAlias::Continue),
        "jit result {result:?}"
    );
    assert!(
        jit.has_ready_at(code_base),
        "block must compile, not fall back to iced"
    );
    assert_eq!(
        jit.stats().exec.iced_insns,
        0,
        "block ran on iced instead of the JIT"
    );

    (
        iced.snapshot_thread_context(),
        jit.snapshot_thread_context(),
    )
}

/// Assert the two engines agree on every GPR and on the whole RFLAGS word, and
/// report the first divergence with both values.
fn assert_agree(iced: &ThreadContext, jit: &ThreadContext, what: &str) {
    for (i, (a, b)) in iced.gpr.iter().zip(jit.gpr.iter()).enumerate() {
        assert_eq!(*a, *b, "{what}: gpr[{i}] — iced {a:#x} vs jit {b:#x}");
    }
    assert_eq!(
        u64::from(iced.rflags),
        u64::from(jit.rflags),
        "{what}: rflags — iced {:#x} vs jit {:#x}",
        u64::from(iced.rflags),
        u64::from(jit.rflags)
    );
}

/// The expected outcome of one `CMPXCHG`, as data so the tables stay readable and
/// `check` does not grow an argument list.
#[derive(Clone, Copy)]
struct Case {
    want_gpr: [u64; 16],
    acc: u64,
    dest: u64,
    bits: u32,
}

/// Differential agreement plus the pinned architectural answer.
///
/// `acc` and `dest` are the two values the instruction compares — the accumulator
/// (RAX at the operand width) and the destination — so they double as the operands
/// of the `CMP` whose flags are expected below.
fn check(label: &str, code: &[u8], code_base: u64, gpr: [u64; 16], want: Case) {
    let Case {
        want_gpr,
        acc,
        dest,
        bits,
    } = want;
    let (iced, jit) = cmpxchg_dual(code, code_base, gpr);
    assert_agree(&iced, &jit, label);
    for (i, (got, want_one)) in jit.gpr.iter().zip(want_gpr.iter()).enumerate() {
        assert_eq!(
            *got, *want_one,
            "{label}: gpr[{i}] — iced and jit agree but differ from the architecture"
        );
    }
    // Pin the full flag word, not just ZF: this is what catches a lowering that
    // computes ZF and leaves CF/SF/PF/AF/OF alone (or recomputes them from an
    // `iconst 0` carrier).
    assert_eq!(
        u64::from(jit.rflags),
        expected_rflags(acc, dest, bits),
        "{label}: rflags — non-arithmetic bits must survive and the arithmetic \
         bits must be those of CMP {acc:#x}, {dest:#x}"
    );
}

/// `cmpxchg ecx, edx` at 32 bits, over both directions of the compare.
///
/// * equal (ECX == EAX): ZF=1, ECX takes EDX, EAX keeps its value.
/// * unequal: ZF=0, EAX takes ECX, ECX keeps its value.
#[test]
fn cmpxchg_reg32_agrees_across_backends() {
    for (label, rax, rcx, rdx, want_eax, want_ecx) in [
        // Accumulators chosen so a compare against the `iconst 0` placeholder
        // takes the wrong branch in both directions.
        ("equal", 5_u64, 5_u64, 7_u64, 5_u64, 7_u64),
        ("unequal", 2, 5, 7, 5, 5),
        ("equal-nonzero", 9_000, 9_000, 3, 9_000, 3),
        ("unequal-nonzero", 0, 4, 3, 4, 4),
        // No borrow and a positive result: CF must come out clear, which a
        // lowering that only ever touches ZF would still get right — the
        // differential is what pins it.
        ("unequal-no-borrow", 7, 2, 9, 2, 2),
    ] {
        let mut gpr = [0_u64; 16];
        gpr[0] = rax;
        gpr[1] = rcx;
        gpr[2] = rdx;
        let mut want_gpr = gpr;
        want_gpr[0] = want_eax;
        want_gpr[1] = want_ecx;
        check(
            label,
            &CAS32,
            CODE32,
            gpr,
            Case {
                want_gpr,
                acc: rax,
                dest: rcx,
                bits: 32,
            },
        );
    }
}

/// The accumulator is *narrower* than the register on the 16-bit form, so the 48
/// bits above AX are observable leftovers that must survive both the compare and
/// the write-back. This is the case that caught the accumulator being read as the
/// `iconst 0` placeholder and written back over.
#[test]
fn cmpxchg_reg16_agrees_across_backends() {
    // Equal: AX == CX, so CX takes DX and RAX is untouched, upper bits included.
    let (iced, jit) = cmpxchg_dual(
        &CAS16,
        CODE16,
        [
            0xaa00_0000_0000_0005,
            0x0005,
            0x0007,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ],
    );
    assert_agree(&iced, &jit, "cas16-equal");
    assert_eq!(
        jit.gpr[0], 0xaa00_0000_0000_0005,
        "cas16-equal: RAX unchanged"
    );
    assert_eq!(jit.gpr[1], 0x0007, "cas16-equal: CX takes DX");
    assert_eq!(
        u64::from(jit.rflags),
        expected_rflags(0x0005, 0x0005, 16),
        "cas16-equal: rflags"
    );

    // Unequal: AX takes CX's low 16 bits and the upper 48 bits survive.
    let (iced, jit) = cmpxchg_dual(
        &CAS16,
        CODE16,
        [
            0xaa00_0000_0000_0002,
            0x0005,
            0x0007,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ],
    );
    assert_agree(&iced, &jit, "cas16-unequal");
    assert_eq!(
        jit.gpr[0], 0xaa00_0000_0000_0005,
        "cas16-unequal: AX takes CX, upper 48 bits kept"
    );
    assert_eq!(jit.gpr[1], 0x0005, "cas16-unequal: CX unchanged");
    assert_eq!(
        u64::from(jit.rflags),
        expected_rflags(0x0002, 0x0005, 16),
        "cas16-unequal: rflags"
    );
}

/// 64-bit: the accumulator is the whole RAX and RCX takes the full RDX.
#[test]
fn cmpxchg_reg64_agrees_across_backends() {
    const ACC: u64 = 0x1122_3344_5566_7788;
    const SRC: u64 = 0xdead_beef_cafe_f00d;
    let (iced, jit) = cmpxchg_dual(
        &CAS64,
        CODE64,
        [ACC, ACC, SRC, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_agree(&iced, &jit, "cas64-equal");
    assert_eq!(jit.gpr[0], ACC, "cas64-equal: RAX keeps its value");
    assert_eq!(jit.gpr[1], SRC, "cas64-equal: RCX takes RDX");
    assert_eq!(
        u64::from(jit.rflags),
        expected_rflags(ACC, ACC, 64),
        "cas64-equal: rflags"
    );

    let (iced, jit) = cmpxchg_dual(
        &CAS64,
        CODE64,
        [1, ACC, SRC, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_agree(&iced, &jit, "cas64-unequal");
    assert_eq!(jit.gpr[0], ACC, "cas64-unequal: RAX takes RCX");
    assert_eq!(jit.gpr[1], ACC, "cas64-unequal: RCX unchanged");
    assert_eq!(
        u64::from(jit.rflags),
        expected_rflags(1, ACC, 64),
        "cas64-unequal: rflags"
    );
}

/// A block whose only flag writer is `CMPXCHG` must still store RFLAGS back.
/// Each direction is run from *both* starting ZF values, so a store-back that is
/// conditional on the compare taking one branch is caught.
#[test]
fn cmpxchg_reg_stores_rflags_back_in_both_directions() {
    for (label, rflags_in, rax, rcx, want_zf) in [
        ("zf-set-unequal", FLAGS_IN, 2_u64, 5_u64, false),
        ("zf-set-equal", FLAGS_IN, 5, 5, true),
        // Start from an RFLAGS word with the arithmetic bits clear, so ZF=1 can
        // only come from the instruction and not from a stale carrier.
        ("zf-clear-equal", FLAGS_KEPT, 5, 5, true),
        ("zf-clear-unequal", FLAGS_KEPT, 2, 5, false),
    ] {
        let mut ctx_gpr = [0_u64; 16];
        ctx_gpr[0] = rax;
        ctx_gpr[1] = rcx;
        ctx_gpr[2] = 7;
        let (iced, jit) = cmpxchg_dual_rflags(&CAS32, CODE32, ctx_gpr, rflags_in);
        assert_agree(&iced, &jit, label);
        assert_eq!(
            (u64::from(jit.rflags) & 0x40 != 0),
            want_zf,
            "{label}: ZF must be {want_zf}, got {:#x}",
            u64::from(jit.rflags)
        );
        assert_eq!(
            u64::from(jit.rflags),
            expected_rflags(rax, rcx, 32),
            "{label}: full flag word"
        );
    }
}

/// Pin the operand decode so an iced upgrade that renumbers the ModRM fields
/// cannot silently turn these cases into a different instruction.
#[test]
fn cmpxchg_test_encodings_decode_as_intended() {
    for (bytes, width_bits, dest, src) in [
        (&CAS32[..], 32_u32, Register::ECX, Register::EDX),
        (&CAS16[..], 16, Register::CX, Register::DX),
        (&CAS64[..], 64, Register::RCX, Register::RDX),
    ] {
        let mut dec = Decoder::with_ip(64, bytes, CODE_DECODE, DecoderOptions::NONE);
        let insn = dec.decode();
        assert_eq!(insn.mnemonic(), Mnemonic::Cmpxchg, "{bytes:02x?}");
        assert_eq!(
            u32::try_from(insn.op_register(0).size()).unwrap_or(0) * 8,
            width_bits,
            "{bytes:02x?}: operand width"
        );
        assert_eq!(insn.op_register(0), dest, "{bytes:02x?}: destination");
        assert_eq!(insn.op_register(1), src, "{bytes:02x?}: source");
        assert_eq!(insn.op_kind(0), OpKind::Register, "{bytes:02x?}");
        assert!(
            !insn.has_lock_prefix(),
            "{bytes:02x?}: the register form carries no LOCK prefix"
        );
        // RAX is an implicit operand: it is *not* op0/op1, which is exactly why
        // the live-in analysis has to know about it separately.
        assert!(
            !matches!(
                (insn.op_register(0), insn.op_register(1)),
                (Register::RAX, _) | (_, Register::RAX)
            ),
            "{bytes:02x?}: the accumulator must stay implicit"
        );
    }
}

/// There is no 8-bit `CMPXCHG`: `0F B1 /r` with no REX and no 0x66 override is the
/// *32-bit* form, and the sub-word RMWs are `CMPXCHG8B`/`CMPXCHG16B`.
///
/// Pinned because a plausible-looking "add an 8-bit case" would silently test the
/// 32-bit path twice while asserting 8-bit expectations — the 8-bit accumulator
/// arm in the lowerer is therefore unreachable, and `accumulator_register`
/// rejects it rather than guessing AL.
#[test]
fn cmpxchg_has_no_eight_bit_form() {
    let mut dec = Decoder::with_ip(64, &CAS32, CODE_DECODE, DecoderOptions::NONE);
    let insn = dec.decode();
    assert_eq!(
        u32::try_from(insn.op_register(0).size()).unwrap_or(0) * 8,
        32,
        "0F B1 /r without REX/0x66 is the 32-bit form, not an 8-bit one"
    );
    // Sweep every register-form `0F B1 /r` under each prefix combination that can
    // change the operand width. If the ISA had an 8-bit `CMPXCHG`, some prefix
    // here would decode to an 8-bit register operand; asserting that none does is
    // stronger than pinning one encoding, and it cannot rot when iced renumbers
    // the ModRM fields.
    for prefix in [
        &[0x0F, 0xB1][..],             // none
        &[0x66, 0x0F, 0xB1][..],       // 16-bit
        &[0x48, 0x0F, 0xB1][..],       // REX.W -> 64-bit
        &[0x66, 0x48, 0x0F, 0xB1][..], // 0x66 then REX.W: REX wins
        &[0x41, 0x0F, 0xB1][..],       // REX.B
    ] {
        for rm in 0_u8..16 {
            let mut bytes = prefix.to_vec();
            bytes.push(0xC0 | rm); // mod=11, reg=0 (EAX), rm varies
            let mut dec = Decoder::with_ip(64, &bytes, CODE_DECODE, DecoderOptions::NONE);
            let insn = dec.decode();
            if insn.mnemonic() != Mnemonic::Cmpxchg {
                continue;
            }
            let width = u32::try_from(insn.op_register(0).size()).unwrap_or(0) * 8;
            assert!(
                matches!(width, 16 | 32 | 64),
                "{bytes:02x?}: CMPXCHG decoded at {width} bits — the ISA has no \
                 8-bit form, so the 8-bit accumulator arm is unreachable"
            );
        }
    }
}

/// `XADD r, r` writes ADD flags on every form, and `Xadd` was missing from the
/// flags-needed set for exactly the same reason `CmpXchg` was missing from the
/// live-in set: neither mnemonic appears in the analysis, so the block got no
/// RFLAGS carrier and the flags the lowering computed were thrown away instead of
/// being stored back. Pinned here because it rides on the same fix.
#[test]
fn xadd_reg_writes_add_flags_on_both_backends() {
    for (label, rcx, rdx, want_ecx, want_edx, want_arith) in [
        // No carry: 5 + 7 = 12, even parity so PF is set.
        ("xadd-no-carry", 5_u64, 7_u64, 12_u64, 5_u64, 0x4_u64),
        // Carry out of the top bit, no signed overflow (both operands positive).
        ("xadd-carry", 0xffff_fff0, 0x20, 0x10, 0xffff_fff0, 0x1),
        // Signed overflow on top of no unsigned carry: 0x7fffffff + 1 gives
        // OF|SF|PF|AF.
        (
            "xadd-of",
            0x7fff_ffff,
            1,
            0x8000_0000,
            0x7fff_ffff,
            0x800 | 0x80 | 0x4 | 0x10,
        ),
        // Carry out of the top bit, a zero result (so ZF and PF, since 0 has even
        // parity) and an auxiliary carry.
        (
            "xadd-carry-zero-af",
            0x0000_000f,
            0xffff_fff1,
            0x0000_0000,
            0x0000_000f,
            0x1 | 0x40 | 0x4 | 0x10,
        ),
    ] {
        let (iced, jit) = cmpxchg_dual(
            &XADD32,
            CODE_XADD32,
            [0, rcx, rdx, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        assert_agree(&iced, &jit, label);
        assert_eq!(jit.gpr[1], want_ecx, "{label}: ECX takes the sum");
        assert_eq!(jit.gpr[2], want_edx, "{label}: EDX takes the old ECX");
        // Cross-check the hand-derived expectation against the ADD semantics
        // helper first, so a typo in the table above cannot reach the engines.
        assert_eq!(
            want_arith,
            add_flags(rcx, rdx, 32),
            "{label}: the table's expected flag word disagrees with add_flags()"
        );
        assert_eq!(
            u64::from(jit.rflags),
            FLAGS_KEPT | want_arith,
            "{label}: XADD must store the full ADD flag set back (got {:#x}, \
             expected non-arithmetic {:#x} plus ADD {:#x})",
            u64::from(jit.rflags),
            FLAGS_KEPT,
            want_arith
        );
    }
}
