//! SSE/SSE2 lowering: mov/pack family, `punpck`/`pshufd` shuffles, packed-integer
//! //! binops/shifts (Neon-accelerated), and the packed-integer host ABI helpers.

use super::super::config::JitConfig;
use super::analysis::{i8x16_to_pair, pair_to_i8x16, read_xmm_pair, store_xmm_pair, xmm_index};
use super::emit::MemEnv;
use super::flags::iconst_u64;
use super::gpr::{effective_addr, read_gpr, write_gpr};
use super::mem::{call_load, call_store};

use crate::exec::{self};
use cranelift::prelude::*;
use cranelift_codegen::ir::MemFlagsData;
use iced_x86::{Instruction, Mnemonic, OpKind};

/// Packed integer SSE2 lane op on one u64 half (SIMD-off path + pack/pmul*).
///
/// Delegates to the shared [`exec::sse_int_binop_half`] core so the interpreter
/// and the JIT compute identical lane math.
pub(crate) extern "C" fn wie_sse_int_binop(op: u64, a: u64, b: u64) -> u64 {
    let Ok(op) = exec::SseIntOp::try_from(op) else {
        return a;
    };
    exec::sse_int_binop_half(op, a, b)
}

/// Packed SSE2 shift on one u64 half (imm splat or per-lane counts).
pub(crate) extern "C" fn wie_sse_shift(op: u64, a: u64, count: u64) -> u64 {
    let Ok(op) = exec::SseShiftOp::try_from(op) else {
        return a;
    };
    exec::sse_shift_half(op, a, count)
}

/// `pshufb` result bytes 0..8 (table = a, mask = b).
pub(crate) extern "C" fn wie_sse_pshufb_lo(a_lo: u64, a_hi: u64, b_lo: u64, b_hi: u64) -> u64 {
    exec::sse_pshufb_lo(a_lo, a_hi, b_lo, b_hi)
}

/// `pshufb` result bytes 8..16.
pub(crate) extern "C" fn wie_sse_pshufb_hi(a_lo: u64, a_hi: u64, b_lo: u64, b_hi: u64) -> u64 {
    exec::sse_pshufb_hi(a_lo, a_hi, b_lo, b_hi)
}

/// Load 4/8/16 bytes from guest mem into (lo, hi) u64 pair (hi=0 for <16).
pub(super) fn load_sse_mem(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    nbytes: u32,
    insn_ip: u64,
) -> Result<(Value, Value), String> {
    match nbytes {
        4 | 8 => {
            let lo = call_load(bcx, mem, gpr, rflags, addr, nbytes, insn_ip)?;
            let hi = iconst_u64(bcx, 0);
            Ok((lo, hi))
        }
        16 => {
            let lo = call_load(bcx, mem, gpr, rflags, addr, 8, insn_ip)?;
            let addr_hi = bcx.ins().iadd_imm(addr, 8);
            let hi = call_load(bcx, mem, gpr, rflags, addr_hi, 8, insn_ip)?;
            Ok((lo, hi))
        }
        _ => Err(format!("sse load width {nbytes}")),
    }
}

pub(super) fn store_sse_mem(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    lo: Value,
    hi: Value,
    nbytes: u32,
    insn_ip: u64,
) -> Result<(), String> {
    match nbytes {
        4 | 8 => call_store(bcx, mem, gpr, rflags, addr, nbytes, lo, insn_ip),
        16 => {
            call_store(bcx, mem, gpr, rflags, addr, 8, lo, insn_ip)?;
            let addr_hi = bcx.ins().iadd_imm(addr, 8);
            call_store(bcx, mem, gpr, rflags, addr_hi, 8, hi, insn_ip)
        }
        _ => Err(format!("sse store width {nbytes}")),
    }
}

/// movaps/movups/movdqa/movdqu/movss/movsd.
pub(super) fn lower_sse_mov(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    nbytes: u32,
    scalar_merge: bool,
) -> Result<(), String> {
    let ip = instr.ip();
    let (src_lo, src_hi) = match instr.op1_kind() {
        OpKind::Register if instr.op_register(1).is_xmm() => {
            read_xmm_pair(xmm, instr.op_register(1))?
        }
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            load_sse_mem(bcx, mem, gpr, rflags, addr, nbytes, ip)?
        }
        _ => return Err("sse mov src".into()),
    };

    match instr.op0_kind() {
        OpKind::Register if instr.op_register(0).is_xmm() => {
            let dst = instr.op_register(0);
            let di = xmm_index(dst)?;
            let (lo, hi) = if scalar_merge && nbytes < 16 {
                let (old_lo, old_hi) = read_xmm_pair(xmm, dst)?;
                match nbytes {
                    4 => {
                        // Keep bits [63:32] of old_lo; replace low 32 from src.
                        let hi32 = iconst_u64(bcx, 0xffff_ffff_0000_0000);
                        let mask = iconst_u64(bcx, 0xffff_ffff);
                        let cleared = bcx.ins().band(old_lo, hi32);
                        let low = bcx.ins().band(src_lo, mask);
                        (bcx.ins().bor(cleared, low), old_hi)
                    }
                    8 => (src_lo, old_hi),
                    _ => (src_lo, src_hi),
                }
            } else if nbytes < 16 {
                // Non-merge partial: zero-extend into xmm (movdqa-style partial not used).
                (src_lo, iconst_u64(bcx, 0))
            } else {
                (src_lo, src_hi)
            };
            store_xmm_pair(bcx, mem, xmm, di, lo, hi);
            Ok(())
        }
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            store_sse_mem(bcx, mem, gpr, rflags, addr, src_lo, src_hi, nbytes, ip)
        }
        _ => Err("sse mov dst".into()),
    }
}

pub(super) fn lower_sse_movq(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
) -> Result<(), String> {
    let ip = instr.ip();
    let r0 = instr.op_register(0);
    let r1 = instr.op_register(1);
    // xmm, xmm/m64
    if r0.is_xmm() {
        let (lo, _) = match instr.op1_kind() {
            OpKind::Register if r1.is_xmm() => read_xmm_pair(xmm, r1)?,
            OpKind::Register => {
                let v = read_gpr(gpr, r1)?;
                (v, iconst_u64(bcx, 0))
            }
            OpKind::Memory => {
                let addr = effective_addr(bcx, instr, gpr)?;
                load_sse_mem(bcx, mem, gpr, rflags, addr, 8, ip)?
            }
            _ => return Err("movq src".into()),
        };
        // movq to xmm: zero-extend, high 64 bits are always zeroed.
        let hi_zero = iconst_u64(bcx, 0);
        store_xmm_pair(bcx, mem, xmm, xmm_index(r0)?, lo, hi_zero);
        return Ok(());
    }
    // r64, xmm / m64 from xmm
    if instr.op1_kind() == OpKind::Register && r1.is_xmm() {
        let (lo, _) = read_xmm_pair(xmm, r1)?;
        if instr.op0_kind() == OpKind::Memory {
            let addr = effective_addr(bcx, instr, gpr)?;
            return call_store(bcx, mem, gpr, rflags, addr, 8, lo, ip);
        }
        return write_gpr(bcx, gpr, dirty, r0, lo);
    }
    // mem, xmm
    if instr.op0_kind() == OpKind::Memory && r1.is_xmm() {
        let (lo, _) = read_xmm_pair(xmm, r1)?;
        let addr = effective_addr(bcx, instr, gpr)?;
        return call_store(bcx, mem, gpr, rflags, addr, 8, lo, ip);
    }
    Err("movq form".into())
}

pub(super) fn lower_sse_movd(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
) -> Result<(), String> {
    let ip = instr.ip();
    let r0 = instr.op_register(0);
    let r1 = instr.op_register(1);
    if r0.is_xmm() {
        let lo = match instr.op1_kind() {
            OpKind::Register => {
                let v = read_gpr(gpr, r1)?;
                let m = iconst_u64(bcx, 0xffff_ffff);
                bcx.ins().band(v, m)
            }
            OpKind::Memory => {
                let addr = effective_addr(bcx, instr, gpr)?;
                call_load(bcx, mem, gpr, rflags, addr, 4, ip)?
            }
            _ => return Err("movd src".into()),
        };
        // Zero-extend into XMM.
        let zero = iconst_u64(bcx, 0);
        store_xmm_pair(bcx, mem, xmm, xmm_index(r0)?, lo, zero);
        return Ok(());
    }
    if r1.is_xmm() {
        let (lo, _) = read_xmm_pair(xmm, r1)?;
        let m = iconst_u64(bcx, 0xffff_ffff);
        let v = bcx.ins().band(lo, m);
        if instr.op0_kind() == OpKind::Memory {
            let addr = effective_addr(bcx, instr, gpr)?;
            return call_store(bcx, mem, gpr, rflags, addr, 4, v, ip);
        }
        return write_gpr(bcx, gpr, dirty, r0, v);
    }
    Err("movd form".into())
}

/// `MOVHPS` — move 64 bits between XMM upper half and memory.
pub(super) fn lower_sse_movhps(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    _dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
) -> Result<(), String> {
    let ip = instr.ip();
    let r0 = instr.op_register(0);
    if r0.is_xmm() {
        // xmm, m64: load 8 bytes from memory into upper 64 bits
        let addr = effective_addr(bcx, instr, gpr)?;
        let loaded = call_load(bcx, mem, gpr, rflags, addr, 8, ip)?;
        let (old_lo, _) = read_xmm_pair(xmm, r0)?;
        store_xmm_pair(bcx, mem, xmm, xmm_index(r0)?, old_lo, loaded);
        return Ok(());
    }
    // m64, xmm: store upper 64 bits of XMM to memory
    let r1 = instr.op_register(1);
    let (_, hi) = read_xmm_pair(xmm, r1)?;
    let addr = effective_addr(bcx, instr, gpr)?;
    call_store(bcx, mem, gpr, rflags, addr, 8, hi, ip)
}

/// `MOVHLPS` / `MOVLHPS` — move packed floats between XMM halves (reg-to-reg only).
pub(super) fn lower_sse_movhlps(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    xmm: &mut [Value; 32],
    mem: &mut MemEnv,
) -> Result<(), String> {
    let r0 = instr.op_register(0);
    let r1 = instr.op_register(1);
    let (lo, hi) = read_xmm_pair(xmm, r0)?;
    let (src_lo, src_hi) = read_xmm_pair(xmm, r1)?;
    let (new_lo, new_hi) = match instr.mnemonic() {
        Mnemonic::Movhlps => (src_hi, hi), // src[127:64] → dst[63:0]
        _ => (lo, src_lo),                 // src[63:0] → dst[127:64] (Movlhps)
    };
    store_xmm_pair(bcx, mem, xmm, xmm_index(r0)?, new_lo, new_hi);
    Ok(())
}

/// `PUNPCKLQDQ` / `PUNPCKHQDQ` — unpack quadwords (SSE2).
pub(super) fn lower_sse_punpck(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    xmm: &mut [Value; 32],
    mem: &mut MemEnv,
) -> Result<(), String> {
    let r0 = instr.op_register(0);
    let r1 = instr.op_register(1);
    let (lo_a, hi_a) = read_xmm_pair(xmm, r0)?;
    let (lo_b, hi_b) = read_xmm_pair(xmm, r1)?;
    let (new_lo, new_hi) = match instr.mnemonic() {
        Mnemonic::Punpcklqdq => (lo_a, lo_b),
        _ => (hi_a, hi_b), // Punpckhqdq
    };
    store_xmm_pair(bcx, mem, xmm, xmm_index(r0)?, new_lo, new_hi);
    Ok(())
}

/// `PUNPCKL/H{bw,wd,dq}` — byte/word/dword unpack.
///
/// NEON path: a single byte-granular `shuffle` with a fixed mask. Helper path
/// (`WIE_JIT_SIMD=0`): the L-family is half-aligned (low sub-lanes of each
/// half); the H-family reuses the L-op over the high halves plus a
/// high-sub-lane variant for the result's high half.
pub(super) fn lower_sse_punpck_lanes(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
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
        _ => return Err("punpck lanes src".into()),
    };
    let (lo, hi) = if JitConfig::get().simd_enabled() {
        let a8 = pair_to_i8x16(bcx, mem.flags, a_lo, a_hi);
        let b8 = pair_to_i8x16(bcx, mem.flags, b_lo, b_hi);
        let mask = sse_punpck_shuffle_mask(instr.mnemonic());
        let imm = shuffle_imm(bcx, mask);
        let r = bcx.ins().shuffle(a8, b8, imm);
        i8x16_to_pair(bcx, mem.flags, r)
    } else {
        // Both result halves come from the same source half (a_lo/b_lo for the
        // L-family, a_hi/b_hi for the H-family); the low half interleaves the
        // low sub-lanes, the high half the high sub-lanes.
        let (l_op, h_op) = match instr.mnemonic() {
            Mnemonic::Punpcklbw | Mnemonic::Punpckhbw => {
                (exec::SseIntOp::Punpcklbw, exec::SseIntOp::PunpckHiBw)
            }
            Mnemonic::Punpcklwd | Mnemonic::Punpckhwd => {
                (exec::SseIntOp::Punpcklwd, exec::SseIntOp::PunpckHiWd)
            }
            Mnemonic::Punpckldq | Mnemonic::Punpckhdq => {
                (exec::SseIntOp::Punpckldq, exec::SseIntOp::PunpckHiDq)
            }
            _ => return Err("punpck lanes op".into()),
        };
        let h_family = matches!(
            instr.mnemonic(),
            Mnemonic::Punpckhbw | Mnemonic::Punpckhwd | Mnemonic::Punpckhdq
        );
        let (x_lo, x_hi) = if h_family { (a_hi, b_hi) } else { (a_lo, b_lo) };
        let sref = mem.sse_int_ref.ok_or("sse int helper missing")?;
        let l_op_v = iconst_u64(bcx, l_op.to_abi());
        let h_op_v = iconst_u64(bcx, h_op.to_abi());
        let call_lo = bcx.ins().call(sref, &[l_op_v, x_lo, x_hi]);
        let lo = bcx.inst_results(call_lo)[0];
        let call_hi = bcx.ins().call(sref, &[h_op_v, x_lo, x_hi]);
        let hi = bcx.inst_results(call_hi)[0];
        (lo, hi)
    };
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// Store a 16-byte shuffle mask as a Cranelift `Immediate` handle.
pub(super) fn shuffle_imm(
    bcx: &mut FunctionBuilder<'_>,
    mask: u128,
) -> cranelift::codegen::ir::Immediate {
    let bytes = mask.to_le_bytes();
    bcx.func
        .dfg
        .immediates
        .push(cranelift::codegen::ir::ConstantData::from(&bytes[..]))
}

/// Byte-shuffle mask for a byte/word/dword unpack.
pub(super) fn sse_punpck_shuffle_mask(m: Mnemonic) -> u128 {
    let mut out = 0_u128;
    let set_byte = |out: &mut u128, idx: usize, val: u32| {
        *out |= u128::from(val) << (idx * 8);
    };
    match m {
        Mnemonic::Punpcklbw => {
            for k in 0..8 {
                set_byte(&mut out, 2 * k, k as u32);
                set_byte(&mut out, 2 * k + 1, 16 + k as u32);
            }
        }
        Mnemonic::Punpckhbw => {
            for k in 0..8 {
                set_byte(&mut out, 2 * k, (8 + k) as u32);
                set_byte(&mut out, 2 * k + 1, (24 + k) as u32);
            }
        }
        Mnemonic::Punpcklwd => {
            for k in 0..4 {
                set_byte(&mut out, 4 * k, 2 * k as u32);
                set_byte(&mut out, 4 * k + 1, 2 * k as u32 + 1);
                set_byte(&mut out, 4 * k + 2, 16 + 2 * k as u32);
                set_byte(&mut out, 4 * k + 3, 17 + 2 * k as u32);
            }
        }
        Mnemonic::Punpckhwd => {
            for k in 0..4 {
                set_byte(&mut out, 4 * k, (8 + 2 * k) as u32);
                set_byte(&mut out, 4 * k + 1, (9 + 2 * k) as u32);
                set_byte(&mut out, 4 * k + 2, (24 + 2 * k) as u32);
                set_byte(&mut out, 4 * k + 3, (25 + 2 * k) as u32);
            }
        }
        Mnemonic::Punpckldq => {
            for k in 0..2 {
                for b in 0..4 {
                    set_byte(&mut out, 8 * k + b, (4 * k + b) as u32);
                    set_byte(&mut out, 8 * k + 4 + b, (16 + 4 * k + b) as u32);
                }
            }
        }
        Mnemonic::Punpckhdq => {
            for k in 0..2 {
                for b in 0..4 {
                    set_byte(&mut out, 8 * k + b, (8 + 4 * k + b) as u32);
                    set_byte(&mut out, 8 * k + 4 + b, (24 + 4 * k + b) as u32);
                }
            }
        }
        _ => {}
    }
    out
}

/// `PSHUFD` — shuffle dword lanes (imm8 control).
pub(super) fn lower_sse_pshufd(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (s_lo, s_hi) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            load_sse_mem(bcx, mem, gpr, rflags, addr, 16, instr.ip())?
        }
        _ => return Err("pshufd src".into()),
    };
    let imm = instr.immediate(2) & 0xff;
    let (lo, hi) = if JitConfig::get().simd_enabled() {
        let a8 = pair_to_i8x16(bcx, mem.flags, s_lo, s_hi);
        let mask = sse_pshufd_mask(imm);
        let imm_h = shuffle_imm(bcx, mask);
        let r = bcx.ins().shuffle(a8, a8, imm_h);
        i8x16_to_pair(bcx, mem.flags, r)
    } else {
        let l0 = u32::try_from(imm & 3).unwrap_or(0);
        let l1 = u32::try_from((imm >> 2) & 3).unwrap_or(0);
        let l2 = u32::try_from((imm >> 4) & 3).unwrap_or(0);
        let l3 = u32::try_from((imm >> 6) & 3).unwrap_or(0);
        let d0 = sse_dword_lane(bcx, s_lo, s_hi, l0);
        let d1 = sse_dword_lane(bcx, s_lo, s_hi, l1);
        let d2 = sse_dword_lane(bcx, s_lo, s_hi, l2);
        let d3 = sse_dword_lane(bcx, s_lo, s_hi, l3);
        let d1s = bcx.ins().ishl_imm(d1, 32);
        let d3s = bcx.ins().ishl_imm(d3, 32);
        let lo = bcx.ins().bor(d0, d1s);
        let hi = bcx.ins().bor(d2, d3s);
        (lo, hi)
    };
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// `PSHUFLW` / `PSHUFHW` — shuffle low/high 16-bit lanes.
pub(super) fn lower_sse_pshuflw_hw(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
) -> Result<(), String> {
    let dst = instr.op_register(0);
    let di = xmm_index(dst)?;
    let (s_lo, s_hi) = match instr.op1_kind() {
        OpKind::Register => read_xmm_pair(xmm, instr.op_register(1))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            load_sse_mem(bcx, mem, gpr, rflags, addr, 16, instr.ip())?
        }
        _ => return Err("pshuflw/hw src".into()),
    };
    let imm = instr.immediate(2) & 0xff;
    let low = instr.mnemonic() == Mnemonic::Pshuflw;
    let (lo, hi) = if JitConfig::get().simd_enabled() {
        let a8 = pair_to_i8x16(bcx, mem.flags, s_lo, s_hi);
        let mask = sse_pshuflw_hw_mask(imm, low);
        let imm_h = shuffle_imm(bcx, mask);
        let r = bcx.ins().shuffle(a8, a8, imm_h);
        i8x16_to_pair(bcx, mem.flags, r)
    } else {
        let i0 = u32::try_from(imm & 3).unwrap_or(0);
        let i1 = u32::try_from((imm >> 2) & 3).unwrap_or(0);
        let i2 = u32::try_from((imm >> 4) & 3).unwrap_or(0);
        let i3 = u32::try_from((imm >> 6) & 3).unwrap_or(0);
        let w = |bcx: &mut FunctionBuilder<'_>, src: Value, lane: u32| -> Value {
            let v = if lane == 0 {
                src
            } else {
                bcx.ins().ushr_imm(src, i64::from(lane) * 16)
            };
            let mask16 = iconst_u64(bcx, 0xffff);
            bcx.ins().band(v, mask16)
        };
        if low {
            // Low 4 words shuffled from src words 0-3 (all in s_lo); high copied.
            let w0 = w(bcx, s_lo, i0);
            let w1 = w(bcx, s_lo, i1);
            let w2 = w(bcx, s_lo, i2);
            let w3 = w(bcx, s_lo, i3);
            let b1 = bcx.ins().ishl_imm(w1, 16);
            let b2 = bcx.ins().ishl_imm(w2, 32);
            let b3 = bcx.ins().ishl_imm(w3, 48);
            let bc_ = bcx.ins().bor(b1, b2);
            let bc_ = bcx.ins().bor(bc_, b3);
            let lo = bcx.ins().bor(w0, bc_);
            (lo, s_hi)
        } else {
            // Low words copied; high 4 words shuffled from src words 4-7 (s_hi).
            let w4 = w(bcx, s_hi, i0);
            let w5 = w(bcx, s_hi, i1);
            let w6 = w(bcx, s_hi, i2);
            let w7 = w(bcx, s_hi, i3);
            let b1 = bcx.ins().ishl_imm(w5, 16);
            let b2 = bcx.ins().ishl_imm(w6, 32);
            let b3 = bcx.ins().ishl_imm(w7, 48);
            let bc_ = bcx.ins().bor(b1, b2);
            let bc_ = bcx.ins().bor(bc_, b3);
            let hi = bcx.ins().bor(w4, bc_);
            (s_lo, hi)
        }
    };
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}

/// dword-lane select from (s_lo, s_hi) for the SIMD-off pshufd path.
pub(super) fn sse_dword_lane(
    bcx: &mut FunctionBuilder<'_>,
    s_lo: Value,
    s_hi: Value,
    lane: u32,
) -> Value {
    let base = if lane < 2 { s_lo } else { s_hi };
    let v = if lane.is_multiple_of(2) {
        base
    } else {
        bcx.ins().ushr_imm(base, 32)
    };
    let mask = iconst_u64(bcx, 0xffff_ffff);
    bcx.ins().band(v, mask)
}

/// Byte-shuffle mask for pshufd (single-source).
pub(super) fn sse_pshufd_mask(imm: u64) -> u128 {
    let mut out = 0_u128;
    for i in 0..4 {
        let src_lane = (imm >> (2 * i)) & 3;
        for b in 0..4 {
            out |= u128::from((src_lane * 4 + b) as u32) << ((i * 4 + b) * 8);
        }
    }
    out
}

/// Byte-shuffle mask for pshuflw (`low`) / pshufhw.
pub(super) fn sse_pshuflw_hw_mask(imm: u64, low: bool) -> u128 {
    let mut out = 0_u128;
    let set_word = |out: &mut u128, dst_idx: usize, src_lane: u32| {
        let base = u64::from(src_lane) * 2;
        for b in 0_u64..2 {
            *out |= u128::from(base + b) << ((dst_idx * 2 + usize::try_from(b).unwrap_or(0)) * 8);
        }
    };
    if low {
        for i in 0..4 {
            let src_lane = u32::try_from((imm >> (2 * i)) & 3).unwrap_or(0);
            set_word(&mut out, i, src_lane);
        }
        // High words copied unchanged.
        for i in 4..8 {
            set_word(&mut out, i, u32::try_from(i).unwrap_or(0));
        }
    } else {
        for i in 0..4 {
            set_word(&mut out, i, u32::try_from(i).unwrap_or(0));
        }
        for i in 0..4 {
            let src_lane = 4 + u32::try_from((imm >> (2 * i)) & 3).unwrap_or(0);
            set_word(&mut out, i + 4, src_lane);
        }
    }
    out
}

/// Map a packed-integer mnemonic to its ABI opcode (arithmetic/compare/pack only).
pub(super) fn sse_int_op(m: Mnemonic) -> Option<exec::SseIntOp> {
    Some(match m {
        Mnemonic::Paddb => exec::SseIntOp::Paddb,
        Mnemonic::Paddw => exec::SseIntOp::Paddw,
        Mnemonic::Paddd => exec::SseIntOp::Paddd,
        Mnemonic::Paddq => exec::SseIntOp::Paddq,
        Mnemonic::Psubb => exec::SseIntOp::Psubb,
        Mnemonic::Psubw => exec::SseIntOp::Psubw,
        Mnemonic::Psubd => exec::SseIntOp::Psubd,
        Mnemonic::Psubq => exec::SseIntOp::Psubq,
        Mnemonic::Paddsb => exec::SseIntOp::Paddsb,
        Mnemonic::Paddsw => exec::SseIntOp::Paddsw,
        Mnemonic::Paddusb => exec::SseIntOp::Paddusb,
        Mnemonic::Paddusw => exec::SseIntOp::Paddusw,
        Mnemonic::Psubsb => exec::SseIntOp::Psubsb,
        Mnemonic::Psubsw => exec::SseIntOp::Psubsw,
        Mnemonic::Psubusb => exec::SseIntOp::Psubusb,
        Mnemonic::Psubusw => exec::SseIntOp::Psubusw,
        Mnemonic::Pmullw => exec::SseIntOp::Pmullw,
        Mnemonic::Pmulhw => exec::SseIntOp::Pmulhw,
        Mnemonic::Pmulhuw => exec::SseIntOp::Pmulhuw,
        Mnemonic::Pmuludq => exec::SseIntOp::Pmuludq,
        Mnemonic::Pmaddwd => exec::SseIntOp::Pmaddwd,
        Mnemonic::Pcmpeqb => exec::SseIntOp::Pcmpeqb,
        Mnemonic::Pcmpeqw => exec::SseIntOp::Pcmpeqw,
        Mnemonic::Pcmpeqd => exec::SseIntOp::Pcmpeqd,
        Mnemonic::Pcmpgtb => exec::SseIntOp::Pcmpgtb,
        Mnemonic::Pcmpgtw => exec::SseIntOp::Pcmpgtw,
        Mnemonic::Pcmpgtd => exec::SseIntOp::Pcmpgtd,
        Mnemonic::Packsswb => exec::SseIntOp::Packsswb,
        Mnemonic::Packssdw => exec::SseIntOp::Packssdw,
        Mnemonic::Packuswb => exec::SseIntOp::Packuswb,
        _ => return None,
    })
}

/// Map a packed-shift mnemonic to its ABI opcode.
pub(super) fn sse_shift_op(m: Mnemonic) -> Option<exec::SseShiftOp> {
    Some(match m {
        Mnemonic::Psllw => exec::SseShiftOp::Psllw,
        Mnemonic::Pslld => exec::SseShiftOp::Pslld,
        Mnemonic::Psllq => exec::SseShiftOp::Psllq,
        Mnemonic::Psrlw => exec::SseShiftOp::Psrlw,
        Mnemonic::Psrld => exec::SseShiftOp::Psrld,
        Mnemonic::Psrlq => exec::SseShiftOp::Psrlq,
        Mnemonic::Psraw => exec::SseShiftOp::Psraw,
        Mnemonic::Psrad => exec::SseShiftOp::Psrad,
        _ => return None,
    })
}

/// Bitcast a lo/hi u64 pair to a 128-bit vector lane type.
pub(super) fn pair_to_vec(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    lo: Value,
    hi: Value,
    ty: Type,
) -> Value {
    let v = pair_to_i8x16(bcx, flags, lo, hi);
    bcx.ins().bitcast(ty, flags, v)
}

/// Bitcast a 128-bit vector back to a lo/hi u64 pair.
pub(super) fn vec_to_pair(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    v: Value,
) -> (Value, Value) {
    let as_i8 = bcx.ins().bitcast(types::I8X16, flags, v);
    i8x16_to_pair(bcx, flags, as_i8)
}

/// Apply a lane-wise vector binary op to a lo/hi pair.
pub(super) fn vec_binop(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    a_lo: Value,
    a_hi: Value,
    b_lo: Value,
    b_hi: Value,
    ty: Type,
    op: fn(&mut FunctionBuilder<'_>, Value, Value) -> Value,
) -> (Value, Value) {
    let a = pair_to_vec(bcx, flags, a_lo, a_hi, ty);
    let b = pair_to_vec(bcx, flags, b_lo, b_hi, ty);
    let r = op(bcx, a, b);
    vec_to_pair(bcx, flags, r)
}

/// Host-helper fallback for a packed integer op (SIMD-off + pack/pmul*).
pub(super) fn sse_int_binop_helper(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    op: exec::SseIntOp,
    a_lo: Value,
    a_hi: Value,
    b_lo: Value,
    b_hi: Value,
) -> Result<(Value, Value), String> {
    let sref = mem.sse_int_ref.ok_or("sse int helper missing")?;
    let op_v = iconst_u64(bcx, op.to_abi());
    let c1 = bcx.ins().call(sref, &[op_v, a_lo, b_lo]);
    let lo = bcx.inst_results(c1)[0];
    let c2 = bcx.ins().call(sref, &[op_v, a_hi, b_hi]);
    let hi = bcx.inst_results(c2)[0];
    Ok((lo, hi))
}

/// Packed integer binary op: dst xmm, src xmm/m128.
pub(super) fn lower_sse_int_binop(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
    xmm: &mut [Value; 32],
    op: exec::SseIntOp,
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
        _ => return Err("sse int binop src".into()),
    };
    let (lo, hi) = if JitConfig::get().simd_enabled() {
        match op {
            exec::SseIntOp::Paddb => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I8X16,
                |b, x, y| b.ins().iadd(x, y),
            ),
            exec::SseIntOp::Paddw => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I16X8,
                |b, x, y| b.ins().iadd(x, y),
            ),
            exec::SseIntOp::Paddd => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I32X4,
                |b, x, y| b.ins().iadd(x, y),
            ),
            exec::SseIntOp::Paddq => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I64X2,
                |b, x, y| b.ins().iadd(x, y),
            ),
            exec::SseIntOp::Psubb => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I8X16,
                |b, x, y| b.ins().isub(x, y),
            ),
            exec::SseIntOp::Psubw => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I16X8,
                |b, x, y| b.ins().isub(x, y),
            ),
            exec::SseIntOp::Psubd => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I32X4,
                |b, x, y| b.ins().isub(x, y),
            ),
            exec::SseIntOp::Psubq => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I64X2,
                |b, x, y| b.ins().isub(x, y),
            ),
            exec::SseIntOp::Paddsb => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I8X16,
                |b, x, y| b.ins().sadd_sat(x, y),
            ),
            exec::SseIntOp::Paddsw => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I16X8,
                |b, x, y| b.ins().sadd_sat(x, y),
            ),
            exec::SseIntOp::Paddusb => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I8X16,
                |b, x, y| b.ins().uadd_sat(x, y),
            ),
            exec::SseIntOp::Paddusw => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I16X8,
                |b, x, y| b.ins().uadd_sat(x, y),
            ),
            exec::SseIntOp::Psubsb => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I8X16,
                |b, x, y| b.ins().ssub_sat(x, y),
            ),
            exec::SseIntOp::Psubsw => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I16X8,
                |b, x, y| b.ins().ssub_sat(x, y),
            ),
            exec::SseIntOp::Psubusb => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I8X16,
                |b, x, y| b.ins().usub_sat(x, y),
            ),
            exec::SseIntOp::Psubusw => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I16X8,
                |b, x, y| b.ins().usub_sat(x, y),
            ),
            exec::SseIntOp::Pmullw => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I16X8,
                |b, x, y| b.ins().imul(x, y),
            ),
            exec::SseIntOp::Pcmpeqb => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I8X16,
                |b, x, y| b.ins().icmp(IntCC::Equal, x, y),
            ),
            exec::SseIntOp::Pcmpeqw => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I16X8,
                |b, x, y| b.ins().icmp(IntCC::Equal, x, y),
            ),
            exec::SseIntOp::Pcmpeqd => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I32X4,
                |b, x, y| b.ins().icmp(IntCC::Equal, x, y),
            ),
            exec::SseIntOp::Pcmpgtb => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I8X16,
                |b, x, y| b.ins().icmp(IntCC::SignedGreaterThan, x, y),
            ),
            exec::SseIntOp::Pcmpgtw => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I16X8,
                |b, x, y| b.ins().icmp(IntCC::SignedGreaterThan, x, y),
            ),
            exec::SseIntOp::Pcmpgtd => vec_binop(
                bcx,
                mem.flags,
                a_lo,
                a_hi,
                b_lo,
                b_hi,
                types::I32X4,
                |b, x, y| b.ins().icmp(IntCC::SignedGreaterThan, x, y),
            ),
            // No NEON lowering: host helper in both modes. (Punpck variants
            // are routed to `lower_sse_punpck_lanes`; kept here for
            // exhaustiveness.)
            exec::SseIntOp::Pmulhw
            | exec::SseIntOp::Pmulhuw
            | exec::SseIntOp::Pmuludq
            | exec::SseIntOp::Pmaddwd
            | exec::SseIntOp::Packsswb
            | exec::SseIntOp::Packssdw
            | exec::SseIntOp::Packuswb
            | exec::SseIntOp::Punpcklbw
            | exec::SseIntOp::Punpcklwd
            | exec::SseIntOp::Punpckldq
            | exec::SseIntOp::PunpckHiBw
            | exec::SseIntOp::PunpckHiWd
            | exec::SseIntOp::PunpckHiDq => {
                sse_int_binop_helper(bcx, mem, op, a_lo, a_hi, b_lo, b_hi)?
            }
        }
    } else {
        sse_int_binop_helper(bcx, mem, op, a_lo, a_hi, b_lo, b_hi)?
    };
    store_xmm_pair(bcx, mem, xmm, di, lo, hi);
    Ok(())
}
