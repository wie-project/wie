//! SSE/FP execution for the iced interpreter, plus the half-lane helpers shared
//! with the JIT lowering.
//!
//! The ABI-stable opcode enums (`SseIntOp` / `SseShiftOp` / `SseFpUnOp` /
//! `SseFpBinOp` / `SseCvtOp`) live in [`super::sse_types`].

use crate::CpuError;
use crate::consts::{
    BITS_PER_BYTE, DWORD_BITS, DWORD_BYTES, DWORD_MASK, QWORD_BITS, QWORD_BYTES, SPLAT_DWORD,
    SPLAT_WORD, WORD_BITS, XMM_BYTES,
};
use crate::mem::GuestMemory;
use crate::regs::{RegFile, Rflags};
use iced_x86::{Instruction, Mnemonic, OpKind};

use super::{
    AccessType, FpOp, InvalidMem, SseBitOp, SseCvtOp, SseFpBinOp, SseFpUnOp, SseIntOp, SseShiftOp,
    StepExecError, effective_address, read_mem_value, write_mem_value,
};

/// Packed f64 lanes in an XMM register (2 × 64-bit).
const F64_LANES: usize = 2;
/// Packed f32 lanes in an XMM register (4 × 32-bit).
const F32_LANES: usize = 4;

/// Bytes of a scalar FP operand: 8 for double, 4 for single.
#[must_use]
fn scalar_fp_bytes(is_double: bool) -> usize {
    if is_double { QWORD_BYTES } else { DWORD_BYTES }
}

/// All-ones mask covering the low `nbytes` bytes of a 128-bit XMM value.
#[must_use]
fn xmm_low_mask(nbytes: usize) -> u128 {
    (1u128 << (nbytes.saturating_mul(8))) - 1
}

pub(super) fn is_sse_movsd(instr: &Instruction) -> bool {
    instr.op0_register().is_xmm()
        || instr.op1_register().is_xmm()
        || matches!(
            instr.code(),
            iced_x86::Code::Movsd_xmm_xmmm64 | iced_x86::Code::Movsd_xmmm64_xmm
        )
}

pub(super) fn exec_sse_mov(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    nbytes: usize,
    scalar_merge: bool,
) -> Result<(), StepExecError> {
    let val = read_sse_op(mem, regs, instr, 1, nbytes)?;
    write_sse_op(mem, regs, instr, 0, val, nbytes, scalar_merge)
}

pub(super) fn exec_sse_bitwise(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseBitOp,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    let r = match op {
        SseBitOp::Xor => a ^ b,
        SseBitOp::And => a & b,
        SseBitOp::Or => a | b,
        SseBitOp::Andn => (!a) & b,
    };
    write_sse_op(mem, regs, instr, 0, r, 16, false)
}

fn fp32(op: FpOp, a: f32, b: f32) -> f32 {
    match op {
        FpOp::Add => a + b,
        FpOp::Sub => a - b,
        FpOp::Mul => a * b,
        FpOp::Div => a / b,
    }
}

fn fp64(op: FpOp, a: f64, b: f64) -> f64 {
    match op {
        FpOp::Add => a + b,
        FpOp::Sub => a - b,
        FpOp::Mul => a * b,
        FpOp::Div => a / b,
    }
}

/// SSE compare predicate (imm8 0..7). NaN semantics per IEEE-754 as encoded
/// by the x86 table: LT/LE false on NaN, UNORD true on NaN, NEQ true on NaN,
/// NLT/NLE true on NaN, ORD false on NaN.
fn fp_cmp_pred(pred: i32, a: f64, b: f64) -> bool {
    match pred {
        0 => a == b,
        1 => a < b,
        2 => a <= b,
        3 => a.is_nan() || b.is_nan(),
        4 => a != b,
        // Negated form is the x86 NLT/NLE encoding: true on unordered (NaN).
        // `partial_cmp` rewrites obscure that; see doc comment above.
        #[expect(clippy::neg_cmp_op_on_partial_ord)]
        5 => !(a < b),
        #[expect(clippy::neg_cmp_op_on_partial_ord)]
        6 => !(a <= b),
        7 => !a.is_nan() && !b.is_nan(),
        _ => false,
    }
}

/// `CMPPD/CMPPS/CMPSS/CMPSD (xmm, xmm/m, imm8)` — FP compare producing an
/// all-ones/zero lane mask. Scalar forms modify only the low element and
/// preserve the destination's upper bits.
pub(super) fn exec_sse_cmp_fp(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    packed: bool,
    double: bool,
) -> Result<(), StepExecError> {
    if instr.op_kind(0) != OpKind::Register || !instr.op_register(0).is_xmm() {
        return Err(StepExecError::Cpu(CpuError::Message(
            "fp cmp destination must be xmm".into(),
        )));
    }
    if instr.op_count() < 3 || instr.op_kind(2) != OpKind::Immediate8 {
        return Err(StepExecError::Cpu(CpuError::Message(
            "fp cmp missing imm8 predicate".into(),
        )));
    }
    let pred = (instr.immediate(2) & 0xff) as u8 as i32;
    let esize = if double { 8_usize } else { 4 };
    let lanes = if packed { 16 / esize } else { 1 };

    // Source: full vector for packed forms; low element only for scalar.
    let src_bits = if packed { 128 } else { esize * 8 };
    let src = read_sse_op(mem, regs, instr, 1, src_bits / 8)?;

    // Destination register value (upper bits preserved for scalar forms).
    let dst_reg = instr.op_register(0);
    let dst_val = regs.read_xmm(dst_reg)?;

    let mut result: u128 = 0;
    for lane in 0..lanes {
        let shift = (lane * esize * 8) as u32;
        let mask = if double {
            let raw = (src >> shift) as u64;
            let a = f64::from_bits((dst_val >> shift) as u64);
            let b = f64::from_bits(raw);
            bool_mask(fp_cmp_pred(pred, a, b), esize)
        } else {
            let raw = ((src >> shift) & 0xffff_ffff) as u32;
            let a = f32::from_bits(((dst_val >> shift) & 0xffff_ffff) as u32);
            let b = f32::from_bits(raw);
            bool_mask(fp_cmp_pred(pred, f64::from(a), f64::from(b)), esize)
        };
        result |= (mask as u128) << shift;
    }

    let new_val = if packed {
        result
    } else {
        let low_mask: u128 = if esize == 8 {
            0xffff_ffff_ffff_ffff_u128
        } else {
            0xffff_ffff
        };
        (dst_val & !low_mask) | (result & low_mask)
    };
    write_sse_op(mem, regs, instr, 0, new_val, 16, false)
}

fn bool_mask(v: bool, esize: usize) -> u64 {
    if v {
        match esize {
            4 => u64::from(u32::MAX),
            _ => u64::MAX,
        }
    } else {
        0
    }
}

/// `RCPPS/RCPSS/RSQRTPS/RSQRTSS` — packed/scalar reciprocal and reciprocal
/// square-root of packed singles. Real hardware computes these approximately;
/// we use exact IEEE ops, which is within the documented tolerance envelope.
pub(super) fn exec_sse_rcp_rsqrt(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    packed: bool,
    recip_sqrt: bool,
) -> Result<(), StepExecError> {
    let esize = 4_usize;
    let lanes = if packed { 16 / esize } else { 1 };
    let src_bits = if packed { 128 } else { esize * 8 };
    let src = read_sse_op(mem, regs, instr, 1, src_bits / 8)?;

    let mut result: u128 = 0;
    for lane in 0..lanes {
        let shift = (lane * esize * 8) as u32;
        let a = f32::from_bits(((src >> shift) & 0xffff_ffff) as u32);
        let v = if recip_sqrt { 1.0 / a.sqrt() } else { 1.0 / a };
        result |= (u128::from(v.to_bits())) << shift;
    }

    // Scalar forms preserve the destination's upper bits.
    if packed {
        write_sse_op(mem, regs, instr, 0, result, 16, false)
    } else {
        let dst_reg = instr.op_register(0);
        let dst_val = regs.read_xmm(dst_reg)?;
        let low_mask: u128 = 0xffff_ffff;
        write_sse_op(
            mem,
            regs,
            instr,
            0,
            (dst_val & !low_mask) | (result & low_mask),
            16,
            false,
        )
    }
}

/// `MOVMSKPD/MOVMSKPS r32, xmm` — pack the sign bit of each FP element into
/// the low bits of the destination GPR (remaining bits zeroed).
pub(super) fn exec_sse_movmsk(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    double: bool,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, 16)? as u64;
    let (lanes, esize_bits) = if double { (2_usize, 64_u64) } else { (4, 32) };
    let mut mask: u64 = 0;
    for lane in 0..lanes {
        let shift = lane as u64 * esize_bits;
        let sign = (src >> (shift + esize_bits - 1)) & 1;
        mask |= sign << lane;
    }
    regs.write_reg(instr.op_register(0), mask)?;
    Ok(())
}

pub(super) fn exec_sse_scalar_fp(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: FpOp,
    is_f64: bool,
) -> Result<(), StepExecError> {
    if is_f64 {
        let a = read_sse_op(mem, regs, instr, 0, 8)?;
        let b = read_sse_op(mem, regs, instr, 1, 8)?;
        let fa = f64::from_bits(a as u64);
        let fb = f64::from_bits(b as u64);
        let r = u128::from(fp64(op, fa, fb).to_bits());
        write_sse_op(mem, regs, instr, 0, r, 8, true)
    } else {
        let a = read_sse_op(mem, regs, instr, 0, 4)?;
        let b = read_sse_op(mem, regs, instr, 1, 4)?;
        let fa = f32::from_bits(a as u32);
        let fb = f32::from_bits(b as u32);
        let r = u128::from(fp32(op, fa, fb).to_bits());
        write_sse_op(mem, regs, instr, 0, r, 4, true)
    }
}

pub(super) fn exec_sse_packed_fp(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: FpOp,
    is_f64: bool,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    let mut out = 0_u128;
    if is_f64 {
        for i in 0..F64_LANES {
            let shift = i * usize::try_from(QWORD_BITS).unwrap_or(0);
            let fa = f64::from_bits(((a >> shift) & u128::from(u64::MAX)) as u64);
            let fb = f64::from_bits(((b >> shift) & u128::from(u64::MAX)) as u64);
            out |= u128::from(fp64(op, fa, fb).to_bits()) << shift;
        }
    } else {
        for i in 0..F32_LANES {
            let shift = i * usize::try_from(DWORD_BITS).unwrap_or(0);
            let fa = f32::from_bits(((a >> shift) & 0xffff_ffff) as u32);
            let fb = f32::from_bits(((b >> shift) & 0xffff_ffff) as u32);
            out |= u128::from(fp32(op, fa, fb).to_bits()) << shift;
        }
    }
    write_sse_op(mem, regs, instr, 0, out, 16, false)
}

pub(super) fn exec_sse_movq(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    // `MOVQ xmm, r/m64`: low quadword moved, bits [127:64] cleared.
    if instr.op0_register().is_xmm() {
        let v = read_sse_op(mem, regs, instr, 1, QWORD_BYTES)?;
        let v = v & u128::from(u64::MAX);
        regs.write_xmm(instr.op_register(0), v)?;
        return Ok(());
    }
    // `MOVQ r/m64, xmm`: extract the low quadword.
    if instr.op1_register().is_xmm() {
        let v = regs.read_xmm(instr.op_register(1))? as u64;
        if instr.op0_kind() == OpKind::Memory {
            write_mem_value(mem, effective_address(regs, instr)?, v, QWORD_BYTES)?;
        } else {
            regs.write_reg(instr.op_register(0), v)?;
        }
        return Ok(());
    }
    Err(StepExecError::Cpu(CpuError::Message(
        "movq without xmm".into(),
    )))
}

pub(super) fn exec_sse_movd(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    if instr.op0_register().is_xmm() {
        let v = if instr.op1_kind() == OpKind::Memory {
            read_mem_value(mem, effective_address(regs, instr)?, DWORD_BYTES)?
        } else {
            regs.read_reg(instr.op_register(1))? & 0xffff_ffff
        };
        // Zero-extend into XMM.
        regs.write_xmm(instr.op_register(0), u128::from(v as u32))?;
        return Ok(());
    }
    if instr.op1_register().is_xmm() {
        let v = regs.read_xmm(instr.op_register(1))? as u64 & 0xffff_ffff;
        if instr.op0_kind() == OpKind::Memory {
            write_mem_value(mem, effective_address(regs, instr)?, v, DWORD_BYTES)?;
        } else {
            regs.write_reg(instr.op_register(0), v)?;
        }
        return Ok(());
    }
    Err(StepExecError::Cpu(CpuError::Message(
        "movd without xmm".into(),
    )))
}

/// `PMOVMSKB r32, xmm` — pack the MSB (sign bit) of each of the 16 bytes of
/// the source XMM register into the low 16 bits of the destination GPR
/// (byte 0's sign bit → bit 0, … byte 15's sign bit → bit 15); the upper
/// bits of the GPR are zeroed (the 32-bit destination write zero-extends).
///
/// The VEX-encoded `VPMOVMSKB` decodes to the same mnemonic and register
/// operands, so this handler covers both forms.
pub(super) fn exec_sse_pmovmskb(
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = regs.read_xmm(instr.op_register(1))?;
    let mut mask: u32 = 0;
    for i in 0..XMM_BYTES {
        let shift = i.saturating_mul(8);
        let byte = (src >> shift) & 0xff;
        if byte & 0x80 != 0 {
            mask |= 1_u32 << i;
        }
    }
    regs.write_reg(instr.op_register(0), u64::from(mask))?;
    Ok(())
}

/// `MOVHPS` — move 64 bits between XMM upper half and memory.
///
/// Two forms:
/// - `MOVHPS xmm, m64`  — load 8 bytes from m64 into xmm[127:64], low 64 unchanged
/// - `MOVHPS m64, xmm`  — store xmm[127:64] to m64
pub(super) fn exec_sse_movhps(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    if instr.op0_register().is_xmm() {
        // xmm_dst, m64_src  → load into upper 64 bits
        let src = read_sse_op(mem, regs, instr, 1, QWORD_BYTES)?; // 8 bytes from memory
        let old = regs.read_xmm(instr.op_register(0))?; // current XMM value
        let new = (old & u128::from(u64::MAX)) | (src << 64); // merge into upper half
        regs.write_xmm(instr.op_register(0), new)?;
    } else {
        // m64_dst, xmm_src  → store upper 64 bits to memory
        let src_reg = instr.op_register(1);
        let xmm_val = regs.read_xmm(src_reg)?;
        let upper = (xmm_val >> 64) as u64;
        write_sse_op(mem, regs, instr, 0, u128::from(upper), QWORD_BYTES, false)?;
    }
    Ok(())
}

/// `MOVHLPS` / `MOVLHPS` — move packed floats between XMM upper/lower halves.
///
/// - `MOVHLPS xmm1, xmm2`: xmm1[63:0] = xmm2[127:64], xmm1[127:64] unchanged
/// - `MOVLHPS xmm1, xmm2`: xmm1[127:64] = xmm2[63:0], xmm1[63:0] unchanged
pub(super) fn exec_sse_movhlps(
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src = instr.op_register(1);
    let dst_val = regs.read_xmm(dst)?;
    let src_val = regs.read_xmm(src)?;
    let new = match instr.mnemonic() {
        Mnemonic::Movhlps => {
            // upper 64 of src → lower 64 of dst, keep dst[127:64]
            (dst_val & !u128::from(u64::MAX)) | ((src_val >> 64) & u128::from(u64::MAX))
        }
        _ => {
            // Mnemonic::Movlhps:
            // lower 64 of src → upper 64 of dst, keep dst[63:0]
            (dst_val & u128::from(u64::MAX)) | ((src_val & u128::from(u64::MAX)) << 64)
        }
    };
    regs.write_xmm(dst, new)?;
    Ok(())
}

/// `PUNPCKLQDQ` / `PUNPCKHQDQ` — unpack and interleave quadwords from two XMM registers.
///
/// - `PUNPCKLQDQ xmm1, xmm2`: keep low half of dst, place low half of src in high half.
/// - `PUNPCKHQDQ xmm1, xmm2`: place high half of dst in low half, high half of src in high half.
pub(super) fn exec_sse_punpck(
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src = instr.op_register(1);
    let a = regs.read_xmm(dst)?;
    let b = regs.read_xmm(src)?;
    let new = if instr.mnemonic() == Mnemonic::Punpcklqdq {
        (a & u128::from(u64::MAX)) | ((b & u128::from(u64::MAX)) << 64)
    } else {
        // Punpckhqdq
        ((a >> 64) & u128::from(u64::MAX)) | (b & !u128::from(u64::MAX))
    };
    regs.write_xmm(dst, new)?;
    Ok(())
}

/// `PSHUFD` — shuffle doublewords from XMM register (SSE2).
///
/// Copies four 32-bit lanes from `src` to `dst` according to an imm8
/// control byte at the end of the instruction encoding.
pub(super) fn exec_sse_pshufd(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src_val = read_sse_op(mem, regs, instr, 1, XMM_BYTES)?;
    let imm8 = instr.immediate(2) as u8;
    let mut result: u128 = 0;
    for i in 0..F32_LANES {
        let src_lane = ((imm8 >> (i * 2)) & 3) as usize;
        let lane = ((src_val >> (src_lane * 32)) & 0xffff_ffff) as u32;
        result |= u128::from(lane) << (i * 32);
    }
    regs.write_xmm(dst, result)?;
    Ok(())
}

/// `SHUFPD` — shuffle packed double-precision values from two sources (SSE2).
///
/// Each of the two 64-bit destination lanes independently selects the low or
/// high lane of the first source (the destination register, `imm8` bit 0) or
/// of the second source (register/memory, `imm8` bit 1). The compiler emits
/// this for 128-bit string/struct copies (e.g. the UCRT `wcscpy` fast path),
/// so a guest reaching it must not die with "unimplemented mnemonic".
pub(super) fn exec_sse_shufpd(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src1 = regs.read_xmm(dst)?;
    let src2 = read_sse_op(mem, regs, instr, 1, 16)?;
    let imm8 = instr.immediate(2) as u8;
    let low = if imm8 & 1 == 0 {
        src1 & u128::from(u64::MAX)
    } else {
        src2 & u128::from(u64::MAX)
    };
    let high = if imm8 & 2 == 0 {
        src1 & !u128::from(u64::MAX)
    } else {
        src2 & !u128::from(u64::MAX)
    };
    regs.write_xmm(dst, low | high)?;
    Ok(())
}

/// `PSHUFLW` / `PSHUFHW` — shuffle the low/high 16-bit lanes of an XMM register.
pub(super) fn exec_sse_pshuflw_hw(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src_val = read_sse_op(mem, regs, instr, 1, XMM_BYTES)?;
    let imm8 = instr.immediate(2) as u8;
    let low = instr.mnemonic() == Mnemonic::Pshuflw;
    // The untouched half (high words for pshuflw, low words for pshufhw) is
    // copied verbatim; only the other four words are permuted.
    let mut result = if low {
        src_val & (u128::from(u64::MAX) << 64)
    } else {
        src_val & u128::from(u64::MAX)
    };
    let base = if low { 0 } else { F32_LANES };
    for i in 0..F32_LANES {
        let src_lane = ((imm8 >> (i * 2)) & 3) as usize;
        let from = if low { src_lane } else { F32_LANES + src_lane };
        let word = ((src_val >> (from * 16)) & 0xffff) as u16;
        result |= u128::from(word) << ((base + i) * 16);
    }
    regs.write_xmm(dst, result)?;
    Ok(())
}

/// `PSHUFB` — byte-wise table lookup (SSSE3).
///
/// For each byte `i`: `dst[i] = (mask[i] & 0x80) ? 0 : table[mask[i] & 0x0F]`
/// with `table = dst (op0)` and `mask = src (op1)`.
pub(super) fn exec_sse_pshufb(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let table = read_sse_op(mem, regs, instr, 0, 16)?;
    let mask = read_sse_op(mem, regs, instr, 1, 16)?;
    let result = u128::from(sse_pshufb_lo(
        table as u64,
        (table >> 64) as u64,
        mask as u64,
        (mask >> 64) as u64,
    )) | (u128::from(sse_pshufb_hi(
        table as u64,
        (table >> 64) as u64,
        mask as u64,
        (mask >> 64) as u64,
    )) << 64);
    write_sse_op(mem, regs, instr, 0, result, 16, false)
}

/// Packed integer binary op (arithmetic / compare / pack) on full 128 bits.
pub(super) fn exec_sse_int_binop(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseIntOp,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    write_sse_op(mem, regs, instr, 0, sse_int_binop_u128(op, a, b), 16, false)
}

/// `Punpckl/H{bw,wd,dq}` — byte/word/dword unpack (the qdq variants use
/// [`exec_sse_punpck`], which is a pure half swap).
pub(super) fn exec_sse_punpck_lanes(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, 16)?;
    let b = read_sse_op(mem, regs, instr, 1, 16)?;
    let a_lo = a as u64;
    let a_hi = (a >> 64) as u64;
    let b_lo = b as u64;
    let b_hi = (b >> 64) as u64;
    // Both result halves come from the *same* source half: the L-family reads
    // the low 8/16 bytes of each operand (a_lo/b_lo), the H-family the high
    // bytes (a_hi/b_hi); the low result half interleaves the low sub-lanes and
    // the high result half the high sub-lanes.
    let (l_op, h_op, src_lo, src_hi) = match instr.mnemonic() {
        Mnemonic::Punpcklbw => (SseIntOp::Punpcklbw, SseIntOp::PunpckHiBw, a_lo, b_lo),
        Mnemonic::Punpcklwd => (SseIntOp::Punpcklwd, SseIntOp::PunpckHiWd, a_lo, b_lo),
        Mnemonic::Punpckldq => (SseIntOp::Punpckldq, SseIntOp::PunpckHiDq, a_lo, b_lo),
        Mnemonic::Punpckhbw => (SseIntOp::Punpcklbw, SseIntOp::PunpckHiBw, a_hi, b_hi),
        Mnemonic::Punpckhwd => (SseIntOp::Punpcklwd, SseIntOp::PunpckHiWd, a_hi, b_hi),
        Mnemonic::Punpckhdq => (SseIntOp::Punpckldq, SseIntOp::PunpckHiDq, a_hi, b_hi),
        _ => {
            return Err(StepExecError::Cpu(CpuError::Message(format!(
                "punpck lanes {:?}",
                instr.mnemonic()
            ))));
        }
    };
    let lo = sse_int_binop_half(l_op, src_lo, src_hi);
    let hi = sse_int_binop_half(h_op, src_lo, src_hi);
    write_sse_op(
        mem,
        regs,
        instr,
        0,
        u128::from(lo) | (u128::from(hi) << 64),
        16,
        false,
    )
}

/// Map a packed-integer mnemonic to its ABI opcode (arithmetic/compare/pack only).
pub(super) fn sse_int_op(m: Mnemonic) -> SseIntOp {
    match m {
        Mnemonic::Paddb => SseIntOp::Paddb,
        Mnemonic::Paddw => SseIntOp::Paddw,
        Mnemonic::Paddd => SseIntOp::Paddd,
        Mnemonic::Paddq => SseIntOp::Paddq,
        Mnemonic::Psubb => SseIntOp::Psubb,
        Mnemonic::Psubw => SseIntOp::Psubw,
        Mnemonic::Psubd => SseIntOp::Psubd,
        Mnemonic::Psubq => SseIntOp::Psubq,
        Mnemonic::Paddsb => SseIntOp::Paddsb,
        Mnemonic::Paddsw => SseIntOp::Paddsw,
        Mnemonic::Paddusb => SseIntOp::Paddusb,
        Mnemonic::Paddusw => SseIntOp::Paddusw,
        Mnemonic::Psubsb => SseIntOp::Psubsb,
        Mnemonic::Psubsw => SseIntOp::Psubsw,
        Mnemonic::Psubusb => SseIntOp::Psubusb,
        Mnemonic::Psubusw => SseIntOp::Psubusw,
        Mnemonic::Pmullw => SseIntOp::Pmullw,
        Mnemonic::Pmulhw => SseIntOp::Pmulhw,
        Mnemonic::Pmulhuw => SseIntOp::Pmulhuw,
        Mnemonic::Pmuludq => SseIntOp::Pmuludq,
        Mnemonic::Pmaddwd => SseIntOp::Pmaddwd,
        Mnemonic::Pcmpeqb => SseIntOp::Pcmpeqb,
        Mnemonic::Pcmpeqw => SseIntOp::Pcmpeqw,
        Mnemonic::Pcmpeqd => SseIntOp::Pcmpeqd,
        Mnemonic::Pcmpgtb => SseIntOp::Pcmpgtb,
        Mnemonic::Pcmpgtw => SseIntOp::Pcmpgtw,
        Mnemonic::Pcmpgtd => SseIntOp::Pcmpgtd,
        Mnemonic::Packsswb => SseIntOp::Packsswb,
        Mnemonic::Packssdw => SseIntOp::Packssdw,
        Mnemonic::Packuswb => SseIntOp::Packuswb,
        _ => {
            // Unreachable: the dispatch arms only call `sse_int_op` for the
            // mnemonics enumerated above.
            SseIntOp::Paddb
        }
    }
}

/// Map a packed-shift mnemonic to its ABI opcode.
pub(super) fn sse_shift_op(m: Mnemonic) -> SseShiftOp {
    match m {
        Mnemonic::Psllw => SseShiftOp::Psllw,
        Mnemonic::Pslld => SseShiftOp::Pslld,
        Mnemonic::Psllq => SseShiftOp::Psllq,
        Mnemonic::Psrlw => SseShiftOp::Psrlw,
        Mnemonic::Psrld => SseShiftOp::Psrld,
        Mnemonic::Psrlq => SseShiftOp::Psrlq,
        Mnemonic::Psraw => SseShiftOp::Psraw,
        Mnemonic::Psrad => SseShiftOp::Psrad,
        _ => {
            // Unreachable: the dispatch arms only call `sse_shift_op` for the
            // mnemonics enumerated above.
            SseShiftOp::Psllw
        }
    }
}

/// `Psll/Psrl/Psra{w,d,q}` — packed shifts (imm8 or variable XMM count).
pub(super) fn exec_sse_shift(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseShiftOp,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, XMM_BYTES)?;
    let count = match instr.op_kind(1) {
        OpKind::Immediate8 | OpKind::Immediate16 | OpKind::Immediate32 => {
            let c = u64::from(instr.immediate(1) as u8);
            let width = match op {
                SseShiftOp::Psllw | SseShiftOp::Psrlw | SseShiftOp::Psraw => WORD_BITS,
                SseShiftOp::Pslld | SseShiftOp::Psrld | SseShiftOp::Psrad => DWORD_BITS,
                SseShiftOp::Psllq | SseShiftOp::Psrlq => QWORD_BITS,
            };
            let splat = match width {
                WORD_BITS => c * SPLAT_WORD,
                DWORD_BITS => c * SPLAT_DWORD,
                _ => c,
            };
            u128::from(splat) | (u128::from(splat) << 64)
        }
        _ => read_sse_op(mem, regs, instr, 1, XMM_BYTES)?,
    };
    write_sse_op(
        mem,
        regs,
        instr,
        0,
        sse_shift_u128(op, a, count),
        XMM_BYTES,
        false,
    )
}

/// `Psrldq/Psldq` — byte-granular shift of the whole 128-bit XMM register
/// (66 0F 73 /3 ib and /7 ib). Only the imm8 form exists; a count of 16 or
/// more zeroes the register (the shift is masked to 4 bits on x86).
pub(super) fn exec_sse_byte_shift(
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let a = regs.read_xmm(dst)?;
    let bytes = (instr.immediate(1) & 0xff) as u32;
    let right = instr.mnemonic() == Mnemonic::Psrldq;
    regs.write_xmm(dst, sse_byte_shift_u128(right, a, bytes))?;
    Ok(())
}

/// Whole-XMM byte shift core shared by both directions.
fn sse_byte_shift_u128(right: bool, a: u128, bytes: u32) -> u128 {
    if bytes >= 16 {
        return 0;
    }
    if bytes == 0 {
        return a;
    }
    let bits = u128::from(bytes) * 8;
    if right { a >> bits } else { a << bits }
}

/// `Sqrtss/Sqrtsd` — scalar FP square root, merged into the destination's low lane.
pub(super) fn exec_sse_sqrt_scalar(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    is_double: bool,
) -> Result<(), StepExecError> {
    let nbytes = scalar_fp_bytes(is_double);
    let src = read_sse_op(mem, regs, instr, 1, nbytes)?;
    let op = if is_double {
        SseFpUnOp::Sqrtsd
    } else {
        SseFpUnOp::Sqrtss
    };
    let r = sse_fp_unop(op, src as u64);
    write_sse_op(mem, regs, instr, 0, u128::from(r), nbytes, true)
}

/// `Sqrtps/Sqrtpd` — packed FP square root (reads op1 only).
pub(super) fn exec_sse_sqrt_packed(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    is_double: bool,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, XMM_BYTES)?;
    let op = if is_double {
        SseFpUnOp::Sqrtpd
    } else {
        SseFpUnOp::Sqrtps
    };
    let r = u128::from(sse_fp_unop(op, src as u64))
        | (u128::from(sse_fp_unop(op, (src >> QWORD_BITS) as u64)) << QWORD_BITS);
    write_sse_op(mem, regs, instr, 0, r, XMM_BYTES, false)
}

/// `Minss/Minsd/Maxss/Maxsd` — scalar FP min/max, merged into the low lane.
pub(super) fn exec_sse_minmax_scalar(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseFpBinOp,
) -> Result<(), StepExecError> {
    let is_double = matches!(op, SseFpBinOp::Minsd | SseFpBinOp::Maxsd);
    let nbytes = scalar_fp_bytes(is_double);
    let a = read_sse_op(mem, regs, instr, 0, nbytes)?;
    let b = read_sse_op(mem, regs, instr, 1, nbytes)?;
    let r = sse_fp_binop(op, a as u64, b as u64);
    write_sse_op(mem, regs, instr, 0, u128::from(r), nbytes, true)
}

/// `Minps/Maxps/Minpd/Maxpd` — packed FP min/max.
pub(super) fn exec_sse_minmax_packed(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: SseFpBinOp,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, XMM_BYTES)?;
    let b = read_sse_op(mem, regs, instr, 1, XMM_BYTES)?;
    let r = u128::from(sse_fp_binop(op, a as u64, b as u64))
        | (u128::from(sse_fp_binop(
            op,
            (a >> QWORD_BITS) as u64,
            (b >> QWORD_BITS) as u64,
        )) << QWORD_BITS);
    write_sse_op(mem, regs, instr, 0, r, XMM_BYTES, false)
}

/// `Comiss/Comisd/Ucomiss/Ucomisd` — compare FP, set ZF/PF/CF (OF/AF/SF cleared).
pub(super) fn exec_sse_comis(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    is_double: bool,
) -> Result<(), StepExecError> {
    let nbytes = scalar_fp_bytes(is_double);
    let a = read_sse_op(mem, regs, instr, 0, nbytes)?;
    let b = read_sse_op(mem, regs, instr, 1, nbytes)?;
    let (unordered, eq, lt) = if is_double {
        let fa = f64::from_bits(a as u64);
        let fb = f64::from_bits(b as u64);
        (fa.is_nan() || fb.is_nan(), fa == fb, fa < fb)
    } else {
        let fa = f32::from_bits(a as u32);
        let fb = f32::from_bits(b as u32);
        (fa.is_nan() || fb.is_nan(), fa == fb, fa < fb)
    };
    regs.set_flag(Rflags::CF, lt || unordered);
    regs.set_flag(Rflags::PF, unordered);
    regs.set_flag(Rflags::ZF, eq || unordered);
    regs.set_flag(Rflags::OF, false);
    regs.set_flag(Rflags::AF, false);
    regs.set_flag(Rflags::SF, false);
    Ok(())
}

/// `Cvtsi2ss/Cvtsi2sd` — signed integer → scalar FP, merged into the low lane.
pub(super) fn exec_sse_cvt_gpr_to_fp(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let is_double = instr.mnemonic() == Mnemonic::Cvtsi2sd;
    // 64-bit when the source is r64 or m64 (memory width is authoritative).
    let is64 = match instr.op1_kind() {
        OpKind::Memory => instr.memory_size().size() == 8,
        _ => instr.op_register(1).size() == 8,
    };
    let src = if instr.op1_kind() == OpKind::Memory {
        let addr = effective_address(regs, instr)?;
        let w = if is64 { QWORD_BYTES } else { DWORD_BYTES };
        read_mem_value(mem, addr, w)?
    } else {
        regs.read_reg(instr.op_register(1))?
    };
    let op = match (is_double, is64) {
        (false, false) => SseCvtOp::Cvtsi2ss32,
        (false, true) => SseCvtOp::Cvtsi2ss64,
        (true, false) => SseCvtOp::Cvtsi2sd32,
        (true, true) => SseCvtOp::Cvtsi2sd64,
    };
    let bits = sse_cvt(op, src);
    let nbytes = scalar_fp_bytes(is_double);
    write_sse_op(mem, regs, instr, 0, u128::from(bits), nbytes, true)
}

/// `Cvttss2si/Cvtss2si/Cvttsd2si/Cvtsd2si` — scalar FP → signed integer.
pub(super) fn exec_sse_cvt_fp_to_gpr(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let is_double = matches!(instr.mnemonic(), Mnemonic::Cvttsd2si | Mnemonic::Cvtsd2si);
    let trunc = matches!(instr.mnemonic(), Mnemonic::Cvttss2si | Mnemonic::Cvttsd2si);
    let is64 = instr.op_register(0).size() == 8;
    let nbytes = scalar_fp_bytes(is_double);
    let a = read_sse_op(mem, regs, instr, 1, nbytes)?;
    let op = match (is_double, is64, trunc) {
        (false, false, true) => SseCvtOp::Cvttss2si32,
        (false, false, false) => SseCvtOp::Cvtss2si32,
        (false, true, true) => SseCvtOp::Cvttss2si64,
        (false, true, false) => SseCvtOp::Cvtss2si64,
        (true, false, true) => SseCvtOp::Cvttsd2si32,
        (true, false, false) => SseCvtOp::Cvtsd2si32,
        (true, true, true) => SseCvtOp::Cvttsd2si64,
        (true, true, false) => SseCvtOp::Cvtsd2si64,
    };
    let v = sse_cvt(op, a as u64);
    regs.write_reg(instr.op_register(0), v)
        .map_err(StepExecError::Cpu)
}

/// `Cvtps2dq/Cvtdq2ps/Cvttps2dq` — packed FP ↔ int (reads op1 only).
pub(super) fn exec_sse_cvt_packed(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, 16)?;
    let op = match instr.mnemonic() {
        Mnemonic::Cvtps2dq => SseCvtOp::Cvtps2dq,
        Mnemonic::Cvtdq2ps => SseCvtOp::Cvtdq2ps,
        _ => SseCvtOp::Cvttps2dq,
    };
    let r =
        u128::from(sse_cvt(op, src as u64)) | (u128::from(sse_cvt(op, (src >> 64) as u64)) << 64);
    write_sse_op(mem, regs, instr, 0, r, 16, false)
}

/// `Cvtdq2pd xmm, xmm/m64` — convert two packed signed dwords (the low 64
/// bits of the source) to two packed doubles in the destination. The upper
/// 64 bits of an XMM source are ignored.
pub(super) fn exec_sse_cvtdq2pd(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, QWORD_BYTES)?;
    let d0 = f64::from(src as u32 as i32);
    let d1 = f64::from(((src >> DWORD_BITS) as u32) as i32);
    let r = u128::from(d0.to_bits()) | (u128::from(d1.to_bits()) << QWORD_BITS);
    write_sse_op(mem, regs, instr, 0, r, XMM_BYTES, false)
}

/// `Cvtps2pd xmm, xmm/m64` — convert two packed single-precision floats (the
/// low 64 bits of the source) to two packed doubles in the destination. The
/// upper 64 bits of an XMM source are ignored.
pub(super) fn exec_sse_cvtps2pd(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, QWORD_BYTES)?;
    let f0 = f64::from(f32::from_bits(src as u32));
    let f1 = f64::from(f32::from_bits((src >> DWORD_BITS) as u32));
    let r = u128::from(f0.to_bits()) | (u128::from(f1.to_bits()) << QWORD_BITS);
    write_sse_op(mem, regs, instr, 0, r, XMM_BYTES, false)
}

/// `Cvtpd2dq xmm, xmm/m128` — convert two packed doubles to two packed signed
/// dwords (MXCSR rounding, nearest-even by default). The upper 64 bits of the
/// destination are zeroed.
pub(super) fn exec_sse_cvtpd2dq(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, XMM_BYTES)?;
    let lo = sse_cvt(SseCvtOp::Cvtpd2dq, src as u64);
    let hi = sse_cvt(SseCvtOp::Cvtpd2dq, (src >> QWORD_BITS) as u64);
    let r = u128::from(lo) | (u128::from(hi) << DWORD_BITS);
    write_sse_op(mem, regs, instr, 0, r, XMM_BYTES, false)
}

/// `Cvtpd2ps xmm, xmm/m128` — convert two packed doubles to two packed singles
/// (MXCSR rounding). The upper 64 bits of the destination are zeroed.
pub(super) fn exec_sse_cvtpd2ps(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, XMM_BYTES)?;
    let lo = sse_cvt(SseCvtOp::Cvtpd2ps, src as u64);
    let hi = sse_cvt(SseCvtOp::Cvtpd2ps, (src >> QWORD_BITS) as u64);
    let r = u128::from(lo) | (u128::from(hi) << DWORD_BITS);
    write_sse_op(mem, regs, instr, 0, r, XMM_BYTES, false)
}

/// `Cvttpd2dq xmm, xmm/m128` — convert two packed doubles to two packed signed
/// dwords with TRUNCATION toward zero. The upper 64 bits of the destination
/// are zeroed.
pub(super) fn exec_sse_cvttpd2dq(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = read_sse_op(mem, regs, instr, 1, XMM_BYTES)?;
    let lo = sse_cvt(SseCvtOp::Cvttpd2dq, src as u64);
    let hi = sse_cvt(SseCvtOp::Cvttpd2dq, (src >> QWORD_BITS) as u64);
    let r = u128::from(lo) | (u128::from(hi) << DWORD_BITS);
    write_sse_op(mem, regs, instr, 0, r, XMM_BYTES, false)
}

/// `Cvtsd2ss xmm, xmm/m64` — convert the low double to a single; bits 32-127
/// of the destination are preserved.
pub(super) fn exec_sse_cvtsd2ss(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src = read_sse_op(mem, regs, instr, 1, QWORD_BYTES)?;
    let f = f64::from_bits(src as u64) as f32;
    let old = regs.read_xmm(dst)?;
    // Keep bits 32-127; replace only the low 32 bits with the f32 result.
    let r = (old & 0xffff_ffff_ffff_ffff_ffff_ffff_0000_0000_u128) | u128::from(f.to_bits());
    regs.write_xmm(dst, r)?;
    Ok(())
}

/// `Cvtss2sd xmm, xmm/m32` — convert the low single to a double; bits 64-127
/// of the destination are preserved.
pub(super) fn exec_sse_cvtss2sd(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src = read_sse_op(mem, regs, instr, 1, DWORD_BYTES)?;
    let f = f64::from(f32::from_bits(src as u32));
    let old = regs.read_xmm(dst)?;
    let r = (old & 0xffff_ffff_ffff_ffff_0000_0000_0000_0000_u128) | u128::from(f.to_bits());
    regs.write_xmm(dst, r)?;
    Ok(())
}

/// `Unpcklpd` — unpack low packed double-precision floats (identical to punpcklqdq).
pub(super) fn exec_sse_unpcklpd(
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let dst = instr.op_register(0);
    let src = instr.op_register(1);
    let a = regs.read_xmm(dst)?;
    let b = regs.read_xmm(src)?;
    // Low 64 bits from dst, low 64 bits from src.
    let result = (a & u128::from(u64::MAX)) | ((b & u128::from(u64::MAX)) << QWORD_BITS);
    regs.write_xmm(dst, result)?;
    Ok(())
}

/// `Psadbw` — sum of absolute differences of unsigned bytes.
/// Low 8 bytes → summed into lower 16 bits of lower qword.
/// High 8 bytes → summed into lower 16 bits of upper qword.
pub(super) fn exec_sse_psadbw(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let a = read_sse_op(mem, regs, instr, 0, XMM_BYTES)?;
    let b = read_sse_op(mem, regs, instr, 1, XMM_BYTES)?;
    let low_sum: u64 = (0..QWORD_BYTES)
        .map(|i| {
            let shift = i * usize::try_from(BITS_PER_BYTE).unwrap_or(0);
            let a_byte = ((a >> shift) & 0xff) as u64;
            let b_byte = ((b >> shift) & 0xff) as u64;
            a_byte.abs_diff(b_byte)
        })
        .sum();
    let high_sum: u64 = (0..QWORD_BYTES)
        .map(|i| {
            let shift = (i + QWORD_BYTES) * usize::try_from(BITS_PER_BYTE).unwrap_or(0);
            let a_byte = ((a >> shift) & 0xff) as u64;
            let b_byte = ((b >> shift) & 0xff) as u64;
            a_byte.abs_diff(b_byte)
        })
        .sum();
    let result = u128::from(low_sum) | (u128::from(high_sum) << 64);
    write_sse_op(mem, regs, instr, 0, result, XMM_BYTES, false)
}

// --- Packed integer SSE2: shared lane core (interpreter + JIT host helpers) ---
//
// Every op here is *half-aligned*: lanes of 1/2/4/8 bytes never straddle the
// u64 boundary of an XMM register, so the whole 128-bit op decomposes into two
// independent 64-bit halves. The JIT (with `WIE_JIT_SIMD=0`) and the iced
// interpreter therefore share one half-function per op; the JIT's vector path
// lowers the same ops to NEON directly.

/// Mask of the low `bits` bits of a u64 (all 64 for `bits >= 64`).
fn u64_mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1_u64 << bits) - 1
    }
}

/// Sign-extend the low `bits` bits of `v`.
fn sign_extend_bits(v: u64, bits: u32) -> i64 {
    if bits >= 64 {
        v as i64
    } else {
        let sh = 64 - bits;
        ((v << sh) as i64) >> sh
    }
}

/// Apply `f` lane-wise over `bits`-wide lanes of two u64 halves.
fn sse_lane_binop<F>(a: u64, b: u64, bits: u32, f: F) -> u64
where
    F: Fn(u64, u64) -> u64,
{
    let lanes = QWORD_BITS / bits;
    let mask = u64_mask(bits);
    let mut out = 0_u64;
    for i in 0..lanes {
        let sh = i * bits;
        let x = (a >> sh) & mask;
        let y = (b >> sh) & mask;
        out |= (f(x, y) & mask) << sh;
    }
    out
}

/// Apply `f` lane-wise where lane `i` of `count` is that lane's shift amount.
fn sse_lane_shift<F>(a: u64, count: u64, bits: u32, f: F) -> u64
where
    F: Fn(u64, u64) -> u64,
{
    sse_lane_binop(a, count, bits, f)
}

/// One u64 half of a packed integer binary op.
pub(crate) fn sse_int_binop_half(op: SseIntOp, a: u64, b: u64) -> u64 {
    match op {
        SseIntOp::Paddb => sse_lane_binop(a, b, 8, u64::wrapping_add),
        SseIntOp::Paddw => sse_lane_binop(a, b, 16, u64::wrapping_add),
        SseIntOp::Paddd => sse_lane_binop(a, b, 32, u64::wrapping_add),
        SseIntOp::Paddq => a.wrapping_add(b),
        SseIntOp::Psubb => sse_lane_binop(a, b, 8, u64::wrapping_sub),
        SseIntOp::Psubw => sse_lane_binop(a, b, 16, u64::wrapping_sub),
        SseIntOp::Psubd => sse_lane_binop(a, b, 32, u64::wrapping_sub),
        SseIntOp::Psubq => a.wrapping_sub(b),
        SseIntOp::Paddsb => sse_lane_binop(a, b, 8, |x, y| {
            u64::from(
                (sign_extend_bits(x, 8) as i8).saturating_add(sign_extend_bits(y, 8) as i8) as u8,
            )
        }),
        SseIntOp::Paddsw => sse_lane_binop(a, b, 16, |x, y| {
            u64::from(
                (sign_extend_bits(x, 16) as i16).saturating_add(sign_extend_bits(y, 16) as i16)
                    as u16,
            )
        }),
        SseIntOp::Paddusb => sse_lane_binop(a, b, 8, |x, y| (x + y).min(0xff)),
        SseIntOp::Paddusw => sse_lane_binop(a, b, 16, |x, y| (x + y).min(0xffff)),
        SseIntOp::Psubsb => sse_lane_binop(a, b, 8, |x, y| {
            u64::from(
                (sign_extend_bits(x, 8) as i8).saturating_sub(sign_extend_bits(y, 8) as i8) as u8,
            )
        }),
        SseIntOp::Psubsw => sse_lane_binop(a, b, 16, |x, y| {
            u64::from(
                (sign_extend_bits(x, 16) as i16).saturating_sub(sign_extend_bits(y, 16) as i16)
                    as u16,
            )
        }),
        SseIntOp::Psubusb => sse_lane_binop(a, b, 8, u64::saturating_sub),
        SseIntOp::Psubusw => sse_lane_binop(a, b, 16, u64::saturating_sub),
        SseIntOp::Pmullw => sse_lane_binop(a, b, 16, u64::wrapping_mul),
        SseIntOp::Pmulhw => sse_lane_binop(a, b, 16, |x, y| {
            ((sign_extend_bits(x, 16) * sign_extend_bits(y, 16)) >> 16) as u64
        }),
        SseIntOp::Pmulhuw => sse_lane_binop(a, b, 16, |x, y| x.wrapping_mul(y) >> 16),
        // Result qword i = low dword of each operand's qword i (half-aligned).
        SseIntOp::Pmuludq => (a & 0xffff_ffff).wrapping_mul(b & 0xffff_ffff),
        SseIntOp::Pmaddwd => {
            let mut out = 0_u64;
            for i in 0..2 {
                let w0 = sign_extend_bits((a >> (i * 32)) & 0xffff, 16);
                let w1 = sign_extend_bits((a >> (i * 32 + 16)) & 0xffff, 16);
                let x0 = sign_extend_bits((b >> (i * 32)) & 0xffff, 16);
                let x1 = sign_extend_bits((b >> (i * 32 + 16)) & 0xffff, 16);
                let r = w0.wrapping_mul(x0).wrapping_add(w1.wrapping_mul(x1));
                out |= ((r as u64) & 0xffff_ffff) << (i * 32);
            }
            out
        }
        SseIntOp::Pcmpeqb => sse_lane_binop(a, b, 8, |x, y| if x == y { 0xff } else { 0 }),
        SseIntOp::Pcmpeqw => sse_lane_binop(a, b, 16, |x, y| if x == y { 0xffff } else { 0 }),
        SseIntOp::Pcmpeqd => sse_lane_binop(a, b, 32, |x, y| if x == y { 0xffff_ffff } else { 0 }),
        SseIntOp::Pcmpgtb => sse_lane_binop(a, b, 8, |x, y| {
            if sign_extend_bits(x, 8) > sign_extend_bits(y, 8) {
                0xff
            } else {
                0
            }
        }),
        SseIntOp::Pcmpgtw => sse_lane_binop(a, b, 16, |x, y| {
            if sign_extend_bits(x, 16) > sign_extend_bits(y, 16) {
                0xffff
            } else {
                0
            }
        }),
        SseIntOp::Pcmpgtd => sse_lane_binop(a, b, 32, |x, y| {
            if sign_extend_bits(x, 32) > sign_extend_bits(y, 32) {
                0xffff_ffff
            } else {
                0
            }
        }),
        SseIntOp::Packsswb => {
            let sat = |w: u64| (sign_extend_bits(w, 16).clamp(-128, 127) as u64) & 0xff;
            let mut out = 0_u64;
            for i in 0..4 {
                out |= sat((a >> (i * 16)) & 0xffff) << (i * 8);
            }
            for i in 0..4 {
                out |= sat((b >> (i * 16)) & 0xffff) << ((i + 4) * 8);
            }
            out
        }
        SseIntOp::Packssdw => {
            let sat = |d: u64| (sign_extend_bits(d, 32).clamp(-32768, 32767) as u64) & 0xffff;
            let mut out = 0_u64;
            for i in 0..2 {
                out |= sat((a >> (i * 32)) & 0xffff_ffff) << (i * 16);
            }
            for i in 0..2 {
                out |= sat((b >> (i * 32)) & 0xffff_ffff) << ((i + 2) * 16);
            }
            out
        }
        SseIntOp::Packuswb => {
            let sat = |w: u64| (sign_extend_bits(w, 16).clamp(0, 255) as u64) & 0xff;
            let mut out = 0_u64;
            for i in 0..4 {
                out |= sat((a >> (i * 16)) & 0xffff) << (i * 8);
            }
            for i in 0..4 {
                out |= sat((b >> (i * 16)) & 0xffff) << ((i + 4) * 8);
            }
            out
        }
        SseIntOp::Punpcklbw
        | SseIntOp::Punpcklwd
        | SseIntOp::Punpckldq
        | SseIntOp::PunpckHiBw
        | SseIntOp::PunpckHiWd
        | SseIntOp::PunpckHiDq => {
            let bits = match op {
                SseIntOp::Punpcklbw | SseIntOp::PunpckHiBw => 8,
                SseIntOp::Punpcklwd | SseIntOp::PunpckHiWd => 16,
                _ => 32,
            };
            let high = matches!(
                op,
                SseIntOp::PunpckHiBw | SseIntOp::PunpckHiWd | SseIntOp::PunpckHiDq
            );
            sse_punpck_half(a, b, bits, high)
        }
    }
}

/// Interleave the low (`high == false`) or high (`high == true`) sub-lanes of
/// two u64 halves, producing one u64 of the unpacked result.
fn sse_punpck_half(a: u64, b: u64, bits: u32, high: bool) -> u64 {
    let lanes = (64 / bits) / 2;
    let mask = u64_mask(bits);
    let src_shift = if high { lanes } else { 0 };
    let mut out = 0_u64;
    for i in 0..lanes {
        let x = (a >> ((src_shift + i) * bits)) & mask;
        let y = (b >> ((src_shift + i) * bits)) & mask;
        out |= x << (2 * i * bits);
        out |= y << ((2 * i + 1) * bits);
    }
    out
}

/// One u64 half of a packed shift. `count` holds per-lane counts (the JIT
/// splats an imm8 into every lane for the immediate forms). Counts at or above
/// the element width produce 0 (x86 PSLL/PSRL/PSRA semantics).
pub(crate) fn sse_shift_half(op: SseShiftOp, a: u64, count: u64) -> u64 {
    match op {
        SseShiftOp::Psllw => sse_lane_shift(a, count, WORD_BITS, |x, c| {
            if c >= u64::from(WORD_BITS) {
                0
            } else {
                x.wrapping_shl(c as u32)
            }
        }),
        SseShiftOp::Pslld => sse_lane_shift(a, count, DWORD_BITS, |x, c| {
            if c >= u64::from(DWORD_BITS) {
                0
            } else {
                x.wrapping_shl(c as u32)
            }
        }),
        SseShiftOp::Psllq => sse_lane_shift(a, count, QWORD_BITS, |x, c| {
            if c >= u64::from(QWORD_BITS) {
                0
            } else {
                x.wrapping_shl(c as u32)
            }
        }),
        SseShiftOp::Psrlw => sse_lane_shift(a, count, WORD_BITS, |x, c| {
            if c >= u64::from(WORD_BITS) {
                0
            } else {
                x.wrapping_shr(c as u32)
            }
        }),
        SseShiftOp::Psrld => sse_lane_shift(a, count, DWORD_BITS, |x, c| {
            if c >= u64::from(DWORD_BITS) {
                0
            } else {
                x.wrapping_shr(c as u32)
            }
        }),
        SseShiftOp::Psrlq => sse_lane_shift(a, count, QWORD_BITS, |x, c| {
            if c >= u64::from(QWORD_BITS) {
                0
            } else {
                x.wrapping_shr(c as u32)
            }
        }),
        SseShiftOp::Psraw => sse_lane_shift(a, count, WORD_BITS, |x, c| {
            if c >= u64::from(WORD_BITS) {
                0
            } else {
                (sign_extend_bits(x, WORD_BITS) >> c) as u64
            }
        }),
        SseShiftOp::Psrad => sse_lane_shift(a, count, DWORD_BITS, |x, c| {
            if c >= u64::from(DWORD_BITS) {
                0
            } else {
                (sign_extend_bits(x, DWORD_BITS) >> c) as u64
            }
        }),
    }
}

/// Byte-index from a pshufb control byte (low nibble selects the table byte;
/// bit 7 means the result byte is 0).
fn pshufb_src_index(mi: u8) -> Option<u32> {
    if mi & 0x80 != 0 {
        None
    } else {
        Some(u32::from(mi & 0x0f))
    }
}

/// Result bytes 0..8 of `pshufb table, mask` (table = a, mask = b).
pub(crate) fn sse_pshufb_lo(a_lo: u64, a_hi: u64, b_lo: u64, b_hi: u64) -> u64 {
    let table = [a_lo, a_hi];
    let mask = [b_lo, b_hi];
    let mut out = 0_u64;
    for i in 0..8 {
        let mi = ((mask[i / 8] >> ((i % 8) * 8)) & 0xff) as u8;
        let byte = match pshufb_src_index(mi) {
            Some(j) => ((table[usize::try_from(j / 8).unwrap_or(0)] >> ((j % 8) * 8)) & 0xff) as u8,
            None => 0,
        };
        out |= u64::from(byte) << (i * 8);
    }
    out
}

/// Result bytes 8..16 of `pshufb table, mask`.
pub(crate) fn sse_pshufb_hi(a_lo: u64, a_hi: u64, b_lo: u64, b_hi: u64) -> u64 {
    let table = [a_lo, a_hi];
    let mask = [b_lo, b_hi];
    let mut out = 0_u64;
    for i in 8..16 {
        let mi = ((mask[i / 8] >> ((i % 8) * 8)) & 0xff) as u8;
        let byte = match pshufb_src_index(mi) {
            Some(j) => ((table[usize::try_from(j / 8).unwrap_or(0)] >> ((j % 8) * 8)) & 0xff) as u8,
            None => 0,
        };
        out |= u64::from(byte) << ((i - 8) * 8);
    }
    out
}

/// x86 MINSS/MINPS semantics: if either operand is NaN the source (`b`) wins
/// (Rust's `f32::min`/`max` return the non-NaN operand instead).
fn sse_fp_minmax32(a: f32, b: f32, want_min: bool) -> f32 {
    if a.is_nan() || b.is_nan() {
        return b;
    }
    if want_min {
        if a < b { a } else { b }
    } else if a > b {
        a
    } else {
        b
    }
}

fn sse_fp_minmax64(a: f64, b: f64, want_min: bool) -> f64 {
    if a.is_nan() || b.is_nan() {
        return b;
    }
    if want_min {
        if a < b { a } else { b }
    } else if a > b {
        a
    } else {
        b
    }
}

/// Packed/scalar FP unary (sqrt family) on one u64 half.
pub(crate) fn sse_fp_unop(op: SseFpUnOp, a: u64) -> u64 {
    match op {
        SseFpUnOp::Sqrtps => {
            let f0 = f32::from_bits((a & DWORD_MASK) as u32).sqrt();
            let f1 = f32::from_bits(((a >> DWORD_BITS) & DWORD_MASK) as u32).sqrt();
            u64::from(f0.to_bits()) | (u64::from(f1.to_bits()) << DWORD_BITS)
        }
        SseFpUnOp::Sqrtpd | SseFpUnOp::Sqrtsd => f64::from_bits(a).sqrt().to_bits(),
        SseFpUnOp::Sqrtss => u64::from(f32::from_bits((a & DWORD_MASK) as u32).sqrt().to_bits()),
    }
}

/// Packed/scalar FP min/max on one u64 half.
pub(crate) fn sse_fp_binop(op: SseFpBinOp, a: u64, b: u64) -> u64 {
    match op {
        SseFpBinOp::Minps | SseFpBinOp::Maxps => {
            let want_min = op == SseFpBinOp::Minps;
            let f0 = sse_fp_minmax32(
                f32::from_bits((a & DWORD_MASK) as u32),
                f32::from_bits((b & DWORD_MASK) as u32),
                want_min,
            );
            let f1 = sse_fp_minmax32(
                f32::from_bits(((a >> DWORD_BITS) & DWORD_MASK) as u32),
                f32::from_bits(((b >> DWORD_BITS) & DWORD_MASK) as u32),
                want_min,
            );
            u64::from(f0.to_bits()) | (u64::from(f1.to_bits()) << DWORD_BITS)
        }
        SseFpBinOp::Minpd | SseFpBinOp::Maxpd => sse_fp_minmax64(
            f64::from_bits(a),
            f64::from_bits(b),
            op == SseFpBinOp::Minpd,
        )
        .to_bits(),
        SseFpBinOp::Minss | SseFpBinOp::Maxss => u64::from(
            sse_fp_minmax32(
                f32::from_bits((a & DWORD_MASK) as u32),
                f32::from_bits((b & DWORD_MASK) as u32),
                op == SseFpBinOp::Minss,
            )
            .to_bits(),
        ),
        SseFpBinOp::Minsd | SseFpBinOp::Maxsd => sse_fp_minmax64(
            f64::from_bits(a),
            f64::from_bits(b),
            op == SseFpBinOp::Minsd,
        )
        .to_bits(),
    }
}

fn f32_to_i32_trunc(f: f32) -> i32 {
    if f.is_nan() || !(-2_147_483_648.0..2_147_483_648.0).contains(&f) {
        return i32::MIN;
    }
    f.trunc() as i32
}

fn f32_to_i32_round(f: f32) -> i32 {
    if f.is_nan() || !(-2_147_483_648.0..2_147_483_648.0).contains(&f) {
        return i32::MIN;
    }
    f.round_ties_even() as i32
}

fn f32_to_i64_trunc(f: f32) -> i64 {
    if f.is_nan() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        return i64::MIN;
    }
    f.trunc() as i64
}

fn f32_to_i64_round(f: f32) -> i64 {
    if f.is_nan() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        return i64::MIN;
    }
    f.round_ties_even() as i64
}

fn f64_to_i32_trunc(f: f64) -> i32 {
    if f.is_nan() || !(-2_147_483_648.0..2_147_483_648.0).contains(&f) {
        return i32::MIN;
    }
    f.trunc() as i32
}

fn f64_to_i32_round(f: f64) -> i32 {
    if f.is_nan() || !(-2_147_483_648.0..2_147_483_648.0).contains(&f) {
        return i32::MIN;
    }
    f.round_ties_even() as i32
}

fn f64_to_i64_trunc(f: f64) -> i64 {
    if f.is_nan() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        return i64::MIN;
    }
    f.trunc() as i64
}

fn f64_to_i64_round(f: f64) -> i64 {
    if f.is_nan() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        return i64::MIN;
    }
    f.round_ties_even() as i64
}

/// Convert on one u64 half. `a` is the raw input bits (GPR value, FP bits, or
/// a half holding two 32-bit lanes for the packed forms).
pub(crate) fn sse_cvt(op: SseCvtOp, a: u64) -> u64 {
    match op {
        SseCvtOp::Cvtsi2ss32 => u64::from(((a as u32 as i32) as f32).to_bits()),
        SseCvtOp::Cvtsi2ss64 => u64::from(((a as i64) as f32).to_bits()),
        SseCvtOp::Cvtsi2sd32 => f64::from(a as u32 as i32).to_bits(),
        SseCvtOp::Cvtsi2sd64 => ((a as i64) as f64).to_bits(),
        SseCvtOp::Cvttss2si32 => u64::from(f32_to_i32_trunc(f32::from_bits(a as u32)) as u32),
        SseCvtOp::Cvtss2si32 => u64::from(f32_to_i32_round(f32::from_bits(a as u32)) as u32),
        SseCvtOp::Cvttss2si64 => f32_to_i64_trunc(f32::from_bits(a as u32)) as u64,
        SseCvtOp::Cvtss2si64 => f32_to_i64_round(f32::from_bits(a as u32)) as u64,
        SseCvtOp::Cvttsd2si32 => u64::from(f64_to_i32_trunc(f64::from_bits(a)) as u32),
        SseCvtOp::Cvtsd2si32 => u64::from(f64_to_i32_round(f64::from_bits(a)) as u32),
        SseCvtOp::Cvttsd2si64 => f64_to_i64_trunc(f64::from_bits(a)) as u64,
        SseCvtOp::Cvtsd2si64 => f64_to_i64_round(f64::from_bits(a)) as u64,
        SseCvtOp::Cvtps2dq => {
            let d0 = f32_to_i32_round(f32::from_bits((a & DWORD_MASK) as u32)) as u32;
            let d1 =
                f32_to_i32_round(f32::from_bits(((a >> DWORD_BITS) & DWORD_MASK) as u32)) as u32;
            u64::from(d0) | (u64::from(d1) << DWORD_BITS)
        }
        SseCvtOp::Cvttps2dq => {
            let d0 = f32_to_i32_trunc(f32::from_bits((a & DWORD_MASK) as u32)) as u32;
            let d1 =
                f32_to_i32_trunc(f32::from_bits(((a >> DWORD_BITS) & DWORD_MASK) as u32)) as u32;
            u64::from(d0) | (u64::from(d1) << DWORD_BITS)
        }
        SseCvtOp::Cvtdq2ps => {
            let f0 = (a as u32 as i32) as f32;
            let f1 = ((a >> DWORD_BITS) as u32 as i32) as f32;
            u64::from(f0.to_bits()) | (u64::from(f1.to_bits()) << DWORD_BITS)
        }
        // One f64 lane (the 64-bit input half) → i32 dword, MXCSR round-nearest.
        SseCvtOp::Cvtpd2dq => u64::from(f64_to_i32_round(f64::from_bits(a)) as u32),
        // 1 f64 lane (64-bit half) → i32 (truncate toward zero).
        SseCvtOp::Cvttpd2dq => u64::from(f64_to_i32_trunc(f64::from_bits(a)) as u32),
        // One f64 lane → f32 bits (one single lane of the packed result).
        SseCvtOp::Cvtpd2ps => u64::from((f64::from_bits(a) as f32).to_bits()),
        // Scalar converts: one lane, 32/64-bit result.
        SseCvtOp::Cvtsd2ss => u64::from((f64::from_bits(a) as f32).to_bits()),
        SseCvtOp::Cvtss2sd => f64::from(f32::from_bits(a as u32)).to_bits(),
    }
}

/// Full 128-bit packed integer binop (interpreter path).
fn sse_int_binop_u128(op: SseIntOp, a: u128, b: u128) -> u128 {
    u128::from(sse_int_binop_half(op, a as u64, b as u64))
        | (u128::from(sse_int_binop_half(op, (a >> 64) as u64, (b >> 64) as u64)) << 64)
}

/// Full 128-bit packed shift (interpreter path). For the imm forms the caller
/// splats the count into every lane.
fn sse_shift_u128(op: SseShiftOp, a: u128, count: u128) -> u128 {
    u128::from(sse_shift_half(op, a as u64, count as u64))
        | (u128::from(sse_shift_half(op, (a >> 64) as u64, (count >> 64) as u64)) << 64)
}

fn read_sse_op(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: u32,
    nbytes: usize,
) -> Result<u128, StepExecError> {
    match instr.op_kind(op) {
        OpKind::Register if instr.op_register(op).is_xmm() => {
            let v = regs.read_xmm(instr.op_register(op))?;
            let mask = if nbytes >= XMM_BYTES {
                u128::MAX
            } else {
                xmm_low_mask(nbytes)
            };
            Ok(v & mask)
        }
        OpKind::Memory => {
            let addr = effective_address(regs, instr)?;
            let mut buf = [0_u8; 16];
            let slice = buf
                .get_mut(..nbytes)
                .ok_or_else(|| StepExecError::Cpu(CpuError::Message("sse read size".into())))?;
            if let Err(e) = mem.read(addr, slice) {
                drop(e);
                return Err(StepExecError::InvalidMemory(InvalidMem {
                    access_type: AccessType::Read,
                    address: addr,
                    size: i32::try_from(nbytes).unwrap_or(0),
                    value: 0,
                }));
            }
            let mut v = 0_u128;
            for (i, b) in slice.iter().enumerate() {
                v |= u128::from(*b) << (i.saturating_mul(8));
            }
            Ok(v)
        }
        OpKind::Register => {
            // GPR source for movd/movq-like
            Ok(u128::from(regs.read_reg(instr.op_register(op))?))
        }
        other => Err(StepExecError::Cpu(CpuError::Message(format!(
            "sse read op kind {other:?}"
        )))),
    }
}

fn write_sse_op(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: u32,
    value: u128,
    nbytes: usize,
    scalar_merge: bool,
) -> Result<(), StepExecError> {
    match instr.op_kind(op) {
        OpKind::Register if instr.op_register(op).is_xmm() => {
            let reg = instr.op_register(op);
            let new = if scalar_merge && nbytes < XMM_BYTES {
                let old = regs.read_xmm(reg)?;
                let mask = xmm_low_mask(nbytes);
                (old & !mask) | (value & mask)
            } else if nbytes >= XMM_BYTES {
                value
            } else {
                // Zero upper bits for full vector store of partial (non-merge).
                value & xmm_low_mask(nbytes)
            };
            // The three branches above already produce the exact value to
            // store: merged low bits for scalar forms, zero-masked low bits
            // for partial non-merge stores, and the full value otherwise.
            regs.write_xmm(reg, new)?;
            Ok(())
        }
        OpKind::Memory => {
            let addr = effective_address(regs, instr)?;
            let mut buf = [0_u8; 16];
            for i in 0..nbytes {
                if let Some(b) = buf.get_mut(i) {
                    *b = ((value >> (i.saturating_mul(8))) & 0xff) as u8;
                }
            }
            let slice = buf
                .get(..nbytes)
                .ok_or_else(|| StepExecError::Cpu(CpuError::Message("sse write size".into())))?;
            if let Err(e) = mem.write(addr, slice) {
                drop(e);
                return Err(StepExecError::InvalidMemory(InvalidMem {
                    access_type: AccessType::Write,
                    address: addr,
                    size: i32::try_from(nbytes).unwrap_or(0),
                    value: 0,
                }));
            }
            Ok(())
        }
        other => Err(StepExecError::Cpu(CpuError::Message(format!(
            "sse write op kind {other:?}"
        )))),
    }
}
