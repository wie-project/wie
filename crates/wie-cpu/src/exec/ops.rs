//! Instruction-class enums and condition-code helpers for the iced interpreter.
//!
//! `BitOp` / `ArithOp` / `ShiftKind` parameterize the GPR exec helpers; the
//! `cond_from` function evaluates Jcc / CMOVcc / SETcc conditions.

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
    Rcl,
    Rcr,
}

/// Evaluate a Jcc / CMOVcc / SETcc condition from the current rflags.
///
/// One table covers all three families: the iced mnemonics share the same
/// condition suffix (`Je`/`Cmove`/`Sete` all test ZF), mirroring the grouped
/// suffix arms of the JIT's `lower_cond`.
pub(super) fn cond_from(m: Mnemonic, regs: &RegFile) -> bool {
    let (zf, cf, sf, of, pf) = (
        regs.flag(Rflags::ZF),
        regs.flag(Rflags::CF),
        regs.flag(Rflags::SF),
        regs.flag(Rflags::OF),
        regs.flag(Rflags::PF),
    );
    match m {
        Mnemonic::Je | Mnemonic::Cmove | Mnemonic::Sete => zf,
        Mnemonic::Jne | Mnemonic::Cmovne | Mnemonic::Setne => !zf,
        Mnemonic::Ja | Mnemonic::Cmova | Mnemonic::Seta => !cf && !zf,
        Mnemonic::Jae | Mnemonic::Cmovae | Mnemonic::Setae => !cf,
        Mnemonic::Jb | Mnemonic::Cmovb | Mnemonic::Setb => cf,
        Mnemonic::Jbe | Mnemonic::Cmovbe | Mnemonic::Setbe => cf || zf,
        Mnemonic::Jg | Mnemonic::Cmovg | Mnemonic::Setg => !zf && sf == of,
        Mnemonic::Jge | Mnemonic::Cmovge | Mnemonic::Setge => sf == of,
        Mnemonic::Jl | Mnemonic::Cmovl | Mnemonic::Setl => sf != of,
        Mnemonic::Jle | Mnemonic::Cmovle | Mnemonic::Setle => zf || sf != of,
        Mnemonic::Jo | Mnemonic::Cmovo | Mnemonic::Seto => of,
        Mnemonic::Jno | Mnemonic::Cmovno | Mnemonic::Setno => !of,
        Mnemonic::Js | Mnemonic::Cmovs | Mnemonic::Sets => sf,
        Mnemonic::Jns | Mnemonic::Cmovns | Mnemonic::Setns => !sf,
        Mnemonic::Jp | Mnemonic::Cmovp | Mnemonic::Setp => pf,
        Mnemonic::Jnp | Mnemonic::Cmovnp | Mnemonic::Setnp => !pf,
        _ => false,
    }
}
