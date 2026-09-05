//! Wave 4 x87 subset: the integer/float scalar stack ops games and CRTs
//! actually use (`fld/fstp/fadd/fmul/fsub/fdiv/fcom/fild/fistp/...`).
//!
//! The stack model is the architectural one: eight f64 physical registers,
//! TOP indexes st(0); `push`/`pop` move TOP. Status-word condition codes
//! (C0/C2/C3) are set by compare forms and observed via `fnstsw`/`fstsw` —
//! the `fcom → fnstsw ax → sahf → jcc` idiom. Exotic transcodencals
//! (`f2xm1`, `fsin`, `fprem`, …) stay unimplemented and fall through to the
//! degrade-not-die fallback.

use iced_x86::{Instruction, OpKind, Register};

use super::{StepExecError, effective_address, read_mem_value, write_mem_value};
use crate::mem::GuestMemory;
use crate::regs::{RegFile, X87_C0, X87_C2, X87_C3};

impl RegFile {
    /// Read st(i) (TOP-relative).
    #[must_use]
    pub(crate) fn x87_st(&self, i: u8) -> f64 {
        self.x87[usize::from((self.x87_top.wrapping_add(i)) & 0x07)]
    }

    /// Write st(i) (TOP-relative).
    pub(crate) fn x87_set_st(&mut self, i: u8, value: f64) {
        self.x87[usize::from((self.x87_top.wrapping_add(i)) & 0x07)] = value;
    }

    /// Push a value onto the x87 stack (becomes the new st(0)).
    pub(crate) fn x87_push(&mut self, value: f64) {
        self.x87_top = self.x87_top.wrapping_sub(1) & 0x07;
        self.x87_set_st(0, value);
    }

    /// Pop st(0) off the x87 stack.
    pub(crate) fn x87_pop(&mut self) -> f64 {
        let value = self.x87_st(0);
        self.x87_top = self.x87_top.wrapping_add(1) & 0x07;
        value
    }
}

/// The x87 `mem` operand's byte width (0 when the operand is not memory).
fn mem_width(instr: &Instruction) -> usize {
    if instr.op_kind(0) != OpKind::Memory {
        return 0;
    }
    instr.memory_size().size()
}

/// Read the memory operand as f64 (`fld`/`fadd m64` family).
fn read_mem_f64(
    mem: &GuestMemory,
    regs: &RegFile,
    instr: &Instruction,
) -> Result<f64, StepExecError> {
    let addr = effective_address(regs, instr)?;
    let width = mem_width(instr);
    let raw = read_mem_value(mem, addr, width)?;
    Ok(match width {
        4 => f64::from(f32::from_bits(u32::try_from(raw).unwrap_or(0))),
        _ => f64::from_bits(raw),
    })
}

/// Write an f64 to the memory operand (`fst`/`fstp`), truncating m32 forms.
fn write_mem_f64(
    mem: &GuestMemory,
    regs: &RegFile,
    instr: &Instruction,
    value: f64,
) -> Result<(), StepExecError> {
    let addr = effective_address(regs, instr)?;
    let width = mem_width(instr);
    match width {
        4 => {
            let bits = u64::from((value as f32).to_bits());
            write_mem_value(mem, addr, bits, 4)
        }
        _ => write_mem_value(mem, addr, value.to_bits(), 8),
    }
}

/// Signed-integer mem operand (`fild`/`fiadd` family); width selects i16/i32/i64.
fn read_mem_int(
    mem: &GuestMemory,
    regs: &RegFile,
    instr: &Instruction,
) -> Result<i64, StepExecError> {
    let addr = effective_address(regs, instr)?;
    let width = mem_width(instr);
    let raw = read_mem_value(mem, addr, width)?;
    Ok(match width {
        2 => i64::from(i16::try_from(raw & 0xFFFF).unwrap_or(0)),
        4 => i64::from(i32::try_from(raw & 0xFFFF_FFFF).unwrap_or(0)),
        _ => i64::try_from(raw).unwrap_or(0),
    })
}

/// Store st(0) as a signed integer (`fist`/`fistp`).
fn write_mem_int(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    value: f64,
    pop: bool,
) -> Result<(), StepExecError> {
    let addr = effective_address(regs, instr)?;
    let width = mem_width(instr);
    let truncated = value.trunc();
    let (bits, size) = match width {
        2 => (
            u64::from(i16::try_from(truncated as i64).unwrap_or(0) as u16),
            2,
        ),
        4 => (
            u64::from(i32::try_from(truncated as i64).unwrap_or(0) as u32),
            4,
        ),
        _ => (u64::try_from(truncated as i64).unwrap_or(0), 8),
    };
    write_mem_value(mem, addr, bits, size)?;
    if pop {
        regs.x87_pop();
    }
    Ok(())
}

/// x87 arithmetic kinds (`fadd/fsub/fsubr/fmul/fdiv/fdivr` families).
#[derive(Clone, Copy)]
enum Arith {
    Add,
    Sub,
    Subr,
    Mul,
    Div,
    Divr,
}

fn arith(kind: Arith, dst: f64, src: f64) -> f64 {
    match kind {
        Arith::Add => dst + src,
        Arith::Sub => dst - src,
        Arith::Subr => src - dst,
        Arith::Mul => dst * src,
        Arith::Div => dst / src,
        Arith::Divr => src / dst,
    }
}

/// Register-form x87 arithmetic: the DESTINATION is op0 (`st0` or `st(i)`),
/// the SOURCE the other; `pop` forms always write st(i) then pop st(0).
/// This encodes FSUB vs FSUBR vs FSUBP vs FSUBRP operand order exactly.
fn apply_reg(regs: &mut RegFile, instr: &Instruction, kind: Arith, pop: bool) {
    let i = sti_index(instr);
    let op0_is_st0 = instr.op0_register() == iced_x86::Register::ST0;
    let (dst_i, src_v, dst_v) = if op0_is_st0 {
        (0_u8, regs.x87_st(i), regs.x87_st(0))
    } else {
        (i, regs.x87_st(0), regs.x87_st(i))
    };
    let result = arith(kind, dst_v, src_v);
    regs.x87_set_st(dst_i, result);
    if pop {
        regs.x87_pop();
    }
}

/// The st(i) register operand index an x87 register-form instruction names.
fn sti_index(instr: &Instruction) -> u8 {
    // iced decodes x87 register forms via op_register; ST0..ST7 numbers 32..39.
    for reg in [instr.op0_register(), instr.op1_register()] {
        let n = reg.number();
        if (32..=39).contains(&n) {
            return u8::try_from(n - 32).unwrap_or(0);
        }
    }
    1
}

/// Compare st(0) against `other` and set C0/C2/C3 (`fcom` semantics).
fn compare(regs: &mut RegFile, other: f64) {
    let a = regs.x87_st(0);
    let (c0, c2, c3) = if a < other {
        (true, false, false)
    } else if a > other {
        (false, false, false)
    } else if a == other {
        (false, false, true)
    } else {
        (true, true, true) // unordered (NaN)
    };
    let mut sw = regs.x87_sw & !(X87_C0 | X87_C2 | X87_C3);
    if c0 {
        sw |= X87_C0;
    }
    if c2 {
        sw |= X87_C2;
    }
    if c3 {
        sw |= X87_C3;
    }
    regs.x87_sw = sw;
}

/// Shared mem/register/int-form arithmetic driver: mem forms write st(0)
/// (`st0 = st0 <op> m` with the R forms reversing to `m <op> st0`), register
/// forms route through [`apply_reg`].
fn exec_x87_arith(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
    kind: Arith,
) -> Result<bool, StepExecError> {
    use iced_x86::Mnemonic as M;
    let mnemonic = instr.mnemonic();
    let int_form = matches!(
        mnemonic,
        M::Fiadd | M::Fisub | M::Fisubr | M::Fimul | M::Fidiv | M::Fidivr
    );
    let pop = matches!(
        mnemonic,
        M::Faddp | M::Fsubp | M::Fsubrp | M::Fmulp | M::Fdivp | M::Fdivrp
    );
    if instr.op_kind(0) == OpKind::Memory {
        // Mem forms always target st(0); int forms read a signed integer.
        let operand = if int_form {
            read_mem_int(mem, regs, instr)? as f64
        } else {
            read_mem_f64(mem, regs, instr)?
        };
        let st0 = regs.x87_st(0);
        regs.x87_set_st(0, arith(kind, st0, operand));
    } else {
        apply_reg(regs, instr, kind, pop);
    }
    Ok(true)
}

/// Execute an x87 instruction (`Ok(false)` = not handled — the caller's
/// catch-all decides, degrading rather than stopping).
pub(crate) fn exec_x87(
    mem: &GuestMemory,
    regs: &mut RegFile,
    instr: &Instruction,
) -> Result<bool, StepExecError> {
    use iced_x86::Mnemonic as M;
    let handled = match instr.mnemonic() {
        M::Fld => {
            if instr.op_kind(0) == OpKind::Memory {
                let value = read_mem_f64(mem, regs, instr)?;
                regs.x87_push(value);
            } else {
                let i = sti_index(instr);
                let value = regs.x87_st(i);
                regs.x87_push(value);
            }
            true
        }
        M::Fldz => {
            regs.x87_push(0.0);
            true
        }
        M::Fld1 => {
            regs.x87_push(1.0);
            true
        }
        M::Fst | M::Fstp => {
            let pop = instr.mnemonic() == M::Fstp;
            if instr.op_kind(0) == OpKind::Memory {
                let value = regs.x87_st(0);
                write_mem_f64(mem, regs, instr, value)?;
                if pop {
                    regs.x87_pop();
                }
            } else {
                let value = regs.x87_st(0);
                let i = sti_index(instr);
                regs.x87_set_st(i, value);
                if pop {
                    regs.x87_pop();
                }
            }
            true
        }
        M::Fild => {
            let raw = read_mem_int(mem, regs, instr)?;
            regs.x87_push(raw as f64);
            true
        }
        M::Fist | M::Fistp => {
            let value = regs.x87_st(0);
            write_mem_int(mem, regs, instr, value, instr.mnemonic() == M::Fistp)?;
            true
        }
        M::Fadd | M::Faddp | M::Fiadd => exec_x87_arith(mem, regs, instr, Arith::Add)?,
        M::Fsub | M::Fsubp | M::Fsubr | M::Fsubrp | M::Fisub | M::Fisubr => {
            let kind = match instr.mnemonic() {
                M::Fsubr | M::Fsubrp | M::Fisubr | M::Fidivr => Arith::Subr,
                _ => Arith::Sub,
            };
            exec_x87_arith(mem, regs, instr, kind)?
        }
        M::Fmul | M::Fmulp | M::Fimul => exec_x87_arith(mem, regs, instr, Arith::Mul)?,
        M::Fdiv | M::Fdivp | M::Fdivr | M::Fdivrp | M::Fidiv | M::Fidivr => {
            let kind = match instr.mnemonic() {
                M::Fdivr | M::Fdivrp | M::Fidivr => Arith::Divr,
                _ => Arith::Div,
            };
            exec_x87_arith(mem, regs, instr, kind)?
        }
        M::Fcom
        | M::Fcomp
        | M::Fcompp
        | M::Ficom
        | M::Ficomp
        | M::Fucom
        | M::Fucomp
        | M::Fucompp => {
            let other = if matches!(instr.mnemonic(), M::Ficom | M::Ficomp)
                || instr.op_kind(0) == OpKind::Memory
            {
                if matches!(instr.mnemonic(), M::Ficom | M::Ficomp) {
                    read_mem_int(mem, regs, instr)? as f64
                } else {
                    read_mem_f64(mem, regs, instr)?
                }
            } else {
                regs.x87_st(sti_index(instr))
            };
            compare(regs, other);
            match instr.mnemonic() {
                M::Fcomp | M::Ficomp | M::Fucomp => {
                    regs.x87_pop();
                }
                M::Fcompp | M::Fucompp => {
                    regs.x87_pop();
                    regs.x87_pop();
                }
                _ => {}
            }
            true
        }
        M::Fnstsw | M::Fstsw => {
            // Store the status word: AX (register form) or m16 (memory form).
            if instr.op_kind(0) == OpKind::Memory {
                let addr = effective_address(regs, instr)?;
                write_mem_value(mem, addr, u64::from(regs.x87_sw), 2)?;
            } else {
                let current = regs.read_reg(Register::AX).unwrap_or(0);
                let value = (current & 0xFFFF_FFFF_FFFF_0000) | u64::from(regs.x87_sw);
                regs.write_reg(Register::AX, value)
                    .map_err(StepExecError::from)?;
            }
            true
        }
        M::Fnstcw | M::Fstcw => {
            if instr.op_kind(0) == OpKind::Memory {
                let addr = effective_address(regs, instr)?;
                write_mem_value(mem, addr, u64::from(regs.x87_cw), 2)?;
            }
            true
        }
        M::Fldcw => {
            if instr.op_kind(0) == OpKind::Memory {
                let addr = effective_address(regs, instr)?;
                let raw = read_mem_value(mem, addr, 2)?;
                regs.x87_cw = u16::try_from(raw & 0xFFFF).unwrap_or(0x037F);
            }
            true
        }
        M::Fchs => {
            let value = regs.x87_st(0);
            regs.x87_set_st(0, -value);
            true
        }
        M::Fabs => {
            let value = regs.x87_st(0);
            regs.x87_set_st(0, value.abs());
            true
        }
        M::Fsqrt => {
            let value = regs.x87_st(0);
            regs.x87_set_st(0, value.sqrt());
            true
        }
        _ => false,
    };
    Ok(handled)
}
