//! Lazy flag computation: ZS/PF/logic/add/sub/inc-dec/not/neg and flag-bit
//! //! select/replace helpers.

#![allow(
    clippy::cast_possible_wrap, // mem width / offset → i32 for Cranelift
    clippy::many_single_char_names, // flag temps d/s/r in flags_* helpers
    clippy::too_many_arguments
)]

use super::emit::MemEnv;
use super::gpr::{op_width_bits, read_op_mem, write_op_mem};
use super::insn::PendingFlags;

use crate::regs::Rflags;
use cranelift::prelude::*;
use iced_x86::Instruction;

pub(super) fn lower_inc_dec_lazy(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    pending: &mut PendingFlags,
    mem: &mut MemEnv,
    inc: bool,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 0)?;
    let a_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let a = mask_width(bcx, a_raw, bits);
    let one = bcx.ins().iconst(types::I64, 1);
    let res_raw = if inc {
        bcx.ins().iadd(a, one)
    } else {
        bcx.ins().isub(a, one)
    };
    let res = mask_width(bcx, res_raw, bits);
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, res, bits)?;
    *pending = if inc {
        PendingFlags::Inc { a, res, bits }
    } else {
        PendingFlags::Dec { a, res, bits }
    };
    Ok(())
}

pub(super) fn lower_not(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 0)?;
    let a_raw = read_op_mem(bcx, instr, 0, gpr, rflags, mem)?;
    let a = mask_width(bcx, a_raw, bits);
    let not_a = bcx.ins().bnot(a);
    let res = mask_width(bcx, not_a, bits);
    write_op_mem(bcx, instr, 0, gpr, dirty, rflags, mem, res, bits)
}

pub(super) fn lower_neg_lazy(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    pending: &mut PendingFlags,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 0)?;
    let a_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let a = mask_width(bcx, a_raw, bits);
    let zero = bcx.ins().iconst(types::I64, 0);
    let sub = bcx.ins().isub(zero, a);
    let res = mask_width(bcx, sub, bits);
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, res, bits)?;
    // NEG is 0 - a (SUB flags).
    *pending = PendingFlags::Sub {
        a: zero,
        b: a,
        res,
        bits,
    };
    Ok(())
}

// --- RFLAGS (match `regs::set_*_flags`) ---

pub(super) fn iconst_u64(bcx: &mut FunctionBuilder<'_>, v: u64) -> Value {
    bcx.ins()
        .iconst(types::I64, i64::from_ne_bytes(v.to_ne_bytes()))
}

pub(super) fn mask_width(bcx: &mut FunctionBuilder<'_>, v: Value, bits: u32) -> Value {
    if bits >= 64 {
        return v;
    }
    let m = if bits == 32 {
        0xffff_ffff_u64
    } else if bits == 16 {
        0xffff
    } else {
        0xff
    };
    let mv = iconst_u64(bcx, m);
    bcx.ins().band(v, mv)
}

pub(super) fn sign_bit(bits: u32) -> u64 {
    1_u64 << bits.saturating_sub(1).min(63)
}

pub(super) fn flag_bit(bcx: &mut FunctionBuilder<'_>, flags: Value, bit: Rflags) -> Value {
    let m = iconst_u64(bcx, u64::from(bit));
    bcx.ins().band(flags, m)
}

pub(super) fn replace_flag(
    bcx: &mut FunctionBuilder<'_>,
    flags: Value,
    bit: Rflags,
    on: Value,
) -> Value {
    let clear = iconst_u64(bcx, !u64::from(bit));
    let base = bcx.ins().band(flags, clear);
    bcx.ins().bor(base, on)
}

pub(super) fn select_flag(bcx: &mut FunctionBuilder<'_>, cond: Value, bit: Rflags) -> Value {
    let bit_v = iconst_u64(bcx, u64::from(bit));
    let zero = iconst_u64(bcx, 0);
    bcx.ins().select(cond, bit_v, zero)
}

pub(super) fn pf_flag(bcx: &mut FunctionBuilder<'_>, result: Value) -> Value {
    let mut x = mask_width(bcx, result, 8);
    let s4 = bcx.ins().ushr_imm(x, 4);
    x = bcx.ins().bxor(x, s4);
    let s2 = bcx.ins().ushr_imm(x, 2);
    x = bcx.ins().bxor(x, s2);
    let s1 = bcx.ins().ushr_imm(x, 1);
    x = bcx.ins().bxor(x, s1);
    let one = iconst_u64(bcx, 1);
    let odd = bcx.ins().band(x, one);
    let is_even = bcx.ins().icmp_imm(IntCC::Equal, odd, 0);
    select_flag(bcx, is_even, Rflags::PF)
}

pub(super) fn clear_flags(bcx: &mut FunctionBuilder<'_>, old: Value, bits: Rflags) -> Value {
    let m = iconst_u64(bcx, !u64::from(bits));
    bcx.ins().band(old, m)
}

pub(super) fn flags_zs_pf(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    result: Value,
    bits: u32,
) -> Value {
    let r = mask_width(bcx, result, bits);
    let f = clear_flags(bcx, old, Rflags::ZF | Rflags::SF | Rflags::PF);
    let is_z = bcx.ins().icmp_imm(IntCC::Equal, r, 0);
    let zf = select_flag(bcx, is_z, Rflags::ZF);
    let sb = iconst_u64(bcx, sign_bit(bits));
    let sign = bcx.ins().band(r, sb);
    let is_s = bcx.ins().icmp_imm(IntCC::NotEqual, sign, 0);
    let sf = select_flag(bcx, is_s, Rflags::SF);
    let pf = pf_flag(bcx, r);
    let f = bcx.ins().bor(f, zf);
    let f = bcx.ins().bor(f, sf);
    bcx.ins().bor(f, pf)
}

pub(super) fn flags_logic(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    result: Value,
    bits: u32,
) -> Value {
    let f = clear_flags(
        bcx,
        old,
        Rflags::ZF | Rflags::SF | Rflags::PF | Rflags::CF | Rflags::OF,
    );
    flags_zs_pf(bcx, f, result, bits)
}

pub(super) fn flags_add(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    dst: Value,
    src: Value,
    result: Value,
    bits: u32,
) -> Value {
    let d = mask_width(bcx, dst, bits);
    let s = mask_width(bcx, src, bits);
    let r = mask_width(bcx, result, bits);
    let f = clear_flags(
        bcx,
        old,
        Rflags::CF | Rflags::ZF | Rflags::SF | Rflags::PF | Rflags::OF | Rflags::AF,
    );
    let cf_cond = bcx.ins().icmp(IntCC::UnsignedLessThan, r, d);
    let cf = select_flag(bcx, cf_cond, Rflags::CF);
    let f = bcx.ins().bor(f, cf);
    let f = flags_zs_pf(bcx, f, r, bits);
    let sb = iconst_u64(bcx, sign_bit(bits));
    let dr = bcx.ins().bxor(d, r);
    let sr = bcx.ins().bxor(s, r);
    let both = bcx.ins().band(dr, sr);
    let of_bits = bcx.ins().band(both, sb);
    let of_cond = bcx.ins().icmp_imm(IntCC::NotEqual, of_bits, 0);
    let of = select_flag(bcx, of_cond, Rflags::OF);
    let f = bcx.ins().bor(f, of);
    let x = bcx.ins().bxor(d, s);
    let y = bcx.ins().bxor(x, r);
    let ten = iconst_u64(bcx, 0x10);
    let af_b = bcx.ins().band(y, ten);
    let af_cond = bcx.ins().icmp_imm(IntCC::NotEqual, af_b, 0);
    let af = select_flag(bcx, af_cond, Rflags::AF);
    bcx.ins().bor(f, af)
}

pub(super) fn flags_sub(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    dst: Value,
    src: Value,
    result: Value,
    bits: u32,
) -> Value {
    let d = mask_width(bcx, dst, bits);
    let s = mask_width(bcx, src, bits);
    let r = mask_width(bcx, result, bits);
    let f = clear_flags(
        bcx,
        old,
        Rflags::CF | Rflags::ZF | Rflags::SF | Rflags::PF | Rflags::OF | Rflags::AF,
    );
    let cf_cond = bcx.ins().icmp(IntCC::UnsignedLessThan, d, s);
    let cf = select_flag(bcx, cf_cond, Rflags::CF);
    let f = bcx.ins().bor(f, cf);
    let f = flags_zs_pf(bcx, f, r, bits);
    let sb = iconst_u64(bcx, sign_bit(bits));
    let ds = bcx.ins().bxor(d, s);
    let dr = bcx.ins().bxor(d, r);
    let both = bcx.ins().band(ds, dr);
    let of_bits = bcx.ins().band(both, sb);
    let of_cond = bcx.ins().icmp_imm(IntCC::NotEqual, of_bits, 0);
    let of = select_flag(bcx, of_cond, Rflags::OF);
    let f = bcx.ins().bor(f, of);
    let x = bcx.ins().bxor(d, s);
    let y = bcx.ins().bxor(x, r);
    let ten = iconst_u64(bcx, 0x10);
    let af_b = bcx.ins().band(y, ten);
    let af_cond = bcx.ins().icmp_imm(IntCC::NotEqual, af_b, 0);
    let af = select_flag(bcx, af_cond, Rflags::AF);
    bcx.ins().bor(f, af)
}
