//! SSE floating-point lowering: scalar/packed FP binops, compare, converts,
//! bitwise ops, `pshufb`/shifts via host helpers, and the float ABI (`FloatBinOp`).

use super::super::config::JitConfig;
use super::SseBit;
use super::analysis::{i8x16_to_pair, pair_to_i8x16, read_xmm_pair, store_xmm_pair, xmm_index};
use super::emit::MemEnv;
use super::flags::iconst_u64;
use super::gpr::{bool_to_i64, effective_addr, is_imm_kind, read_gpr, write_gpr};
use super::mem::call_load;
use super::sse::{load_sse_mem, pair_to_vec, vec_to_pair};

use super::super::block::mem_width_bytes;

use crate::exec::{self};
use crate::regs::Rflags;
use cranelift::prelude::*;
use iced_x86::{Instruction, OpKind};

/// SSE floating-point binary operation.
///
/// As with [`StringOpKind`], the numeric form is an ABI detail: the JIT passes
/// it to [`wie_f32_binop`] / [`wie_f64_binop`] through an `extern "C"` `u64`.
/// [`Self::to_abi`] and [`TryFrom<u64>`] are the only places that encoding
/// appears; every other site names the operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FloatBinOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl FloatBinOp {
    pub(super) fn to_abi(self) -> u64 {
        match self {
            Self::Add => 0,
            Self::Sub => 1,
            Self::Mul => 2,
            Self::Div => 3,
        }
    }
}

impl TryFrom<u64> for FloatBinOp {
    type Error = ();

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Add),
            1 => Ok(Self::Sub),
            2 => Ok(Self::Mul),
            3 => Ok(Self::Div),
            _ => Err(()),
        }
    }
}

/// IEEE width an SSE FP instruction operates on.
///
/// Replaces an `is_f64: bool` parameter that sat next to the opcode at call
/// sites, making them read `(.., 3, true)` — two unlabelled literals.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FloatWidth {
    F32,
    F64,
}

/// Scalar f32 binop; args/result in low 32 bits.
pub(crate) extern "C" fn wie_f32_binop(op: u64, a: u64, b: u64) -> u64 {
    let fa = f32::from_bits(a as u32);
    let fb = f32::from_bits(b as u32);
    // Unreachable in practice: the lowering only ever emits `to_abi` values.
    // Returning the left operand preserves the previous defensive behaviour
    // (these helpers have no JitCtx, so they cannot raise a fault).
    let Ok(op) = FloatBinOp::try_from(op) else {
        return a;
    };
    let r = match op {
        FloatBinOp::Add => fa + fb,
        FloatBinOp::Sub => fa - fb,
        FloatBinOp::Mul => fa * fb,
        FloatBinOp::Div => fa / fb,
    };
    u64::from(r.to_bits())
}

/// Scalar f64 binop.
pub(crate) extern "C" fn wie_f64_binop(op: u64, a: u64, b: u64) -> u64 {
    let fa = f64::from_bits(a);
    let fb = f64::from_bits(b);
    let Ok(op) = FloatBinOp::try_from(op) else {
        return a;
    };
    let r = match op {
        FloatBinOp::Add => fa + fb,
        FloatBinOp::Sub => fa - fb,
        FloatBinOp::Mul => fa * fb,
        FloatBinOp::Div => fa / fb,
    };
    r.to_bits()
}

/// FP unary (sqrt family) on one u64 half.
pub(crate) extern "C" fn wie_sse_fp_unop(op: u64, a: u64) -> u64 {
    let Ok(op) = exec::SseFpUnOp::try_from(op) else {
        return a;
    };
    exec::sse_fp_unop(op, a)
}

/// FP min/max on one u64 half.
pub(crate) extern "C" fn wie_sse_fp_binop(op: u64, a: u64, b: u64) -> u64 {
    let Ok(op) = exec::SseFpBinOp::try_from(op) else {
        return a;
    };
    exec::sse_fp_binop(op, a, b)
}

/// Integer↔FP convert on one u64 half.
pub(crate) extern "C" fn wie_sse_cvt(op: u64, a: u64) -> u64 {
    let Ok(op) = exec::SseCvtOp::try_from(op) else {
        return a;
    };
    exec::sse_cvt(op, a)
}

/// Packed shift: `dst xmm, imm8` or `dst xmm, xmm/m128` (variable count).
pub(super) fn lower_sse_shift(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: exec::SseShiftOp,
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (a_lo, a_hi) = read_xmm_pair(xmm, dst)?;
    let (lane_ty, width) = match op {
        exec::SseShiftOp::Psllw | exec::SseShiftOp::Psrlw | exec::SseShiftOp::Psraw => {
            (types::I16X8, 16_u32)
        }
        exec::SseShiftOp::Pslld | exec::SseShiftOp::Psrld | exec::SseShiftOp::Psrad => {
            (types::I32X4, 32_u32)
        }
        exec::SseShiftOp::Psllq | exec::SseShiftOp::Psrlq => (types::I64X2, 64_u32),
    };
    let imm = match instr.op1_kind() {
        k if is_imm_kind(k) => Some(instr.immediate(1) & 0xff),
        _ => None,
    };
    let (count_lo, count_hi) = if imm.is_none() {
        match instr.op1_kind() {
            OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
            OpKind::Memory => {
                let addr = effective_addr(bcx, instr, gpr)?;
                load_sse_mem(bcx, mem, gpr, rflags, addr, 16, instr.ip())?
            }
            _ => return Err("sse shift src".into()),
        }
    } else {
        (iconst_u64(bcx, 0), iconst_u64(bcx, 0))
    };
    let (lo, hi) = if JitConfig::get().simd_enabled() {
        if let Some(c) = imm {
            if c >= u64::from(width) {
                // x86: count >= element width → all zeroes.
                (iconst_u64(bcx, 0), iconst_u64(bcx, 0))
            } else {
                // The AArch64 lowering of vector ishl/ushr/sshr only handles a
                // scalar count (it vec_dup's it), so pass the count as a scalar.
                let a_vec = pair_to_vec(bcx, mem.flags, a_lo, a_hi, lane_ty);
                let c_v = iconst_u64(bcx, c);
                let c_n = bcx.ins().ireduce(types::I32, c_v);
                let r = shift_vec(bcx, op, a_vec, c_n);
                vec_to_pair(bcx, mem.flags, r)
            }
        } else {
            // Per-lane variable counts are not supported by the AArch64 vector
            // shift rules → per-lane host helper in both modes.
            let sref = mem.sse_shift_ref.ok_or("sse shift helper missing")?;
            let op_v = iconst_u64(bcx, op.to_abi());
            let c1 = bcx.ins().call(sref, &[op_v, a_lo, count_lo]);
            let lo = bcx.inst_results(c1)[0];
            let c2 = bcx.ins().call(sref, &[op_v, a_hi, count_hi]);
            let hi = bcx.inst_results(c2)[0];
            (lo, hi)
        }
    } else {
        let sref = mem.sse_shift_ref.ok_or("sse shift helper missing")?;
        let op_v = iconst_u64(bcx, op.to_abi());
        let (c_lo, c_hi) = match imm {
            Some(c) => {
                let splat = splat_count(c, width);
                (iconst_u64(bcx, splat), iconst_u64(bcx, splat))
            }
            None => (count_lo, count_hi),
        };
        let c1 = bcx.ins().call(sref, &[op_v, a_lo, c_lo]);
        let lo = bcx.inst_results(c1)[0];
        let c2 = bcx.ins().call(sref, &[op_v, a_hi, c_hi]);
        let hi = bcx.inst_results(c2)[0];
        (lo, hi)
    };
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// Vector shift by (compile-time or per-lane) count.
pub(super) fn shift_vec(
    bcx: &mut FunctionBuilder<'_>,
    op: exec::SseShiftOp,
    a: Value,
    c: Value,
) -> Value {
    match op {
        exec::SseShiftOp::Psllw | exec::SseShiftOp::Pslld | exec::SseShiftOp::Psllq => {
            bcx.ins().ishl(a, c)
        }
        exec::SseShiftOp::Psrlw | exec::SseShiftOp::Psrld | exec::SseShiftOp::Psrlq => {
            bcx.ins().ushr(a, c)
        }
        exec::SseShiftOp::Psraw | exec::SseShiftOp::Psrad => bcx.ins().sshr(a, c),
    }
}

/// Splat an imm8 shift count into every lane of a u64 half.
pub(super) fn splat_count(imm: u64, width: u32) -> u64 {
    match width {
        16 => {
            let c = imm & 0xffff;
            c * 0x0001_0001_0001_0001
        }
        32 => {
            let c = imm & 0xffff_ffff;
            c * 0x0000_0001_0000_0001
        }
        _ => imm,
    }
}

/// `PSHUFB` — byte-wise table lookup (real semantics; the old no-op silently
/// corrupted any guest that used it).
pub(super) fn lower_sse_pshufb(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    // table = op0 (dst), mask = op1 (src).
    let (a_lo, a_hi) = read_xmm_pair(xmm, dst)?;
    let (b_lo, b_hi) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            load_sse_mem(bcx, mem, gpr, rflags, addr, 16, instr.ip())?
        }
        _ => return Err("pshufb src".into()),
    };
    let lo_ref = mem.sse_pshufb_lo_ref.ok_or("pshufb helper missing")?;
    let hi_ref = mem.sse_pshufb_hi_ref.ok_or("pshufb helper missing")?;
    let c1 = bcx.ins().call(lo_ref, &[a_lo, a_hi, b_lo, b_hi]);
    let lo = bcx.inst_results(c1)[0];
    let c2 = bcx.ins().call(hi_ref, &[a_lo, a_hi, b_lo, b_hi]);
    let hi = bcx.inst_results(c2)[0];
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// Scalar FP unary (sqrtss/sqrtsd): dst low lane = f(op1 low lane), upper preserved.
pub(super) fn lower_sse_fp_unop_scalar(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: exec::SseFpUnOp,
) -> Result<(), String> {
    let is_double = matches!(op, exec::SseFpUnOp::Sqrtsd);
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (old_lo, old_hi) = read_xmm_pair(xmm, dst)?;
    let (src_lo, _) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let w = if is_double { 8 } else { 4 };
            load_sse_mem(bcx, mem, gpr, rflags, addr, w, instr.ip())?
        }
        _ => return Err("fp unop scalar src".into()),
    };
    let uref = mem.sse_fp_unop_ref.ok_or("fp unop helper missing")?;
    let op_v = iconst_u64(bcx, op.to_abi());
    let call = bcx.ins().call(uref, &[op_v, src_lo]);
    let r = bcx.inst_results(call)[0];
    let (lo, hi) = if is_double {
        (r, old_hi)
    } else {
        let mask = iconst_u64(bcx, 0xffff_ffff);
        let hi32 = iconst_u64(bcx, 0xffff_ffff_0000_0000);
        let cleared = bcx.ins().band(old_lo, hi32);
        let low = bcx.ins().band(r, mask);
        (bcx.ins().bor(cleared, low), old_hi)
    };
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// Packed FP unary (sqrtps/sqrtpd): reads op1 only, writes op0.
pub(super) fn lower_sse_fp_unop_packed(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: exec::SseFpUnOp,
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (s_lo, s_hi) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            load_sse_mem(bcx, mem, gpr, rflags, addr, 16, instr.ip())?
        }
        _ => return Err("fp unop packed src".into()),
    };
    let uref = mem.sse_fp_unop_ref.ok_or("fp unop helper missing")?;
    let op_v = iconst_u64(bcx, op.to_abi());
    let c1 = bcx.ins().call(uref, &[op_v, s_lo]);
    let lo = bcx.inst_results(c1)[0];
    let c2 = bcx.ins().call(uref, &[op_v, s_hi]);
    let hi = bcx.inst_results(c2)[0];
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// Scalar FP min/max: dst low lane = min/max(dst, src), upper preserved.
pub(super) fn lower_sse_fp_binop_scalar(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: exec::SseFpBinOp,
) -> Result<(), String> {
    let is_double = matches!(op, exec::SseFpBinOp::Minsd | exec::SseFpBinOp::Maxsd);
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (old_lo, old_hi) = read_xmm_pair(xmm, dst)?;
    let (b_lo, _) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let w = if is_double { 8 } else { 4 };
            load_sse_mem(bcx, mem, gpr, rflags, addr, w, instr.ip())?
        }
        _ => return Err("fp binop scalar src".into()),
    };
    let bref = mem.sse_fp_binop_ref.ok_or("fp binop helper missing")?;
    let op_v = iconst_u64(bcx, op.to_abi());
    let call = bcx.ins().call(bref, &[op_v, old_lo, b_lo]);
    let r = bcx.inst_results(call)[0];
    let (lo, hi) = if is_double {
        (r, old_hi)
    } else {
        let mask = iconst_u64(bcx, 0xffff_ffff);
        let hi32 = iconst_u64(bcx, 0xffff_ffff_0000_0000);
        let cleared = bcx.ins().band(old_lo, hi32);
        let low = bcx.ins().band(r, mask);
        (bcx.ins().bor(cleared, low), old_hi)
    };
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// Packed FP min/max: reads op0 and op1.
pub(super) fn lower_sse_fp_binop_packed(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: exec::SseFpBinOp,
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (a_lo, a_hi) = read_xmm_pair(xmm, dst)?;
    let (b_lo, b_hi) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            load_sse_mem(bcx, mem, gpr, rflags, addr, 16, instr.ip())?
        }
        _ => return Err("fp binop packed src".into()),
    };
    let bref = mem.sse_fp_binop_ref.ok_or("fp binop helper missing")?;
    let op_v = iconst_u64(bcx, op.to_abi());
    let c1 = bcx.ins().call(bref, &[op_v, a_lo, b_lo]);
    let lo = bcx.inst_results(c1)[0];
    let c2 = bcx.ins().call(bref, &[op_v, a_hi, b_hi]);
    let hi = bcx.inst_results(c2)[0];
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// `Comiss/Comisd/Ucomiss/Ucomisd`: compare FP, set ZF/PF/CF, clear OF/AF/SF.
///
/// COMISS and UCOMISS produce identical flag results (they differ only in
/// #IA exception behavior, which we do not model).
pub(super) fn lower_sse_comis(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: &mut Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    is_double: bool,
) -> Result<(), String> {
    let ip = instr.ip();
    let (a_lo, _) = read_xmm_pair(xmm, instr.op_register(0))?;
    let (b_lo, _) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let w = if is_double { 8 } else { 4 };
            load_sse_mem(bcx, mem, gpr, *rflags, addr, w, ip)?
        }
        _ => return Err("comis src".into()),
    };
    let (fa, fb) = if is_double {
        (
            bcx.ins().bitcast(types::F64, mem.flags, a_lo),
            bcx.ins().bitcast(types::F64, mem.flags, b_lo),
        )
    } else {
        let a32 = bcx.ins().ireduce(types::I32, a_lo);
        let b32 = bcx.ins().ireduce(types::I32, b_lo);
        (
            bcx.ins().bitcast(types::F32, mem.flags, a32),
            bcx.ins().bitcast(types::F32, mem.flags, b32),
        )
    };
    let eq = bcx.ins().fcmp(FloatCC::Equal, fa, fb);
    let lt = bcx.ins().fcmp(FloatCC::LessThan, fa, fb);
    let un = bcx.ins().fcmp(FloatCC::Unordered, fa, fb);
    let cf_c = bcx.ins().bor(lt, un);
    let zf_c = bcx.ins().bor(eq, un);
    let cf1 = bool_to_i64(bcx, cf_c);
    let pf1 = bool_to_i64(bcx, un);
    let zf1 = bool_to_i64(bcx, zf_c);
    let pf_b = bcx.ins().ishl_imm(pf1, 2);
    let zf_b = bcx.ins().ishl_imm(zf1, 6);
    let pz = bcx.ins().bor(pf_b, zf_b);
    let bits = bcx.ins().bor(cf1, pz);
    let clear = iconst_u64(
        bcx,
        u64::from(Rflags::CF | Rflags::PF | Rflags::ZF | Rflags::OF | Rflags::AF | Rflags::SF),
    );
    let not_clear = bcx.ins().bnot(clear);
    let base = bcx.ins().band(*rflags, not_clear);
    *rflags = bcx.ins().bor(base, bits);
    Ok(())
}

/// `Cvtsi2ss/Cvtsi2sd`: signed int → scalar FP, merged into dst low lane.
pub(super) fn lower_sse_cvt_gpr_to_fp(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    is_double: bool,
) -> Result<(), String> {
    let _ = dirty;
    let ip = instr.ip();
    let is64 = match instr.op1_kind() {
        OpKind::Register => instr.op_register(1).size() == 8,
        OpKind::Memory => mem_width_bytes(instr)? == 8,
        _ => return Err("cvt gpr src".into()),
    };
    let src = match instr.op1_kind() {
        OpKind::Register => read_gpr(gpr, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let w = mem_width_bytes(instr)?;
            call_load(bcx, mem, gpr, rflags, addr, w, ip)?
        }
        _ => return Err("cvt gpr src".into()),
    };
    let op = match (is_double, is64) {
        (false, false) => exec::SseCvtOp::Cvtsi2ss32,
        (false, true) => exec::SseCvtOp::Cvtsi2ss64,
        (true, false) => exec::SseCvtOp::Cvtsi2sd32,
        (true, true) => exec::SseCvtOp::Cvtsi2sd64,
    };
    let cref = mem.sse_cvt_ref.ok_or("cvt helper missing")?;
    let op_v = iconst_u64(bcx, op.to_abi());
    let call = bcx.ins().call(cref, &[op_v, src]);
    let bits = bcx.inst_results(call)[0];
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (old_lo, old_hi) = read_xmm_pair(xmm, dst)?;
    let (lo, hi) = if is_double {
        (bits, old_hi)
    } else {
        let mask = iconst_u64(bcx, 0xffff_ffff);
        let hi32 = iconst_u64(bcx, 0xffff_ffff_0000_0000);
        let cleared = bcx.ins().band(old_lo, hi32);
        let low = bcx.ins().band(bits, mask);
        (bcx.ins().bor(cleared, low), old_hi)
    };
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// `Cvttss2si/Cvtss2si/Cvttsd2si/Cvtsd2si`: scalar FP → signed integer.
pub(super) fn lower_sse_cvt_fp_to_gpr(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    is_double: bool,
    trunc: bool,
) -> Result<(), String> {
    let ip = instr.ip();
    let is64 = instr.op_register(0).size() == 8;
    let (lo, _) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let w = if is_double { 8 } else { 4 };
            load_sse_mem(bcx, mem, gpr, rflags, addr, w, ip)?
        }
        _ => return Err("cvt fp src".into()),
    };
    let op = match (is_double, is64, trunc) {
        (false, false, true) => exec::SseCvtOp::Cvttss2si32,
        (false, false, false) => exec::SseCvtOp::Cvtss2si32,
        (false, true, true) => exec::SseCvtOp::Cvttss2si64,
        (false, true, false) => exec::SseCvtOp::Cvtss2si64,
        (true, false, true) => exec::SseCvtOp::Cvttsd2si32,
        (true, false, false) => exec::SseCvtOp::Cvtsd2si32,
        (true, true, true) => exec::SseCvtOp::Cvttsd2si64,
        (true, true, false) => exec::SseCvtOp::Cvtsd2si64,
    };
    let cref = mem.sse_cvt_ref.ok_or("cvt helper missing")?;
    let op_v = iconst_u64(bcx, op.to_abi());
    let call = bcx.ins().call(cref, &[op_v, lo]);
    let v = bcx.inst_results(call)[0];
    write_gpr(bcx, gpr, dirty, instr.op_register(0), v)
}

/// `Cvtps2dq/Cvtdq2ps/Cvttps2dq`: packed FP ↔ int, reads op1 only.
pub(super) fn lower_sse_cvt_packed(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: exec::SseCvtOp,
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (s_lo, s_hi) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            load_sse_mem(bcx, mem, gpr, rflags, addr, 16, instr.ip())?
        }
        _ => return Err("cvt packed src".into()),
    };
    let cref = mem.sse_cvt_ref.ok_or("cvt helper missing")?;
    let op_v = iconst_u64(bcx, op.to_abi());
    let c1 = bcx.ins().call(cref, &[op_v, s_lo]);
    let lo = bcx.inst_results(c1)[0];
    let c2 = bcx.ins().call(cref, &[op_v, s_hi]);
    let hi = bcx.inst_results(c2)[0];
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

pub(super) fn lower_sse_bitwise(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: SseBit,
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (a_lo, a_hi) = read_xmm_pair(xmm, dst)?;
    let (b_lo, b_hi) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            load_sse_mem(bcx, mem, gpr, rflags, addr, 16, instr.ip())?
        }
        _ => return Err("sse bitwise src".into()),
    };
    let (lo, hi) = if JitConfig::get().simd_enabled() {
        let a = pair_to_i8x16(bcx, mem.flags, a_lo, a_hi);
        let b = pair_to_i8x16(bcx, mem.flags, b_lo, b_hi);
        let c = match op {
            SseBit::Xor => bcx.ins().bxor(a, b),
            SseBit::And => bcx.ins().band(a, b),
            SseBit::Or => bcx.ins().bor(a, b),
            // andn: ~a & b  (Intel: dest = NOT(dest) AND src)
            SseBit::Andn => {
                let na = bcx.ins().bnot(a);
                bcx.ins().band(na, b)
            }
        };
        i8x16_to_pair(bcx, mem.flags, c)
    } else {
        match op {
            SseBit::Xor => (bcx.ins().bxor(a_lo, b_lo), bcx.ins().bxor(a_hi, b_hi)),
            SseBit::And => (bcx.ins().band(a_lo, b_lo), bcx.ins().band(a_hi, b_hi)),
            SseBit::Or => (bcx.ins().bor(a_lo, b_lo), bcx.ins().bor(a_hi, b_hi)),
            SseBit::Andn => {
                let na_lo = bcx.ins().bnot(a_lo);
                let na_hi = bcx.ins().bnot(a_hi);
                (bcx.ins().band(na_lo, b_lo), bcx.ins().band(na_hi, b_hi))
            }
        }
    };
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// Emit the native Cranelift FP instruction for `op`.
///
/// Exhaustive: previously any unrecognised opcode fell through to `fadd`,
/// so a mis-encoded operation silently computed an addition.
pub(super) fn clif_fbinop(
    bcx: &mut FunctionBuilder<'_>,
    op: FloatBinOp,
    a: Value,
    b: Value,
) -> Value {
    match op {
        FloatBinOp::Add => bcx.ins().fadd(a, b),
        FloatBinOp::Sub => bcx.ins().fsub(a, b),
        FloatBinOp::Mul => bcx.ins().fmul(a, b),
        FloatBinOp::Div => bcx.ins().fdiv(a, b),
    }
}

/// Scalar SSE FP: ss (f32 merge) or sd (f64 merge). `op`: 0=add 1=sub 2=mul 3=div.
pub(super) fn lower_sse_scalar_fp(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: FloatBinOp,
    width: FloatWidth,
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (a_lo, a_hi) = read_xmm_pair(xmm, dst)?;
    let (b_lo, b_hi) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let nbytes = if width == FloatWidth::F64 { 8 } else { 4 };
            load_sse_mem(bcx, mem, gpr, rflags, addr, nbytes, instr.ip())?
        }
        _ => return Err("sse scalar fp src".into()),
    };
    let _ = b_hi;
    let (new_lo, new_hi) = if JitConfig::get().simd_enabled() {
        if width == FloatWidth::F64 {
            let fa = bcx.ins().bitcast(types::F64, mem.flags, a_lo);
            let fb = bcx.ins().bitcast(types::F64, mem.flags, b_lo);
            let fr = clif_fbinop(bcx, op, fa, fb);
            let r = bcx.ins().bitcast(types::I64, mem.flags, fr);
            (r, a_hi)
        } else {
            // Operate on low f32; merge bits [63:32] of old_lo.
            let a32 = bcx.ins().ireduce(types::I32, a_lo);
            let b32 = bcx.ins().ireduce(types::I32, b_lo);
            let fa = bcx.ins().bitcast(types::F32, mem.flags, a32);
            let fb = bcx.ins().bitcast(types::F32, mem.flags, b32);
            let fr = clif_fbinop(bcx, op, fa, fb);
            let r32 = bcx.ins().bitcast(types::I32, mem.flags, fr);
            let r64 = bcx.ins().uextend(types::I64, r32);
            let hi32 = iconst_u64(bcx, 0xffff_ffff_0000_0000);
            let cleared = bcx.ins().band(a_lo, hi32);
            (bcx.ins().bor(cleared, r64), a_hi)
        }
    } else {
        let op_v = iconst_u64(bcx, op.to_abi());
        if width == FloatWidth::F64 {
            let fref = mem.f64_ref.ok_or("f64 helper missing")?;
            let call = bcx.ins().call(fref, &[op_v, a_lo, b_lo]);
            let r = bcx.inst_results(call)[0];
            (r, a_hi)
        } else {
            let fref = mem.f32_ref.ok_or("f32 helper missing")?;
            let call = bcx.ins().call(fref, &[op_v, a_lo, b_lo]);
            let r = bcx.inst_results(call)[0];
            let mask = iconst_u64(bcx, 0xffff_ffff);
            let hi32 = iconst_u64(bcx, 0xffff_ffff_0000_0000);
            let cleared = bcx.ins().band(a_lo, hi32);
            let low = bcx.ins().band(r, mask);
            (bcx.ins().bor(cleared, low), a_hi)
        }
    };
    store_xmm_pair(bcx, mem, xmm, di, new_lo, new_hi);
    Ok(())
}

/// Packed SSE FP: ps (4×f32) or pd (2×f64).
pub(super) fn lower_sse_packed_fp(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: FloatBinOp,
    width: FloatWidth,
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (a_lo, a_hi) = read_xmm_pair(xmm, dst)?;
    let (b_lo, b_hi) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            load_sse_mem(bcx, mem, gpr, rflags, addr, 16, instr.ip())?
        }
        _ => return Err("sse packed fp src".into()),
    };
    if JitConfig::get().simd_enabled() {
        let a8 = pair_to_i8x16(bcx, mem.flags, a_lo, a_hi);
        let b8 = pair_to_i8x16(bcx, mem.flags, b_lo, b_hi);
        let (lo, hi) = if width == FloatWidth::F64 {
            let a = bcx.ins().bitcast(types::F64X2, mem.flags, a8);
            let b = bcx.ins().bitcast(types::F64X2, mem.flags, b8);
            let c = clif_fbinop(bcx, op, a, b);
            let c8 = bcx.ins().bitcast(types::I8X16, mem.flags, c);
            i8x16_to_pair(bcx, mem.flags, c8)
        } else {
            let a = bcx.ins().bitcast(types::F32X4, mem.flags, a8);
            let b = bcx.ins().bitcast(types::F32X4, mem.flags, b8);
            let c = clif_fbinop(bcx, op, a, b);
            let c8 = bcx.ins().bitcast(types::I8X16, mem.flags, c);
            i8x16_to_pair(bcx, mem.flags, c8)
        };
        store_xmm_pair(bcx, mem, xmm, di, lo, hi);
        return Ok(());
    }
    let op_v = iconst_u64(bcx, op.to_abi());
    if width == FloatWidth::F64 {
        let fref = mem.f64_ref.ok_or("f64 helper missing")?;
        let call0 = bcx.ins().call(fref, &[op_v, a_lo, b_lo]);
        let r0 = bcx.inst_results(call0)[0];
        let call1 = bcx.ins().call(fref, &[op_v, a_hi, b_hi]);
        let r1 = bcx.inst_results(call1)[0];
        store_xmm_pair(bcx, mem, xmm, di, r0, r1);
    } else {
        let fref = mem.f32_ref.ok_or("f32 helper missing")?;
        let mask = iconst_u64(bcx, 0xffff_ffff);
        let sh = iconst_u64(bcx, 32);
        let mut pack_pair = |half_a: Value, half_b: Value| -> Result<Value, String> {
            let a0 = bcx.ins().band(half_a, mask);
            let b0 = bcx.ins().band(half_b, mask);
            let a1 = bcx.ins().ushr(half_a, sh);
            let b1 = bcx.ins().ushr(half_b, sh);
            let call0 = bcx.ins().call(fref, &[op_v, a0, b0]);
            let r0_raw = bcx.inst_results(call0)[0];
            let r0 = bcx.ins().band(r0_raw, mask);
            let call1 = bcx.ins().call(fref, &[op_v, a1, b1]);
            let r1_raw = bcx.inst_results(call1)[0];
            let r1 = bcx.ins().band(r1_raw, mask);
            let r1s = bcx.ins().ishl(r1, sh);
            Ok(bcx.ins().bor(r0, r1s))
        };
        let lo = pack_pair(a_lo, b_lo)?;
        let hi = pack_pair(a_hi, b_hi)?;
        store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    }
    Ok(())
}

#[cfg(test)]
mod float_abi_tests {
    use super::FloatBinOp;

    const ALL: [FloatBinOp; 4] = [
        FloatBinOp::Add,
        FloatBinOp::Sub,
        FloatBinOp::Mul,
        FloatBinOp::Div,
    ];

    /// The lowering encodes the op into an `extern "C"` u64 that
    /// `wie_f32_binop` / `wie_f64_binop` decode. A mismatch would silently
    /// compute a different arithmetic operation.
    #[test]
    fn float_binop_abi_roundtrips() {
        for op in ALL {
            assert_eq!(FloatBinOp::try_from(op.to_abi()), Ok(op), "{op:?}");
        }
    }

    #[test]
    fn float_binop_abi_values_are_stable() {
        assert_eq!(FloatBinOp::Add.to_abi(), 0);
        assert_eq!(FloatBinOp::Sub.to_abi(), 1);
        assert_eq!(FloatBinOp::Mul.to_abi(), 2);
        assert_eq!(FloatBinOp::Div.to_abi(), 3);
    }

    #[test]
    fn float_binop_rejects_out_of_range() {
        for raw in [4_u64, 5, u64::MAX] {
            assert_eq!(FloatBinOp::try_from(raw), Err(()), "raw={raw}");
        }
    }

    /// Guards the helper decode end-to-end: each opcode must produce the
    /// arithmetic it names, not the `add` the old `_ =>` arm fell back to.
    #[test]
    fn float_helpers_compute_the_named_op() {
        let (a32, b32) = (8.0_f32, 2.0_f32);
        let enc = |x: f32| u64::from(x.to_bits());
        let dec32 = |x: u64| f32::from_bits(u32::try_from(x & 0xffff_ffff).unwrap_or(0));
        for (op, want) in [
            (FloatBinOp::Add, 10.0_f32),
            (FloatBinOp::Sub, 6.0),
            (FloatBinOp::Mul, 16.0),
            (FloatBinOp::Div, 4.0),
        ] {
            let got = dec32(super::wie_f32_binop(op.to_abi(), enc(a32), enc(b32)));
            assert!((got - want).abs() < f32::EPSILON, "{op:?}: {got} != {want}");
        }

        let (a64, b64) = (8.0_f64, 2.0_f64);
        for (op, want) in [
            (FloatBinOp::Add, 10.0_f64),
            (FloatBinOp::Sub, 6.0),
            (FloatBinOp::Mul, 16.0),
            (FloatBinOp::Div, 4.0),
        ] {
            let got = f64::from_bits(super::wie_f64_binop(
                op.to_abi(),
                a64.to_bits(),
                b64.to_bits(),
            ));
            assert!((got - want).abs() < f64::EPSILON, "{op:?}: {got} != {want}");
        }
    }
}
