//! Guest-memory access: host-callable `wie_jit_*` helpers (registered as JIT
//! symbols) and the IR-emitting load/store/pin helpers used by block emission.

use super::emit::{HoistedPin, MemEnv, check_fault_after_ucrt};
use super::flags::iconst_u64;
use super::gpr::sticky_tlb_probe;
use super::tlb::{set_fault, tlb_page_ptr};
use super::{
    CHAIN_SLOTS, EDGE_IC_SLOTS, JitCtx, OFF_MEM_GEN, OFF_PINS, PIN_STRIDE, TLB_PROT_R, TLB_PROT_W,
    chain_hash,
};

use crate::exec::{self, StringOpKind};
use crate::jit::config::JitConfig;
use crate::regs::{RegFile, Rflags};
use cranelift::codegen::ir::{BlockArg, FuncRef};
use cranelift::prelude::*;
use cranelift_codegen::ir::MemFlagsData;

// --- Host mem helpers (registered as JIT symbols) ---

/// Soft-translate a contiguous guest span to a host pointer (0 on failure).
///
/// Used by inline string fast-path; never returns a guest VA.
pub(crate) unsafe extern "C" fn wie_jit_host_span(
    ctx: *mut JitCtx,
    guest_va: u64,
    len: u64,
    write: u64,
) -> u64 {
    if ctx.is_null() || len == 0 {
        return 0;
    }
    // SAFETY: live JitCtx for the block.
    let ctx = unsafe { &mut *ctx };
    if ctx.fault != 0 {
        return 0;
    }
    let len_usize = usize::try_from(len).unwrap_or(0);
    if len_usize == 0 {
        return 0;
    }
    // SAFETY: mem pointer set by run_compiled.
    let mem = unsafe { &*ctx.mem };
    match mem.host_span(guest_va, len_usize, write != 0) {
        Some(p) if !p.is_null() => p as u64,
        _ => 0,
    }
}

/// Lookup host block pointer for `va` (0 = miss). Used for late-bound chaining.
///
/// Checks monomorphic edge IC first, then the open-addressing chain
/// table. On a table hit, installs into an edge-IC slot for the next transfer.
///
/// `extern "C" fn(ctx, va) -> fn_ptr`
pub(crate) unsafe extern "C" fn wie_jit_chain_lookup(ctx: *mut JitCtx, va: u64) -> u64 {
    if va == 0 || ctx.is_null() {
        return 0;
    }
    // SAFETY: `run_compiled` sets chain_* to live tables for the block duration.
    let ctx = unsafe { &mut *ctx };
    // Edge IC (data plane): monomorphic last-hit successors.
    for i in 0..EDGE_IC_SLOTS {
        if ctx.edge_ic_va[i] == va {
            let f = ctx.edge_ic_fn[i];
            if f != 0 {
                return f;
            }
        }
    }
    if ctx.chain_slots.is_null() {
        return 0;
    }
    // SAFETY: `chain_slots` points to a live [ChainSlot; CHAIN_SLOTS] array for
    // the duration of this call (set by `run_compiled`).
    let slots = unsafe { std::slice::from_raw_parts(ctx.chain_slots, CHAIN_SLOTS) };
    let mut i = chain_hash(va);
    // Bounded probe; empty slot ends search.
    for _ in 0..16 {
        let s = slots[i];
        if s.va == va {
            if s.fn_ptr != 0 {
                // Install monomorphic edge IC (RR victim).
                let slot =
                    usize::try_from(ctx.edge_ic_rr % u64::try_from(EDGE_IC_SLOTS).unwrap_or(4))
                        .unwrap_or(0);
                ctx.edge_ic_va[slot] = va;
                ctx.edge_ic_fn[slot] = s.fn_ptr;
                ctx.edge_ic_rr = ctx.edge_ic_rr.wrapping_add(1);
            }
            return s.fn_ptr;
        }
        if s.va == 0 {
            return 0;
        }
        i = (i + 1) & (CHAIN_SLOTS - 1);
    }
    0
}

/// `extern "C" fn(ctx, addr, size, insn_ip) -> value`
pub(crate) unsafe extern "C" fn wie_jit_load(
    ctx: *mut JitCtx,
    addr: u64,
    size: u64,
    insn_ip: u64,
) -> u64 {
    // SAFETY: caller passes a live `JitCtx` for the duration of the block.
    let ctx = unsafe { &mut *ctx };
    ctx.load_calls = ctx.load_calls.saturating_add(1);

    if ctx.fault != 0 {
        return 0;
    }
    let size_usize = usize::try_from(size).unwrap_or(0);
    if size_usize == 0 || size_usize > 8 {
        set_fault(ctx, insn_ip, addr, size, 0);
        return 0;
    }
    // Fast path: single-page TLB with SPC R bit + generation.
    // SAFETY: TLB pointer is a live page from guest map for this block.
    if let Some(p) = unsafe { tlb_page_ptr(ctx, addr, size_usize, false) } {
        let mut buf = [0_u8; 8];
        // SAFETY: `p` points into a mapped page with `size_usize` bytes in range.
        unsafe {
            std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), size_usize);
        }
        return match size_usize {
            1 => u64::from(buf[0]),
            2 => u64::from(u16::from_le_bytes([buf[0], buf[1]])),
            4 => u64::from(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])),
            8 => u64::from_le_bytes(buf),
            _ => 0,
        };
    }
    // Slow path: multi-page, miss, or SPC deny on TLB.
    // SAFETY: `mem` set by `run_compiled`.
    let mem = unsafe { &*ctx.mem };
    let mut buf = [0_u8; 8];
    if mem.read(addr, &mut buf[..size_usize]).is_err() {
        set_fault(ctx, insn_ip, addr, size, 0);
        return 0;
    }
    match size_usize {
        1 => u64::from(buf[0]),
        2 => u64::from(u16::from_le_bytes([buf[0], buf[1]])),
        4 => u64::from(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])),
        8 => u64::from_le_bytes(buf),
        _ => 0,
    }
}

/// `extern "C" fn(ctx, addr, size, value, insn_ip)`
pub(crate) unsafe extern "C" fn wie_jit_store(
    ctx: *mut JitCtx,
    addr: u64,
    size: u64,
    value: u64,
    insn_ip: u64,
) {
    // SAFETY: caller passes a live `JitCtx` for the duration of the block.
    let ctx = unsafe { &mut *ctx };
    ctx.store_calls = ctx.store_calls.saturating_add(1);
    if ctx.fault != 0 {
        return;
    }
    let size_usize = usize::try_from(size).unwrap_or(0);
    if size_usize == 0 || size_usize > 8 {
        set_fault(ctx, insn_ip, addr, size, 1);
        return;
    }
    let bytes = value.to_le_bytes();
    // SAFETY: TLB pointer is a live page from guest map; W bit checked.
    if let Some(p) = unsafe { tlb_page_ptr(ctx, addr, size_usize, true) } {
        // SAFETY: `p` points into a mapped page with `size_usize` bytes in range.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, size_usize);
        }
        return;
    }
    // SAFETY: `mem` set by `run_compiled`.
    let mem = unsafe { &*ctx.mem };
    if mem.write(addr, &bytes[..size_usize]).is_err() {
        set_fault(ctx, insn_ip, addr, size, 1);
    }
}

/// Bulk string helper: `(ctx, op, size, flags, insn_ip) -> stay`.
///
/// `op`: 0=stos 1=movs 2=lods 3=scas 4=cmps
/// `flags`: bit0=rep, bit1=repe, bit2=repne
/// Returns 1 if RIP should stay on `insn_ip`, 0 to fall through (caller uses next_ip).
pub(crate) unsafe extern "C" fn wie_jit_string(
    ctx: *mut JitCtx,
    op: u64,
    size: u64,
    flags: u64,
    insn_ip: u64,
) -> u64 {
    // SAFETY: live JitCtx for the block.
    let ctx = unsafe { &mut *ctx };
    if ctx.fault != 0 {
        return 0;
    }
    let size_usize = usize::try_from(size).unwrap_or(0);
    if !matches!(size_usize, 1 | 2 | 4 | 8) {
        set_fault(ctx, insn_ip, 0, size, 0);
        return 0;
    }
    let Ok(kind) = StringOpKind::try_from(op) else {
        set_fault(ctx, insn_ip, 0, size, 0);
        return 0;
    };
    let rep = exec::RepPrefix::from_abi(flags);

    let mut regs = RegFile::new();
    for i in 0..16 {
        regs.set_gpr(i, ctx.gpr[i]);
    }
    regs.set_rflags_checked(Rflags::from(ctx.rflags));
    regs.rip = insn_ip;

    // SAFETY: mem pointer set by run_compiled.
    let mem = unsafe { &*ctx.mem };
    match exec::run_string_op(mem, &mut regs, kind, size_usize, rep) {
        Ok(stay) => {
            for i in 0..16 {
                ctx.gpr[i] = regs.gpr(i);
            }
            ctx.rflags = u64::from(regs.rflags);
            u64::from(stay)
        }
        Err(exec::StepExecError::InvalidMemory(inv)) => {
            for i in 0..16 {
                ctx.gpr[i] = regs.gpr(i);
            }
            ctx.rflags = u64::from(regs.rflags);
            set_fault(
                ctx,
                insn_ip,
                inv.address,
                u64::try_from(inv.size).unwrap_or(0),
                u64::try_from(inv.access_type.as_i32()).unwrap_or(0),
            );
            0
        }
        Err(exec::StepExecError::Cpu(_)) => {
            set_fault(ctx, insn_ip, 0, size, 0);
            0
        }
    }
}

/// Load one pin slot into SSA once (block entry). Fields are invariant until
/// the next `run_compiled` (protect/free bumps gen and refills pins).
pub(super) fn hoist_pin_slot(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    slot: usize,
) -> HoistedPin {
    let base_off = i64::from(OFF_PINS) + i64::from(PIN_STRIDE) * i64::try_from(slot).unwrap_or(0);
    let gb_p = bcx.ins().iadd_imm(ctx_ptr, base_off);
    let ge_p = bcx.ins().iadd_imm(ctx_ptr, base_off + 8);
    let hb_p = bcx.ins().iadd_imm(ctx_ptr, base_off + 16);
    let gen_p = bcx.ins().iadd_imm(ctx_ptr, base_off + 24);
    let allow_p = bcx.ins().iadd_imm(ctx_ptr, base_off + 32);
    let mem_gen_p = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_MEM_GEN));

    let guest_base = bcx.ins().load(types::I64, flags, gb_p, 0);
    let guest_end = bcx.ins().load(types::I64, flags, ge_p, 0);
    let host_base = bcx.ins().load(types::I64, flags, hb_p, 0);
    let pin_gen = bcx.ins().load(types::I64, flags, gen_p, 0);
    let allow = bcx.ins().load(types::I64, flags, allow_p, 0);
    let mem_gen = bcx.ins().load(types::I64, flags, mem_gen_p, 0);

    let base_nz = bcx.ins().icmp_imm(IntCC::NotEqual, host_base, 0);
    let gen_ok = bcx.ins().icmp(IntCC::Equal, pin_gen, mem_gen);
    let live = bcx.ins().band(base_nz, gen_ok);
    HoistedPin {
        guest_base,
        guest_end,
        host_base,
        allow,
        live,
    }
}

/// Bounds + R/W probe against a **hoisted** pin (no JitCtx reloads).
///
/// Soft translate: `host = host_base + (addr - guest_base)`.
pub(super) fn hoisted_pin_probe(
    bcx: &mut FunctionBuilder<'_>,
    pin: &HoistedPin,
    addr: Value,
    size: u32,
    write: bool,
) -> (Value, Value) {
    let size_v = iconst_u64(bcx, u64::from(size));
    let end = bcx.ins().iadd(addr, size_v);
    let no_wrap = bcx.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, end, addr);
    let lo_ok = bcx
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, addr, pin.guest_base);
    let hi_ok = bcx
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, end, pin.guest_end);
    let need = if write { TLB_PROT_W } else { TLB_PROT_R };
    let need_v = iconst_u64(bcx, need);
    let prot_bits = bcx.ins().band(pin.allow, need_v);
    let prot_ok = bcx.ins().icmp_imm(IntCC::NotEqual, prot_bits, 0);

    let ok1 = bcx.ins().band(pin.live, no_wrap);
    let ok2 = bcx.ins().band(ok1, lo_ok);
    let ok3 = bcx.ins().band(ok2, hi_ok);
    let ok = bcx.ins().band(ok3, prot_ok);

    let rel = bcx.ins().isub(addr, pin.guest_base);
    let host = bcx.ins().iadd(pin.host_base, rel);
    (ok, host)
}

/// Zero-extend a loaded integer of `size` bytes to i64.
///
/// `flags` should be the caller's guest-data alias-tagged flags (`mem.guest_flags`)
/// so this load doesn't alias JitCtx metadata for Cranelift's alias analysis.
pub(super) fn load_guest_bytes(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    host: Value,
    size: u32,
) -> Value {
    match size {
        1 => {
            let v = bcx.ins().load(types::I8, flags, host, 0);
            bcx.ins().uextend(types::I64, v)
        }
        2 => {
            let v = bcx.ins().load(types::I16, flags, host, 0);
            bcx.ins().uextend(types::I64, v)
        }
        4 => {
            let v = bcx.ins().load(types::I32, flags, host, 0);
            bcx.ins().uextend(types::I64, v)
        }
        _ => bcx.ins().load(types::I64, flags, host, 0),
    }
}

pub(super) fn store_guest_bytes(
    bcx: &mut FunctionBuilder<'_>,
    flags: MemFlagsData,
    host: Value,
    size: u32,
    value: Value,
) {
    match size {
        1 => {
            let v = bcx.ins().ireduce(types::I8, value);
            bcx.ins().store(flags, v, host, 0);
        }
        2 => {
            let v = bcx.ins().ireduce(types::I16, value);
            bcx.ins().store(flags, v, host, 0);
        }
        4 => {
            let v = bcx.ins().ireduce(types::I32, value);
            bcx.ins().store(flags, v, host, 0);
        }
        _ => {
            bcx.ins().store(flags, value, host, 0);
        }
    }
}

pub(super) fn call_load(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    size: u32,
    insn_ip: u64,
) -> Result<Value, String> {
    let load_ref = mem.load_ref.ok_or("load helper missing")?;
    // `WIE_JIT_MEM=slow`: helper only (oracle / bisect).
    if !JitConfig::get().mem_inline_enabled() {
        return Ok(emit_load_helper(
            bcx, mem, gpr, rflags, addr, size, insn_ip, load_ref,
        ));
    }

    // Block-wide super-fast path: entry guard already proved the whole access
    // range sits in the stack pin — emit a bare host load (no bounds IR).
    if let Some(super_s) = mem.super_stack {
        let host = bcx.ins().iadd(super_s.bias, addr);
        return Ok(load_guest_bytes(bcx, mem.guest_flags, host, size));
    }

    // CFG-ordered probes (not `select`).
    // Order: stack → sticky → largest data pin → helper.
    // Sticky first keeps single-page streams free of pin-bounds IR; the data
    // pin catches multi-page thrash inside VirtualAlloc / heap after sticky miss.
    let merge = bcx.create_block();
    bcx.append_block_param(merge, types::I64);

    if let Some(ref pin) = mem.stack_pin {
        let (ok, host) = hoisted_pin_probe(bcx, pin, addr, size, false);
        emit_probe_stage(bcx, ok, host, merge, |bcx, host| {
            Some(load_guest_bytes(bcx, mem.guest_flags, host, size))
        });
    }

    {
        let (ok, host) = sticky_tlb_probe(bcx, mem, addr, size, false);
        emit_probe_stage(bcx, ok, host, merge, |bcx, host| {
            Some(load_guest_bytes(bcx, mem.guest_flags, host, size))
        });
    }

    for pin in &mem.data_pins {
        let (ok, host) = hoisted_pin_probe(bcx, pin, addr, size, false);
        emit_probe_stage(bcx, ok, host, merge, |bcx, host| {
            Some(load_guest_bytes(bcx, mem.guest_flags, host, size))
        });
    }

    let slow_val = emit_load_helper(bcx, mem, gpr, rflags, addr, size, insn_ip, load_ref);
    bcx.ins().jump(merge, &[BlockArg::Value(slow_val)]);

    bcx.switch_to_block(merge);
    bcx.seal_block(merge);
    Ok(bcx.block_params(merge)[0])
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
/// Emit one CFG-ordered probe stage of the load/store miss chain.
///
/// Branches on `ok` to a fresh `hit` block (which runs `hit_effect` and jumps
/// to `merge`, threading its optional value as a merge arg) and a fresh `miss`
/// block; leaves the builder positioned at the sealed `miss` block so the next
/// stage (or the slow-path helper) continues there. Monomorphized closures
/// keep this compile-path-only — emitted IR is identical per call site.
fn emit_probe_stage(
    bcx: &mut FunctionBuilder<'_>,
    ok: Value,
    host: Value,
    merge: Block,
    hit_effect: impl FnOnce(&mut FunctionBuilder<'_>, Value) -> Option<Value>,
) {
    let hit = bcx.create_block();
    let miss = bcx.create_block();
    bcx.ins().brif(ok, hit, &[], miss, &[]);
    bcx.switch_to_block(hit);
    bcx.seal_block(hit);
    match hit_effect(bcx, host) {
        Some(v) => bcx.ins().jump(merge, &[BlockArg::Value(v)]),
        None => bcx.ins().jump(merge, &[]),
    };
    bcx.switch_to_block(miss);
    bcx.seal_block(miss);
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_load_helper(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    size: u32,
    insn_ip: u64,
    load_ref: FuncRef,
) -> Value {
    let size_v = bcx.ins().iconst(types::I64, i64::from(size));
    let ip_v = iconst_u64(bcx, insn_ip);
    let call = bcx.ins().call(load_ref, &[mem.ctx_ptr, addr, size_v, ip_v]);
    let slow_val = bcx.inst_results(call)[0];
    check_fault_after_ucrt(bcx, mem, gpr, rflags);
    slow_val
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn call_store(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    size: u32,
    value: Value,
    insn_ip: u64,
) -> Result<(), String> {
    let store_ref = mem.store_ref.ok_or("store helper missing")?;
    if !JitConfig::get().mem_inline_enabled() {
        emit_store_helper(bcx, mem, gpr, rflags, addr, size, value, insn_ip, store_ref);
        return Ok(());
    }

    // Block-wide super-fast path (see `call_load`).
    if let Some(super_s) = mem.super_stack {
        let host = bcx.ins().iadd(super_s.bias, addr);
        store_guest_bytes(bcx, mem.guest_flags, host, size, value);
        return Ok(());
    }

    // Same CFG order as `call_load`: stack → sticky → data pin → helper.
    let merge = bcx.create_block();

    if let Some(ref pin) = mem.stack_pin {
        let (ok, host) = hoisted_pin_probe(bcx, pin, addr, size, true);
        emit_probe_stage(bcx, ok, host, merge, |bcx, host| {
            store_guest_bytes(bcx, mem.guest_flags, host, size, value);
            None
        });
    }

    {
        let (ok, host) = sticky_tlb_probe(bcx, mem, addr, size, true);
        emit_probe_stage(bcx, ok, host, merge, |bcx, host| {
            store_guest_bytes(bcx, mem.guest_flags, host, size, value);
            None
        });
    }

    for pin in &mem.data_pins {
        let (ok, host) = hoisted_pin_probe(bcx, pin, addr, size, true);
        emit_probe_stage(bcx, ok, host, merge, |bcx, host| {
            store_guest_bytes(bcx, mem.guest_flags, host, size, value);
            None
        });
    }

    emit_store_helper(bcx, mem, gpr, rflags, addr, size, value, insn_ip, store_ref);
    bcx.ins().jump(merge, &[]);

    bcx.switch_to_block(merge);
    bcx.seal_block(merge);
    Ok(())
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_store_helper(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
    addr: Value,
    size: u32,
    value: Value,
    insn_ip: u64,
    store_ref: FuncRef,
) {
    let size_v = bcx.ins().iconst(types::I64, i64::from(size));
    let ip_v = iconst_u64(bcx, insn_ip);
    bcx.ins()
        .call(store_ref, &[mem.ctx_ptr, addr, size_v, value, ip_v]);
    check_fault_after_ucrt(bcx, mem, gpr, rflags);
}
