//! Block emission: `emit_body_and_term`, the block-wide stack guard, and the
//! terminator helpers (chaining, shadow stack, self-loops, fast UCRT inline ops).

use super::analysis::{ensure_gprs_loaded, ensure_xmm_loaded};
use super::flags::iconst_u64;
use super::gpr::mark_dirty;
use super::insn::{PendingFlags, flush_pending, lower_insn};
use super::mem::call_load;
use super::string::lower_string;
use super::{
    EDGE_IC_SLOTS, MAX_CHAIN_DEPTH, OFF_CHAIN_DEPTH, OFF_EDGE_IC_FN, OFF_EDGE_IC_VA, OFF_FAULT,
    OFF_RIP, OFF_SHADOW_RET, OFF_SHADOW_SP, SHADOW_DEPTH, TLB_PROT_R, TLB_PROT_W, flag_cond,
    lower_term,
};

use super::super::block::{BlockStackPinPlan, BlockTerm, DecodedInsn, is_string_op};
use super::super::fast_api::{self, FastApiKind};

use ahash::HashMap;
use cranelift::codegen::ir::{BlockArg, FuncRef, SigRef};
use cranelift::prelude::*;
use cranelift_codegen::ir::MemFlagsData;
use iced_x86::Mnemonic;

pub(super) fn term_chain_targets(t: BlockTerm) -> Vec<u64> {
    match t {
        BlockTerm::Jmp { target } | BlockTerm::Call { target, .. } => vec![target],
        BlockTerm::Jcc {
            taken, not_taken, ..
        } => vec![taken, not_taken],
        BlockTerm::Ret => vec![],
    }
}

/// Write SSA GPRs back into `JitCtx`.
///
/// When `gpr_dirty` is `Some`, only dirty+loaded regs are stored (reg-mapping opt).
/// When `None`, every loaded reg is stored (safe default for host helpers / unknown).
// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn writeback_gprs(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    gpr: &[Value; 16],
    gpr_loaded: &[bool; 16],
    gpr_dirty: Option<&[bool; 16]>,
    rflags: Value,
    rflags_ptr: Value,
    store_flags: bool,
) {
    for i in 0..16 {
        let do_store = match gpr_dirty {
            Some(d) => d[i] && gpr_loaded[i],
            None => gpr_loaded[i],
        };
        if do_store {
            let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
            let p = bcx.ins().iadd_imm(ctx_ptr, off);
            bcx.ins().store(flags, gpr[i], p, 0);
        }
    }
    if store_flags {
        bcx.ins().store(flags, rflags, rflags_ptr, 0);
    }
}

/// Build block-param args for a self-loop header: live GPRs + optional rflags.
pub(super) fn loop_header_args(
    gpr: &[Value; 16],
    live: &[bool; 16],
    rflags: Value,
    pass_flags: bool,
) -> Vec<BlockArg> {
    let mut args = Vec::with_capacity(17);
    for i in 0..16 {
        if live[i] {
            args.push(BlockArg::Value(gpr[i]));
        }
    }
    if pass_flags {
        args.push(BlockArg::Value(rflags));
    }
    args
}

/// Push guest `return_ip` onto the software shadow return stack.
pub(super) fn shadow_push(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    return_ip: u64,
) {
    let sp_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_SHADOW_SP));
    let sp = bcx.ins().load(types::I64, flags, sp_ptr, 0);
    let mask = iconst_u64(bcx, (SHADOW_DEPTH as u64) - 1);
    let idx = bcx.ins().band(sp, mask);
    let base = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_SHADOW_RET));
    let three = iconst_u64(bcx, 3);
    let off = bcx.ins().ishl(idx, three); // * sizeof(u64)
    let slot = bcx.ins().iadd(base, off);
    let retv = iconst_u64(bcx, return_ip);
    bcx.ins().store(flags, retv, slot, 0);
    let sp1 = bcx.ins().iadd_imm(sp, 1);
    bcx.ins().store(flags, sp1, sp_ptr, 0);
}

/// On `ret`: if shadow top matches `ret_va`, pop; else clear shadow (mispredict / longjmp).
/// Returns `ret_va` unchanged (prediction only affects chaining likelihood via continuity).
pub(super) fn shadow_pop_check(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    ret_va: Value,
) -> Value {
    let sp_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_SHADOW_SP));
    let sp = bcx.ins().load(types::I64, flags, sp_ptr, 0);
    let zero = iconst_u64(bcx, 0);
    let has = bcx.ins().icmp(IntCC::NotEqual, sp, zero);
    let do_blk = bcx.create_block();
    let cont = bcx.create_block();
    bcx.append_block_param(cont, types::I64); // ret_va passthrough
    // Always continue with ret_va; side-effect is shadow maintenance.
    bcx.ins()
        .brif(has, do_blk, &[], cont, &[BlockArg::Value(ret_va)]);

    bcx.switch_to_block(do_blk);
    bcx.seal_block(do_blk);
    let sp1 = bcx.ins().iadd_imm(sp, -1);
    let mask = iconst_u64(bcx, (SHADOW_DEPTH as u64) - 1);
    let idx = bcx.ins().band(sp1, mask);
    let base = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_SHADOW_RET));
    let three = iconst_u64(bcx, 3);
    let off = bcx.ins().ishl(idx, three);
    let slot = bcx.ins().iadd(base, off);
    let predicted = bcx.ins().load(types::I64, flags, slot, 0);
    let ok = bcx.ins().icmp(IntCC::Equal, predicted, ret_va);
    // Match → commit pop; mismatch → clear entire shadow.
    let new_sp = bcx.ins().select(ok, sp1, zero);
    bcx.ins().store(flags, new_sp, sp_ptr, 0);
    bcx.ins().jump(cont, &[BlockArg::Value(ret_va)]);

    bcx.switch_to_block(cont);
    bcx.seal_block(cont);
    bcx.block_params(cont)[0]
}

/// Writeback + set RIP + call successor (direct or late-bound), then return.
///
/// Uses host C ABI `call`/`call_indirect` (not Tail/`return_call`) so blocks stay
/// callable from Rust as `extern "C"`. Nesting is capped by [`MAX_CHAIN_DEPTH`]:
/// past the limit we return to the Rust dispatcher with RIP already advanced.
// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_chain_or_exit(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    gpr: &[Value; 16],
    gpr_loaded: &[bool; 16],
    gpr_dirty: Option<&[bool; 16]>,
    rflags: Value,
    rflags_ptr: Value,
    store_flags: bool,
    exit: Block,
    exit_rip: Value,
    href: Option<FuncRef>,
    lookup_ref: FuncRef,
    block_sig_ref: SigRef,
) {
    let rip_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_RIP));
    bcx.ins().store(flags, exit_rip, rip_ptr, 0);
    // Chain to another block: successor reloads from JitCtx, so flush dirty SSA.
    writeback_gprs(
        bcx,
        ctx_ptr,
        flags,
        gpr,
        gpr_loaded,
        gpr_dirty,
        rflags,
        rflags_ptr,
        store_flags,
    );

    // Host-stack guard: each hop nests a C frame. Cap and re-enter from Rust.
    let depth_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_CHAIN_DEPTH));
    let depth = bcx.ins().load(types::I64, flags, depth_ptr, 0);
    let max_d = iconst_u64(bcx, MAX_CHAIN_DEPTH);
    let too_deep = bcx
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, depth, max_d);
    let deep_blk = bcx.create_block();
    let chain_blk = bcx.create_block();
    bcx.ins().brif(too_deep, deep_blk, &[], chain_blk, &[]);

    bcx.switch_to_block(deep_blk);
    bcx.seal_block(deep_blk);
    // RIP + GPRs already written; pop back to the dispatcher.
    bcx.ins().return_(&[]);

    bcx.switch_to_block(chain_blk);
    bcx.seal_block(chain_blk);
    let depth1 = bcx.ins().iadd_imm(depth, 1);
    bcx.ins().store(flags, depth1, depth_ptr, 0);

    if let Some(f) = href {
        bcx.ins().call(f, &[ctx_ptr]);
        bcx.ins().store(flags, depth, depth_ptr, 0);
        bcx.ins().return_(&[]);
        return;
    }
    // Monomorphic edge IC (data plane) before full chain-table helper.
    // On hit: call_indirect without `wie_jit_chain_lookup`. Miss → helper (which
    // also consults IC + table and may install a new IC entry).
    let mut ic_ok = bcx.ins().iconst(types::I8, 0);
    let zero = iconst_u64(bcx, 0);
    let mut ic_fn = zero;
    for slot in 0..EDGE_IC_SLOTS {
        let off_va = i64::from(OFF_EDGE_IC_VA) + i64::try_from(slot.saturating_mul(8)).unwrap_or(0);
        let off_fn = i64::from(OFF_EDGE_IC_FN) + i64::try_from(slot.saturating_mul(8)).unwrap_or(0);
        let va_p = bcx.ins().iadd_imm(ctx_ptr, off_va);
        let fn_p = bcx.ins().iadd_imm(ctx_ptr, off_fn);
        let slot_va = bcx.ins().load(types::I64, flags, va_p, 0);
        let slot_fn = bcx.ins().load(types::I64, flags, fn_p, 0);
        let va_ok = bcx.ins().icmp(IntCC::Equal, slot_va, exit_rip);
        let fn_nz = bcx.ins().icmp_imm(IntCC::NotEqual, slot_fn, 0);
        let hit_i = bcx.ins().band(va_ok, fn_nz);
        let first = bcx.ins().icmp_imm(IntCC::Equal, ic_ok, 0);
        let take = bcx.ins().band(hit_i, first);
        ic_fn = bcx.ins().select(take, slot_fn, ic_fn);
        ic_ok = bcx.ins().bor(ic_ok, hit_i);
    }
    let ic_hit_blk = bcx.create_block();
    let ic_miss_blk = bcx.create_block();
    bcx.ins().brif(ic_ok, ic_hit_blk, &[], ic_miss_blk, &[]);

    bcx.switch_to_block(ic_hit_blk);
    bcx.seal_block(ic_hit_blk);
    bcx.ins().call_indirect(block_sig_ref, ic_fn, &[ctx_ptr]);
    bcx.ins().store(flags, depth, depth_ptr, 0);
    bcx.ins().return_(&[]);

    bcx.switch_to_block(ic_miss_blk);
    bcx.seal_block(ic_miss_blk);
    // Late-bound: open-addressing chain table (successors compiled after us).
    let call = bcx.ins().call(lookup_ref, &[ctx_ptr, exit_rip]);
    let fn_ptr = bcx.inst_results(call)[0];
    let hit = bcx.ins().icmp_imm(IntCC::NotEqual, fn_ptr, 0);
    let hit_blk = bcx.create_block();
    let miss_blk = bcx.create_block();
    bcx.ins().brif(hit, hit_blk, &[], miss_blk, &[]);
    bcx.switch_to_block(hit_blk);
    bcx.seal_block(hit_blk);
    bcx.ins().call_indirect(block_sig_ref, fn_ptr, &[ctx_ptr]);
    bcx.ins().store(flags, depth, depth_ptr, 0);
    bcx.ins().return_(&[]);
    bcx.switch_to_block(miss_blk);
    bcx.seal_block(miss_blk);
    // No nested call — restore depth before the ordinary exit path.
    bcx.ins().store(flags, depth, depth_ptr, 0);
    jump_exit(bcx, exit, gpr, rflags);
}

/// Self-loop terminator: re-enter header via SSA block params (no JitCtx traffic),
/// or chain the non-loop edge with dirty writeback.
///
/// `loop_header` must **not** be the function entry block — Cranelift rejects
/// edges into entry (`remove_constant_phis` / `edge.block != entry_block`).
// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn lower_self_loop_term(
    bcx: &mut FunctionBuilder<'_>,
    term: BlockTerm,
    start_rip: u64,
    loop_header: Block,
    live: &[bool; 16],
    pass_flags: bool,
    ctx_ptr: Value,
    flags: MemFlagsData,
    gpr: &mut [Value; 16],
    gpr_loaded: &[bool; 16],
    gpr_dirty: &[bool; 16],
    rflags: Value,
    rflags_ptr: Value,
    _needs_flags: bool,
    exit: Block,
    chain_refs: &HashMap<u64, FuncRef>,
    lookup_ref: FuncRef,
    block_sig_ref: SigRef,
) -> Result<bool, String> {
    match term {
        BlockTerm::Jmp { target } if target == start_rip => {
            // Stay in native SSA — pass live regs as header params (no store/reload).
            let args = loop_header_args(gpr, live, rflags, pass_flags);
            bcx.ins().jump(loop_header, &args);
            Ok(true)
        }
        BlockTerm::Jcc {
            mnemonic,
            taken,
            not_taken,
        } => {
            let cond = flag_cond(bcx, rflags, mnemonic)?;
            let taken_blk = bcx.create_block();
            let not_blk = bcx.create_block();
            bcx.ins().brif(cond, taken_blk, &[], not_blk, &[]);

            for (blk, va) in [(taken_blk, taken), (not_blk, not_taken)] {
                bcx.switch_to_block(blk);
                bcx.seal_block(blk);
                if va == start_rip {
                    let args = loop_header_args(gpr, live, rflags, pass_flags);
                    bcx.ins().jump(loop_header, &args);
                } else {
                    let rv = iconst_u64(bcx, va);
                    emit_chain_or_exit(
                        bcx,
                        ctx_ptr,
                        flags,
                        gpr,
                        gpr_loaded,
                        Some(gpr_dirty),
                        rflags,
                        rflags_ptr,
                        true,
                        exit,
                        rv,
                        chain_refs.get(&va).copied(),
                        lookup_ref,
                        block_sig_ref,
                    );
                }
            }
            Ok(true)
        }
        _ => Err("self_loop term not jcc/jmp".into()),
    }
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn lower_jcc_chain(
    bcx: &mut FunctionBuilder<'_>,
    mnemonic: Mnemonic,
    taken: u64,
    not_taken: u64,
    t_ref: Option<FuncRef>,
    n_ref: Option<FuncRef>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    gpr: &[Value; 16],
    gpr_loaded: &[bool; 16],
    gpr_dirty: Option<&[bool; 16]>,
    rflags: Value,
    rflags_ptr: Value,
    needs_flags: bool,
    exit: Block,
    lookup_ref: FuncRef,
    block_sig_ref: SigRef,
) -> Result<bool, String> {
    let cond = flag_cond(bcx, rflags, mnemonic)?;
    let taken_blk = bcx.create_block();
    let not_blk = bcx.create_block();
    bcx.ins().brif(cond, taken_blk, &[], not_blk, &[]);
    for (blk, va, href) in [(taken_blk, taken, t_ref), (not_blk, not_taken, n_ref)] {
        bcx.switch_to_block(blk);
        bcx.seal_block(blk);
        let rv = iconst_u64(bcx, va);
        emit_chain_or_exit(
            bcx,
            ctx_ptr,
            flags,
            gpr,
            gpr_loaded,
            gpr_dirty,
            rflags,
            rflags_ptr,
            needs_flags,
            exit,
            rv,
            href,
            lookup_ref,
            block_sig_ref,
        );
    }
    Ok(true)
}

/// Emit direct host call for a fast UCRT import (P1 + P3 inlines).
pub(super) fn lower_fast_ucrt(
    bcx: &mut FunctionBuilder<'_>,
    kind: FastApiKind,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let rcx = gpr[1];
    let rdx = gpr[2];
    let r8 = gpr[8];
    let r9 = gpr[9];
    match kind {
        // P3: inline `__acrt_iob_func` (ix → FILE* cookie) without a host call.
        FastApiKind::AcrtIobFunc => {
            let zero = iconst_u64(bcx, 0);
            let one = iconst_u64(bcx, 1);
            let two = iconst_u64(bcx, 2);
            let mask = iconst_u64(bcx, 0xffff_ffff);
            let ix = bcx.ins().band(rcx, mask);
            let is0 = bcx.ins().icmp(IntCC::Equal, ix, zero);
            let is1 = bcx.ins().icmp(IntCC::Equal, ix, one);
            let is2 = bcx.ins().icmp(IntCC::Equal, ix, two);
            let f0 = iconst_u64(bcx, fast_api::file_cookie(0));
            let f1 = iconst_u64(bcx, fast_api::file_cookie(1));
            let f2 = iconst_u64(bcx, fast_api::file_cookie(2));
            // is0→f0, else is1→f1, else is2→f2, else 0
            let step2 = bcx.ins().select(is2, f2, zero);
            let step1 = bcx.ins().select(is1, f1, step2);
            gpr[0] = bcx.ins().select(is0, f0, step1);
            mark_dirty(dirty, 0);
            Ok(())
        }
        // P3: inline `strlen` as a byte-scan loop in IR.
        FastApiKind::Strlen => lower_inline_strlen(bcx, gpr, dirty, rflags, mem),
        FastApiKind::Malloc => {
            let fref = mem.ucrt_refs[kind as usize].ok_or("malloc import")?;
            let call = bcx.ins().call(fref, &[mem.ctx_ptr, rcx]);
            gpr[0] = bcx.inst_results(call)[0];
            mark_dirty(dirty, 0);
            check_fault_after_ucrt(bcx, mem, gpr, rflags);
            Ok(())
        }
        FastApiKind::Free => {
            let fref = mem.ucrt_refs[kind as usize].ok_or("free import")?;
            bcx.ins().call(fref, &[mem.ctx_ptr, rcx]);
            gpr[0] = iconst_u64(bcx, 0);
            mark_dirty(dirty, 0);
            check_fault_after_ucrt(bcx, mem, gpr, rflags);
            Ok(())
        }
        FastApiKind::Memcpy => {
            let fref = mem.ucrt_refs[kind as usize].ok_or("memcpy import")?;
            let call = bcx.ins().call(fref, &[mem.ctx_ptr, rcx, rdx, r8]);
            gpr[0] = bcx.inst_results(call)[0];
            mark_dirty(dirty, 0);
            check_fault_after_ucrt(bcx, mem, gpr, rflags);
            Ok(())
        }
        FastApiKind::Fwrite => {
            let fref = mem.ucrt_refs[kind as usize].ok_or("fwrite import")?;
            let call = bcx.ins().call(fref, &[mem.ctx_ptr, rcx, rdx, r8, r9]);
            gpr[0] = bcx.inst_results(call)[0];
            mark_dirty(dirty, 0);
            check_fault_after_ucrt(bcx, mem, gpr, rflags);
            Ok(())
        }
        FastApiKind::Fflush => {
            let fref = mem.ucrt_refs[kind as usize].ok_or("fflush import")?;
            let call = bcx.ins().call(fref, &[rcx]);
            gpr[0] = bcx.inst_results(call)[0];
            mark_dirty(dirty, 0);
            Ok(())
        }
    }
}

/// Inline `strlen`: byte loop with load helper until NUL.
pub(super) fn lower_inline_strlen(
    bcx: &mut FunctionBuilder<'_>,
    gpr: &mut [Value; 16],
    dirty: &mut [bool; 16],
    rflags: Value,
    mem: &mut MemEnv,
) -> Result<(), String> {
    let s = gpr[1];
    let zero = iconst_u64(bcx, 0);
    // s == 0 → 0
    let is_null = bcx.ins().icmp(IntCC::Equal, s, zero);
    let cont = bcx.create_block();
    let done = bcx.create_block();
    bcx.append_block_param(done, types::I64);
    let null_args = [BlockArg::Value(zero)];
    bcx.ins().brif(is_null, done, &null_args, cont, &[]);

    bcx.switch_to_block(cont);
    bcx.seal_block(cont);
    let header = bcx.create_block();
    bcx.append_block_param(header, types::I64); // ptr
    bcx.append_block_param(header, types::I64); // len
    let s_arg = [BlockArg::Value(s), BlockArg::Value(zero)];
    bcx.ins().jump(header, &s_arg);

    bcx.switch_to_block(header);
    let ptr = bcx.block_params(header)[0];
    let len = bcx.block_params(header)[1];
    let byte = call_load(bcx, mem, gpr, rflags, ptr, 1, 0)?;
    let is_nul = bcx.ins().icmp(IntCC::Equal, byte, zero);
    let next_ptr = bcx.ins().iadd_imm(ptr, 1);
    let next_len = bcx.ins().iadd_imm(len, 1);
    let body = bcx.create_block();
    let done_args = [BlockArg::Value(len)];
    bcx.ins().brif(is_nul, done, &done_args, body, &[]);
    bcx.switch_to_block(body);
    bcx.seal_block(body);
    let back = [BlockArg::Value(next_ptr), BlockArg::Value(next_len)];
    bcx.ins().jump(header, &back);
    bcx.seal_block(header);

    bcx.switch_to_block(done);
    bcx.seal_block(done);
    gpr[0] = bcx.block_params(done)[0];
    mark_dirty(dirty, 0);
    Ok(())
}

pub(super) fn check_fault_after_ucrt(
    bcx: &mut FunctionBuilder<'_>,
    mem: &MemEnv,
    gpr: &[Value; 16],
    rflags: Value,
) {
    let fault_ptr = bcx.ins().iadd_imm(mem.ctx_ptr, i64::from(OFF_FAULT));
    let fault = bcx.ins().load(types::I64, mem.flags, fault_ptr, 0);
    let is_fault = bcx.ins().icmp_imm(IntCC::NotEqual, fault, 0);
    let cont = bcx.create_block();
    let args = exit_args(gpr, rflags);
    bcx.ins().brif(is_fault, mem.exit, &args, cont, &[]);
    bcx.switch_to_block(cont);
    bcx.seal_block(cont);
}

/// Pin fields loaded once per block (loop-invariant for the `run_compiled` lifetime).
///
/// Perf: reloading `MemPin` from `JitCtx` on every guest load/store
/// doubled `long_loop` wall time. Hoist once; per-access only does bounds math.
#[derive(Clone, Copy)]
pub(super) struct HoistedPin {
    pub(super) guest_base: Value,
    pub(super) guest_end: Value,
    pub(super) host_base: Value,
    pub(super) allow: Value,
    /// `host_base != 0 && pin_gen == mem_gen` (stable for the block).
    pub(super) live: Value,
}

/// Block-wide stack pin super-fast path: one entry guard, then bare host memops.
///
/// `host = bias + guest_va` with `bias = host_base - guest_base`. Valid only after
/// the block-wide range guard has passed for the (invariant) stack base register.
#[derive(Clone, Copy)]
pub(super) struct SuperStack {
    /// `host_base.wrapping_sub(guest_base)`.
    pub(super) bias: Value,
}

pub(super) struct MemEnv {
    pub(super) ctx_ptr: Value,
    pub(super) load_ref: Option<cranelift::codegen::ir::FuncRef>,
    pub(super) store_ref: Option<cranelift::codegen::ir::FuncRef>,
    pub(super) string_ref: Option<cranelift::codegen::ir::FuncRef>,
    pub(super) host_span_ref: Option<cranelift::codegen::ir::FuncRef>,
    pub(super) f32_ref: Option<cranelift::codegen::ir::FuncRef>,
    pub(super) f64_ref: Option<cranelift::codegen::ir::FuncRef>,
    /// Flags for JitCtx accesses (gpr slots, rflags, TLB/sticky/pin state, fault, etc.).
    pub(super) flags: MemFlagsData,
    /// Flags for soft-translated guest memory accesses through pin bias / sticky ptr
    /// / super stack / host_span. Tagged with a distinct `AliasRegion` so JitCtx
    /// stores do not alias-clobber guest loads (and vice-versa), unblocking LICM/CSE
    /// on sticky metadata inside memop-dense loops.
    pub(super) guest_flags: MemFlagsData,
    pub(super) exit: Block,
    pub(super) ucrt_refs: [Option<FuncRef>; 7],
    /// Packed integer SSE2 lane op helper (`wie_sse_int_binop`).
    pub(super) sse_int_ref: Option<FuncRef>,
    /// Packed SSE2 shift helper (`wie_sse_shift`).
    pub(super) sse_shift_ref: Option<FuncRef>,
    /// `pshufb` result low/high half helpers.
    pub(super) sse_pshufb_lo_ref: Option<FuncRef>,
    pub(super) sse_pshufb_hi_ref: Option<FuncRef>,
    /// FP unary / min-max helpers.
    pub(super) sse_fp_unop_ref: Option<FuncRef>,
    pub(super) sse_fp_binop_ref: Option<FuncRef>,
    /// Integer↔FP convert helper.
    pub(super) sse_cvt_ref: Option<FuncRef>,
    /// Stack region pin (slot 0), hoisted at block entry when inline mem is on.
    pub(super) stack_pin: Option<HoistedPin>,
    /// Data pins (slots 1..): process heap + VirtualAlloc spans, after sticky.
    pub(super) data_pins: Vec<HoistedPin>,
    /// When set, load/store use `bias + addr` with **no** per-access bounds checks.
    pub(super) super_stack: Option<SuperStack>,
}

pub(super) fn jump_exit(
    bcx: &mut FunctionBuilder<'_>,
    exit: Block,
    gpr: &[Value; 16],
    rflags: Value,
) {
    let args = exit_args(gpr, rflags);
    bcx.ins().jump(exit, &args);
}

pub(super) fn exit_args(gpr: &[Value; 16], rflags: Value) -> [BlockArg; 17] {
    let mut args = [BlockArg::Value(gpr[0]); 17];
    for i in 0..16 {
        args[i] = BlockArg::Value(gpr[i]);
    }
    args[16] = BlockArg::Value(rflags);
    args
}

// The wide signature is a load-bearing JIT lowering helper carrying the whole lowering env.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_body_and_term(
    bcx: &mut FunctionBuilder<'_>,
    body: &[DecodedInsn],
    term: Option<BlockTerm>,
    term_insn: Option<&DecodedInsn>,
    call_fast: Option<FastApiKind>,
    start_rip: u64,
    end_rip: u64,
    self_loop: bool,
    loop_header: Block,
    live_eff: &[bool; 16],
    pass_flags: bool,
    needs_flags: bool,
    ctx_ptr: Value,
    flags: MemFlagsData,
    rflags_ptr: Value,
    exit: Block,
    chain_refs: &HashMap<u64, FuncRef>,
    lookup_ref: FuncRef,
    block_sig_ref: SigRef,
    gpr_vals: &mut [Value; 16],
    gpr_loaded: &mut [bool; 16],
    gpr_dirty: &mut [bool; 16],
    rflags_val: &mut Value,
    xmm_vals: &mut [Value; 32],
    xmm_loaded: &mut [bool; 16],
    mem_env: &mut MemEnv,
) -> Result<(), String> {
    let mut pending = PendingFlags::None;
    let mut string_exit_rip: Option<Value> = None;

    for d in body {
        ensure_gprs_loaded(bcx, ctx_ptr, gpr_vals, gpr_loaded, &d.instr, flags);
        ensure_xmm_loaded(bcx, ctx_ptr, flags, &d.instr, xmm_vals, xmm_loaded);
        if is_string_op(&d.instr) {
            flush_pending(bcx, rflags_val, &mut pending);
            string_exit_rip = Some(lower_string(
                bcx, &d.instr, gpr_vals, rflags_val, gpr_loaded, mem_env,
            )?);
        } else {
            lower_insn(
                bcx,
                &d.instr,
                gpr_vals,
                gpr_dirty,
                rflags_val,
                &mut pending,
                mem_env,
                xmm_vals,
            )?;
        }
    }

    if matches!(term, Some(BlockTerm::Jcc { .. }))
        || needs_flags
        || !matches!(pending, PendingFlags::None)
    {
        flush_pending(bcx, rflags_val, &mut pending);
    }

    if let Some(t) = term {
        if let Some(ti) = term_insn {
            ensure_gprs_loaded(bcx, ctx_ptr, gpr_vals, gpr_loaded, &ti.instr, flags);
        }
        if matches!(t, BlockTerm::Call { .. } | BlockTerm::Ret)
            && call_fast.is_none()
            && !gpr_loaded[4]
        {
            let p = bcx.ins().iadd_imm(ctx_ptr, 4 * 8);
            gpr_vals[4] = bcx.ins().load(types::I64, flags, p, 0);
            gpr_loaded[4] = true;
        }
        let term_ip = term_insn.map_or(0, |ti| ti.instr.ip());

        if let (Some(kind), BlockTerm::Call { return_ip, .. }) = (call_fast, t) {
            for idx in [1_usize, 2, 8, 9] {
                if !gpr_loaded[idx] {
                    let p = bcx
                        .ins()
                        .iadd_imm(ctx_ptr, i64::try_from(idx * 8).unwrap_or(0));
                    gpr_vals[idx] = bcx.ins().load(types::I64, flags, p, 0);
                    gpr_loaded[idx] = true;
                }
            }
            if !gpr_loaded[0] {
                let p = bcx.ins().iadd_imm(ctx_ptr, 0);
                gpr_vals[0] = bcx.ins().load(types::I64, flags, p, 0);
                gpr_loaded[0] = true;
            }
            lower_fast_ucrt(bcx, kind, gpr_vals, gpr_dirty, *rflags_val, mem_env)?;
            let exit_rip = iconst_u64(bcx, return_ip);
            emit_chain_or_exit(
                bcx,
                ctx_ptr,
                flags,
                gpr_vals,
                gpr_loaded,
                Some(gpr_dirty),
                *rflags_val,
                rflags_ptr,
                true,
                exit,
                exit_rip,
                chain_refs.get(&return_ip).copied(),
                lookup_ref,
                block_sig_ref,
            );
        } else if self_loop {
            let _ = lower_self_loop_term(
                bcx,
                t,
                start_rip,
                loop_header,
                live_eff,
                pass_flags,
                ctx_ptr,
                flags,
                gpr_vals,
                gpr_loaded,
                gpr_dirty,
                *rflags_val,
                rflags_ptr,
                needs_flags,
                exit,
                chain_refs,
                lookup_ref,
                block_sig_ref,
            )?;
        } else {
            if let BlockTerm::Call { return_ip, .. } = t {
                shadow_push(bcx, ctx_ptr, flags, return_ip);
            }
            let exit_rip = lower_term(bcx, t, gpr_vals, gpr_dirty, *rflags_val, mem_env, term_ip)?;
            let exit_rip = if matches!(t, BlockTerm::Ret) {
                shadow_pop_check(bcx, ctx_ptr, flags, exit_rip)
            } else {
                exit_rip
            };
            match t {
                BlockTerm::Jcc {
                    mnemonic,
                    taken,
                    not_taken,
                } => {
                    let t_ref = chain_refs.get(&taken).copied();
                    let n_ref = chain_refs.get(&not_taken).copied();
                    let _ = lower_jcc_chain(
                        bcx,
                        mnemonic,
                        taken,
                        not_taken,
                        t_ref,
                        n_ref,
                        ctx_ptr,
                        flags,
                        gpr_vals,
                        gpr_loaded,
                        Some(gpr_dirty),
                        *rflags_val,
                        rflags_ptr,
                        needs_flags,
                        exit,
                        lookup_ref,
                        block_sig_ref,
                    )?;
                }
                BlockTerm::Jmp { target } | BlockTerm::Call { target, .. } => {
                    emit_chain_or_exit(
                        bcx,
                        ctx_ptr,
                        flags,
                        gpr_vals,
                        gpr_loaded,
                        Some(gpr_dirty),
                        *rflags_val,
                        rflags_ptr,
                        needs_flags,
                        exit,
                        exit_rip,
                        chain_refs.get(&target).copied(),
                        lookup_ref,
                        block_sig_ref,
                    );
                }
                BlockTerm::Ret => {
                    emit_chain_or_exit(
                        bcx,
                        ctx_ptr,
                        flags,
                        gpr_vals,
                        gpr_loaded,
                        Some(gpr_dirty),
                        *rflags_val,
                        rflags_ptr,
                        needs_flags,
                        exit,
                        exit_rip,
                        None,
                        lookup_ref,
                        block_sig_ref,
                    );
                }
            }
        }
    } else if let Some(sr) = string_exit_rip {
        emit_chain_or_exit(
            bcx,
            ctx_ptr,
            flags,
            gpr_vals,
            gpr_loaded,
            Some(gpr_dirty),
            *rflags_val,
            rflags_ptr,
            needs_flags,
            exit,
            sr,
            None,
            lookup_ref,
            block_sig_ref,
        );
    } else {
        let exit_rip = iconst_u64(bcx, end_rip);
        emit_chain_or_exit(
            bcx,
            ctx_ptr,
            flags,
            gpr_vals,
            gpr_loaded,
            Some(gpr_dirty),
            *rflags_val,
            rflags_ptr,
            needs_flags,
            exit,
            exit_rip,
            chain_refs.get(&end_rip).copied(),
            lookup_ref,
            block_sig_ref,
        );
    }
    Ok(())
}

/// Single prologue guard: `[base+min_disp, base+max_end)` ⊆ pin and rights match.
pub(super) fn emit_block_wide_stack_guard(
    bcx: &mut FunctionBuilder<'_>,
    pin: &HoistedPin,
    base: Value,
    plan: &BlockStackPinPlan,
) -> Value {
    let min_d = iconst_u64(bcx, u64::from_ne_bytes(plan.min_disp.to_ne_bytes()));
    let max_e = iconst_u64(bcx, u64::from_ne_bytes(plan.max_end.to_ne_bytes()));
    let lo = bcx.ins().iadd(base, min_d);
    let hi = bcx.ins().iadd(base, max_e);
    // Span must not wrap the address space.
    let no_wrap = bcx.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, hi, lo);
    let lo_ok = bcx
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, lo, pin.guest_base);
    let hi_ok = bcx
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, hi, pin.guest_end);

    let mut need = 0_u64;
    if plan.needs_r {
        need |= TLB_PROT_R;
    }
    if plan.needs_w {
        need |= TLB_PROT_W;
    }
    let need_v = iconst_u64(bcx, need);
    let prot_bits = bcx.ins().band(pin.allow, need_v);
    let prot_ok = if need == 0 {
        bcx.ins().iconst(types::I8, 1)
    } else {
        bcx.ins().icmp(IntCC::Equal, prot_bits, need_v)
    };

    let ok1 = bcx.ins().band(pin.live, no_wrap);
    let ok2 = bcx.ins().band(ok1, lo_ok);
    let ok3 = bcx.ins().band(ok2, hi_ok);
    bcx.ins().band(ok3, prot_ok)
}
