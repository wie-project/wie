//! Block emission: `emit_body_and_term`, the block-wide stack guard, and the
//! terminator helpers (chaining, self-loops, fast UCRT inline ops).

use super::analysis::{ensure_gprs_loaded, ensure_xmm_loaded, entry_decode_mask};
use super::flags::{FlagState, decompose_bits, iconst_u64, pack_once, packed_word, resync_state};
use super::gpr::mark_dirty;
use super::insn::{
    PendingFlags, flush_pending, flush_pending_deferred, flush_pending_impl, lower_insn,
};
use super::mem::call_load;
use super::string::lower_string;
use super::{
    CHAIN_OFFSET_MASK, CHAIN_SLOT_BYTES, CHAIN_SLOT_BYTES_SHIFT, EDGE_IC_SLOTS, GOLDEN_RATIO_64,
    MAX_CHAIN_DEPTH, OFF_CHAIN_DEPTH, OFF_CHAIN_HOPS, OFF_CHAIN_SLOTS, OFF_CHAIN_STORE_OPS,
    OFF_EDGE_IC_FN, OFF_EDGE_IC_VA, OFF_FAULT, OFF_INSN_ACC, OFF_INV_GEN_BAKED, OFF_INV_GEN_PTR,
    OFF_RIP, TLB_PROT_R, TLB_PROT_W, chain_slot_offset, chain_slot_offset_next, flag_cond,
    flag_cond_fs, lower_term,
};

use super::super::block::{BlockStackPinPlan, BlockTerm, DecodedInsn};
use super::super::fast_api::{self, FastApiKind};
use super::err::{LowerError, noted};
use super::opclass::is_string_op;

use ahash::HashMap;
use cranelift::codegen::ir::{BlockArg, FuncRef, InstructionData, SigRef, ValueDef};
use cranelift::prelude::*;
use cranelift_codegen::ir::MemFlagsData;
use iced_x86::Mnemonic;

pub(super) fn term_chain_targets(t: BlockTerm) -> Vec<u64> {
    match t {
        BlockTerm::Jmp { target } | BlockTerm::Call { target, .. } => vec![target],
        BlockTerm::Jcc {
            taken, not_taken, ..
        } => vec![taken, not_taken],
        // No chain edge: the target is a runtime value.
        BlockTerm::CallDyn { .. } | BlockTerm::Ret => vec![],
    }
}

/// Loop-carried count of guest instructions **dynamically** retired by one
/// block (plus, through [`Self::flush`], by every block chained behind it).
///
/// `JitStats::exec.jit_insns` used to charge a compiled block its *static*
/// decoded length once per block **entry**, so a self-looping block
/// undercounted by its trip factor — `micro-exes/long_loop` reported
/// `total=25` for ~3x10^8 retired instructions. This counter is the fix: the
/// block seeds it with its static length, adds the same length on every back
/// edge, and the shared exit block folds the result into
/// [`super::JitCtx::insn_acc`], which `run_compiled` charges.
///
/// Why it is cheap enough for the hot path:
///
/// - Only a **self-loop** pays inside the loop, and it pays exactly one
///   register `add` per trip. The value is a Cranelift *variable*, so the SSA
///   builder threads it through the loop header as a block parameter and the
///   register allocator keeps it in a register — no load/store per iteration
///   (the same reason the self-loop path already passes live GPRs and flags as
///   SSA values instead of round-tripping `JitCtx`).
/// - Non-loop blocks emit no per-instruction work at all: the seeded constant
///   is folded into the exit block's single load-add-store, next to the 16 GPR
///   stores that block already performs.
///
/// Exactness: a self-loop block is straight-line from its entry to its
/// terminator (the decoder stops at the first terminator), so every body
/// instruction retires on every trip and `trips * insn_count` is the exact
/// dynamic count. `insn_count` includes the terminator.
#[derive(Clone, Copy)]
pub(super) struct TripCounter {
    var: Variable,
    step: i64,
}

impl TripCounter {
    /// Seed the counter with the block's static length (call in the entry
    /// block, before any other block is entered).
    pub(super) fn declare(bcx: &mut FunctionBuilder<'_>, insn_count: u32) -> Self {
        let step = i64::from(insn_count);
        let var = bcx.declare_var(types::I64);
        let seed = bcx.ins().iconst(types::I64, step);
        bcx.def_var(var, seed);
        Self { var, step }
    }

    /// Charge one more trip. Emitted **only** on a self-loop back edge.
    pub(super) fn bump(&self, bcx: &mut FunctionBuilder<'_>) {
        let cur = bcx.use_var(self.var);
        let next = bcx.ins().iadd_imm(cur, self.step);
        bcx.def_var(self.var, next);
    }

    /// Fold this block's count into the run-wide accumulator. Call once, in
    /// the shared exit block, *before* sealing it (`use_var` needs the block
    /// unsealed so the SSA builder can add the join parameter).
    ///
    /// Every other departure from the function must go through
    /// [`emit_block_exit_return`], which calls this on the way out.
    pub(super) fn flush(&self, bcx: &mut FunctionBuilder<'_>, ctx_ptr: Value, flags: MemFlagsData) {
        let acc_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_INSN_ACC));
        let prev = bcx.ins().load(types::I64, flags, acc_ptr, 0);
        let mine = bcx.use_var(self.var);
        let total = bcx.ins().iadd(prev, mine);
        bcx.ins().store(flags, total, acc_ptr, 0);
    }
}

/// Leave the compiled block: fold this block's retired count into
/// [`super::JitCtx::insn_acc`], then return to the host caller.
///
/// **This is the only correct way to end a Cranelift block from `emit.rs`.**
/// A block has several exits — the shared `exit` block (which folds the count
/// itself, then stores the exit GPRs), the chain-depth cap, the three
/// post-transfer returns, and the invalidation-guard's stale arm — and all of
/// them must charge the work this block actually retired. Reaching for
/// `bcx.ins().return_(&[])` directly compiles and runs, but silently drops the
/// count, because `TripCounter`'s total lives in an SSA variable that only
/// `flush` reads.
///
/// The bug this exists to prevent: `flush` used to be emitted *only* in the
/// shared `exit` block, so every block that transferred to a successor lost its
/// count. Only the last block of each native chain was charged, which made
/// `exec.jit_insns` report single digits for chain-heavy code (`insn_per_entry`
/// 7.1 on a 7-Zip archive run) and made the profile useless for judging
/// chain work.
///
/// Exactly one of these executes per block invocation, and exactly one
/// `flush` runs per invocation, so the fold stays non-duplicating: the other
/// exits (`exit`, and `miss_blk`'s jump into it) must NOT also flush.
fn emit_block_exit_return(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    trip: &TripCounter,
) {
    trip.flush(bcx, ctx_ptr, flags);
    bcx.ins().return_(&[]);
}

/// The single source of truth for *which* GPRs `writeback_gprs` stores.
///
/// `gpr_dirty = Some(d)` stores every body-written reg (`d[i]`) **and** every
/// reg that was loaded at entry (`gpr_loaded[i]`, which covers read-only
/// live-ins and string-op counter/pointer writes not tracked in the dirty set);
/// `None` (trampoline-style full flush) stores every loaded reg. Every caller
/// of the predicate — [`writeback_gprs`] and [`chain_store_count`] — must go
/// through this function so the emitted counter can never drift from the code.
#[inline]
fn gpr_store_predicate(i: usize, gpr_loaded: &[bool; 16], gpr_dirty: Option<&[bool; 16]>) -> bool {
    match gpr_dirty {
        Some(d) => d[i] || gpr_loaded[i],
        None => gpr_loaded[i],
    }
}

/// Number of GPR slots [`writeback_gprs`] stores for this edge — i.e. the
/// `chain_store_ops` charge added by [`emit_chain_hop_accounting`].
///
/// Computed with the same predicate the emitter uses (see
/// [`gpr_store_predicate`]), so the counter is exact rather than estimated.
/// Excludes the `rflags` slot, which is not a GPR.
pub(super) fn chain_store_count(gpr_loaded: &[bool; 16], gpr_dirty: Option<&[bool; 16]>) -> u64 {
    (0..16).fold(0_u64, |n, i| {
        if gpr_store_predicate(i, gpr_loaded, gpr_dirty) {
            n.saturating_add(1)
        } else {
            n
        }
    })
}

/// Charge one taken chain edge to the run-wide chain counters.
///
/// Emitted on each of `emit_chain_or_exit`'s three successor-call sites (direct
/// `FuncRef`, edge-IC hit, chain-table hit) — never on the depth-cap exit or
/// the table-miss exit, so `chain_hops` counts native transfers and not block
/// terminations. `store_ops` is the compile-time GPR-store count of *this*
/// block's edge ([`chain_store_count`]), so this is two load-add-stores, not a
/// runtime loop.
fn emit_chain_hop_accounting(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    store_ops: u64,
) {
    let hops_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_CHAIN_HOPS));
    let hops = bcx.ins().load(types::I64, flags, hops_ptr, 0);
    let hops1 = bcx.ins().iadd_imm(hops, 1);
    bcx.ins().store(flags, hops1, hops_ptr, 0);
    let ops_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_CHAIN_STORE_OPS));
    let ops = bcx.ins().load(types::I64, flags, ops_ptr, 0);
    let ops_n = bcx.ins().iadd_imm(ops, store_ops.cast_signed());
    bcx.ins().store(flags, ops_n, ops_ptr, 0);
}

/// Write SSA GPRs back into `JitCtx`.
///
/// When `gpr_dirty` is `Some`, every body-written (dirty) reg is stored —
/// whether or not it was loaded at entry. A reg written without a prior read
/// (e.g. `mov rcx,r12` setting a call argument) is not a live-in, so
/// `gpr_loaded[i]` is false; gating the store on `dirty && loaded` silently
/// dropped such writes from the JitCtx and the next chained block (the callee)
/// reloaded a stale value. Loaded regs are stored too (idempotent for
/// read-only live-ins, and covers string-op counter/pointer writes that are
/// not tracked in `gpr_dirty`). When `None`, every loaded reg is stored.
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
    for (i, value) in gpr.iter().enumerate() {
        if gpr_store_predicate(i, gpr_loaded, gpr_dirty) {
            let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
            let p = bcx.ins().iadd_imm(ctx_ptr, off);
            bcx.ins().store(flags, *value, p, 0);
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

/// Emit the cross-thread code-invalidation guard for a chained edge.
///
/// Loads `JitShared::invalidate_gen` through `ctx.inv_gen_ptr` (a sequential
/// atomic load — on ARM64 an acquire `ldar`, and `other_side_effects` keeps
/// Cranelift from hoisting or CSE-ing it out of hot backedge loops) and
/// compares it against `JitCtx::inv_gen_baked`, read out of the frame's
/// context rather than baked in as an immediate.
///
/// **The bake is the frame's, not the block's.** `ctx.inv_gen_baked` is stamped
/// by the dispatcher from `meta.inv_gen` on entry to `run_compiled`, and
/// `trampolines::chain_tail` already compares against that same value, so this
/// guard now matches the Rust-side twin exactly. The guard is therefore
/// *coarser* than a per-block bake: a block chained to from an entry compiled
/// before an unrelated invalidation sees a stale frame bake and exits even
/// though its own bytes are untouched. That direction is the safe one —
/// `invalidate_gen` is monotonic, so a stale bake can only ever cost an
/// **extra exit to the dispatcher**, never a wrong answer — and it is what
/// makes a restored blob chainable at all: with a per-block literal baked in,
/// a blob written by an earlier process carried the wrong value forever and
/// bailed at every edge.
///
/// **Invariant: the emitted code carries no per-process value.** Every runtime
/// input reaches the guard through `JitCtx` (a relocation-free struct the host
/// fills per dispatch) or through a re-checked relocation, so a blob's
/// behaviour after restore depends only on this process's state. This is what
/// [`crate::jit::engine::code_cache`] relies on to replay a blob without
/// re-deriving anything from the capturing process.
///
/// On mismatch the `on_stale` closure flushes whatever state the caller still
/// holds in SSA (registers / RIP), then control returns to the Rust
/// dispatcher exactly like the depth-exceeded exit. The next dispatch sees
/// the bumped generation, purges its chain table, and re-decodes fresh guest
/// bytes.
///
/// Returns the continuation block (current position switched onto it) that
/// execution takes when no invalidation was observed.
fn emit_inv_gen_check(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    trip: &TripCounter,
    on_stale: impl FnOnce(&mut FunctionBuilder<'_>),
) -> Block {
    let ptr_slot = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_INV_GEN_PTR));
    let gen_ptr = bcx.ins().load(types::I64, flags, ptr_slot, 0);
    let cur_gen = bcx.ins().atomic_load(types::I64, flags, gen_ptr);
    let baked_slot = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_INV_GEN_BAKED));
    let baked = bcx.ins().load(types::I64, flags, baked_slot, 0);
    let stale = bcx.ins().icmp(IntCC::NotEqual, cur_gen, baked);
    let stale_blk = bcx.create_block();
    let cont_blk = bcx.create_block();
    bcx.ins().brif(stale, stale_blk, &[], cont_blk, &[]);

    bcx.switch_to_block(stale_blk);
    bcx.seal_block(stale_blk);
    on_stale(bcx);
    // RIP + GPRs flushed by `on_stale` (or already written by the caller):
    // pop back to the dispatcher like the depth-exceeded path.
    emit_block_exit_return(bcx, ctx_ptr, flags, trip);

    bcx.switch_to_block(cont_blk);
    bcx.seal_block(cont_blk);
    cont_blk
}

/// The guest VA a chain edge transfers to, **if it is a compile-time
/// constant**.
///
/// `emit_chain_or_exit` receives the exit RIP as an SSA value and nothing else,
/// because the terminator lowering that produces it is shared by static edges
/// (`jmp` / `jcc` / direct `call` — `lower_term` emits `iconst_u64(target)`)
/// and dynamic ones (`ret`, `call r/m`, a string op's
/// `select(stay, cur, next)`). The inline chain-table probe bakes the target's
/// hash as an immediate, and that is only possible for the static shape, so this
/// reads the immediate back out of the value definition.
///
/// **Why this cannot cost correctness:** the probe still compares the slot's
/// full 64-bit `va` against the runtime `exit_rip` before using the fn pointer
/// (see tier 2 in [`emit_chain_or_exit`]). A wrong or missing constant therefore
/// degrades to "probe misses, host helper resolves it" — the same path a dynamic
/// edge takes — never to a wrong successor.
fn static_rip(bcx: &FunctionBuilder<'_>, v: Value) -> Option<u64> {
    let ValueDef::Result(inst, 0) = bcx.func.dfg.value_def(v) else {
        return None;
    };
    match bcx.func.dfg.insts[inst] {
        InstructionData::UnaryImm { imm, .. } => Some(imm.bits().cast_unsigned()),
        _ => None,
    }
}

/// Writeback + set RIP + chain to the successor (nested host call).
///
/// Blocks remain callable from Rust through the host C ABI. Past
/// `MAX_CHAIN_DEPTH`, the emitter returns to the Rust dispatcher with RIP
/// already advanced. Every edge keeps the invalidation-generation guard.
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
    inv_guard: bool,
    trip: &TripCounter,
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

    if inv_guard {
        // Cross-thread invalidation guard before touching any chain
        // machinery: RIP (= exit_rip) + GPRs are already flushed above, so a
        // mismatch just returns to the dispatcher for purge + re-decode.
        let _ = emit_inv_gen_check(bcx, ctx_ptr, flags, trip, |_| {});
    }

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
    emit_block_exit_return(bcx, ctx_ptr, flags, trip);

    bcx.switch_to_block(chain_blk);
    bcx.seal_block(chain_blk);
    let depth1 = bcx.ins().iadd_imm(depth, 1);
    bcx.ins().store(flags, depth1, depth_ptr, 0);

    // Past the depth cap, so every branch below really does transfer control to
    // a compiled successor: charge the hop + this edge's GPR stores.
    let store_ops = chain_store_count(gpr_loaded, gpr_dirty);

    // Successor resolution, from cheapest to last resort. Tier 0 is a direct
    // `call`; tiers 1–2 are inline probes that transfer with a plain
    // `call_indirect` from emitted code; the Rust helper
    // (`wie_jit_chain_lookup`) is reached only when neither inline tier
    // resolves the target.
    //
    // Tier 0 — a direct `call` to a successor compiled into this very module
    // (same tier, earlier compile). No lookup at all.
    if let Some(f) = href {
        emit_chain_hop_accounting(bcx, ctx_ptr, flags, store_ops);
        bcx.ins().call(f, &[ctx_ptr]);
        bcx.ins().store(flags, depth, depth_ptr, 0);
        // The callee wrote back the authoritative GPRs and charged its own
        // count into `insn_acc`; only THIS block's count is still outstanding.
        emit_block_exit_return(bcx, ctx_ptr, flags, trip);
        return;
    }

    // One shared transfer block: every tier below that resolves a fn pointer
    // passes it here as a block parameter, so the hop accounting, the depth
    // restore, the trip flush and the indirect call are emitted ONCE per chain
    // site instead of once per tier (this used to be three near-identical
    // copies).
    let hit_blk = bcx.create_block();
    bcx.append_block_param(hit_blk, types::I64);

    // Tier 1 — monomorphic edge IC (`JitCtx::edge_ic_{va,fn}`), one branch per
    // slot. A hit in slot 0 costs two loads, two compares, one `band` and a
    // branch, and the first match wins structurally, so no per-slot
    // `icmp/band/select/bor` bookkeeping is emitted on the *hit* path.
    for slot in 0..EDGE_IC_SLOTS {
        let step = i64::try_from(slot.saturating_mul(8)).unwrap_or(0);
        let va_p = bcx
            .ins()
            .iadd_imm(ctx_ptr, i64::from(OFF_EDGE_IC_VA) + step);
        let fn_p = bcx
            .ins()
            .iadd_imm(ctx_ptr, i64::from(OFF_EDGE_IC_FN) + step);
        let slot_va = bcx.ins().load(types::I64, flags, va_p, 0);
        let slot_fn = bcx.ins().load(types::I64, flags, fn_p, 0);
        let va_ok = bcx.ins().icmp(IntCC::Equal, slot_va, exit_rip);
        let fn_nz = bcx.ins().icmp_imm(IntCC::NotEqual, slot_fn, 0);
        let hit = bcx.ins().band(va_ok, fn_nz);
        let after = bcx.create_block();
        bcx.ins()
            .brif(hit, hit_blk, &[BlockArg::Value(slot_fn)], after, &[]);
        bcx.switch_to_block(after);
        bcx.seal_block(after);
    }

    // Tier 2 — the chain table's first two probe slots, inline.
    //
    // For a static chain target (every `jmp`/`jcc`/direct-`call` edge)
    // `static_rip` recovers the VA, so `chain_slot_offset` has already done the
    // hashing on this thread: the probe is `load chain_slots`, one address add,
    // two loads, two compares, and the offsets are baked immediates. A dynamic
    // target (`ret` / `call r/m` / a string op) folds the same arithmetic into
    // the emitted code instead: golden-ratio multiply, shift, mask — exactly
    // `chain_hash`, evaluated per hop.
    //
    // Either way the two slots probed are the two the host linear probe would
    // visit first, so a target inserted into its home slot — or displaced by one
    // earlier occupant — resolves without leaving emitted code. That case used
    // to pay a host call into `wie_jit_chain_lookup` on every hop.
    let slots_p = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_CHAIN_SLOTS));
    let slots_base = bcx.ins().load(types::I64, flags, slots_p, 0);
    let slot_addrs: [Value; 2] = match static_rip(bcx, exit_rip) {
        Some(va) => [
            bcx.ins().iadd_imm(slots_base, chain_slot_offset(va)),
            bcx.ins().iadd_imm(slots_base, chain_slot_offset_next(va)),
        ],
        None => {
            let golden = iconst_u64(bcx, GOLDEN_RATIO_64);
            let h = bcx.ins().imul(exit_rip, golden);
            // `chain_hash` keeps the product's low 9 bits; the slot stride folds
            // in as a left shift whose overflow the mask discards.
            let off0 = bcx.ins().ishl_imm(h, i64::from(CHAIN_SLOT_BYTES_SHIFT));
            let off0 = bcx.ins().band_imm(off0, CHAIN_OFFSET_MASK);
            let off1 = bcx.ins().iadd_imm(off0, CHAIN_SLOT_BYTES);
            let off1 = bcx.ins().band_imm(off1, CHAIN_OFFSET_MASK);
            [
                bcx.ins().iadd(slots_base, off0),
                bcx.ins().iadd(slots_base, off1),
            ]
        }
    };
    for slot_p in slot_addrs {
        let slot_va = bcx.ins().load(types::I64, flags, slot_p, 0);
        let slot_fn = bcx.ins().load(types::I64, flags, slot_p, 8);
        let va_ok = bcx.ins().icmp(IntCC::Equal, slot_va, exit_rip);
        let fn_nz = bcx.ins().icmp_imm(IntCC::NotEqual, slot_fn, 0);
        let hit = bcx.ins().band(va_ok, fn_nz);
        let after = bcx.create_block();
        bcx.ins()
            .brif(hit, hit_blk, &[BlockArg::Value(slot_fn)], after, &[]);
        bcx.switch_to_block(after);
        bcx.seal_block(after);
    }

    // Tier 3 — late-bound: open-addressing chain table (successors compiled
    // after us), including the rest of the linear probe and the edge-IC
    // install.
    let call = bcx.ins().call(lookup_ref, &[ctx_ptr, exit_rip]);
    let fn_ptr = bcx.inst_results(call)[0];
    let hit = bcx.ins().icmp_imm(IntCC::NotEqual, fn_ptr, 0);
    let miss_blk = bcx.create_block();
    bcx.ins()
        .brif(hit, hit_blk, &[BlockArg::Value(fn_ptr)], miss_blk, &[]);

    bcx.switch_to_block(hit_blk);
    bcx.seal_block(hit_blk);
    let target = bcx.block_params(hit_blk)[0];
    emit_chain_hop_accounting(bcx, ctx_ptr, flags, store_ops);
    bcx.ins().call_indirect(block_sig_ref, target, &[ctx_ptr]);
    bcx.ins().store(flags, depth, depth_ptr, 0);
    // The callee wrote back the authoritative GPRs and charged its own
    // count into `insn_acc`; only THIS block's count is still outstanding.
    emit_block_exit_return(bcx, ctx_ptr, flags, trip);

    bcx.switch_to_block(miss_blk);
    bcx.seal_block(miss_blk);
    // No successor resolved: no nested call, so restore depth before the
    // ordinary exit path and let the dispatcher (or iced) take it from RIP.
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
    flag_state: Option<&FlagState>,
    rflags_ptr: Value,
    _needs_flags: bool,
    exit: Block,
    chain_refs: &HashMap<u64, FuncRef>,
    lookup_ref: FuncRef,
    block_sig_ref: SigRef,
    inv_guard: bool,
    trip: TripCounter,
) -> Result<bool, LowerError> {
    match term {
        BlockTerm::Jmp { target } if target == start_rip => {
            // Stay in native SSA — pass live regs as header params (no store/reload).
            //
            // # What the back edge costs, measured — and what it is NOT
            //
            // On the scaled blit `0x1800d8fb0` (54 % of DOOM Retro's guest time)
            // the back edge's emitted tail is **35 of the iteration's 239 host
            // instructions**, and 27 of those 35 are `[sp, …]` slot traffic: 13
            // `ldur` before the invalidation guard and 14 `stur` after it. The 13
            // live-in params that produce it are the guest registers homed at
            // `#0x08, #0x10, #0x18, #0x40, #0x70, #0x78, #0x80, #0x88, #0x90,
            // #0xa8, #0xb0, #0xb8, #0xc0, #0xc8`; the guard they straddle is
            // `ldr inv_gen_ptr` / `ldar` / `ldr inv_gen_baked` / `cmp` / `b.ne`.
            //
            // Those pairs are the backend **materialising the header's phi
            // params through stack slots**, because the 13 values cannot be
            // coalesced across the acquire load. They are live writes, not a
            // redundant writeback: every one of those slots is read by off-path
            // blocks of the same unit (e.g. `#0x70` at `…7e8` and `…9ec`, `#0x90`
            // at `…7c8`, `#0xa8` at `…7ac`), so a store that skipped them would
            // hand a stale guest register to a probe fallback.
            //
            // In particular this is **not** [`writeback_gprs`] and **not**
            // [`gpr_store_predicate`]: that path stores to `ctx_ptr + 8*i`, and
            // this block's tail contains exactly two ctx-relative instructions,
            // both loads (the guard). Its one `writeback_gprs` call sits inside
            // the `on_stale` closure below, so it is cold. Changing the store
            // predicate removes nothing here — measured, by trying to attribute
            // 26 "cancel pairs" to it and finding the address spaces differ.
            //
            // The lever is therefore the loop-carried set and its coalescing, not
            // the writeback: fewer live-in params, or passing the slot's value
            // directly instead of a `load` of the slot it is stored back to.
            // Either is a semantic change to block-parameter passing and needs
            // its own slice; pessimism here costs 2 instructions per register,
            // optimism costs a guest register.
            //
            // Numbers from `/tmp/blk_afeager2.txt` (unit 785, iteration path 239),
            // analysis in `docs/architecture/scaled-blit-block-attribution.md`.
            let args = loop_header_args(gpr, live, rflags, pass_flags);
            // Charge this trip before jumping back: the block body retires
            // `insn_count` instructions again on the next iteration.
            trip.bump(bcx);
            if inv_guard {
                // Guarded backedge: on generation mismatch, flush SSA state
                // (regs + RIP = start_rip) and return to the dispatcher; the
                // next dispatch re-decodes the loop body from current bytes.
                let _ = emit_inv_gen_check(bcx, ctx_ptr, flags, &trip, |bcx| {
                    // Cold boundary: the dispatcher re-dispatches from
                    // `ctx.rflags`, so this is one of the places the word must
                    // exist. The backedge itself does not build it.
                    let rflags = packed_word(bcx, flag_state, rflags);
                    writeback_gprs(
                        bcx,
                        ctx_ptr,
                        flags,
                        gpr,
                        gpr_loaded,
                        Some(gpr_dirty),
                        rflags,
                        rflags_ptr,
                        true,
                    );
                    let rip_val = iconst_u64(bcx, start_rip);
                    let rip_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_RIP));
                    bcx.ins().store(flags, rip_val, rip_ptr, 0);
                });
            }
            bcx.ins().jump(loop_header, &args);
            Ok(true)
        }
        BlockTerm::Jcc {
            mnemonic,
            taken,
            not_taken,
        } => {
            let cond = match flag_state {
                Some(fs) => flag_cond_fs(bcx, fs, mnemonic)?,
                None => flag_cond(bcx, rflags, mnemonic)?,
            };
            let taken_blk = bcx.create_block();
            let not_blk = bcx.create_block();
            bcx.ins().brif(cond, taken_blk, &[], not_blk, &[]);

            for (blk, va) in [(taken_blk, taken), (not_blk, not_taken)] {
                bcx.switch_to_block(blk);
                bcx.seal_block(blk);
                if va == start_rip {
                    let args = loop_header_args(gpr, live, rflags, pass_flags);
                    trip.bump(bcx);
                    if inv_guard {
                        // Same guarded backedge for jcc edges that re-enter
                        // this block.
                        let _ = emit_inv_gen_check(bcx, ctx_ptr, flags, &trip, |bcx| {
                            // Same cold boundary as the plain backedge above.
                            let rflags = packed_word(bcx, flag_state, rflags);
                            writeback_gprs(
                                bcx,
                                ctx_ptr,
                                flags,
                                gpr,
                                gpr_loaded,
                                Some(gpr_dirty),
                                rflags,
                                rflags_ptr,
                                true,
                            );
                            let rip_val = iconst_u64(bcx, start_rip);
                            let rip_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_RIP));
                            bcx.ins().store(flags, rip_val, rip_ptr, 0);
                        });
                    }
                    bcx.ins().jump(loop_header, &args);
                } else {
                    // Loop-exit edge: the flags leave the compiled code here.
                    let rflags = packed_word(bcx, flag_state, rflags);
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
                        inv_guard,
                        &trip,
                    );
                }
            }
            Ok(true)
        }
        _ => Err(LowerError::form("self_loop_terminator")),
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
    flag_state: Option<&FlagState>,
    rflags_ptr: Value,
    needs_flags: bool,
    exit: Block,
    lookup_ref: FuncRef,
    block_sig_ref: SigRef,
    inv_guard: bool,
    trip: &TripCounter,
) -> Result<bool, LowerError> {
    let cond = match flag_state {
        Some(fs) => flag_cond_fs(bcx, fs, mnemonic)?,
        None => flag_cond(bcx, rflags, mnemonic)?,
    };
    let taken_blk = bcx.create_block();
    let not_blk = bcx.create_block();
    bcx.ins().brif(cond, taken_blk, &[], not_blk, &[]);
    for (blk, va, href) in [(taken_blk, taken, t_ref), (not_blk, not_taken, n_ref)] {
        bcx.switch_to_block(blk);
        bcx.seal_block(blk);
        // The word is built *on* the edge, not in front of the branch: an edge
        // out of the block is where the flags leave the compiled code, so it is
        // also the only place the carrier has to exist.
        let rflags = packed_word(bcx, flag_state, rflags);
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
            inv_guard,
            trip,
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
) -> Result<(), LowerError> {
    // No opt-out needed here: `emit_body_and_term` records the terminator's
    // snapshot (`fault_flags` after the body flush) before this arm runs, so a
    // fault inside the inline fast-API import packs exactly the state `pack_once`
    // already handed to the call. Measured: removing an explicit clear here
    // changes no test and no word, because the two agree by construction.
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
            let fref = mem.ucrt_refs[kind as usize]
                .ok_or_else(|| LowerError::verifier("internal: malloc import not declared"))?;
            let call = bcx.ins().call(fref, &[mem.ctx_ptr, rcx]);
            gpr[0] = bcx.inst_results(call)[0];
            mark_dirty(dirty, 0);
            check_fault_after_ucrt(bcx, mem, gpr, rflags);
            Ok(())
        }
        FastApiKind::Free => {
            let fref = mem.ucrt_refs[kind as usize]
                .ok_or_else(|| LowerError::verifier("internal: free import not declared"))?;
            bcx.ins().call(fref, &[mem.ctx_ptr, rcx]);
            gpr[0] = iconst_u64(bcx, 0);
            mark_dirty(dirty, 0);
            check_fault_after_ucrt(bcx, mem, gpr, rflags);
            Ok(())
        }
        FastApiKind::Memcpy => {
            let fref = mem.ucrt_refs[kind as usize]
                .ok_or_else(|| LowerError::verifier("internal: memcpy import not declared"))?;
            let call = bcx.ins().call(fref, &[mem.ctx_ptr, rcx, rdx, r8]);
            gpr[0] = bcx.inst_results(call)[0];
            mark_dirty(dirty, 0);
            check_fault_after_ucrt(bcx, mem, gpr, rflags);
            Ok(())
        }
        FastApiKind::Fwrite => {
            let fref = mem.ucrt_refs[kind as usize]
                .ok_or_else(|| LowerError::verifier("internal: fwrite import not declared"))?;
            let call = bcx.ins().call(fref, &[mem.ctx_ptr, rcx, rdx, r8, r9]);
            gpr[0] = bcx.inst_results(call)[0];
            mark_dirty(dirty, 0);
            check_fault_after_ucrt(bcx, mem, gpr, rflags);
            Ok(())
        }
        FastApiKind::Fflush => {
            let fref = mem.ucrt_refs[kind as usize]
                .ok_or_else(|| LowerError::verifier("internal: fflush import not declared"))?;
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
) -> Result<(), LowerError> {
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
    let fault_blk = bcx.create_block();
    bcx.ins().brif(is_fault, fault_blk, &[], cont, &[]);

    // The word the guest observes at a fault is built *here*, on the edge that
    // leaves, and nowhere else: `fault_flags`/`fault_pending` describe the
    // pre-instruction architectural state, so the same CLIF predicates the hot
    // path would have materialised are available in this block, dominating it.
    bcx.switch_to_block(fault_blk);
    bcx.seal_block(fault_blk);
    let word = fault_word(bcx, mem, rflags);
    jump_exit(bcx, mem.exit, gpr, word);

    bcx.switch_to_block(cont);
    bcx.seal_block(cont);
}

/// The RFLAGS word to hand out at a fault edge.
///
/// `rflags` is the carrier the caller holds. When `mem.fault_flags` is `Some`,
/// the caller's carrier may predate a flag writer that is still pending, so the
/// pending flags are applied to a *copy* of the state and the result is packed —
/// all of it in the fault block, so nothing is paid on the path that takes the
/// access. `None` means the carrier is already authoritative (no SSA state, or a
/// caller that materialised it itself).
fn fault_word(bcx: &mut FunctionBuilder<'_>, mem: &MemEnv, rflags: Value) -> Value {
    let Some(fs) = mem.fault_flags else {
        return rflags;
    };
    let mut state = Some(fs);
    let mut pending = mem.fault_pending;
    let mut carrier = rflags;
    flush_pending_impl(bcx, &mut carrier, &mut pending, &mut state, false);
    packed_word(bcx, state.as_ref(), carrier)
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

/// Block-invariant inputs of the direct-mapped refill probe, hoisted at entry.
///
/// Exactly [`HoistedPin`]'s trade: two `iadd_imm`s and one load at block entry
/// instead of on every memory operand. `mem_gen` is invariant for the block for
/// the same reason [`HoistedPin::live`] is — the generation only moves on a
/// guest map/protect, which returns through the dispatcher.
#[derive(Clone)]
pub(super) struct RefillProbe {
    /// `ctx_ptr + OFF_REFILL` — base of the [`RefillSlot`](super::RefillSlot) array.
    pub(super) table: Value,
    /// `ctx.mem_gen`, the generation half of the slot tag.
    pub(super) mem_gen: Value,
    /// Hoisted per-operand translation homes, published at entry by
    /// [`mem::block_homes`](super::mem::block_homes).
    ///
    /// They live here, rather than on [`MemEnv`] beside `stack_pin`, because this
    /// descriptor already **is** the "this block has an inline memory probe"
    /// flag: it is `None` under `WIE_JIT_MEM=slow`, on the stack super path (which
    /// emits bare host memops and must carry no guard), in region mode, and in a
    /// block with no memory operand. Tying the homes to it makes the gating
    /// structural instead of a convention four construction sites have to
    /// remember — with no descriptor there is no guard, and the emitted code is
    /// byte-for-byte today's.
    ///
    /// Empty is also "no guard": a block that is not a self-loop never publishes
    /// one (it runs once, so a prologue span cannot be amortised), and a miss
    /// always falls through to the probe chain, never into a hole.
    pub(super) homes: Vec<super::mem::HomeEntry>,
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
    pub(super) div64_ref: Option<FuncRef>,
    /// Stack region pin (slot 0), hoisted at block entry when inline mem is on.
    pub(super) stack_pin: Option<HoistedPin>,
    /// Data pins (slots 1..): process heap + VirtualAlloc spans, after the refill
    /// probe (only with `WIE_JIT_MEM=pin`).
    pub(super) data_pins: Vec<HoistedPin>,
    /// Direct-mapped refill table + generation, hoisted at block entry.
    ///
    /// `None` means "no inline probe in this block": either inline mem is off
    /// (`WIE_JIT_MEM=slow`, helper-only by construction) or the block takes the
    /// stack super path, which never probes. A `None` here is therefore a
    /// fallthrough to the helper, never a hole.
    pub(super) refill: Option<RefillProbe>,
    /// When set, load/store use `bias + addr` with **no** per-access bounds checks.
    pub(super) super_stack: Option<SuperStack>,
    /// The architectural flags to hand the guest if **this** instruction's
    /// memory access faults: the SSA state as it stood before the instruction.
    ///
    /// A fault observes EFLAGS, and it observes the *pre-instruction* state (a
    /// faulting instruction does not retire), so this is recorded by
    /// `lower_insn` at the top of every instruction and applied + packed only in
    /// [`check_fault_after_ucrt`]'s fault block. That is what keeps a block's
    /// hot path free of the pack for a flag writer that nothing in the block
    /// reads: the word is built where it is observed, on the cold edge, instead
    /// of before every probe guard.
    ///
    /// `None` means no SSA state is live (the carrier in `rflags` is already the
    /// word, as on the interpreter-shaped path), and the paths that hand
    /// `check_fault_after_ucrt` a carrier they materialised themselves — the
    /// string helpers, which reload the word the helper wrote — clear this first.
    pub(super) fault_flags: Option<FlagState>,
    /// The flag writer that has been recorded in `fault_flags` but not yet
    /// applied to it (`PendingFlags`), so the fault edge can apply exactly what
    /// the flush points would have applied.
    pub(super) fault_pending: PendingFlags,
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
    inv_guard: bool,
    gpr_vals: &mut [Value; 16],
    gpr_loaded: &mut [bool; 16],
    gpr_dirty: &mut [bool; 16],
    rflags_val: &mut Value,
    xmm_vals: &mut [Value; 32],
    xmm_loaded: &mut [bool; 16],
    mem_env: &mut MemEnv,
    trip: TripCounter,
) -> Result<(), LowerError> {
    let mut pending = PendingFlags::None;
    let mut string_exit_rip: Option<Value> = None;
    let ssa_flags = super::super::config::JitConfig::get().ssa_flags_enabled();
    // The SSA state is what the body's consumers read, so it is gated on the
    // block touching flags at all — not on whether the incoming *word* is
    // carried (`pass_flags`). A self-loop that writes flags before it reads them
    // carries no word across the backedge; its state starts from the word loaded
    // once in the entry block, which is what keeps the bits `FlagState` does not
    // track (DF, IF, reserved) intact when an exit edge packs.
    let mut flag_state: Option<FlagState> = if ssa_flags && needs_flags {
        // Six `band` + six `icmp` + six `iconst` — twenty host instructions, paid
        // on every visit, and on a loop head that is every iteration. Decode only
        // when something hot can observe an individual bit; otherwise carry the
        // word, which makes every pack in this block the identity
        // (`FlagState::carried`). The predicate is conservative — see
        // `analysis::entry_decode_needed` for the shapes that keep the decode and
        // why iced's own read/modify masks are not sufficient on their own.
        // The mask is what the block can *observe* before it writes: `0` carries
        // the word and packs nothing, a single-bit mask decodes one bit, and only
        // a block whose writers are not all "defines six" keeps the full cone.
        // See `analysis::entry_decode_mask`.
        let mask = entry_decode_mask(body, term, term_insn);
        Some(decompose_bits(bcx, *rflags_val, mask))
    } else {
        None
    };

    for d in body {
        ensure_gprs_loaded(bcx, ctx_ptr, gpr_vals, gpr_loaded, &d.instr, flags);
        ensure_xmm_loaded(bcx, ctx_ptr, flags, &d.instr, xmm_vals, xmm_loaded);
        // Every per-instruction refusal is recorded HERE, once, while the
        // instruction is still in hand: one place for both lowering paths, so an
        // `Unsupported` and an "admitted mnemonic, unsupported operand form"
        // refusal are counted and bucketed the same way and can be compared.
        if is_string_op(&d.instr) {
            flush_pending(bcx, rflags_val, &mut pending, &mut flag_state);
            string_exit_rip = Some(
                lower_string(bcx, &d.instr, gpr_vals, rflags_val, gpr_loaded, mem_env)
                    .map_err(|e| noted(&d.instr, e))?,
            );
            // The string helper round-trips packed rflags through JitCtx.
            resync_state(bcx, &mut flag_state, *rflags_val);
        } else {
            lower_insn(
                bcx,
                &d.instr,
                gpr_vals,
                gpr_dirty,
                rflags_val,
                &mut pending,
                &mut flag_state,
                mem_env,
                xmm_vals,
            )
            .map_err(|e| noted(&d.instr, e))?;
        }
    }

    if matches!(term, Some(BlockTerm::Jcc { .. }))
        || needs_flags
        || !matches!(pending, PendingFlags::None)
    {
        // Assign the deferred ALU flags to the SSA state and pack the carrier.
        //
        // No `resync_state` here. `FlagState` is the authority while it exists,
        // and every writer in the body either assigns its i1s directly
        // (`assign_arith`/`assign_logic`/`assign_shift`/`assign_adc`/
        // `assign_sbb`/`assign_mul`, and the pending path above) or re-derives
        // them from the carrier it just rewrote (`popfq`, `cld`, `std`, the
        // bit-test/xadd/cmpxchg/bsr/bsf/lzcnt/imul/div/string/eager paths each
        // call `FlagState::resync`). So re-deriving all six flags from the
        // packed word here — six `band`, six `icmp`, six `iconst` — rebuilt
        // values the state already held. It is conditional on `fs` being
        // `Some`, and `WIE_JIT_SSA_FLAGS=0` makes it a no-op anyway, so
        // deleting the call cannot change behaviour on either path.
        // Deferred when `FlagState` is the authority: the body's consumers are
        // done, so nothing below reads the *word* except the edges that leave
        // the block — and each of those packs it where it is used
        // (`packed_word`). A block whose flags are consumed only by its own
        // terminator therefore never builds one.
        //
        // A terminator that touches memory itself — `call`/`ret` push and pop
        // the return address — is not a hole in this, because `lower_term` is
        // handed the word from `pack_once` at the arm below: it packs from the
        // state *before* that store/load, which is exactly the fault edge's
        // requirement. (Measured: making this flush eager for those terminators
        // changes no observable, because `pack_once` already packs there.)
        if flag_state.is_some() {
            flush_pending_deferred(bcx, rflags_val, &mut pending, &mut flag_state);
        } else {
            flush_pending(bcx, rflags_val, &mut pending, &mut flag_state);
        }
    }

    // The terminator's own memory access (`call` pushes, `ret` pops) has a fault
    // edge too, and it is not covered by `lower_insn`'s per-instruction snapshot:
    // record the state *after* the body, which is what a fault on the terminator
    // observes. The pending writer was just applied to `flag_state` above, so
    // nothing is left pending at this point.
    mem_env.fault_flags = flag_state;
    mem_env.fault_pending = PendingFlags::None;

    // One pack per block, built at the first exit edge that needs the word and
    // reused by any later one: the edges below all observe the same architectural
    // state, so a second rebuild would be the same 14 instructions twice.
    let mut exit_word: Option<Value> = None;
    if let Some(t) = term {
        if let Some(ti) = term_insn {
            ensure_gprs_loaded(bcx, ctx_ptr, gpr_vals, gpr_loaded, &ti.instr, flags);
        }
        if matches!(
            t,
            BlockTerm::Call { .. } | BlockTerm::CallDyn { .. } | BlockTerm::Ret
        ) && call_fast.is_none()
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
            // The helper call and the writeback behind it observe the same
            // state, so they share one pack.
            let rflags_word = pack_once(bcx, &mut exit_word, flag_state.as_ref(), *rflags_val);
            lower_fast_ucrt(bcx, kind, gpr_vals, gpr_dirty, rflags_word, mem_env)?;
            let exit_rip = iconst_u64(bcx, return_ip);
            emit_chain_or_exit(
                bcx,
                ctx_ptr,
                flags,
                gpr_vals,
                gpr_loaded,
                Some(gpr_dirty),
                rflags_word,
                rflags_ptr,
                true,
                exit,
                exit_rip,
                chain_refs.get(&return_ip).copied(),
                lookup_ref,
                block_sig_ref,
                inv_guard,
                &trip,
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
                flag_state.as_ref(),
                rflags_ptr,
                needs_flags,
                exit,
                chain_refs,
                lookup_ref,
                block_sig_ref,
                inv_guard,
                trip,
            )?;
        } else {
            match t {
                BlockTerm::Jcc {
                    mnemonic,
                    taken,
                    not_taken,
                } => {
                    // The condition is derived ONCE, inside `lower_jcc_chain`,
                    // and only the two exit edges consume it. `lower_term` used
                    // to be called first to build a `select(cond, taken,
                    // not_taken)` exit RIP that this arm then dropped on the
                    // floor — a whole second condition derivation (and, before
                    // `flag_cond_fs` learned to read only the named flags, ~26
                    // ops) that no instruction ever read.
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
                        flag_state.as_ref(),
                        rflags_ptr,
                        needs_flags,
                        exit,
                        lookup_ref,
                        block_sig_ref,
                        inv_guard,
                        &trip,
                    )?;
                }
                BlockTerm::Jmp { target } | BlockTerm::Call { target, .. } => {
                    let rflags_word =
                        pack_once(bcx, &mut exit_word, flag_state.as_ref(), *rflags_val);
                    let exit_rip = lower_term(
                        bcx,
                        t,
                        gpr_vals,
                        gpr_dirty,
                        rflags_word,
                        mem_env,
                        term_ip,
                        term_insn.map(|ti| &ti.instr),
                    )?;
                    emit_chain_or_exit(
                        bcx,
                        ctx_ptr,
                        flags,
                        gpr_vals,
                        gpr_loaded,
                        Some(gpr_dirty),
                        rflags_word,
                        rflags_ptr,
                        needs_flags,
                        exit,
                        exit_rip,
                        chain_refs.get(&target).copied(),
                        lookup_ref,
                        block_sig_ref,
                        inv_guard,
                        &trip,
                    );
                }
                // `call r/m64`: the exit RIP is a runtime value, so there is
                // no direct chain edge — exactly like `ret`.
                BlockTerm::CallDyn { .. } | BlockTerm::Ret => {
                    let rflags_word =
                        pack_once(bcx, &mut exit_word, flag_state.as_ref(), *rflags_val);
                    let exit_rip = lower_term(
                        bcx,
                        t,
                        gpr_vals,
                        gpr_dirty,
                        rflags_word,
                        mem_env,
                        term_ip,
                        term_insn.map(|ti| &ti.instr),
                    )?;
                    emit_chain_or_exit(
                        bcx,
                        ctx_ptr,
                        flags,
                        gpr_vals,
                        gpr_loaded,
                        Some(gpr_dirty),
                        rflags_word,
                        rflags_ptr,
                        needs_flags,
                        exit,
                        exit_rip,
                        None,
                        lookup_ref,
                        block_sig_ref,
                        inv_guard,
                        &trip,
                    );
                }
            }
        }
    } else if let Some(sr) = string_exit_rip {
        let rflags_word = pack_once(bcx, &mut exit_word, flag_state.as_ref(), *rflags_val);
        emit_chain_or_exit(
            bcx,
            ctx_ptr,
            flags,
            gpr_vals,
            gpr_loaded,
            Some(gpr_dirty),
            rflags_word,
            rflags_ptr,
            needs_flags,
            exit,
            sr,
            None,
            lookup_ref,
            block_sig_ref,
            inv_guard,
            &trip,
        );
    } else {
        let rflags_word = pack_once(bcx, &mut exit_word, flag_state.as_ref(), *rflags_val);
        let exit_rip = iconst_u64(bcx, end_rip);
        emit_chain_or_exit(
            bcx,
            ctx_ptr,
            flags,
            gpr_vals,
            gpr_loaded,
            Some(gpr_dirty),
            rflags_word,
            rflags_ptr,
            needs_flags,
            exit,
            exit_rip,
            chain_refs.get(&end_rip).copied(),
            lookup_ref,
            block_sig_ref,
            inv_guard,
            &trip,
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{chain_store_count, gpr_store_predicate};

    const NONE_LOADED: [bool; 16] = [false; 16];
    const ALL_LOADED: [bool; 16] = [true; 16];

    /// The counter must agree with the emitter's store predicate on every
    /// register, in both the masked (`Some`) and full-flush (`None`) modes —
    /// this is the invariant that keeps `chain_store_ops` honest, since it is
    /// the number the chain-edge cost is later divided by.
    #[test]
    fn store_count_matches_predicate_slot_by_slot() {
        let loaded = [
            true, false, true, false, true, false, false, true, true, false, true, false, false,
            true, false, false,
        ];
        let dirty = [
            false, true, false, false, true, false, false, false, true, false, false, true, false,
            false, true, false,
        ];
        let n = chain_store_count(&loaded, Some(&dirty));
        let counted = (0..16)
            .filter(|&i| gpr_store_predicate(i, &loaded, Some(&dirty)))
            .count();
        assert_eq!(usize::try_from(n).unwrap_or(usize::MAX), counted);
        // Every dirty register is stored even when it was never loaded: a
        // `mov rcx,r12` call argument is not a live-in, and dropping it from
        // the writeback silently hands a stale register to the callee.
        assert_eq!(
            chain_store_count(&NONE_LOADED, Some(&dirty)),
            u64::try_from(dirty.iter().filter(|&&d| d).count()).unwrap_or(u64::MAX)
        );
        // Read-only live-ins count (they are re-stored under the current
        // predicate — the whole point of the counter).
        assert_eq!(chain_store_count(&loaded, None), 7);
    }

    /// Bounds the counter against the register file: 0 when nothing is live,
    /// 16 when everything is. A value outside this range means the predicate
    /// and the counter have diverged.
    #[test]
    fn store_count_is_bounded_by_the_register_file() {
        assert_eq!(chain_store_count(&NONE_LOADED, Some(&NONE_LOADED)), 0);
        assert_eq!(chain_store_count(&ALL_LOADED, None), 16);
        assert_eq!(chain_store_count(&ALL_LOADED, Some(&ALL_LOADED)), 16);
    }
}
