//! GPR moves, arithmetic, stack, and bit-string ops for the iced interpreter.

#![allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss, // cvtsi2ss/sd: Intel-defined rounding, not a bug
    clippy::cast_sign_loss,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::integer_division,
    clippy::many_single_char_names, // lane helpers (a/b/x/y/mask)
    clippy::float_cmp, // COMISS equality: IEEE == is the architectural result
    clippy::manual_range_contains // f >= hi || f < lo reads clearer than !range
)]

use crate::CpuError;
use crate::mem::GuestMemory;
use crate::regs::{self, RegFile, rflags};
use iced_x86::{Instruction, OpKind, Register};

use super::{
    ACCESS_READ, ArithOp, BitOp, InvalidMem, StepExecError, effective_address, op_size_bytes,
    pop_n, push_n, read_op, write_mem_value, write_op, write_op_sized,
};

pub(super) fn exec_mov(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let src = read_op(mem, regs, instr, 1)?;
    write_op(mem, regs, instr, 0, src)?;
    Ok(())
}

pub(super) fn exec_movzx(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    sign: bool,
) -> Result<(), StepExecError> {
    let src_size = op_size_bytes(instr, 1)?;
    let dst_size = op_size_bytes(instr, 0)?;
    let raw = read_op(mem, regs, instr, 1)?;
    let src_mask = regs::size_mask(src_size);
    let narrow = raw & src_mask;
    let extended = if sign {
        let bits = src_size.saturating_mul(8);
        let shift = 64_u32.saturating_sub(u32::try_from(bits).unwrap_or(64));
        ((narrow as i64) << shift >> shift) as u64
    } else {
        narrow
    };
    // Write with dst size semantics (32-bit zero-extends).
    write_op_sized(mem, regs, instr, 0, extended, dst_size)?;
    Ok(())
}

pub(super) fn exec_lea(regs: &mut RegFile, instr: &Instruction) -> Result<(), StepExecError> {
    let addr = effective_address(regs, instr)?;
    let dst = instr.op_register(0);
    // LEA writes full register size of dest.
    regs.write_reg(dst, addr)?;
    Ok(())
}

pub(super) fn exec_push(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let val = read_op(mem, regs, instr, 0)?;
    // In 64-bit mode push is always 64-bit (except rare 16-bit override).
    let size = match instr.op0_kind() {
        OpKind::Register if instr.op_register(0).size() == 2 => 2_usize,
        _ => 8_usize,
    };
    push_n(mem, regs, val, size)
}

pub(super) fn exec_pop(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = match instr.op0_kind() {
        OpKind::Register if instr.op_register(0).size() == 2 => 2_usize,
        _ => 8_usize,
    };
    let val = pop_n(mem, regs, size)?;
    write_op(mem, regs, instr, 0, val)
}

pub(super) fn exec_arith(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: ArithOp,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let dst = read_op(mem, regs, instr, 0)?;
    let src = read_op(mem, regs, instr, 1)?;
    let mask = regs::size_mask(size);
    let d = dst & mask;
    let s = src & mask;
    let cf = u64::from(regs.flag(rflags::CF));
    let result = match op {
        ArithOp::Add => d.wrapping_add(s),
        ArithOp::Adc => d.wrapping_add(s).wrapping_add(cf),
        ArithOp::Sub | ArithOp::Cmp => d.wrapping_sub(s),
        ArithOp::Sbb => d.wrapping_sub(s).wrapping_sub(cf),
        ArithOp::Xor => d ^ s,
        ArithOp::Or => d | s,
        ArithOp::And => d & s,
    };
    match op {
        ArithOp::Add => {
            regs::set_add_flags(regs, d, s, result, size);
            write_op(mem, regs, instr, 0, result & mask)?;
        }
        ArithOp::Adc => {
            // Flags from full add with carry-in.
            let wide = u128::from(d)
                .wrapping_add(u128::from(s))
                .wrapping_add(u128::from(cf));
            regs::set_add_flags(regs, d, s.wrapping_add(cf), result, size);
            regs.set_flag(rflags::CF, wide > u128::from(mask));
            write_op(mem, regs, instr, 0, result & mask)?;
        }
        ArithOp::Sub => {
            regs::set_sub_flags(regs, d, s, result, size);
            write_op(mem, regs, instr, 0, result & mask)?;
        }
        ArithOp::Sbb => {
            // CF/OF must use full-width borrow: when `s + CF` overflows the
            // operand, masking `borrow` to `size` zeros it and wrongly clears CF
            // (breaks MSVC `cmp; sbb r,r; sbb r,-1` equality idioms used in 7-Zip QI).
            let wide_src = u128::from(s).wrapping_add(u128::from(cf));
            let r = result & mask;
            regs::set_sub_flags(regs, d, s, result, size);
            regs.set_flag(rflags::CF, u128::from(d) < wide_src);
            let d_s = i128::from(sign_extend(d, size));
            let s_s = i128::from(sign_extend(s, size));
            let expected = d_s.wrapping_sub(s_s).wrapping_sub(i128::from(cf != 0));
            let got = i128::from(sign_extend(r, size));
            regs.set_flag(rflags::OF, expected != got);
            write_op(mem, regs, instr, 0, r)?;
        }
        ArithOp::Cmp => {
            regs::set_sub_flags(regs, d, s, result, size);
        }
        ArithOp::Xor | ArithOp::Or | ArithOp::And => {
            regs::set_logic_flags(regs, result, size);
            write_op(mem, regs, instr, 0, result & mask)?;
        }
    }
    Ok(())
}

pub(super) fn exec_imul(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    // Forms: 1-op (RAX/RDX), 2-op (dst *= src), 3-op (dst = src1 * imm).
    let nops = instr.op_count();
    match nops {
        1 => {
            let size = op_size_bytes(instr, 0)?;
            let src = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
            let a = regs.rax() & regs::size_mask(size);
            let product = i128::from(sign_extend(a, size)) * i128::from(sign_extend(src, size));
            write_imul_product(regs, product, size)?;
            Ok(())
        }
        2 => {
            let size = op_size_bytes(instr, 0)?;
            let dst = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
            let src = read_op(mem, regs, instr, 1)? & regs::size_mask(size);
            let product = i128::from(sign_extend(dst, size)) * i128::from(sign_extend(src, size));
            let lo = (product as u64) & regs::size_mask(size);
            write_op(mem, regs, instr, 0, lo)?;
            set_imul_flags(regs, product, size);
            Ok(())
        }
        3 => {
            let size = op_size_bytes(instr, 0)?;
            let src = read_op(mem, regs, instr, 1)? & regs::size_mask(size);
            let imm = read_op(mem, regs, instr, 2)? & regs::size_mask(size);
            let product = i128::from(sign_extend(src, size)) * i128::from(sign_extend(imm, size));
            let lo = (product as u64) & regs::size_mask(size);
            write_op(mem, regs, instr, 0, lo)?;
            set_imul_flags(regs, product, size);
            Ok(())
        }
        _ => Err(StepExecError::Cpu(CpuError::Message(format!(
            "imul with {nops} operands"
        )))),
    }
}

pub(super) fn exec_mul(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let src = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
    let a = regs.rax() & regs::size_mask(size);
    let product = u128::from(a).wrapping_mul(u128::from(src));
    match size {
        1 => {
            regs.write_reg(Register::AX, product as u64 & 0xffff)?;
            let hi = (product >> 8) != 0;
            regs.set_flag(rflags::CF, hi);
            regs.set_flag(rflags::OF, hi);
        }
        2 => {
            regs.write_reg(Register::AX, product as u64 & 0xffff)?;
            regs.write_reg(Register::DX, ((product >> 16) as u64) & 0xffff)?;
            let hi = (product >> 16) != 0;
            regs.set_flag(rflags::CF, hi);
            regs.set_flag(rflags::OF, hi);
        }
        4 => {
            regs.write_reg(Register::EAX, product as u64 & 0xffff_ffff)?;
            regs.write_reg(Register::EDX, ((product >> 32) as u64) & 0xffff_ffff)?;
            let hi = (product >> 32) != 0;
            regs.set_flag(rflags::CF, hi);
            regs.set_flag(rflags::OF, hi);
        }
        _ => {
            regs.set_rax(product as u64);
            regs.set_rdx((product >> 64) as u64);
            let hi = (product >> 64) != 0;
            regs.set_flag(rflags::CF, hi);
            regs.set_flag(rflags::OF, hi);
        }
    }
    Ok(())
}

fn write_imul_product(regs: &mut RegFile, product: i128, size: usize) -> Result<(), StepExecError> {
    match size {
        1 => {
            regs.write_reg(Register::AX, product as u64 & 0xffff)?;
        }
        2 => {
            regs.write_reg(Register::AX, product as u64 & 0xffff)?;
            regs.write_reg(Register::DX, ((product >> 16) as u64) & 0xffff)?;
        }
        4 => {
            regs.write_reg(Register::EAX, product as u64 & 0xffff_ffff)?;
            regs.write_reg(Register::EDX, ((product >> 32) as u64) & 0xffff_ffff)?;
        }
        _ => {
            regs.set_rax(product as u64);
            regs.set_rdx((product >> 64) as u64);
        }
    }
    set_imul_flags(regs, product, size);
    Ok(())
}

fn set_imul_flags(regs: &mut RegFile, product: i128, size: usize) {
    // CF/OF set if high half is not sign-extension of low half.
    let bits = size.saturating_mul(8);
    let lo_bits = bits.min(64);
    let lo = product as u64
        & if lo_bits >= 64 {
            u64::MAX
        } else {
            (1_u64 << lo_bits).wrapping_sub(1)
        };
    let sign_ext = if (lo >> (lo_bits.saturating_sub(1))) & 1 == 1 {
        // negative: high should be all ones for width
        match size {
            1 => i128::from(lo as i8),
            2 => i128::from(lo as i16),
            4 => i128::from(lo as i32),
            _ => i128::from(lo as i64),
        }
    } else {
        i128::from(lo)
    };
    // For 1-op IMUL the full product width is 2*size; for 2/3-op only low size is stored.
    // CF/OF = product does not fit in size bytes as signed.
    let max = match size {
        1 => i128::from(i8::MAX),
        2 => i128::from(i16::MAX),
        4 => i128::from(i32::MAX),
        _ => i128::from(i64::MAX),
    };
    let min = match size {
        1 => i128::from(i8::MIN),
        2 => i128::from(i16::MIN),
        4 => i128::from(i32::MIN),
        _ => i128::from(i64::MIN),
    };
    let overflow = product < min || product > max;
    let _ = sign_ext;
    regs.set_flag(rflags::CF, overflow);
    regs.set_flag(rflags::OF, overflow);
}

fn sign_extend(value: u64, size: usize) -> i64 {
    let bits = size.saturating_mul(8).min(64);
    let shift = 64_u32.saturating_sub(u32::try_from(bits).unwrap_or(64));
    ((value as i64) << shift) >> shift
}

pub(super) fn exec_div(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    signed: bool,
) -> Result<(), StepExecError> {
    let size = op_size_bytes(instr, 0)?;
    let divisor = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
    if divisor == 0 {
        return Err(StepExecError::Cpu(CpuError::DivideByZero(instr.ip())));
    }

    match size {
        1 => {
            let dividend = regs.read_reg(Register::AX)? & 0xffff;
            if signed {
                let num = dividend as i16;
                let den = sign_extend(divisor, 1) as i16;
                let q = num
                    .checked_div(den)
                    .ok_or_else(|| StepExecError::Cpu(CpuError::Message("idiv overflow".into())))?;
                let r = num.wrapping_rem(den);
                let ax = u64::from(u16::from(r as u8) << 8 | u16::from(q as u8));
                regs.write_reg(Register::AX, ax)?;
            } else {
                let q = dividend / divisor;
                let r = dividend % divisor;
                if q > 0xff {
                    return Err(StepExecError::Cpu(CpuError::Message("div overflow".into())));
                }
                regs.write_reg(Register::AX, (r & 0xff) << 8 | (q & 0xff))?;
            }
        }
        2 => {
            let lo = regs.read_reg(Register::AX)? & 0xffff;
            let hi = regs.read_reg(Register::DX)? & 0xffff;
            if signed {
                // DX:AX as i32
                let num = (i32::from(hi as i16) << 16) | i32::from(lo as u16);
                let den = sign_extend(divisor, 2) as i32;
                let q = num
                    .checked_div(den)
                    .ok_or_else(|| StepExecError::Cpu(CpuError::Message("idiv overflow".into())))?;
                let r = num.wrapping_rem(den);
                if !(-32768..=32767).contains(&q) {
                    return Err(StepExecError::Cpu(CpuError::Message(
                        "idiv overflow".into(),
                    )));
                }
                regs.write_reg(Register::AX, q as u64 & 0xffff)?;
                regs.write_reg(Register::DX, r as u64 & 0xffff)?;
            } else {
                let dividend = (hi << 16) | lo;
                let q = dividend / divisor;
                let r = dividend % divisor;
                if q > 0xffff {
                    return Err(StepExecError::Cpu(CpuError::Message("div overflow".into())));
                }
                regs.write_reg(Register::AX, q & 0xffff)?;
                regs.write_reg(Register::DX, r & 0xffff)?;
            }
        }
        4 => {
            let lo = regs.read_reg(Register::EAX)? & 0xffff_ffff;
            let hi = regs.read_reg(Register::EDX)? & 0xffff_ffff;
            if signed {
                let num = (i64::from(hi as i32) << 32) | i64::from(lo as u32);
                let den = sign_extend(divisor, 4);
                let q = num
                    .checked_div(den)
                    .ok_or_else(|| StepExecError::Cpu(CpuError::Message("idiv overflow".into())))?;
                let r = num.wrapping_rem(den);
                if !(i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&q) {
                    return Err(StepExecError::Cpu(CpuError::Message(
                        "idiv overflow".into(),
                    )));
                }
                regs.write_reg(Register::EAX, q as u64 & 0xffff_ffff)?;
                regs.write_reg(Register::EDX, r as u64 & 0xffff_ffff)?;
            } else {
                let dividend = (u128::from(hi) << 32) | u128::from(lo);
                let q = dividend / u128::from(divisor);
                let r = dividend % u128::from(divisor);
                if q > u128::from(u32::MAX) {
                    return Err(StepExecError::Cpu(CpuError::Message("div overflow".into())));
                }
                regs.write_reg(Register::EAX, q as u64 & 0xffff_ffff)?;
                regs.write_reg(Register::EDX, r as u64 & 0xffff_ffff)?;
            }
        }
        _ => {
            let lo = regs.rax();
            let hi = regs.rdx();
            if signed {
                let num = (i128::from(hi as i64) << 64) | i128::from(lo);
                let den = i128::from(sign_extend(divisor, 8));
                let q = num
                    .checked_div(den)
                    .ok_or_else(|| StepExecError::Cpu(CpuError::Message("idiv overflow".into())))?;
                let r = num.wrapping_rem(den);
                if q < i128::from(i64::MIN) || q > i128::from(i64::MAX) {
                    return Err(StepExecError::Cpu(CpuError::Message(
                        "idiv overflow".into(),
                    )));
                }
                regs.set_rax(q as u64);
                regs.set_rdx(r as u64);
            } else {
                let dividend = (u128::from(hi) << 64) | u128::from(lo);
                let q = dividend / u128::from(divisor);
                let r = dividend % u128::from(divisor);
                if q > u128::from(u64::MAX) {
                    return Err(StepExecError::Cpu(CpuError::Message("div overflow".into())));
                }
                regs.set_rax(q as u64);
                regs.set_rdx(r as u64);
            }
        }
    }
    Ok(())
}

pub(super) fn exec_cmov(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    taken: bool,
) -> Result<(), StepExecError> {
    if taken {
        let src = read_op(mem, regs, instr, 1)?;
        write_op(mem, regs, instr, 0, src)?;
    }
    Ok(())
}

pub(super) fn exec_setcc(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    taken: bool,
) -> Result<(), StepExecError> {
    write_op(mem, regs, instr, 0, u64::from(taken))?;
    Ok(())
}

pub(super) fn exec_bswap(regs: &mut RegFile, instr: &Instruction) -> Result<(), StepExecError> {
    let reg = instr.op_register(0);
    let v = regs.read_reg(reg)?;
    let size = reg.size();
    let swapped = match size {
        4 => u64::from((v as u32).swap_bytes()),
        8 => v.swap_bytes(),
        _ => {
            return Err(StepExecError::Cpu(CpuError::Message(format!(
                "bswap size {size}"
            ))));
        }
    };
    regs.write_reg(reg, swapped)?;
    Ok(())
}

pub(super) fn exec_bit(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    op: BitOp,
) -> Result<(), StepExecError> {
    let bit_offset = read_op(mem, regs, instr, 1)?;
    match instr.op0_kind() {
        OpKind::Register => {
            let size = instr.op_register(0).size();
            let bits = size.saturating_mul(8);
            let idx = (bit_offset as u32) % u32::try_from(bits).unwrap_or(64);
            let val = read_op(mem, regs, instr, 0)? & regs::size_mask(size);
            let mask = 1_u64 << idx;
            let cf = (val & mask) != 0;
            regs.set_flag(rflags::CF, cf);
            let new = match op {
                BitOp::Bt => val,
                BitOp::Bts => val | mask,
                BitOp::Btr => val & !mask,
                BitOp::Btc => val ^ mask,
            };
            if !matches!(op, BitOp::Bt) {
                write_op(mem, regs, instr, 0, new)?;
            }
        }
        OpKind::Memory => {
            // Memory bit string: EA + signed(offset)/8, bit = offset & 7.
            let base = effective_address(regs, instr)?;
            let off = bit_offset as i64;
            let byte_delta = off.div_euclid(8);
            let bit = u32::try_from(off.rem_euclid(8)).unwrap_or(0);
            let addr = base.wrapping_add(byte_delta as u64);
            let mut b = [0_u8; 1];
            match mem.read(addr, &mut b) {
                Ok(()) => {}
                Err(e) => {
                    drop(e);
                    return Err(StepExecError::InvalidMemory(InvalidMem {
                        access_type: ACCESS_READ,
                        address: addr,
                        size: 1,
                        value: 0,
                    }));
                }
            }
            let val = u64::from(b[0]);
            let mask = 1_u64 << bit;
            let cf = (val & mask) != 0;
            regs.set_flag(rflags::CF, cf);
            if !matches!(op, BitOp::Bt) {
                let new = match op {
                    BitOp::Bt => val,
                    BitOp::Bts => val | mask,
                    BitOp::Btr => val & !mask,
                    BitOp::Btc => val ^ mask,
                };
                write_mem_value(mem, addr, new, 1)?;
            }
        }
        other => {
            return Err(StepExecError::Cpu(CpuError::Message(format!(
                "bt op0 kind {other:?}"
            ))));
        }
    }
    Ok(())
}
