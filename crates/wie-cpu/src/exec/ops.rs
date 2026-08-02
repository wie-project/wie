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

use crate::regs::{RegFile, Rflags};
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
        Mnemonic::Je => regs.flag(Rflags::ZF),
        Mnemonic::Jne => !regs.flag(Rflags::ZF),
        Mnemonic::Ja => !regs.flag(Rflags::CF) && !regs.flag(Rflags::ZF),
        Mnemonic::Jae => !regs.flag(Rflags::CF),
        Mnemonic::Jb => regs.flag(Rflags::CF),
        Mnemonic::Jbe => regs.flag(Rflags::CF) || regs.flag(Rflags::ZF),
        Mnemonic::Jg => !regs.flag(Rflags::ZF) && regs.flag(Rflags::SF) == regs.flag(Rflags::OF),
        Mnemonic::Jge => regs.flag(Rflags::SF) == regs.flag(Rflags::OF),
        Mnemonic::Jl => regs.flag(Rflags::SF) != regs.flag(Rflags::OF),
        Mnemonic::Jle => regs.flag(Rflags::ZF) || regs.flag(Rflags::SF) != regs.flag(Rflags::OF),
        Mnemonic::Jo => regs.flag(Rflags::OF),
        Mnemonic::Jno => !regs.flag(Rflags::OF),
        Mnemonic::Js => regs.flag(Rflags::SF),
        Mnemonic::Jns => !regs.flag(Rflags::SF),
        Mnemonic::Jp => regs.flag(Rflags::PF),
        Mnemonic::Jnp => !regs.flag(Rflags::PF),
        _ => false,
    }
}

pub(super) fn cond_from_cmov(m: Mnemonic, regs: &RegFile) -> bool {
    match m {
        Mnemonic::Cmove => regs.flag(Rflags::ZF),
        Mnemonic::Cmovne => !regs.flag(Rflags::ZF),
        Mnemonic::Cmova => !regs.flag(Rflags::CF) && !regs.flag(Rflags::ZF),
        Mnemonic::Cmovae => !regs.flag(Rflags::CF),
        Mnemonic::Cmovb => regs.flag(Rflags::CF),
        Mnemonic::Cmovbe => regs.flag(Rflags::CF) || regs.flag(Rflags::ZF),
        Mnemonic::Cmovg => !regs.flag(Rflags::ZF) && regs.flag(Rflags::SF) == regs.flag(Rflags::OF),
        Mnemonic::Cmovge => regs.flag(Rflags::SF) == regs.flag(Rflags::OF),
        Mnemonic::Cmovl => regs.flag(Rflags::SF) != regs.flag(Rflags::OF),
        Mnemonic::Cmovle => regs.flag(Rflags::ZF) || regs.flag(Rflags::SF) != regs.flag(Rflags::OF),
        Mnemonic::Cmovo => regs.flag(Rflags::OF),
        Mnemonic::Cmovno => !regs.flag(Rflags::OF),
        Mnemonic::Cmovs => regs.flag(Rflags::SF),
        Mnemonic::Cmovns => !regs.flag(Rflags::SF),
        Mnemonic::Cmovp => regs.flag(Rflags::PF),
        Mnemonic::Cmovnp => !regs.flag(Rflags::PF),
        _ => false,
    }
}

pub(super) fn cond_from_setcc(m: Mnemonic, regs: &RegFile) -> bool {
    match m {
        Mnemonic::Sete => regs.flag(Rflags::ZF),
        Mnemonic::Setne => !regs.flag(Rflags::ZF),
        Mnemonic::Seta => !regs.flag(Rflags::CF) && !regs.flag(Rflags::ZF),
        Mnemonic::Setae => !regs.flag(Rflags::CF),
        Mnemonic::Setb => regs.flag(Rflags::CF),
        Mnemonic::Setbe => regs.flag(Rflags::CF) || regs.flag(Rflags::ZF),
        Mnemonic::Setg => !regs.flag(Rflags::ZF) && regs.flag(Rflags::SF) == regs.flag(Rflags::OF),
        Mnemonic::Setge => regs.flag(Rflags::SF) == regs.flag(Rflags::OF),
        Mnemonic::Setl => regs.flag(Rflags::SF) != regs.flag(Rflags::OF),
        Mnemonic::Setle => regs.flag(Rflags::ZF) || regs.flag(Rflags::SF) != regs.flag(Rflags::OF),
        Mnemonic::Seto => regs.flag(Rflags::OF),
        Mnemonic::Setno => !regs.flag(Rflags::OF),
        Mnemonic::Sets => regs.flag(Rflags::SF),
        Mnemonic::Setns => !regs.flag(Rflags::SF),
        Mnemonic::Setp => regs.flag(Rflags::PF),
        Mnemonic::Setnp => !regs.flag(Rflags::PF),
        _ => false,
    }
}
