//! Instruction-class enums and condition-code helpers for the iced interpreter.
//!
//! `BitOp` / `ArithOp` / `ShiftKind` parameterize the GPR exec helpers; the
//! `cond_from_*` functions evaluate Jcc / CMOVcc / SETcc conditions.

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

use crate::regs::{RegFile, rflags};
use iced_x86::Mnemonic;

#[derive(Clone, Copy)]
pub(super) enum BitOp {
    Bt,
    Bts,
    Btr,
    Btc,
}

#[derive(Clone, Copy)]
pub(super) enum ArithOp {
    Add,
    Adc,
    Sub,
    Sbb,
    Xor,
    Or,
    And,
    Cmp,
}

#[derive(Clone, Copy)]
pub(super) enum ShiftKind {
    Shl,
    Shr,
    Sar,
    Rol,
    Ror,
}

pub(super) fn cond_from_jcc(m: Mnemonic, regs: &RegFile) -> bool {
    match m {
        Mnemonic::Je => regs.flag(rflags::ZF),
        Mnemonic::Jne => !regs.flag(rflags::ZF),
        Mnemonic::Ja => !regs.flag(rflags::CF) && !regs.flag(rflags::ZF),
        Mnemonic::Jae => !regs.flag(rflags::CF),
        Mnemonic::Jb => regs.flag(rflags::CF),
        Mnemonic::Jbe => regs.flag(rflags::CF) || regs.flag(rflags::ZF),
        Mnemonic::Jg => !regs.flag(rflags::ZF) && regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Jge => regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Jl => regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Jle => regs.flag(rflags::ZF) || regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Jo => regs.flag(rflags::OF),
        Mnemonic::Jno => !regs.flag(rflags::OF),
        Mnemonic::Js => regs.flag(rflags::SF),
        Mnemonic::Jns => !regs.flag(rflags::SF),
        Mnemonic::Jp => regs.flag(rflags::PF),
        Mnemonic::Jnp => !regs.flag(rflags::PF),
        _ => false,
    }
}

pub(super) fn cond_from_cmov(m: Mnemonic, regs: &RegFile) -> bool {
    match m {
        Mnemonic::Cmove => regs.flag(rflags::ZF),
        Mnemonic::Cmovne => !regs.flag(rflags::ZF),
        Mnemonic::Cmova => !regs.flag(rflags::CF) && !regs.flag(rflags::ZF),
        Mnemonic::Cmovae => !regs.flag(rflags::CF),
        Mnemonic::Cmovb => regs.flag(rflags::CF),
        Mnemonic::Cmovbe => regs.flag(rflags::CF) || regs.flag(rflags::ZF),
        Mnemonic::Cmovg => !regs.flag(rflags::ZF) && regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Cmovge => regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Cmovl => regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Cmovle => regs.flag(rflags::ZF) || regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Cmovo => regs.flag(rflags::OF),
        Mnemonic::Cmovno => !regs.flag(rflags::OF),
        Mnemonic::Cmovs => regs.flag(rflags::SF),
        Mnemonic::Cmovns => !regs.flag(rflags::SF),
        Mnemonic::Cmovp => regs.flag(rflags::PF),
        Mnemonic::Cmovnp => !regs.flag(rflags::PF),
        _ => false,
    }
}

pub(super) fn cond_from_setcc(m: Mnemonic, regs: &RegFile) -> bool {
    match m {
        Mnemonic::Sete => regs.flag(rflags::ZF),
        Mnemonic::Setne => !regs.flag(rflags::ZF),
        Mnemonic::Seta => !regs.flag(rflags::CF) && !regs.flag(rflags::ZF),
        Mnemonic::Setae => !regs.flag(rflags::CF),
        Mnemonic::Setb => regs.flag(rflags::CF),
        Mnemonic::Setbe => regs.flag(rflags::CF) || regs.flag(rflags::ZF),
        Mnemonic::Setg => !regs.flag(rflags::ZF) && regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Setge => regs.flag(rflags::SF) == regs.flag(rflags::OF),
        Mnemonic::Setl => regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Setle => regs.flag(rflags::ZF) || regs.flag(rflags::SF) != regs.flag(rflags::OF),
        Mnemonic::Seto => regs.flag(rflags::OF),
        Mnemonic::Setno => !regs.flag(rflags::OF),
        Mnemonic::Sets => regs.flag(rflags::SF),
        Mnemonic::Setns => !regs.flag(rflags::SF),
        Mnemonic::Setp => regs.flag(rflags::PF),
        Mnemonic::Setnp => !regs.flag(rflags::PF),
        _ => false,
    }
}
