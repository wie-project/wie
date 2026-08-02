//! Live-variable analysis and XMM/GPR load tracking for one decoded block.

#![allow(
    clippy::cast_possible_wrap, // mem width / offset → i32 for Cranelift
    clippy::many_single_char_names, // flag temps d/s/r in flags_* helpers
    clippy::too_many_arguments
)]

use super::super::config::JitConfig;
use super::OFF_XMM;
use super::emit::MemEnv;
use super::flags::iconst_u64;
use super::gpr::reg_index;

use super::super::block::{BlockTerm, DecodedInsn, is_string_op};

use cranelift::prelude::*;
use cranelift_codegen::ir::MemFlagsData;
use iced_x86::{Instruction, Mnemonic, OpKind, Register};

pub(super) fn analyze_live_gprs(insns: &[DecodedInsn]) -> [bool; 16] {
    let mut live = [false; 16];
    for d in insns {
        mark_insn_gprs(&d.instr, &mut live);
    }
    live
}

pub(super) fn analyze_live_xmm(insns: &[DecodedInsn]) -> [bool; 16] {
    let mut live = [false; 16];
    for d in insns {
        mark_insn_xmm(&d.instr, &mut live);
    }
    live
}

pub(super) fn analyze_def_xmm(insns: &[DecodedInsn]) -> [bool; 16] {
    let mut defs = [false; 16];
    for d in insns {
        // Destination is typically op0 for SSE ops that write XMM.
        if d.instr.op_count() > 0
            && d.instr.op_kind(0) == OpKind::Register
            && d.instr.op_register(0).is_xmm()
        {
            let n = d.instr.op_register(0).number();
            if n < 16 {
                defs[n] = true;
            }
        }
    }
    defs
}

pub(super) fn xmm_mask_from(bits: &[bool; 16]) -> u16 {
    let mut m = 0_u16;
    for (i, &b) in bits.iter().enumerate() {
        if b {
            m |= 1_u16 << i;
        }
    }
    m
}

pub(super) fn mark_insn_xmm(instr: &Instruction, live: &mut [bool; 16]) {
    for i in 0..instr.op_count() {
        if instr.op_kind(i) == OpKind::Register {
            let r = instr.op_register(i);
            if r.is_xmm() {
                let n = r.number();
                if n < 16 {
                    live[n] = true;
                }
            }
        }
    }
}

pub(super) fn load_xmm_pair(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    idx: usize,
    xmm: &mut [Value; 32],
    loaded: &mut [bool; 16],
) {
    if loaded[idx] {
        return;
    }
    let base = i64::from(OFF_XMM) + i64::try_from(idx.saturating_mul(16)).unwrap_or(0);
    let p = bcx.ins().iadd_imm(ctx_ptr, base);
    if JitConfig::get().simd_enabled() {
        // Single 128-bit load → Neon Q reg; split for lo/hi SSA compatibility.
        let v = bcx.ins().load(types::I8X16, flags, p, 0);
        let as_i64x2 = bcx.ins().bitcast(types::I64X2, flags, v);
        xmm[idx * 2] = bcx.ins().extractlane(as_i64x2, 0);
        xmm[idx * 2 + 1] = bcx.ins().extractlane(as_i64x2, 1);
    } else {
        let plo = p;
        let phi = bcx.ins().iadd_imm(ctx_ptr, base + 8);
        xmm[idx * 2] = bcx.ins().load(types::I64, flags, plo, 0);
        xmm[idx * 2 + 1] = bcx.ins().load(types::I64, flags, phi, 0);
    }
    loaded[idx] = true;
}

// `mark_xmm_dirty_ir` used to emit a `load / or / store` on `JitCtx.xmm_dirty_bits`
// per XMM def. Removed: `CompiledBlock::xmm_may_def_mask` is a static superset of
// what those RMWs computed, and the host exit path now always ORs `xmm_dirty_bits`
// with `xmm_may_def_mask`, so trampolines still contribute their dynamic dirty bits.
// Skipping the per-def RMW eliminates a JitCtx aliasing edge that was blocking
// Cranelift LICM/CSE on sticky/pin metadata inside SSE-heavy loop bodies.

pub(super) fn pair_to_i8x16(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    lo: Value,
    hi: Value,
) -> Value {
    let zero = iconst_u64(bcx, 0);
    let mut v = bcx.ins().splat(types::I64X2, zero);
    v = bcx.ins().insertlane(v, lo, 0);
    v = bcx.ins().insertlane(v, hi, 1);
    bcx.ins().bitcast(types::I8X16, flags, v)
}

pub(super) fn i8x16_to_pair(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    v: Value,
) -> (Value, Value) {
    let as_i64x2 = bcx.ins().bitcast(types::I64X2, flags, v);
    let lo = bcx.ins().extractlane(as_i64x2, 0);
    let hi = bcx.ins().extractlane(as_i64x2, 1);
    (lo, hi)
}

pub(super) fn ensure_xmm_loaded(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    instr: &Instruction,
    xmm: &mut [Value; 32],
    loaded: &mut [bool; 16],
) {
    let mut need = [false; 16];
    mark_insn_xmm(instr, &mut need);
    for i in 0..16 {
        if need[i] && !loaded[i] {
            load_xmm_pair(bcx, ctx_ptr, flags, i, xmm, loaded);
        }
    }
}

/// Write XMM lo/hi SSA and immediately store into JitCtx (fault-safe write-through).
pub(super) fn store_xmm_pair(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    xmm: &mut [Value; 32],
    idx: usize,
    lo: Value,
    hi: Value,
) {
    xmm[idx * 2] = lo;
    xmm[idx * 2 + 1] = hi;
    let base = i64::from(OFF_XMM) + i64::try_from(idx.saturating_mul(16)).unwrap_or(0);
    let p = bcx.ins().iadd_imm(mem.ctx_ptr, base);
    if JitConfig::get().simd_enabled() {
        let v = pair_to_i8x16(bcx, mem.flags, lo, hi);
        bcx.ins().store(mem.flags, v, p, 0);
    } else {
        let phi = bcx.ins().iadd_imm(mem.ctx_ptr, base + 8);
        bcx.ins().store(mem.flags, lo, p, 0);
        bcx.ins().store(mem.flags, hi, phi, 0);
    }
    // No `xmm_dirty_bits` RMW: `xmm_may_def_mask` (computed statically at compile)
    // covers this def, and the host exit path ORs both masks unconditionally.
}

pub(super) fn xmm_index(reg: Register) -> Result<usize, String> {
    if !reg.is_xmm() {
        return Err(format!("not xmm {reg:?}"));
    }
    let n = reg.number();
    if n < 16 {
        Ok(n)
    } else {
        Err(format!("xmm OOB {n}"))
    }
}

pub(super) fn read_xmm_pair(xmm: &[Value; 32], reg: Register) -> Result<(Value, Value), String> {
    let i = xmm_index(reg)?;
    Ok((xmm[i * 2], xmm[i * 2 + 1]))
}

pub(super) fn block_needs_flags(insns: &[DecodedInsn], term: Option<BlockTerm>) -> bool {
    if matches!(term, Some(BlockTerm::Jcc { .. })) {
        return true;
    }
    insns.iter().any(|d| {
        if is_string_op(&d.instr) {
            // SCAS/CMPS write flags; DF is read by all string ops.
            return true;
        }
        matches!(
            d.instr.mnemonic(),
            Mnemonic::Add
                | Mnemonic::Adc
                | Mnemonic::Sub
                | Mnemonic::Sbb
                | Mnemonic::Xor
                | Mnemonic::And
                | Mnemonic::Or
                | Mnemonic::Cmp
                | Mnemonic::Test
                | Mnemonic::Inc
                | Mnemonic::Dec
                | Mnemonic::Neg
                | Mnemonic::Imul
                | Mnemonic::Shl
                | Mnemonic::Sal
                | Mnemonic::Shr
                | Mnemonic::Sar
                | Mnemonic::Rol
                | Mnemonic::Ror
                | Mnemonic::Bt
                | Mnemonic::Bts
                | Mnemonic::Btr
                | Mnemonic::Btc
                | Mnemonic::Cld
                | Mnemonic::Std
                | Mnemonic::Pushfq
                | Mnemonic::Popfq
                | Mnemonic::Cmove
                | Mnemonic::Cmovne
                | Mnemonic::Cmova
                | Mnemonic::Cmovae
                | Mnemonic::Cmovb
                | Mnemonic::Cmovbe
                | Mnemonic::Cmovg
                | Mnemonic::Cmovge
                | Mnemonic::Cmovl
                | Mnemonic::Cmovle
                | Mnemonic::Cmovo
                | Mnemonic::Cmovno
                | Mnemonic::Cmovs
                | Mnemonic::Cmovns
                | Mnemonic::Cmovp
                | Mnemonic::Cmovnp
                | Mnemonic::Sete
                | Mnemonic::Setne
                | Mnemonic::Seta
                | Mnemonic::Setae
                | Mnemonic::Setb
                | Mnemonic::Setbe
                | Mnemonic::Setg
                | Mnemonic::Setge
                | Mnemonic::Setl
                | Mnemonic::Setle
                | Mnemonic::Seto
                | Mnemonic::Setno
                | Mnemonic::Sets
                | Mnemonic::Setns
                | Mnemonic::Setp
                | Mnemonic::Setnp
                | Mnemonic::Comiss
                | Mnemonic::Comisd
                | Mnemonic::Ucomiss
                | Mnemonic::Ucomisd
        )
    })
}

pub(super) fn block_has_mem(insns: &[DecodedInsn]) -> bool {
    insns.iter().any(|d| {
        let m = d.instr.mnemonic();
        if matches!(
            m,
            Mnemonic::Push
                | Mnemonic::Pop
                | Mnemonic::Pushfq
                | Mnemonic::Popfq
                | Mnemonic::Leave
                | Mnemonic::Call
                | Mnemonic::Ret
        ) {
            return true;
        }
        if is_string_op(&d.instr) {
            return true;
        }
        for i in 0..d.instr.op_count() {
            if d.instr.op_kind(i) == OpKind::Memory {
                return true;
            }
        }
        false
    })
}

pub(super) fn block_has_string(insns: &[DecodedInsn]) -> bool {
    insns.iter().any(|d| is_string_op(&d.instr))
}

pub(super) fn block_has_fp(insns: &[DecodedInsn]) -> bool {
    insns.iter().any(|d| {
        matches!(
            d.instr.mnemonic(),
            Mnemonic::Addss
                | Mnemonic::Subss
                | Mnemonic::Mulss
                | Mnemonic::Divss
                | Mnemonic::Addsd
                | Mnemonic::Subsd
                | Mnemonic::Mulsd
                | Mnemonic::Divsd
                | Mnemonic::Addps
                | Mnemonic::Subps
                | Mnemonic::Mulps
                | Mnemonic::Divps
                | Mnemonic::Addpd
                | Mnemonic::Subpd
                | Mnemonic::Mulpd
                | Mnemonic::Divpd
        )
    })
}

pub(super) fn mark_insn_gprs(instr: &Instruction, live: &mut [bool; 16]) {
    for i in 0..instr.op_count() {
        if instr.op_kind(i) == OpKind::Register
            && let Ok(idx) = reg_index(instr.op_register(i))
        {
            live[idx] = true;
        }
    }
    // RSP always live for stack ops / call / ret.
    if matches!(
        instr.mnemonic(),
        Mnemonic::Push
            | Mnemonic::Pop
            | Mnemonic::Pushfq
            | Mnemonic::Popfq
            | Mnemonic::Leave
            | Mnemonic::Call
            | Mnemonic::Ret
    ) {
        live[4] = true; // RSP
    }
    // Leave: RSP←RBP then pop RBP.
    if matches!(instr.mnemonic(), Mnemonic::Leave) {
        live[5] = true; // RBP
    }
    // Cbw/Cwd/Cwde/Cdqe: implicit accumulator (and DX for Cwd).
    match instr.mnemonic() {
        Mnemonic::Cbw | Mnemonic::Cwde | Mnemonic::Cdqe => {
            live[0] = true; // RAX
        }
        Mnemonic::Cwd => {
            live[0] = true; // RAX (AX)
            live[2] = true; // RDX (DX)
        }
        _ => {}
    }
    // String ops touch RAX/RCX/RSI/RDI implicitly (not always in operands).
    if is_string_op(instr) {
        live[0] = true; // RAX
        live[1] = true; // RCX
        live[6] = true; // RSI
        live[7] = true; // RDI
    }
    for i in 0..instr.op_count() {
        if instr.op_kind(i) == OpKind::Memory || instr.mnemonic() == Mnemonic::Lea {
            mark_mem_regs(instr, live);
            break;
        }
    }
}

pub(super) fn mark_mem_regs(instr: &Instruction, live: &mut [bool; 16]) {
    let base = instr.memory_base();
    if base != Register::None
        && base != Register::RIP
        && base != Register::EIP
        && let Ok(idx) = reg_index(base)
    {
        live[idx] = true;
    }
    let index = instr.memory_index();
    if index != Register::None
        && let Ok(idx) = reg_index(index)
    {
        live[idx] = true;
    }
}

pub(super) fn ensure_gprs_loaded(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    gpr: &mut [Value; 16],
    loaded: &mut [bool; 16],
    instr: &Instruction,
    flags: MemFlagsData,
) {
    let mut need = [false; 16];
    mark_insn_gprs(instr, &mut need);
    for i in 0..16 {
        if need[i] && !loaded[i] {
            let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
            let p = bcx.ins().iadd_imm(ctx_ptr, off);
            gpr[i] = bcx.ins().load(types::I64, flags, p, 0);
            loaded[i] = true;
        }
    }
}
