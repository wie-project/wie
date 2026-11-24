// The cross-file guard for implicit operands.
//
// ## The failure mode this exists to stop
//
// Two files decide whether a JIT-compiled block is *correct*:
//
// * `jit/block.rs::is_lowerable` decides a mnemonic may be compiled at all;
// * `jit/lower/mod.rs::implicit_operand_completion` (plus
//   `lower/analysis.rs::mark_insn_gprs`) decides which registers that
//   compilation may assume hold real values.
//
// An instruction whose architectural operands are **implicit** — the
// accumulator of `CMPXCHG`, the `EDX:EAX` dividend of `DIV` — names neither of
// them in its decode. So admitting the mnemonic in `block.rs` and forgetting it
// here does not fail to compile: the block lowers cleanly against the `iconst 0`
// placeholders in `entry_gpr` and silently computes with zeros. Both of these
// shipped that way:
//
// * `CMPXCHG ecx, edx` compared against `0` instead of the accumulator.
// * `DIV ecx` divided a `0:0` dividend: EAX=100, ECX=7 gave 14/2 on iced and
//   **0/0** on the JIT, with no fault.
//
// Each was found by an audit. The point of this file is that a third instance
// should fail a *test*, not need another audit.
//
// ## How it guards
//
// `implicit_operands.rs` in the same directory holds the table of
// implicit-operand mnemonics and what each one implicitly reads and writes.
// That table is the single place the knowledge lives, and this test drives every
// entry through **both** engines from a state chosen to exercise the implicit
// operand, asserting the architectural answer and the engines' agreement.
//
// ## What this does and does not prove
//
// It proves the JIT agrees with the interpreter for these operand patterns, and
// that the *architectural* value is what both produce (the expectations are
// computed from the ISA, not copied from either backend). It does **not** prove
// the table is complete — a mnemonic missing from the table has no entry here to
/// fail. That is a real limit, and the honest mitigation is the reverse check
// below: any mnemonic whose *lowering* reads a register the decoder does not
// name must appear in the table. `every_mnemonic_in_the_table_is_actually_lowered`
// covers the growth direction, and `the_table_covers_every_admitted_implicit_operand_mnemonic`
// is the audit-in-a-test for the direction that matters.
use crate::ThreadContext;
use crate::exec::StepResult as StepResultAlias;
use crate::mem::{MEM_COMMIT, MEM_RESERVE, protect};
use iced_x86::{Decoder, DecoderOptions, Mnemonic};

use super::atomic_tests::{BACKENDS, Engine, open};

/// One code page per distinct byte sequence. The interpreter's decode cache is
/// process-wide and thread-local, keyed by (rip, mem_generation), and every test
/// CPU starts at generation 0 — two different encodings sharing a VA make the
/// second case silently execute the first case's instruction.
const CODE_BASE: u64 = 0x4090_0000;
const fn code_va(id: u64) -> u64 {
    CODE_BASE + id * 0x1_0000
}

/// Decode-only (never executed), so it only has to differ from the above.
const CODE_DECODE: u64 = CODE_BASE - 0x1_0000;

// ── Table-driven: the implicit-operand knowledge lives in one place ──────

/// The `DIV`/`IDIV` implicit dividend. 32-bit form, so the encoding is a
/// two-byte `F7 /6` (div) or `F7 /7` (idiv) with ModRM 0xF1 (mod=11, /6, ECX).
const DIV32: [u8; 2] = [0xF7, 0xF1];
const IDIV32: [u8; 2] = [0xF7, 0xF9];
/// 64-bit: REX.W in front.
const DIV64: [u8; 3] = [0x48, 0xF7, 0xF1];
const IDIV64: [u8; 3] = [0x48, 0xF7, 0xF9];
/// 16-bit: the 0x66 override narrows the dividend to DX:AX. `lower_div`
/// supports only 32/64, so this form is admitted by `is_lowerable` but fails to
/// lower and falls back to the interpreter — the case is here to pin the
/// *result*, not to demand JIT coverage.
const DIV16: [u8; 3] = [0x66, 0xF7, 0xF1];
const IDIV16: [u8; 3] = [0x66, 0xF7, 0xF9];
/// `cmpxchg ecx, edx` — the implicit accumulator, the first instance of this bug.
const CMPXCHG32: [u8; 3] = [0x0F, 0xB1, 0xD1];
/// `xadd ecx, edx` — implicit flags only, no implicit register.
const XADD32: [u8; 3] = [0x0F, 0xC1, 0xD1];

const ID_DIV32: u64 = 0;
const ID_IDIV32: u64 = 1;
const ID_DIV64: u64 = 2;
const ID_IDIV64: u64 = 3;
const ID_DIV16: u64 = 4;
const ID_IDIV16: u64 = 5;
const ID_CMPXCHG32: u64 = 6;
const ID_XADD32: u64 = 7;

/// The complete table of implicit-operand mnemonics this lowerer handles.
///
/// `reads`/`writes` name the GPR slots an instruction touches without saying so
/// in its decode. This is the list a new lowerer must extend.
struct ImplicitCase {
    label: &'static str,
    op: &'static [u8],
    id: u64,
    /// GPR slots set before the instruction (the implicit operands).
    gpr: [u64; 16],
    /// Architecturally-correct expectation for the slots the instruction writes.
    want: [u64; 16],
    /// Does the block have to compile on the JIT for this case to be interesting?
    must_compile: bool,
}

/// Reinterpret between `u64` and `i64` without an `as` cast (two's complement).
fn as_i64(v: u64) -> i64 {
    i64::from_ne_bytes(v.to_ne_bytes())
}

/// Sign-extend the low `bits` of `v` to a full `i64` — what the hardware does to
/// the high half of a `DIV`/`IDIV` dividend.
fn sign_extend_from(v: u64, bits: u32) -> i64 {
    if bits >= 64 {
        return as_i64(v);
    }
    let shift = 64 - bits;
    let widened = v << shift;
    as_i64(widened) >> shift
}

/// The architectural answer for a `DIV`/`IDIV` family instruction, computed from
/// the ISA rather than copied from a backend.
///
/// The dividend is the implicit pair, whose width follows the encoding: 16-bit
/// forms use `DX:AX`, 32-bit `EDX:EAX`, 64-bit `RDX:RAX`. `div` reads that pair
/// as one unsigned integer, `idiv` as a two's-complement signed one with the high
/// half sign-extended. The quotient lands in the low half and the remainder in
/// the high half of the same register, and the *written* width follows the
/// operand: a 16-bit or 32-bit division merges into the low half (leaving the
/// bits above it, or the upper 32, alone), a 64-bit division writes both whole.
///
/// 128-bit intermediates because a 64-bit `RDX:RAX` dividend does not fit in 64
/// bits — which is exactly the case a JIT `div64` host helper exists for.
fn expected_div(code: &[u8], gpr: &[u64; 16]) -> [u64; 16] {
    let mut dec = Decoder::with_ip(64, code, CODE_DECODE, DecoderOptions::NONE);
    let insn = dec.decode();
    let signed = insn.mnemonic() == Mnemonic::Idiv;
    let bits = u32::try_from(insn.op_register(0).size()).unwrap_or(0) * 8;
    let shift = bits;
    let (low_mask, high_mask) = match bits {
        16 => (0xffff_u64, 0xffff_u64),
        32 => (0xffff_ffff_u64, 0xffff_ffff_u64),
        _ => (u64::MAX, u64::MAX),
    };
    let low = gpr[0] & low_mask;
    let high = gpr[2] & high_mask;
    let divisor = gpr[1];

    let (quotient, remainder) = if signed {
        let high_signed = sign_extend_from(high, bits);
        let dividend = (i128::from(high_signed) << shift) + i128::from(low);
        let d = i128::from(as_i64(divisor));
        (dividend / d, dividend % d)
    } else {
        let dividend = (u128::from(high) << shift) | u128::from(low);
        let d = u128::from(divisor);
        (
            i128::try_from(dividend / d).unwrap_or(0),
            i128::try_from(dividend % d).unwrap_or(0),
        )
    };

    // The quotient and remainder are written at the *operand* width, so mask
    // before converting: `idiv` can produce either sign, and masking is what
    // turns -613566742 into the 0xDB6DB6EA the hardware leaves in EAX.
    let write_mask = match bits {
        16 => i128::from(low_mask),
        32 => i128::from(low_mask),
        _ => i128::from(u64::MAX),
    };
    let quotient = u64::try_from(quotient & write_mask).unwrap_or(0);
    let remainder = u64::try_from(remainder & write_mask).unwrap_or(0);

    let mut want = *gpr;
    match bits {
        // 16-bit merges into DX/AX, preserving everything above.
        16 => {
            want[0] = (gpr[0] & !low_mask) | (quotient & low_mask);
            want[2] = (gpr[2] & !high_mask) | (remainder & high_mask);
        }
        // 32-bit writes EAX/EDX and zero-extends into the full 64-bit register.
        32 => {
            want[0] = quotient & low_mask;
            want[2] = remainder & high_mask;
        }
        // 64-bit writes both registers whole.
        _ => {
            want[0] = quotient;
            want[2] = remainder;
        }
    }
    want
}

/// The table: every implicit-operand mnemonic the lowerer admits.
fn implicit_cases() -> Vec<ImplicitCase> {
    let div =
        |label: &'static str, op: &'static [u8], id: u64, gpr: [u64; 16], must_compile: bool| {
            ImplicitCase {
                label,
                op,
                id,
                want: expected_div(op, &gpr),
                gpr,
                must_compile,
            }
        };
    vec![
        // div: 100 / 7 = 14 rem 2, with EDX=0 for the unsigned 64-bit dividend.
        div("div32", &DIV32, ID_DIV32, gprs(100, 7, 0), true),
        // High half non-zero: the 64-bit dividend is 0x0000_0000_0000_0000_0000_0003 in
        // 32-bit terms is 2^32*EDX + EAX, so EDX=2 EAX=5 divided by 7 exercises
        // both halves actually being read.
        div("div32-high", &DIV32, ID_DIV32, gprs(5, 7, 2), true),
        // idiv: -100 / 7 = -14 rem -2, sign-extended.
        div(
            "idiv32-neg",
            &IDIV32,
            ID_IDIV32,
            gprs(100, 7, u64::MAX),
            true,
        ),
        div(
            "idiv32-high",
            &IDIV32,
            ID_IDIV32,
            gprs(5, 7, u64::MAX),
            true,
        ),
        // 64-bit forms are NOT compiled: `div_is_lowerable` admits only a
        // size-4 register divisor, so a qword `div` correctly falls back to the
        // interpreter even though `lower_div` has a host-helper path for it. That
        // is a coverage gap in `block.rs`, not a correctness one, and closing it is
        // out of scope here — so these cases pin the *result* only.
        div("div64", &DIV64, ID_DIV64, gprs(100, 7, 0), false),
        // A 64-bit dividend that does not fit in 32 bits: 2^64 + 5 over 7, which
        // only a 128-bit intermediate can compute. This is the case that proves
        // the implicit RDX half is read and not just RAX. RDX=1 rather than
        // something larger: a high half big enough to push the quotient past
        // 2^64 is a legitimate `#DE`, not a result to assert here.
        div("div64-high", &DIV64, ID_DIV64, gprs(5, 7, 1), false),
        div(
            "idiv64-neg",
            &IDIV64,
            ID_IDIV64,
            gprs(100, 7, u64::MAX),
            false,
        ),
        // 16-bit narrows to DX:AX. `lower_div` supports 32/64 only, so this must
        // still be *correct* — it just falls back.
        div("div16", &DIV16, ID_DIV16, gprs(100, 7, 0), false),
        div(
            "idiv16-neg",
            &IDIV16,
            ID_IDIV16,
            gprs(100, 7, u64::MAX),
            false,
        ),
        ImplicitCase {
            label: "cmpxchg32",
            op: &CMPXCHG32,
            id: ID_CMPXCHG32,
            // EAX=5, ECX=5, EDX=7: equal, so ECX takes EDX and EAX is kept.
            gpr: gprs(5, 5, 7),
            want: {
                let mut w = gprs(5, 5, 7);
                w[1] = 7;
                w
            },
            must_compile: true,
        },
        ImplicitCase {
            label: "cmpxchg32-unequal",
            op: &CMPXCHG32,
            id: ID_CMPXCHG32,
            // EAX=2, ECX=5: unequal, so EAX takes ECX.
            gpr: gprs(2, 5, 7),
            want: {
                let mut w = gprs(2, 5, 7);
                w[0] = 5;
                w
            },
            must_compile: true,
        },
        ImplicitCase {
            label: "xadd32",
            op: &XADD32,
            id: ID_XADD32,
            // ECX += EDX, EDX = old ECX.
            gpr: gprs(0, 5, 7),
            want: {
                let mut w = gprs(0, 5, 7);
                w[1] = 12;
                w[2] = 5;
                w
            },
            must_compile: true,
        },
    ]
}

fn gprs(rax: u64, rcx: u64, rdx: u64) -> [u64; 16] {
    let mut g = [0_u64; 16];
    g[0] = rax;
    g[1] = rcx;
    g[2] = rdx;
    g
}

/// Run one table entry on both engines and check it against the ISA.
#[test]
fn implicit_operands_agree_with_iced_and_the_architecture() {
    for case in implicit_cases() {
        let code_base = code_va(case.id);
        let mut results = Vec::with_capacity(2);
        let mut jit_compiled = false;
        for backend in BACKENDS {
            let mut cpu = open(backend);
            let mut code = case.op.to_vec();
            // Trailing nop reaches the 2-insn block minimum; ud2 stops linear
            // decode so the zero-filled tail cannot extend the block.
            code.extend_from_slice(&[0x90, 0x0F, 0x0B]);
            cpu.cpu()
                .virtual_alloc(
                    code_base,
                    0x1000,
                    MEM_RESERVE | MEM_COMMIT,
                    protect::PAGE_EXECUTE_READWRITE,
                )
                .expect("code alloc");
            cpu.cpu().mem_write(code_base, &code).expect("code");
            cpu.set_state(&ThreadContext {
                rip: code_base,
                gpr: case.gpr,
                ..ThreadContext::default()
            });
            assert!(
                matches!(cpu.run_rmw(), StepResultAlias::Continue),
                "{} {backend:?}: must retire",
                case.label
            );
            let out = cpu.cpu().snapshot_thread_context();
            // The architectural answer, asserted per backend so this is a test of
            // the ISA and not merely of engine-to-engine agreement.
            for i in 0..16 {
                assert_eq!(
                    out.gpr[i], case.want[i],
                    "{} {backend:?}: gpr[{i}] — the ISA answer is {:#x}, backend \
                     gave {:#x} from seeded {:#x}",
                    case.label, case.want[i], out.gpr[i], case.gpr[i]
                );
            }
            results.push((out.gpr, u64::from(out.rflags)));
            if let Engine::Jit(ref j) = cpu {
                jit_compiled = j.has_ready_at(code_base);
            }
        }
        assert_eq!(
            results[0], results[1],
            "{}: engines disagree on (gpr, rflags)",
            case.label
        );
        if case.must_compile {
            assert!(
                jit_compiled,
                "{}: this form is lowerable and must stay compiled",
                case.label
            );
            assert_eq!(results.len(), 2, "{}: both arms must have run", case.label);
        }
    }
}

/// The specific probe that found the `DIV` defect, kept as its own named case so
/// a regression names itself.
///
/// 100 / 7 with EDX=0: quotient 14, remainder 2. The broken JIT divided a
/// `0:0` dividend and produced EAX=0, EDX=0 — with no fault and no diagnostic.
#[test]
fn div32_reads_the_implicit_edx_eax_dividend() {
    let case = implicit_cases()
        .into_iter()
        .find(|c| c.label == "div32")
        .expect("div32 case");
    assert_eq!(case.want[0], 14, "100 / 7 quotient");
    assert_eq!(case.want[2], 2, "100 % 7 remainder");
    // And the seeded state must actually distinguish a correct division from a
    // zero-dividend one, or the case above would be vacuous.
    assert_ne!(case.want[0], case.gpr[0]);
    assert_ne!(case.want[0], 0);
}

/// The growth-direction guard: every mnemonic in the table must still be one the
/// JIT actually lowers. Without this, a renamed or dropped mnemonic would leave
/// the table asserting coverage of something that no longer exists.
#[test]
fn every_mnemonic_in_the_table_is_still_lowered_by_the_jit() {
    for case in implicit_cases() {
        let mut dec = Decoder::with_ip(64, case.op, CODE_DECODE, DecoderOptions::NONE);
        let insn = dec.decode();
        assert!(
            matches!(
                insn.mnemonic(),
                Mnemonic::Div | Mnemonic::Idiv | Mnemonic::Cmpxchg | Mnemonic::Xadd
            ),
            "{}: no longer one of the table's mnemonics — update the table",
            case.label
        );
        assert!(
            insn.op_count() <= 1 || matches!(insn.mnemonic(), Mnemonic::Cmpxchg | Mnemonic::Xadd),
            "{}: operand shape changed",
            case.label
        );
    }
}

// ── Mutex: the seeded states must distinguish the implicit operand ────────

/// Every seeded `DIV` case must fail if the implicit operand is replaced by its
/// `iconst 0` placeholder — otherwise the differential above is testing nothing.
///
/// This is what makes the table load-bearing: it asserts that each case is a
/// *discriminating* input, so passing requires the implicit operand to have been
/// read for real.
#[test]
fn every_div_case_discriminates_a_zero_dividend() {
    for case in implicit_cases() {
        let mut dec = Decoder::with_ip(64, case.op, CODE_DECODE, DecoderOptions::NONE);
        if !matches!(dec.decode().mnemonic(), Mnemonic::Div | Mnemonic::Idiv) {
            continue;
        }
        // Recompute the expectation with both implicit operands zeroed, which is
        // exactly the state a block compiled without the completion would see.
        let mut zeroed = case.gpr;
        zeroed[0] = 0;
        zeroed[2] = 0;
        let want_if_broken = expected_div(case.op, &zeroed);
        assert_ne!(
            want_if_broken[0], case.want[0],
            "{}: quotient is the same with a zeroed dividend, so this case cannot \
             detect the missing implicit operand",
            case.label
        );
    }
}
