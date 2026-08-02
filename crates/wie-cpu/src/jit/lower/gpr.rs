//! GPR operand helpers (read/write, effective-address, sticky-TLB probe) and the
//! //! mov/ALU/shift/push/pop/imul/div/cmpxchg lowering family.

use super::emit::MemEnv;
use super::flags::{
    clear_flags, flag_bit, flags_add, flags_logic, flags_sub, iconst_u64, mask_width, replace_flag,
    select_flag,
};
use super::insn::PendingFlags;
use super::mem::{call_load, call_store};
use super::{
    OFF_MEM_GEN, OFF_STICKY_GEN, OFF_STICKY_PAGE, OFF_STICKY_PROT, OFF_STICKY_PTR, STICKY_WAYS,
    TLB_PROT_R, TLB_PROT_W,
};

use super::super::block::mem_width_bytes;

use crate::mem::PAGE_SIZE;
use crate::regs::Rflags;
use cranelift::codegen::ir::BlockArg;
use cranelift::prelude::*;
use iced_x86::{Instruction, Mnemonic, OpKind, Register};

pub(super) fn sext_to_i64(bcx: &mut FunctionBuilder<'_>, v: Value, bits: u32) -> Value {
    match bits {
        8 => {
            let t = bcx.ins().ireduce(types::I8, v);
            bcx.ins().sextend(types::I64, t)
        }
        16 => {
            let t = bcx.ins().ireduce(types::I16, v);
            bcx.ins().sextend(types::I64, t)
        }
        32 => {
            let t = bcx.ins().ireduce(types::I32, v);
            bcx.ins().sextend(types::I64, t)
        }
        _ => v,
    }
}

pub(super) fn bool_to_i64(bcx: &mut FunctionBuilder<'_>, b: Value) -> Value {
    let one = iconst_u64(bcx, 1);
    let zero = iconst_u64(bcx, 0);
    bcx.ins().select(b, one, zero)
}

pub(super) fn flag_set(bcx: &mut FunctionBuilder<'_>, rflags: Value, bit: Rflags) -> Value {
    let m = iconst_u64(bcx, u64::from(bit));
    let v = bcx.ins().band(rflags, m);
    bcx.ins().icmp_imm(IntCC::NotEqual, v, 0)
}

#[derive(Clone, Copy)]
pub(super) enum Arith {
    Add,
    Adc,
    Sub,
    Sbb,
    Xor,
    And,
    Or,
}

pub(super) fn reg_index(reg: Register) -> Result<usize, String> {
    let full = match reg.full_register() {
        Register::RAX => 0,
        Register::RCX => 1,
        Register::RDX => 2,
        Register::RBX => 3,
        Register::RSP => 4,
        Register::RBP => 5,
        Register::RSI => 6,
        Register::RDI => 7,
        Register::R8 => 8,
        Register::R9 => 9,
        Register::R10 => 10,
        Register::R11 => 11,
        Register::R12 => 12,
        Register::R13 => 13,
        Register::R14 => 14,
        Register::R15 => 15,
        other => return Err(format!("unsupported reg {other:?}")),
    };
    Ok(full)
}

pub(super) fn reg_size_bits(reg: Register) -> u32 {
    match reg.size() {
        1 => 8,
        2 => 16,
        4 => 32,
        _ => 64,
    }
}

pub(super) fn read_gpr(gpr: &[Value; 16], reg: Register) -> Result<Value, String> {
    let i = reg_index(reg)?;
    Ok(gpr[i])
}

pub(super) fn write_gpr(
    bcx: &mut FunctionBuilder<'_>,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    reg: Register,
    val: Value,
) -> Result<(), String> {
    let i = reg_index(reg)?;
    let bits = reg_size_bits(reg);
    let new_v = if bits == 64 {
        val
    } else if bits == 32 {
        let lo = bcx.ins().ireduce(types::I32, val);
        bcx.ins().uextend(types::I64, lo)
    } else if bits == 16 {
        let old = gpr[i];
        let mask = bcx.ins().iconst(types::I64, !0xffff_i64);
        let cleared = bcx.ins().band(old, mask);
        let low_mask = bcx.ins().iconst(types::I64, 0xffff);
        let low = bcx.ins().band(val, low_mask);
        bcx.ins().bor(cleared, low)
    } else {
        if matches!(
            reg,
            Register::AH | Register::BH | Register::CH | Register::DH
        ) {
            return Err("AH/BH/CH/DH not in JIT v1".into());
        }
        let old = gpr[i];
        let mask = bcx.ins().iconst(types::I64, !0xff_i64);
        let cleared = bcx.ins().band(old, mask);
        let low_mask = bcx.ins().iconst(types::I64, 0xff);
        let low = bcx.ins().band(val, low_mask);
        bcx.ins().bor(cleared, low)
    };
    gpr[i] = new_v;
    dirty[i] = true;
    Ok(())
}

#[inline]
pub(super) fn mark_dirty(dirty: &mut [bool; 16], idx: usize) {
    dirty[idx] = true;
}

/// Whether `k` is any immediate operand kind (for `imm8`-style SSE immediates).
pub(super) fn is_imm_kind(k: OpKind) -> bool {
    matches!(
        k,
        OpKind::Immediate8
            | OpKind::Immediate8_2nd
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64
    )
}

pub(super) fn read_imm(bcx: &mut FunctionBuilder<'_>, instr: &Instruction, op: u32) -> Value {
    let imm = instr.immediate(op);
    bcx.ins()
        .iconst(types::I64, i64::from_ne_bytes(imm.to_ne_bytes()))
}

pub(super) fn read_op(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    op: u32,
    gpr: &[Value; 16],
) -> Result<Value, String> {
    match instr.op_kind(op) {
        OpKind::Register => read_gpr(gpr, instr.op_register(op)),
        OpKind::Immediate8
        | OpKind::Immediate8_2nd
        | OpKind::Immediate16
        | OpKind::Immediate32
        | OpKind::Immediate64
        | OpKind::Immediate8to16
        | OpKind::Immediate8to32
        | OpKind::Immediate8to64
        | OpKind::Immediate32to64 => Ok(read_imm(bcx, instr, op)),
        other => Err(format!("op kind {other:?}")),
    }
}

pub(super) fn effective_addr(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &[Value; 16],
) -> Result<Value, String> {
    let base = instr.memory_base();
    let disp = instr.memory_displacement64();
    let disp_c = iconst_u64(bcx, disp);
    // RIP-relative: iced already folded next_ip+disp into displacement64.
    if base == Register::RIP || base == Register::EIP {
        return Ok(disp_c);
    }
    let mut addr = disp_c;
    if base != Register::None {
        let b = read_gpr(gpr, base)?;
        addr = bcx.ins().iadd(b, addr);
    }
    let index = instr.memory_index();
    if index != Register::None {
        let idx = read_gpr(gpr, index)?;
        let scale = u64::from(instr.memory_index_scale());
        let scaled = if scale <= 1 {
            idx
        } else {
            let s = iconst_u64(bcx, scale);
            bcx.ins().imul(idx, s)
        };
        addr = bcx.ins().iadd(addr, scaled);
    }
    Ok(addr)
}

/// Operand size in bits for ALU (from reg or memory width).
pub(super) fn op_width_bits(instr: &Instruction, op: u32) -> Result<u32, String> {
    match instr.op_kind(op) {
        OpKind::Register => Ok(reg_size_bits(instr.op_register(op))),
        OpKind::Memory => Ok(mem_width_bytes(instr)?.saturating_mul(8)),
        _ => {
            // Immediate: use peer operand size.
            if op == 1 && instr.op0_kind() == OpKind::Register {
                Ok(reg_size_bits(instr.op_register(0)))
            } else if op == 1 && instr.op0_kind() == OpKind::Memory {
                Ok(mem_width_bytes(instr)?.saturating_mul(8))
            } else {
                Ok(64)
            }
        }
    }
}

pub(super) fn read_op_mem(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    op: u32,
    gpr: &[Value; 16],
    rflags: Value,
    mem: &mut MemEnv,
) -> Result<Value, String> {
    match instr.op_kind(op) {
        OpKind::Register => read_gpr(gpr, instr.op_register(op)),
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let width = mem_width_bytes(instr)?;
            call_load(bcx, mem, gpr, rflags, addr, width, instr.ip())
        }
        OpKind::Immediate8
        | OpKind::Immediate8_2nd
        | OpKind::Immediate16
        | OpKind::Immediate32
        | OpKind::Immediate64
        | OpKind::Immediate8to16
        | OpKind::Immediate8to32
        | OpKind::Immediate8to64
        | OpKind::Immediate32to64 => Ok(read_imm(bcx, instr, op)),
        other => Err(format!("op kind {other:?}")),
    }
}

/// Probe multi sticky TLB (last [`STICKY_WAYS`] pages): key, in-page, gen, R|W.
///
/// Returns `(ok_i1, host_ptr)` where `host_ptr` is only valid when `ok` is true.
/// Cascades ways with CFG (first hit wins) so a 2–4 page working set stays in IR.
pub(super) fn sticky_tlb_probe(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    addr: Value,
    size: u32,
    write: bool,
) -> (Value, Value) {
    let page_mask = iconst_u64(bcx, PAGE_SIZE - 1);
    let page_off = bcx.ins().band(addr, page_mask);
    let sh = iconst_u64(bcx, 12);
    let page_key = bcx.ins().ushr(addr, sh);
    let size_v = iconst_u64(bcx, u64::from(size));
    let end = bcx.ins().iadd(page_off, size_v);
    // end <= PAGE_SIZE  ⇒  no cross-page
    let page_sz = iconst_u64(bcx, PAGE_SIZE);
    let in_page = bcx.ins().icmp(IntCC::UnsignedLessThanOrEqual, end, page_sz);

    let mem_gen_p = bcx.ins().iadd_imm(mem.ctx_ptr, i64::from(OFF_MEM_GEN));
    let mem_gen = bcx.ins().load(types::I64, mem.flags, mem_gen_p, 0);
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
    let need_v = iconst_u64(bcx, need);

    // Merge block: (ok_i8_as_i64? use i1 as i64 via select — Cranelift brif uses i8/i1)
    let merge = bcx.create_block();
    bcx.append_block_param(merge, types::I8); // ok
    bcx.append_block_param(merge, types::I64); // host

    let zero = iconst_u64(bcx, 0);
    let zero_i8 = bcx.ins().iconst(types::I8, 0);

    // Probe way 0..N-1; on miss fall through to next / final miss.
    for way in 0..STICKY_WAYS {
        let off = i64::try_from(way.saturating_mul(8)).unwrap_or(0);
        let key_p = bcx
            .ins()
            .iadd_imm(mem.ctx_ptr, i64::from(OFF_STICKY_PAGE) + off);
        let ptr_p = bcx
            .ins()
            .iadd_imm(mem.ctx_ptr, i64::from(OFF_STICKY_PTR) + off);
        let prot_p = bcx
            .ins()
            .iadd_imm(mem.ctx_ptr, i64::from(OFF_STICKY_PROT) + off);
        let gen_p = bcx
            .ins()
            .iadd_imm(mem.ctx_ptr, i64::from(OFF_STICKY_GEN) + off);
        let hot_key = bcx.ins().load(types::I64, mem.flags, key_p, 0);
        let hot_base = bcx.ins().load(types::I64, mem.flags, ptr_p, 0);
        let hot_prot = bcx.ins().load(types::I64, mem.flags, prot_p, 0);
        let hot_gen = bcx.ins().load(types::I64, mem.flags, gen_p, 0);
        let key_ok = bcx.ins().icmp(IntCC::Equal, hot_key, page_key);
        let base_nz = bcx.ins().icmp_imm(IntCC::NotEqual, hot_base, 0);
        let gen_ok = bcx.ins().icmp(IntCC::Equal, hot_gen, mem_gen);
        let prot_bits = bcx.ins().band(hot_prot, need_v);
        let prot_ok = bcx.ins().icmp_imm(IntCC::NotEqual, prot_bits, 0);
        let ok1 = bcx.ins().band(key_ok, base_nz);
        let ok2 = bcx.ins().band(ok1, gen_ok);
        let ok3 = bcx.ins().band(ok2, prot_ok);
        let ok = bcx.ins().band(ok3, in_page);
        let host = bcx.ins().iadd(hot_base, page_off);

        let hit = bcx.create_block();
        let miss = bcx.create_block();
        bcx.ins().brif(ok, hit, &[], miss, &[]);
        bcx.switch_to_block(hit);
        bcx.seal_block(hit);
        let one_i8 = bcx.ins().iconst(types::I8, 1);
        bcx.ins()
            .jump(merge, &[BlockArg::Value(one_i8), BlockArg::Value(host)]);
        bcx.switch_to_block(miss);
        bcx.seal_block(miss);
    }

    // All ways missed.
    bcx.ins()
        .jump(merge, &[BlockArg::Value(zero_i8), BlockArg::Value(zero)]);
    bcx.switch_to_block(merge);
    bcx.seal_block(merge);
    let params = bcx.block_params(merge);
    (params[0], params[1])
}

pub(super) fn lower_mov(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let k0 = instr.op0_kind();
    let k1 = instr.op1_kind();
    let ip = instr.ip();
    match (k0, k1) {
        (OpKind::Register, OpKind::Memory) => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let width = mem_width_bytes(instr)?;
            let val = call_load(bcx, mem, gpr, rflags, addr, width, ip)?;
            write_gpr(bcx, gpr, dirty, instr.op_register(0), val)
        }
        (OpKind::Memory, OpKind::Register) => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let width = mem_width_bytes(instr)?;
            let val = read_gpr(gpr, instr.op_register(1))?;
            call_store(bcx, mem, gpr, rflags, addr, width, val, ip)
        }
        (OpKind::Memory, _) => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let width = mem_width_bytes(instr)?;
            let val = read_op(bcx, instr, 1, gpr)?;
            call_store(bcx, mem, gpr, rflags, addr, width, val, ip)
        }
        (OpKind::Register, _) => {
            let src = read_op(bcx, instr, 1, gpr)?;
            write_gpr(bcx, gpr, dirty, instr.op_register(0), src)
        }
        _ => Err("mov form".into()),
    }
}

pub(super) fn lower_movx(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    signed: bool,
) -> Result<(), String> {
    let src_bits = if instr.op1_kind() == OpKind::Memory {
        mem_width_bytes(instr)?.saturating_mul(8)
    } else {
        reg_size_bits(instr.op_register(1))
    };
    let src = if instr.op1_kind() == OpKind::Memory {
        let addr = effective_addr(bcx, instr, gpr)?;
        let width = mem_width_bytes(instr)?;
        call_load(bcx, mem, gpr, rflags, addr, width, instr.ip())?
    } else {
        read_gpr(gpr, instr.op_register(1))?
    };
    let val = extend_value(bcx, src, src_bits, signed);
    write_gpr(bcx, gpr, dirty, instr.op_register(0), val)
}

pub(super) fn extend_value(
    bcx: &mut FunctionBuilder<'_>,
    src: Value,
    src_bits: u32,
    signed: bool,
) -> Value {
    if signed {
        match src_bits {
            8 => {
                let t = bcx.ins().ireduce(types::I8, src);
                bcx.ins().sextend(types::I64, t)
            }
            16 => {
                let t = bcx.ins().ireduce(types::I16, src);
                bcx.ins().sextend(types::I64, t)
            }
            32 => {
                let t = bcx.ins().ireduce(types::I32, src);
                bcx.ins().sextend(types::I64, t)
            }
            _ => src,
        }
    } else {
        match src_bits {
            8 => {
                let m = bcx.ins().iconst(types::I64, 0xff);
                bcx.ins().band(src, m)
            }
            16 => {
                let m = bcx.ins().iconst(types::I64, 0xffff);
                bcx.ins().band(src, m)
            }
            32 => {
                let m = bcx.ins().iconst(types::I64, 0xffff_ffff);
                bcx.ins().band(src, m)
            }
            _ => src,
        }
    }
}

/// Cwde: sign-extend AX (16-bit) to EAX (32-bit).
/// Cdqe: sign-extend EAX (32-bit) to RAX (64-bit).
/// Both are implicit-accumulator, register-only operations.
pub(super) fn lower_cwde_cdqe(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
) -> Result<(), String> {
    let rax = gpr[0]; // RAX
    let val = if instr.mnemonic() == Mnemonic::Cwde {
        // Cwde: sign-extend AX (bottom 16 bits) to 32-bit EAX (bottom 32 bits).
        // Read RAX, ireduce to I16, sextend to I64 (which zeros upper 32 bits
        // in Cranelift's 64-bit representation), then mask into RAX.
        let low16 = bcx.ins().ireduce(types::I16, rax);
        let ext = bcx.ins().sextend(types::I64, low16);
        // Merge: keep RAX[63:32] unchanged, replace RAX[31:0] with ext[31:0].
        // Since ext is sign-extended, its upper 32 bits are copies of bit 31.
        // But x86 Cwde zero-extends into EAX (upper 32 bits of RAX unchanged).
        // Actually on x86-64, Cwde writes to EAX which zero-extends to RAX.
        // So RAX = zero_extend(sign_extend(AX)).
        ext
    } else {
        // Cdqe: sign-extend EAX (bottom 32 bits) to RAX (full 64 bits).
        // On x86-64, Cdqe writes to RAX (full 64-bit dest).
        let low32 = bcx.ins().ireduce(types::I32, rax);
        bcx.ins().sextend(types::I64, low32)
    };
    write_gpr(bcx, gpr, dirty, Register::RAX, val)
}

/// Cbw: sign-extend AL → AX (upper bytes of RAX unchanged above AX).
pub(super) fn lower_cbw(
    bcx: &mut FunctionBuilder<'_>,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
) -> Result<(), String> {
    let rax = gpr[0];
    let al = bcx.ins().ireduce(types::I8, rax);
    let ax = bcx.ins().sextend(types::I16, al);
    let ax64 = bcx.ins().uextend(types::I64, ax);
    write_gpr(bcx, gpr, dirty, Register::AX, ax64)
}

/// Cwd: sign-extend AX → DX:AX (write DX only; AX unchanged).
pub(super) fn lower_cwd(
    bcx: &mut FunctionBuilder<'_>,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
) -> Result<(), String> {
    let rax = gpr[0];
    let ax = bcx.ins().ireduce(types::I16, rax);
    // DX = 0xFFFF if AX < 0, else 0.
    let ax_s = bcx.ins().sextend(types::I32, ax);
    let is_neg = bcx.ins().icmp_imm(IntCC::SignedLessThan, ax_s, 0);
    let ffff = iconst_u64(bcx, 0xffff);
    let zero = iconst_u64(bcx, 0);
    let dx = bcx.ins().select(is_neg, ffff, zero);
    write_gpr(bcx, gpr, dirty, Register::DX, dx)
}

/// Bswap r32/r64: reverse bytes. r32 write zero-extends into the full GPR.
pub(super) fn lower_bswap(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
) -> Result<(), String> {
    let reg = instr.op_register(0);
    let bits = reg_size_bits(reg);
    let val = read_gpr(gpr, reg)?;
    let swapped = match bits {
        32 => {
            let lo = bcx.ins().ireduce(types::I32, val);
            let s = bcx.ins().bswap(lo);
            bcx.ins().uextend(types::I64, s)
        }
        64 => bcx.ins().bswap(val),
        _ => return Err(format!("bswap size {bits}")),
    };
    write_gpr(bcx, gpr, dirty, reg, swapped)
}

/// Leave: RSP ← RBP; RBP ← [RSP]; RSP += 8 (pop frame).
pub(super) fn lower_leave(
    bcx: &mut FunctionBuilder<'_>,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    ip: u64,
) -> Result<(), String> {
    // MOV RSP, RBP
    gpr[4] = gpr[5];
    mark_dirty(dirty, 4);
    // POP RBP
    let rsp = gpr[4];
    let val = call_load(bcx, mem, gpr, rflags, rsp, 8, ip)?;
    let new_rsp = bcx.ins().iadd_imm(rsp, 8);
    gpr[4] = new_rsp;
    mark_dirty(dirty, 4);
    gpr[5] = val;
    mark_dirty(dirty, 5);
    Ok(())
}

/// Pushfq: push full RFLAGS (64-bit) onto the stack.
pub(super) fn lower_pushfq(
    bcx: &mut FunctionBuilder<'_>,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    ip: u64,
) -> Result<(), String> {
    let rsp = gpr[4];
    let new_rsp = bcx.ins().iadd_imm(rsp, -8);
    call_store(bcx, mem, gpr, rflags, new_rsp, 8, rflags, ip)?;
    gpr[4] = new_rsp;
    mark_dirty(dirty, 4);
    Ok(())
}

/// Popfq: pop into RFLAGS; force reserved bit 1 (ALWAYS1).
pub(super) fn lower_popfq(
    bcx: &mut FunctionBuilder<'_>,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    mem: &mut MemEnv,
    ip: u64,
) -> Result<(), String> {
    let rsp = gpr[4];
    let val = call_load(bcx, mem, gpr, *rflags, rsp, 8, ip)?;
    let new_rsp = bcx.ins().iadd_imm(rsp, 8);
    gpr[4] = new_rsp;
    mark_dirty(dirty, 4);
    // Keep ALWAYS1 set; clear it first then OR so the bit is definite.
    let cleared = clear_flags(bcx, val, Rflags::ALWAYS1);
    let always1 = iconst_u64(bcx, u64::from(Rflags::ALWAYS1));
    *rflags = bcx.ins().bor(cleared, always1);
    Ok(())
}

pub(super) fn lower_lea(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
) -> Result<(), String> {
    let addr = effective_addr(bcx, instr, gpr)?;
    write_gpr(bcx, gpr, dirty, instr.op_register(0), addr)
}

/// 64-bit push (Intel: value of RSP before decrement is what `push rsp` stores).
pub(super) fn lower_push(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let size = 8_u32;
    let val = match instr.op0_kind() {
        OpKind::Register => read_gpr(gpr, instr.op_register(0))?,
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let width = mem_width_bytes(instr)?;
            call_load(bcx, mem, gpr, rflags, addr, width, instr.ip())?
        }
        _ => read_imm(bcx, instr, 0),
    };
    let rsp = gpr[4];
    let new_rsp = bcx.ins().iadd_imm(rsp, -i64::from(size));
    call_store(bcx, mem, gpr, rflags, new_rsp, size, val, instr.ip())?;
    gpr[4] = new_rsp;
    mark_dirty(dirty, 4);
    Ok(())
}

pub(super) fn lower_pop(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let size = 8_u32;
    let rsp = gpr[4];
    let val = call_load(bcx, mem, gpr, rflags, rsp, size, instr.ip())?;
    let new_rsp = bcx.ins().iadd_imm(rsp, i64::from(size));
    gpr[4] = new_rsp;
    mark_dirty(dirty, 4);
    match instr.op0_kind() {
        OpKind::Register => {
            // pop rsp: write the popped value (already advanced rsp).
            write_gpr(bcx, gpr, dirty, instr.op_register(0), val)
        }
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let width = mem_width_bytes(instr)?;
            call_store(bcx, mem, gpr, rflags, addr, width, val, instr.ip())
        }
        _ => Err("pop form".into()),
    }
}

/// Lazy ALU: defer flag packing; overwrite previous pending (last writer wins).
pub(super) fn lower_arith_lazy(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    pending: &mut PendingFlags,
    mem: &mut MemEnv,
    op: Arith,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 0)?;
    let a_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let b_raw = read_op_mem(bcx, instr, 1, gpr, *rflags, mem)?;
    let a = mask_width(bcx, a_raw, bits);
    let b = mask_width(bcx, b_raw, bits);
    let res = match op {
        Arith::Add => bcx.ins().iadd(a, b),
        Arith::Sub => bcx.ins().isub(a, b),
        Arith::Xor => bcx.ins().bxor(a, b),
        Arith::And => bcx.ins().band(a, b),
        Arith::Or => bcx.ins().bor(a, b),
        Arith::Adc | Arith::Sbb => return Err("lazy path only for non-carry ALU".into()),
    };
    let res_m = mask_width(bcx, res, bits);
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, res_m, bits)?;
    *pending = match op {
        Arith::Add => PendingFlags::Add {
            a,
            b,
            res: res_m,
            bits,
        },
        Arith::Sub => PendingFlags::Sub {
            a,
            b,
            res: res_m,
            bits,
        },
        Arith::Xor | Arith::And | Arith::Or => PendingFlags::Logic { res: res_m, bits },
        Arith::Adc | Arith::Sbb => PendingFlags::None,
    };
    Ok(())
}

pub(super) fn lower_cmp_test_lazy(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    rflags: &mut Value,
    pending: &mut PendingFlags,
    mem: &mut MemEnv,
    is_cmp: bool,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 0)?;
    let a_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let b_raw = read_op_mem(bcx, instr, 1, gpr, *rflags, mem)?;
    let a = mask_width(bcx, a_raw, bits);
    let b = mask_width(bcx, b_raw, bits);
    if is_cmp {
        let res_raw = bcx.ins().isub(a, b);
        let res = mask_width(bcx, res_raw, bits);
        *pending = PendingFlags::Sub { a, b, res, bits };
    } else {
        let res_raw = bcx.ins().band(a, b);
        let res = mask_width(bcx, res_raw, bits);
        *pending = PendingFlags::Logic { res, bits };
    }
    Ok(())
}

/// Eager ALU (adc/sbb): needs live CF from flushed flags.
pub(super) fn lower_arith(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    mem: &mut MemEnv,
    op: Arith,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 0)?;
    let a_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let b_raw = read_op_mem(bcx, instr, 1, gpr, *rflags, mem)?;
    let a = mask_width(bcx, a_raw, bits);
    let b = mask_width(bcx, b_raw, bits);
    let cf_val = flag_bit(bcx, *rflags, Rflags::CF);
    let res = match op {
        Arith::Add => bcx.ins().iadd(a, b),
        Arith::Adc => {
            let t = bcx.ins().iadd(a, b);
            bcx.ins().iadd(t, cf_val)
        }
        Arith::Sub => bcx.ins().isub(a, b),
        Arith::Sbb => {
            let t = bcx.ins().isub(a, b);
            bcx.ins().isub(t, cf_val)
        }
        Arith::Xor => bcx.ins().bxor(a, b),
        Arith::And => bcx.ins().band(a, b),
        Arith::Or => bcx.ins().bor(a, b),
    };
    let res_m = mask_width(bcx, res, bits);
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, res_m, bits)?;
    *rflags = match op {
        Arith::Xor | Arith::And | Arith::Or => flags_logic(bcx, *rflags, res_m, bits),
        Arith::Add => flags_add(bcx, *rflags, a, b, res_m, bits),
        Arith::Adc => flags_adc(bcx, *rflags, a, b, cf_val, res_m, bits),
        Arith::Sub => flags_sub(bcx, *rflags, a, b, res_m, bits),
        Arith::Sbb => flags_sbb(bcx, *rflags, a, b, cf_val, res_m, bits),
    };
    Ok(())
}

/// SBB flags: match iced full-width borrow CF (do not mask `s+cf` before CF test).
pub(super) fn flags_sbb(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    d: Value,
    s: Value,
    cf: Value,
    result: Value,
    bits: u32,
) -> Value {
    // Base ZF/SF/PF/AF/OF from (d - s) then correct CF/OF for carry-in.
    let mut f = flags_sub(bcx, old, d, s, result, bits);
    // CF = d < s + cf (full width; s+cf may exceed operand size).
    let s_plus_cf = bcx.ins().iadd(s, cf);
    let cf_b = if bits >= 64 {
        // For 64-bit: overflow of s+cf means always borrow; else d < s+cf.
        let c_ov = bcx.ins().icmp(IntCC::UnsignedLessThan, s_plus_cf, s); // s+cf wrapped
        let c_lt = bcx.ins().icmp(IntCC::UnsignedLessThan, d, s_plus_cf);
        let c_ovi = bool_to_i64(bcx, c_ov);
        let c_lti = bool_to_i64(bcx, c_lt);
        let any = bcx.ins().bor(c_ovi, c_lti);
        let zero = iconst_u64(bcx, 0);
        bcx.ins().icmp(IntCC::NotEqual, any, zero)
    } else {
        // `s`/`d` masked to operand width; `s+cf` may be 2^bits — then CF is always set.
        bcx.ins().icmp(IntCC::UnsignedLessThan, d, s_plus_cf)
    };
    // For bits < 64, s_plus_cf may have bits above `bits` set (when s=mask and cf=1).
    // `d` is masked so d < s_plus_cf is correct when s_plus_cf > mask.
    let cf_on = select_flag(bcx, cf_b, Rflags::CF);
    f = replace_flag(bcx, f, Rflags::CF, cf_on);
    f
}

/// ADC flags: match iced `set_add_flags(d, s+cf, result)` then CF from wide add.
pub(super) fn flags_adc(
    bcx: &mut FunctionBuilder<'_>,
    old: Value,
    d: Value,
    s: Value,
    cf: Value,
    result: Value,
    bits: u32,
) -> Value {
    let s_eff = bcx.ins().iadd(s, cf);
    let s_eff_m = mask_width(bcx, s_eff, bits);
    let mut f = flags_add(bcx, old, d, s_eff_m, result, bits);
    if bits >= 64 {
        let sum_ds = bcx.ins().iadd(d, s);
        let c1 = bcx.ins().icmp(IntCC::UnsignedLessThan, sum_ds, d);
        let sum = bcx.ins().iadd(sum_ds, cf);
        let c2 = bcx.ins().icmp(IntCC::UnsignedLessThan, sum, sum_ds);
        let c1i = bool_to_i64(bcx, c1);
        let c2i = bool_to_i64(bcx, c2);
        let any = bcx.ins().bor(c1i, c2i);
        let zero = iconst_u64(bcx, 0);
        let any_b = bcx.ins().icmp(IntCC::NotEqual, any, zero);
        let cf_on = select_flag(bcx, any_b, Rflags::CF);
        f = replace_flag(bcx, f, Rflags::CF, cf_on);
    } else {
        let t = bcx.ins().iadd(d, s);
        let sum = bcx.ins().iadd(t, cf);
        let sh = iconst_u64(bcx, u64::from(bits));
        let shifted = bcx.ins().ushr(sum, sh);
        let zero = iconst_u64(bcx, 0);
        let cf_b = bcx.ins().icmp(IntCC::NotEqual, shifted, zero);
        let cf_on = select_flag(bcx, cf_b, Rflags::CF);
        f = replace_flag(bcx, f, Rflags::CF, cf_on);
    }
    f
}

pub(super) fn lower_imul(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let nops = instr.op_count();
    let bits = op_width_bits(instr, 0)?;
    let (a_raw, b_raw) = match nops {
        2 => (
            read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?,
            read_op_mem(bcx, instr, 1, gpr, *rflags, mem)?,
        ),
        3 => (
            read_op_mem(bcx, instr, 1, gpr, *rflags, mem)?,
            read_imm(bcx, instr, 2),
        ),
        _ => return Err(format!("imul {nops} ops")),
    };
    let a_m = mask_width(bcx, a_raw, bits);
    let b_m = mask_width(bcx, b_raw, bits);
    let a = sext_to_i64(bcx, a_m, bits);
    let b = sext_to_i64(bcx, b_m, bits);
    let (lo, overflow) = if bits >= 64 {
        let lo = bcx.ins().imul(a, b);
        let hi = bcx.ins().smulhi(a, b);
        let sh = iconst_u64(bcx, 63);
        let sign = bcx.ins().sshr(lo, sh);
        let ov = bcx.ins().icmp(IntCC::NotEqual, hi, sign);
        (lo, ov)
    } else {
        let product = bcx.ins().imul(a, b);
        let lo = mask_width(bcx, product, bits);
        let expected = sext_to_i64(bcx, lo, bits);
        let ov = bcx.ins().icmp(IntCC::NotEqual, product, expected);
        (lo, ov)
    };
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, lo, bits)?;
    let cf_on = select_flag(bcx, overflow, Rflags::CF);
    let of_on = select_flag(bcx, overflow, Rflags::OF);
    let f = replace_flag(bcx, *rflags, Rflags::CF, cf_on);
    *rflags = replace_flag(bcx, f, Rflags::OF, of_on);
    Ok(())
}

/// Lower `div`/`idiv` (32-bit only in v1).
///
/// x86 32-bit division: dividend = EDX:EAX (64-bit), divisor = r/m32,
/// quotient → EAX, remainder → EDX.  `div` is unsigned, `idiv` signed.
///
/// Zero divisor: the iced interpreter raises `DivideByZero`.  Cranelift's
/// `udiv`/`sdiv` trap on zero, which would abort the host process, so the
/// divisor is guarded (clamped to 1) and the result forced to 0 when the
/// original divisor was zero — matching AArch64 hardware `udiv`/`sdiv`
/// semantics.  Only affects buggy guests that divide by zero.
pub(super) fn lower_div(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let signed = instr.mnemonic() == Mnemonic::Idiv;
    let bits = op_width_bits(instr, 0)?;
    if bits != 32 {
        return Err(format!("div: only 32-bit lowered in v1 (got {bits} bits)"));
    }

    // Dividend = EDX:EAX as 64-bit (signed for idiv, unsigned for div).
    let eax = mask_width(bcx, read_gpr(gpr, Register::EAX)?, 32);
    let edx = mask_width(bcx, read_gpr(gpr, Register::EDX)?, 32);
    let eax32 = bcx.ins().ireduce(types::I32, eax);
    let eax64 = bcx.ins().uextend(types::I64, eax32);
    let edx64 = if signed {
        let edx32 = bcx.ins().ireduce(types::I32, edx);
        bcx.ins().sextend(types::I64, edx32)
    } else {
        let edx32 = bcx.ins().ireduce(types::I32, edx);
        bcx.ins().uextend(types::I64, edx32)
    };
    let sh = iconst_u64(bcx, 32);
    let hi = bcx.ins().ishl(edx64, sh);
    let dividend = bcx.ins().bor(hi, eax64);

    // Divisor = operand 0, sign/zero-extended to 64.
    let divisor_operand = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let div_raw = mask_width(bcx, divisor_operand, 32);
    let divisor = if signed {
        let d32 = bcx.ins().ireduce(types::I32, div_raw);
        bcx.ins().sextend(types::I64, d32)
    } else {
        let d32 = bcx.ins().ireduce(types::I32, div_raw);
        bcx.ins().uextend(types::I64, d32)
    };

    // Zero-divisor guard: clamp to 1 for the division, then force q=r=0 when
    // the original divisor was zero (Cranelift traps on zero divisor).
    let zero = iconst_u64(bcx, 0);
    let one = iconst_u64(bcx, 1);
    let is_zero = bcx.ins().icmp(IntCC::Equal, divisor, zero);
    let divisor_safe = bcx.ins().select(is_zero, one, divisor);

    let (q, r) = if signed {
        (
            bcx.ins().sdiv(dividend, divisor_safe),
            bcx.ins().srem(dividend, divisor_safe),
        )
    } else {
        (
            bcx.ins().udiv(dividend, divisor_safe),
            bcx.ins().urem(dividend, divisor_safe),
        )
    };
    let q = bcx.ins().select(is_zero, zero, q);
    let r = bcx.ins().select(is_zero, zero, r);

    // EAX = low 32 of quotient, EDX = low 32 of remainder.
    write_gpr(bcx, gpr, dirty, Register::EAX, q)?;
    write_gpr(bcx, gpr, dirty, Register::EDX, r)?;
    Ok(())
}

/// Lower Bt/Bts/Btr/Btc (bit test / set / reset / complement) with direct flag write.
/// Flushes pending flags first, then reads the dest, computes the bit,
/// sets CF in rflags, and for Bts/Btr/Btc writes the modified value back.
///
/// Note: register forms mask the bit index by operand width. Memory forms use the
/// same simple EA+width path as the rest of the ALU JIT (full bit-string address
/// with large signed offsets remains on the iced path when not lowerable).
pub(super) fn lower_bit_test_op(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 0)?;
    let val_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let val = mask_width(bcx, val_raw, bits);

    // Compute bit index from operand 1 (register or immediate).
    let bit_idx = if instr.op1_kind() == OpKind::Register {
        let idx_raw = read_gpr(gpr, instr.op_register(1))?;
        mask_width(bcx, idx_raw, 32) // x86 masks to 5/6/7 bits depending on size
    } else {
        let imm = read_imm(bcx, instr, 1);
        mask_width(bcx, imm, 32)
    };

    // Mask bit index by operand size (x86: 5 bits for 32-bit, 6 for 64-bit).
    let max_bits = if bits == 64 {
        iconst_u64(bcx, 63)
    } else {
        iconst_u64(bcx, 31) // 32-bit: 5-bit mask
    };
    let idx_masked = bcx.ins().band(bit_idx, max_bits);

    // Compute the bit value at `idx_masked` -> CF = (val >> idx_masked) & 1
    let shifted = bcx.ins().ushr(val, idx_masked);
    let one = iconst_u64(bcx, 1);
    let bit_val = bcx.ins().band(shifted, one);
    let is_set = bcx.ins().icmp_imm(IntCC::NotEqual, bit_val, 0);
    let cf_on = select_flag(bcx, is_set, Rflags::CF);
    *rflags = replace_flag(bcx, *rflags, Rflags::CF, cf_on);

    // For Bts/Btr/Btc, write back the modified value.
    let mnemonic = instr.mnemonic();
    if matches!(mnemonic, Mnemonic::Bts | Mnemonic::Btr | Mnemonic::Btc) {
        let bit_mask = bcx.ins().ishl(one, idx_masked);
        let new_val = if mnemonic == Mnemonic::Bts {
            // Set the bit: val | (1 << idx)
            bcx.ins().bor(val, bit_mask)
        } else if mnemonic == Mnemonic::Btr {
            // Clear the bit: val & !(1 << idx)
            let not_mask = bcx.ins().bnot(bit_mask);
            bcx.ins().band(val, not_mask)
        } else {
            // Complement the bit: val ^ (1 << idx)
            bcx.ins().bxor(val, bit_mask)
        };
        let new_val_full = if bits < 64 {
            // Preserve upper bits of the full register/mem cell when partial.
            let full_mask = iconst_u64(bcx, (1_u64 << bits) - 1);
            let not_full = bcx.ins().bnot(full_mask);
            let cleared = bcx.ins().band(val_raw, not_full);
            bcx.ins().bor(cleared, new_val)
        } else {
            new_val
        };
        write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, new_val_full, bits)?;
    }

    Ok(())
}

/// Lower Xadd (exchange and add): temp = dst; dst = dst + src; src = temp.
/// Sets ADD flags. Flushes pending flags before operation.
pub(super) fn lower_xadd(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 0)?;
    let dst_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let src_raw = read_op_mem(bcx, instr, 1, gpr, *rflags, mem)?;
    let dst_val = mask_width(bcx, dst_raw, bits);
    let src_val = mask_width(bcx, src_raw, bits);

    // sum = dst + src (for flags)
    let sum = if bits == 64 {
        bcx.ins().iadd(dst_val, src_val)
    } else {
        let d = bcx.ins().ireduce(types::I32, dst_val);
        let s = bcx.ins().ireduce(types::I32, src_val);
        bcx.ins().iadd(d, s)
    };
    let sum_ext = sext_to_i64(bcx, sum, bits.min(32));

    // Write sum to dst (operand 0)
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, sum_ext, bits)?;

    // Write original dst to src (operand 1) — src is always a register
    write_gpr(bcx, gpr, dirty, instr.op_register(1), dst_val)?;

    // Set ADD flags
    *rflags = flags_add(bcx, *rflags, dst_val, src_val, sum_ext, bits.min(32));
    Ok(())
}

/// Lower CmpXchg (compare and exchange):
/// Compare dst with accumulator (AL/AX/EAX/RAX). If equal, dst = src, else accumulator = dst.
/// Sets ZF based on the comparison. Flushes pending flags before operation.
pub(super) fn lower_cmpxchg(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 0)?;
    let dst_raw = read_op_mem(bcx, instr, 0, gpr, *rflags, mem)?;
    let dst_val = mask_width(bcx, dst_raw, bits);
    let src_val = read_gpr(gpr, instr.op_register(1))?;
    let acc_val = mask_width(bcx, gpr[0], bits); // RAX/EAX/AX/AL

    // Compare dst with acc: ZF = (dst == acc)
    let eq = bcx.ins().icmp(IntCC::Equal, dst_val, acc_val);

    // Compute result: if equal, new_dst = src, else accumulator = dst
    let new_dst = bcx.ins().select(eq, src_val, dst_val);

    // Write to dst
    write_op_mem(bcx, instr, 0, gpr, dirty, *rflags, mem, new_dst, bits)?;

    // Write to accumulator (RAX) when not equal
    let old_rax = gpr[0];
    let ext_dst = sext_to_i64(bcx, dst_val, bits);
    let new_rax = bcx.ins().select(eq, old_rax, ext_dst);
    write_gpr(bcx, gpr, dirty, Register::RAX, new_rax)?;

    // Set ZF based on comparison
    let zf_on = select_flag(bcx, eq, Rflags::ZF);
    *rflags = replace_flag(bcx, *rflags, Rflags::ZF, zf_on);
    // Architectural: CF, OF, SF, AF, PF may be set based on the comparison but
    // Intel docs mark them as undefined for CmpXchg.
    Ok(())
}

/// Lower Bsr (bit scan reverse): scan src for most significant 1 bit.
/// If src == 0: ZF=1, dst undefined.
/// If src != 0: ZF=0, dst = index of most significant set bit.
pub(super) fn lower_bsr(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: &mut Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let bits = op_width_bits(instr, 1)?;
    let src_raw = read_op_mem(bcx, instr, 1, gpr, *rflags, mem)?;
    let src_val = mask_width(bcx, src_raw, bits);

    // Use Cranelift ctlz (count leading zeros) to find MSB position.
    // Bsr result = bit_width - 1 - ctlz(val) when val != 0
    let bit_width: u32 = if bits <= 32 { 32 } else { 64 };
    let src_ext = if bits < 64 {
        if bits <= 32 {
            let reduced = bcx.ins().ireduce(types::I32, src_val);
            bcx.ins().uextend(types::I64, reduced)
        } else {
            src_val
        }
    } else {
        src_val
    };

    let bw_val = iconst_u64(bcx, u64::from(bit_width.saturating_sub(1)));
    let clz = bcx.ins().clz(src_ext);
    let msb = bcx.ins().isub(bw_val, clz);

    // ZF = (src == 0)
    let zero_c = iconst_u64(bcx, 0);
    let is_zero = bcx.ins().icmp(IntCC::Equal, src_ext, zero_c);

    // Result: if zero, undefined (write 0); else write MSB index
    let result = bcx.ins().select(is_zero, zero_c, msb);
    write_gpr(bcx, gpr, dirty, instr.op_register(0), result)?;

    // Set ZF flag
    let zf_on = select_flag(bcx, is_zero, Rflags::ZF);
    *rflags = replace_flag(bcx, *rflags, Rflags::ZF, zf_on);
    Ok(())
}

pub(super) fn lower_xchg(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let k0 = instr.op0_kind();
    let k1 = instr.op1_kind();
    match (k0, k1) {
        (OpKind::Register, OpKind::Register) => {
            let r0 = instr.op_register(0);
            let r1 = instr.op_register(1);
            let v0 = read_gpr(gpr, r0)?;
            let v1 = read_gpr(gpr, r1)?;
            write_gpr(bcx, gpr, dirty, r0, v1)?;
            write_gpr(bcx, gpr, dirty, r1, v0)
        }
        (OpKind::Register, OpKind::Memory) | (OpKind::Memory, OpKind::Register) => {
            let reg = if k0 == OpKind::Register {
                instr.op_register(0)
            } else {
                instr.op_register(1)
            };
            let bits = reg_size_bits(reg);
            let width = match bits {
                8 => 1_u32,
                16 => 2,
                32 => 4,
                64 => 8,
                other => return Err(format!("xchg bits {other}")),
            };
            let addr = effective_addr(bcx, instr, gpr)?;
            let mem_v = call_load(bcx, mem, gpr, rflags, addr, width, instr.ip())?;
            let reg_v = read_gpr(gpr, reg)?;
            write_gpr(bcx, gpr, dirty, reg, mem_v)?;
            call_store(bcx, mem, gpr, rflags, addr, width, reg_v, instr.ip())
        }
        _ => Err("xchg form".into()),
    }
}

pub(super) fn write_op_mem(
    bcx: &mut FunctionBuilder<'_>,
    instr: &Instruction,
    op: u32,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
    val: Value,
    bits: u32,
) -> Result<(), String> {
    match instr.op_kind(op) {
        OpKind::Register => write_gpr(bcx, gpr, dirty, instr.op_register(op), val),
        OpKind::Memory => {
            let addr = effective_addr(bcx, instr, gpr)?;
            let width = match bits {
                8 => 1_u32,
                16 => 2,
                32 => 4,
                64 => 8,
                other => return Err(format!("bad store bits {other}")),
            };
            call_store(bcx, mem, gpr, rflags, addr, width, val, instr.ip())
        }
        _ => Err("write op form".into()),
    }
}
