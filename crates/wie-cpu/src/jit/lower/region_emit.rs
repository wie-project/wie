//! Region emission: **one** Cranelift function per loop region.
//!
//! [`compile_region`] consumes the description [`form_loop_region`] produces
//! (blocks in reverse postorder, entry live-ins, exits, back edges) and emits
//! one native function for the whole loop:
//!
//! ```text
//!  entry:                            ; the function entry — never a jump target
//!    trip = region_insn_count        ; loop-carried retired-instruction count
//!    gpr[0..16] = load JitCtx.gpr    ; the ONLY load of the guest file
//!    rflags     = load JitCtx.rflags
//!    pins/refill = hoist …           ; block-invariant for the frame
//!    jump header(gpr, rflags)
//!
//!  header / blk_1 / …:               ; one CLIF block per region block
//!    body via lower_insn             ; guest GPRs are SSA values here
//!    flush_pending -> rflags         ; the carrier is architectural per block
//!    jump blk_j(gpr, rflags)         ; region-internal edge: no JitCtx traffic
//!    brif cond, side_t, side_n       ; the back edge is a native jump
//!
//!  exit_N:                           ; one per distinct exit target
//!    writeback the whole guest file  ; the only re-synchronisation point
//!    emit_chain_or_exit(rip = N)     ; the same edge the block path emits
//!
//!  store_exit:                       ; fault path, and chain-table miss
//!    writeback the whole guest file
//!    insn_acc += trip
//!    return
//! ```
//!
//! The win is structural and it is the whole point of the module: `live_in()` is
//! loaded **once per region entry** and the file is stored **once per region
//! exit**, instead of once per block hop. DOOM Retro's hottest loop is two blocks
//! that hop once per iteration; block 1's load-in/store-back disappears entirely.
//!
//! # The guest register file is stale for the whole region
//!
//! This is the invariant every line below is written against. Inside the region,
//! `JitCtx::gpr` holds whatever it held when the region was entered: nothing in
//! the body may read it, and every read is served from an SSA value. Three
//! consequences, each of which is why a piece of this file looks the way it does:
//!
//! 1. **Every GPR is loaded at entry — all 16, not `live_in()`.** `live_in()` says
//!    which values are *used*; loading the other nine costs nine loads once per
//!    region entry and buys two things a narrower load cannot:
//!    [`ensure_gprs_loaded`](super::analysis::ensure_gprs_loaded) can never fire
//!    mid-region (it only loads a register it has not seen defined — a stale read
//!    with no fault, the failure mode this whole module exists to avoid), and
//!    every slot is a *defined, architectural* value at every exit, which is what
//!    makes the conservative writeback below not merely safe but exactly "the
//!    guest's register file".
//! 2. **Every region block takes the whole file as block parameters** (`[i64;
//!    16]` plus the RFLAGS carrier), threaded the way the self-loop path threads
//!    its header. That is what makes a read at any point in the region a read of
//!    the rounded-up architectural value, on every path, resting on no dataflow
//!    claim beyond "these are block parameters".
//! 3. **XMM needs none of this**, because the SSE lowerers are write-through:
//!    `store_xmm_packed` writes `JitCtx::xmm` on every def, so a mid-region
//!    [`ensure_xmm_loaded`] reads the *current* value. XMM stays on the block
//!    path's contract unchanged, and a region containing SSE is correct without a
//!    new line of code — only slower than it needs to be.
//!
//! # What an exit stores, and why that is sufficient
//!
//! The per-block contract — "all dirty regs written at exit" — does not transfer
//! into a region, and the reason is worth stating precisely because it is the
//! sharp edge here. At a block exit the *successor reloads* whatever it needs, so
//! storing the written registers is enough. At a region exit there is no reload:
//! `JitCtx` is the only copy the next compilation unit (or the interpreter) can
//! see, so **every** slot the region may have changed has to be there.
//!
//! Deciding that tightly means, for each exit point `p` and register `r`, "is
//! `JitCtx[r]` stale at `p`?" — a backward must-analysis (does a write reach `p`)
//! crossed with "is the SSA value at `p` the architectural one" (which needs `r ∈
//! live_in` or a write on *every* path to `p`; otherwise `p` holds an entry
//! placeholder that must not be written back over a correct slot). The first half
//! is available — `region.rs` already runs that dataflow for live-ins — but the
//! second half is a claim about which `JitCtx` slots a *future* compilation unit
//! will read, and getting it wrong corrupts a register silently.
//!
//! So the exit stores the whole file: all 16 GPRs plus the RFLAGS word. That is
//! the conservative live-out, it is trivially sufficient (with (1) above, every
//! value stored is the architectural value of that register at that point), and
//! it is exactly what the self-loop path already chooses (`compile_block`:
//! `live_eff.fill(true)`) for exactly this reason. It costs 17 stores **per
//! region exit**, against 32 loads + 32 stores per *iteration* for the two hops
//! it replaces.
//!
//! The narrower set is the obvious follow-up once a measurement says exit stores
//! matter; it needs a live-out analysis in `region.rs` and the write-back-eligible
//! predicate above — not a guess here.
//!
//! # What is reused, and what is not
//!
//! Instruction lowering is [`lower_insn`], unchanged, and memory is the block
//! path's [`MemEnv`] with the same hoisted pins and refill probe. The exit edge is
//! [`emit_chain_or_exit`] — literally the block path's transfer, so the depth cap,
//! invalidation guard, edge IC, chain-table probe and hop accounting are shared
//! rather than re-derived.
//!
//! [`lower_term`](super::lower_term) is deliberately **not** used, because there
//! is nothing left for it to do here: a region terminator's whole effect is "be
//! somewhere else", and inside a region that is a native `jump` to a CLIF block
//! rather than a guest RIP for the dispatcher. Its three arms are `jmp` (returns a
//! target constant nobody in a region reads), `jcc` (returns a verifier error by
//! design — the work lives in `lower_jcc_chain`, which emits *chain edges*, the
//! one thing a region must not do), and `call`/`call r/m`/`ret` (refused at
//! formation). The condition evaluation itself is shared: [`flag_cond_fs`] /
//! [`flag_cond`], the same pair `lower_jcc_chain` calls.
//!
//! # What is refused rather than worked around
//!
//! Everything formation refuses is refused *before* this module runs
//! ([`RegionRefusal`]): calls, returns, string ops, unguardable addresses,
//! irreducible cycles, over-bound regions. Nothing here re-checks those, because
//! a second, weaker check is the thing that eventually disagrees.
//!
//! Two decisions this module does make for itself:
//!
//! * **A bad region *description* is an error, not a fallback.** If a block's
//!   successor is neither inside the region nor one of its recorded exits, or a
//!   recorded exit belongs to no block edge, formation and emission disagree
//!   about the same loop: that is [`LowerError::verifier`], and the caller
//!   compiles the per-block path instead of emitting a transfer nobody described.
//! * **A region does not consume the tier-up budget.** It compiles at the tier
//!   the caller names, and `pipeline.rs` names `OptTier::Base`. `decide_tier`'s
//!   rule is "self-loop", shaped for a single block; giving a whole region a tier
//!   decision is a separate policy question with its own measurements, so it is
//!   not smuggled in here.
//!
//! # Status
//!
//! Wired: `pipeline::try_compile` forms a region for a hot RIP *before* decoding a
//! single block, behind `WIE_JIT_REGION=1` (default off — see
//! [`JitConfig::region_enabled`] for why). The per-block path remains the fallback
//! for every RIP that is not a loop header and for every refusing region.

use super::super::block::BlockTerm;
use super::super::config::JitConfig;
use super::super::engine::IsaEngine;
use super::super::tier::OptTier;
use super::analysis::ensure_xmm_loaded;
use super::emit::{MemEnv, TripCounter, emit_chain_or_exit, exit_args, writeback_gprs};
use super::err::{LowerError, noted};
use super::flags::{FlagState, decompose_packed, iconst_u64};
use super::insn::{PendingFlags, flush_pending, lower_insn};
use super::mem::{hoist_pin_slot, hoist_refill};
use super::region::{GuestInsn, LoopRegion, RegionLimits, RegionRefusal, form_loop_region};
use super::{
    ChainLookup, CompiledBlock, GUEST_DATA_REGION_USER_ID, JitCtx, OFF_INV_GEN_BAKED,
    OFF_INV_GEN_PTR, OFF_RFLAGS, OFF_RIP, dump_ir_if_requested, dump_map_if_requested, flag_cond,
    flag_cond_fs,
};
use crate::exec::HookWindow;
use crate::mem::GuestMemory;
use ahash::HashMap;
use ahash::HashMapExt;
use cranelift::codegen::ir::{AliasRegionData, BlockArg, UserFuncName};
use cranelift::prelude::*;
use cranelift_codegen::ir::MemFlagsData;
use cranelift_module::Module;
use std::borrow::Cow;

/// Guest GPRs carried as SSA block parameters on every region block and exit
/// block, plus the RFLAGS carrier — the arity of [`exit_args`].
///
/// Reusing `exit_args`' order (`gpr[0..16]`, then rflags) for the *internal* edges
/// too is deliberate: the region has one block-parameter convention, so
/// [`exit_args`] and [`writeback_gprs`] — written for the block path's exit block
/// — serve the region's internal edges unchanged.
const STATE_PARAMS: usize = 17;

/// The GPR slots an exit's writeback stores: all of them.
///
/// A constant because the exit stores the whole file (module docs). It is also
/// what [`emit_chain_or_exit`]'s hop accounting charges per hop, so the number
/// reported in `chain_store_ops` is the number of stores the emitted code does.
const ALL_GPRS: [bool; 16] = [true; 16];

/// Form a loop region for `rip` under the default bounds.
///
/// The one thing `pipeline.rs` needs before it decides whether to compile a
/// region: `Err` means "compile a single block as usual", and the payload names
/// the rule that refused, so a refusal can be reported rather than guessed at.
pub(super) fn form_region(
    mem: &GuestMemory,
    hooks: Option<&HookWindow>,
    rip: u64,
) -> Result<LoopRegion, RegionRefusal> {
    form_loop_region(mem, hooks, rip, &RegionLimits::default())
}

/// Where a region block's edge goes.
#[derive(Clone, Copy)]
enum Dest {
    /// Stays inside the region: region-local block index.
    Inside(usize),
    /// Leaves the region: the guest RIP the exit lands on.
    Leave(u64),
}

/// One region block, resolved for emission: its body, its terminator and where
/// its terminator's two slots go.
struct BlockPlan<'r> {
    /// Body instructions, terminator **excluded** — the same split
    /// `emit_body_and_term` makes on the block path.
    body: &'r [GuestInsn],
    term: Option<BlockTerm>,
    /// Slot 0 is a `jmp`'s target, a `jcc`'s taken side, or a terminator-less
    /// block's fallthrough; slot 1 is a `jcc`'s not-taken side.
    dests: [Dest; 2],
}

/// The CLIF blocks one region's edges can reach.
struct Layout {
    /// One block per region block, in `region.blocks()` order.
    state: Vec<Block>,
    /// One block per distinct exit target, in first-seen order.
    exits: Vec<(u64, Block)>,
    /// Back edges as region-local `(from, to)` pairs.
    back_edges: Vec<(usize, usize)>,
    /// Guest VA a stale-generation back-edge bailout re-enters at.
    entry: u64,
}

impl Layout {
    /// The exit block for `va`, or a verifier error when formation recorded no
    /// such exit (see the module docs: a disagreement is an error, not a guess).
    fn exit_block(&self, va: u64) -> Result<Block, LowerError> {
        self.exits
            .iter()
            .find_map(|&(target, blk)| (target == va).then_some(blk))
            .ok_or_else(|| LowerError::verifier("internal: region edge to an unrecorded exit"))
    }

    /// Whether `(from, to)` is one of the region's back edges.
    fn is_back_edge(&self, from: usize, to: usize) -> bool {
        self.back_edges.contains(&(from, to))
    }
}

/// A fresh CLIF block taking the region's state as block parameters.
///
/// Created for every region block, every exit block and the shared fault/return
/// block, so the region has exactly one convention between its parts.
fn state_block(bcx: &mut FunctionBuilder<'_>) -> Block {
    let blk = bcx.create_block();
    for _ in 0..STATE_PARAMS {
        bcx.append_block_param(blk, types::I64);
    }
    blk
}

/// The state block parameters of `blk`, in [`exit_args`] order.
fn state_params(bcx: &FunctionBuilder<'_>, blk: Block) -> ([Value; 16], Value) {
    let params = bcx.block_params(blk);
    let mut gpr = [params[0]; 16];
    gpr.copy_from_slice(&params[..16]);
    (gpr, params[16])
}

/// Resolve each region block's terminator and successors against the region.
///
/// Every edge must be accounted for on both sides: an edge to a VA that is
/// neither a region block nor a recorded exit, and a recorded exit that no edge
/// produces, both mean this emitter and [`form_loop_region`] disagree about the
/// loop. Emitting a transfer from a guess is how a region would jump somewhere
/// the guest never goes, so both are verifier errors and the caller falls back.
fn plan_blocks<'r>(
    region: &'r LoopRegion,
    local: &HashMap<u64, usize>,
) -> Result<Vec<BlockPlan<'r>>, LowerError> {
    let mut plan = Vec::with_capacity(region.blocks().len());
    let mut seen_exits: Vec<(usize, u64)> = Vec::new();
    // The flat instruction array and the per-block slices must agree; if they do
    // not, the byte extents below (and every fault IP) would be attributed to the
    // wrong block.
    debug_assert_eq!(region.insns().len(), region.insn_count());
    for (i, block) in region.blocks().iter().enumerate() {
        let insns = region.block_insns(i);
        let term = block.term();
        // `body_len()` is the description's own statement of where the body ends
        // and the terminator begins — the same split `emit_body_and_term` makes on
        // the block path, so the region and the block lower the same bytes.
        let body_len = block.body_len();
        let body = &insns[..body_len];
        debug_assert_eq!(
            insns.len(),
            block.insn_count(),
            "region block {i} slice disagrees with its declared length"
        );
        let (slots, ndests) = block.successors();
        if ndests == 0 {
            // `ret`: refused at formation, and a region block with no successor
            // contradicts the closure property `region.rs` documents.
            return Err(LowerError::verifier(
                "internal: region block with no successor",
            ));
        }
        let mut dests = [Dest::Leave(0); 2];
        for (slot, &va) in slots[..ndests].iter().enumerate() {
            dests[slot] = match local.get(&va) {
                Some(&j) => Dest::Inside(j),
                None => {
                    seen_exits.push((i, va));
                    Dest::Leave(va)
                }
            };
        }
        plan.push(BlockPlan { body, term, dests });
    }
    for exit in region.exits() {
        if !seen_exits.contains(&(exit.from(), exit.target())) {
            return Err(LowerError::verifier(
                "internal: region exit recorded but no block edge produces it",
            ));
        }
        // The slot -> `taken` convention has to match formation's, or a `jcc`'s
        // two exit blocks would be swapped: slot 0 is a branch target (a `jmp`'s
        // target or a `jcc`'s taken side), slot 1 a fallthrough.
        let block = &region.blocks()[exit.from()];
        let (slots, ndests) = block.successors();
        let slot = (0..ndests).find(|&k| slots[k] == exit.target());
        debug_assert_eq!(
            slot.map(|k| k == 0 && block.term().is_some()),
            Some(exit.is_taken()),
            "region exit {:#x} disagrees with its block's slot order",
            exit.target()
        );
    }
    Ok(plan)
}

/// Emit one region block's body: the per-instruction lowering the block path
/// uses, with the guest file already in SSA.
///
/// Returns the SSA flag state (`None` when `WIE_JIT_SSA_FLAGS=0`), so the
/// terminator reads the flags this body produced instead of re-deriving them from
/// the carrier — the same hand-off `emit_body_and_term` makes to
/// `lower_jcc_chain`.
fn emit_body(
    bcx: &mut FunctionBuilder<'_>,
    plan: &BlockPlan<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    mem: &mut MemEnv,
    gpr: &mut [Value; 16],
    rflags: &mut Value,
) -> Result<Option<FlagState>, LowerError> {
    // The RFLAGS carrier is decomposed per block and re-packed at the end, which
    // is exactly the self-loop header's discipline: the word carried across the
    // edge is architecturally correct, so a trap, a host stop or a `pushfq` inside
    // the loop sees real flags rather than a half-updated carrier.
    let mut flag_state = if JitConfig::get().ssa_flags_enabled() {
        Some(decompose_packed(bcx, *rflags))
    } else {
        None
    };
    let mut pending = PendingFlags::None;
    let mut dirty = [false; 16];
    let mut xmm_vals = [bcx.ins().iconst(types::I64, 0); 32];
    let mut xmm_loaded = [false; 16];

    for guest in plan.body {
        // The fault path (`set_fault(ctx, insn_ip, …)`) is driven by the iced
        // instruction's own IP, so the description's RIP must be the same number or
        // a mid-loop page fault would report a guest IP the guest never ran.
        debug_assert_eq!(
            guest.rip,
            guest.insn.ip(),
            "region instruction RIP disagrees with its decoded IP"
        );
        // XMM only: the SSE lowerers are write-through, so this reads the
        // *current* value (module docs, point 3). There is deliberately no
        // `ensure_gprs_loaded` call — every GPR is a block parameter, so such a
        // call would be dead at best and a stale `JitCtx` read if it ever fired.
        ensure_xmm_loaded(
            bcx,
            ctx_ptr,
            flags,
            &guest.insn,
            &mut xmm_vals,
            &mut xmm_loaded,
        );
        lower_insn(
            bcx,
            &guest.insn,
            gpr,
            &mut dirty,
            rflags,
            &mut pending,
            &mut flag_state,
            mem,
            &mut xmm_vals,
        )
        .map_err(|e| noted(&guest.insn, e))?;
    }
    // Pack the deferred ALU flags into the carrier before the terminator reads
    // them (a `jcc` is the usual reason) and before the edge carries the word to
    // the next block. `PendingFlags::None` is a no-op, so this is unconditional on
    // purpose: it is what makes "the carrier is architectural at every region block
    // boundary" true, rather than "true when the last instruction happened to need
    // it".
    flush_pending(bcx, rflags, &mut pending, &mut flag_state);
    Ok(flag_state)
}

/// The condition for a region `jcc`, from the SSA flag state the body left behind
/// — the same two-way choice `lower_jcc_chain` makes between
/// [`flag_cond_fs`] and [`flag_cond`].
fn emit_cond(
    bcx: &mut FunctionBuilder<'_>,
    flag_state: Option<&FlagState>,
    mnemonic: iced_x86::Mnemonic,
    rflags: Value,
) -> Result<Value, LowerError> {
    match flag_state {
        Some(fs) => flag_cond_fs(bcx, fs, mnemonic),
        None => flag_cond(bcx, rflags, mnemonic),
    }
}

/// Leave the region through a back edge: charge the trip, observe cross-thread
/// invalidation, then jump to the header with the state as block arguments.
///
/// The guard mirrors `emit::emit_inv_gen_check` as used by
/// `emit::lower_self_loop_term` (that helper is private to `emit.rs`): a
/// generation mismatch flushes the whole file plus `rip = region entry` and
/// returns to the dispatcher, which purges and re-decodes. Without it a region
/// would be the one place emitted code runs unboundedly without observing an
/// invalidation — a self-loop block already pays this on its back edge, and it
/// costs one atomic load, one compare and a branch per iteration.
#[allow(clippy::too_many_arguments)]
fn emit_back_edge(
    bcx: &mut FunctionBuilder<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    gpr: &[Value; 16],
    rflags: Value,
    rflags_ptr: Value,
    trip: &TripCounter,
    inv_guard: bool,
    layout: &Layout,
    header: Block,
) {
    // Charge first, so the bailout path accounts for the iteration it just ran.
    trip.bump(bcx);
    if inv_guard {
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
        // The region's SSA state is the only copy: bring it up to date, point the
        // dispatcher at the loop header and leave.
        writeback_gprs(
            bcx, ctx_ptr, flags, gpr, &ALL_GPRS, None, rflags, rflags_ptr, true,
        );
        let rip_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_RIP));
        let rip = iconst_u64(bcx, layout.entry);
        bcx.ins().store(flags, rip, rip_ptr, 0);
        trip.flush(bcx, ctx_ptr, flags);
        bcx.ins().return_(&[]);

        bcx.switch_to_block(cont_blk);
        bcx.seal_block(cont_blk);
    }
    bcx.ins().jump(header, &exit_args(gpr, rflags));
}

/// Emit one region block's terminator as native control flow.
///
/// An edge that stays inside the region is a `jump` (or a `brif` side) to that
/// block's state block — this is the native back edge when the target is the
/// header. An edge that leaves is a jump to that target's exit block, which is
/// where the guest file is re-synchronised and the chain edge is taken. A back
/// edge additionally passes through a landing block when it is one side of a
/// `jcc`, because the trip charge and the invalidation guard have to run on that
/// side only.
#[allow(clippy::too_many_arguments)]
fn emit_term(
    bcx: &mut FunctionBuilder<'_>,
    plan: &BlockPlan<'_>,
    index: usize,
    layout: &Layout,
    ctx_ptr: Value,
    flags: MemFlagsData,
    gpr: &[Value; 16],
    rflags: Value,
    rflags_ptr: Value,
    flag_state: Option<&FlagState>,
    trip: &TripCounter,
    inv_guard: bool,
) -> Result<(), LowerError> {
    /// The CLIF block one edge branches to, and — for a back edge — the landing
    /// block that has to run the charge and guard before reaching `header`.
    struct Edge {
        branch_to: Block,
        work: Option<(Block, Block)>,
    }

    fn edge(
        bcx: &mut FunctionBuilder<'_>,
        dest: Dest,
        from: usize,
        layout: &Layout,
    ) -> Result<Edge, LowerError> {
        let target = match dest {
            Dest::Inside(j) => layout.state[j],
            Dest::Leave(va) => layout.exit_block(va)?,
        };
        Ok(match dest {
            Dest::Inside(j) if layout.is_back_edge(from, j) => {
                // One landing block per back edge. It is both the `brif` target
                // and the block the charge/guard is emitted into, so no block is
                // created that nothing fills.
                let landing = bcx.create_block();
                Edge {
                    branch_to: landing,
                    work: Some((landing, target)),
                }
            }
            _ => Edge {
                branch_to: target,
                work: None,
            },
        })
    }

    // `leave` fills a landing block: charge, guard, then jump to the header.
    fn work(
        bcx: &mut FunctionBuilder<'_>,
        landing: Block,
        header: Block,
        ctx_ptr: Value,
        flags: MemFlagsData,
        gpr: &[Value; 16],
        rflags: Value,
        rflags_ptr: Value,
        trip: &TripCounter,
        inv_guard: bool,
        layout: &Layout,
    ) {
        bcx.switch_to_block(landing);
        bcx.seal_block(landing);
        emit_back_edge(
            bcx, ctx_ptr, flags, gpr, rflags, rflags_ptr, trip, inv_guard, layout, header,
        );
    }

    match plan.term {
        // A terminator-less region block falls through to its single successor.
        None | Some(BlockTerm::Jmp { .. }) => {
            let dest = plan.dests[0];
            let e = edge(bcx, dest, index, layout)?;
            match e.work {
                Some((landing, header)) => {
                    // The edge carries no values: the landing block reads the
                    // state from *this* block, which dominates it.
                    bcx.ins().jump(landing, &[]);
                    work(
                        bcx, landing, header, ctx_ptr, flags, gpr, rflags, rflags_ptr, trip,
                        inv_guard, layout,
                    );
                }
                None => {
                    bcx.ins().jump(e.branch_to, &exit_args(gpr, rflags));
                }
            }
        }
        Some(BlockTerm::Jcc { mnemonic, .. }) => {
            let cond = emit_cond(bcx, flag_state, mnemonic, rflags)?;
            // Both sides carry the identical state: a `jcc` changes no register.
            let taken = edge(bcx, plan.dests[0], index, layout)?;
            let not_taken = edge(bcx, plan.dests[1], index, layout)?;
            let args = exit_args(gpr, rflags);
            // A side that branches straight to a state block passes the state; a side
            // that goes through a landing block passes nothing, because the landing
            // has no parameters — it reads this block's values, which dominate it.
            let t_args: &[BlockArg] = if taken.work.is_some() { &[] } else { &args };
            let n_args: &[BlockArg] = if not_taken.work.is_some() { &[] } else { &args };
            let (t_branch, n_branch) = (taken.branch_to, not_taken.branch_to);
            bcx.ins().brif(cond, t_branch, t_args, n_branch, n_args);
            for e in [taken, not_taken] {
                if let Some((landing, header)) = e.work {
                    work(
                        bcx, landing, header, ctx_ptr, flags, gpr, rflags, rflags_ptr, trip,
                        inv_guard, layout,
                    );
                }
            }
        }
        // All three are refused at formation; reaching one means the description
        // does not match what this emitter can express.
        Some(BlockTerm::Call { target, .. }) => {
            return Err(LowerError::verifier(format!(
                "internal: region contains a call at {target:#x}"
            )));
        }
        Some(BlockTerm::CallDyn { .. }) | Some(BlockTerm::Ret) => {
            return Err(LowerError::verifier(
                "internal: region terminator with no static successor",
            ));
        }
    }
    Ok(())
}

/// Form and compile `rip` as a loop region, or say why not.
///
/// This is the whole of what `pipeline::try_compile` calls, so the two private
/// types this module works in ([`LoopRegion`], [`RegionRefusal`]) never leave
/// `lower`. `None` means "compile a single block exactly as before" — either `rip`
/// is not a loop header, or the region was refused at formation, or an
/// instruction in it was refused at lower time.
///
/// # Locks
///
/// The caller holds the guest-memory **read** guard (`&GuestMemory`) and the
/// engine mutex (`&mut IsaEngine`) for the duration. Taking those two in that
/// order is new, and it is safe for a checkable reason rather than a
/// conventional one: no path in this crate holds a `mem` guard — read or write —
/// across another lock acquisition. Every `write_unpoisoned(&self.shared.mem)`
/// site (`cpu_engine.rs`: `mem_map`, `mem_write`, `virtual_alloc`,
/// `virtual_free`, `virtual_protect`) takes its guard as a statement temporary
/// and drops it before `invalidate_code_range`/`invalidate_tlb`; and
/// `compile_from_kind_shared` drops its read guard before locking `engine` only to
/// satisfy the borrow checker, not an ordering rule. So a thread blocked on the
/// engine mutex holds nothing, and a writer blocked on `mem` holds nothing:
/// neither can close a cycle.
pub(in crate::jit) fn try_compile_region(
    eng: &mut IsaEngine,
    mem: &GuestMemory,
    hooks: Option<&HookWindow>,
    rip: u64,
    chain: &dyn ChainLookup,
    inv_gen_baked: u64,
    tier: OptTier,
) -> Option<CompiledBlock> {
    let region = match form_region(mem, hooks, rip) {
        Ok(region) => region,
        Err(why) => {
            note_region_event("not a region", rip, &format_args!("{why:?}"));
            return None;
        }
    };
    match compile_region(eng, &region, chain, inv_gen_baked, tier) {
        Ok(compiled) => {
            // A region is a rare, high-value compile: log every one at info so a
            // run's `RUST_LOG` says which loops got the treatment, and so the
            // absence of this line next to a refusal line explains a slow loop.
            let live = region.live_in();
            tracing::info!(
                start = format_args!("{rip:#x}"),
                block_starts = ?region
                    .blocks()
                    .iter()
                    .map(|b| format!("{:#x}", b.start()))
                    .collect::<Vec<_>>(),
                blocks = region.blocks().len(),
                insns = region.insn_count(),
                live_in = live.reg_count(),
                live_gprs = ?live.gprs().map(|r| format!("{r:?}")).collect::<Vec<_>>(),
                // XMM is write-through in a region (module docs, point 3), so it
                // is reported separately rather than as part of the entry load.
                xmm_live = ?live.xmms().collect::<Vec<_>>(),
                rflags_live = live.rflags(),
                exits = region.exit_targets().len(),
                back_edges = region.back_edges().len(),
                "jit compiled loop region"
            );
            Some(compiled)
        }
        Err(e) => {
            note_region_event("refused", rip, &format_args!("{e}"));
            None
        }
    }
}

/// Count of region events logged loudly so far, so a guest that is *not* a loop
/// never turns on the log into a per-block flood.
static REGION_REJECTIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many region refusals are logged at `WARN` before the rest go to `DEBUG`.
const REGION_WARN_MAX: u64 = 8;

/// Report one region refusal, loudly for the first few.
///
/// Same shape and the same reason as the block path's rejection counter: refusals
/// are routine on real guests (most hot RIPs are not loop headers, which is the
/// normal outcome), so an unthrottled `WARN` per block would be noise while a
/// silent `DEBUG` would make "why is this loop still hopping?" unanswerable.
fn note_region_event(what: &'static str, rip: u64, why: &dyn std::fmt::Display) {
    let n = REGION_REJECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < REGION_WARN_MAX {
        tracing::warn!(
            start = format_args!("{rip:#x}"),
            reason = %why,
            "jit loop region {what} — falling back to the per-block path"
        );
    } else {
        tracing::debug!(
            start = format_args!("{rip:#x}"),
            reason = %why,
            "jit loop region {what} — falling back to the per-block path"
        );
    }
}

/// Emit one Cranelift function for `region` and finalize it.
///
/// `chain` is the same point lookup [`compile_block`](super::compile_block) takes
/// and must return `FuncId`s from `eng`'s own module (see [`ChainLookup`]). The
/// region's *exit* edges use it exactly as a block terminator does; its internal
/// edges do not (they are `jump`s).
///
/// Returns [`LowerError`] — a refusal, never a partial function — when any
/// instruction in the region cannot be lowered or the region description is
/// internally inconsistent. The caller's fallback is the per-block path, which is
/// always available for the same RIP.
pub(super) fn compile_region(
    eng: &mut IsaEngine,
    region: &LoopRegion,
    chain: &dyn ChainLookup,
    inv_gen_baked: u64,
    tier: OptTier,
) -> Result<CompiledBlock, LowerError> {
    let nblocks = region.blocks().len();
    if nblocks == 0 || region.back_edges().is_empty() {
        // Formation guarantees both — a region is, by definition, a cycle through
        // its entry — and without a back edge there is nothing to make native.
        return Err(LowerError::verifier(
            "internal: region without a back edge is not compilable",
        ));
    }

    let mut local: HashMap<u64, usize> = HashMap::with_capacity(nblocks);
    for (i, block) in region.blocks().iter().enumerate() {
        local.insert(block.start(), i);
    }
    let plan = plan_blocks(region, &local)?;

    // Anonymous: reached only via `FuncId`, never by symbol (see `compile_block`,
    // which skips Cranelift's name table for the same reason).
    let func_id = eng
        .module
        .declare_anonymous_function(&eng.block_sig)
        .map_err(|e| LowerError::verifier(e.to_string()))?;
    // The same defensive reset `compile_block` performs: a refusal part-way leaves
    // the shared `ctx`/`func_ctx` dirty, and Cranelift then panics on the *next*
    // compile of any block. Both resets are idempotent and cheap next to one
    // compile, so they do not depend on how a previous attempt ended.
    eng.module.clear_context(&mut eng.ctx);
    eng.func_ctx = FunctionBuilderContext::new();
    super::flags::reset_mask_cache();
    eng.ctx.func.signature = eng.block_sig.clone();
    eng.ctx.func.name = UserFuncName::user(0, func_id.as_u32());

    {
        let mut bcx = FunctionBuilder::new(&mut eng.ctx.func, &mut eng.func_ctx);
        let entry = bcx.create_block();
        bcx.append_block_params_for_function_params(entry);
        bcx.switch_to_block(entry);
        // Cranelift forbids CFG edges into the function entry block, so the loop
        // header is always a separate block — the same reason `compile_block`'s
        // self-loop path re-enters a dedicated header.
        bcx.seal_block(entry);

        let ctx_ptr = bcx.block_params(entry)[0];
        let trip = TripCounter::declare(
            &mut bcx,
            u32::try_from(region.insn_count()).unwrap_or(u32::MAX),
        );
        let flags = MemFlagsData::trusted();
        let guest_data_region = bcx.func.dfg.alias_regions.insert(AliasRegionData {
            user_id: GUEST_DATA_REGION_USER_ID,
            description: Cow::Borrowed("guest_data"),
        });
        let guest_flags = flags.with_alias_region(Some(guest_data_region));

        // Declare every helper the region body could reach, unconditionally.
        // Unused imports cost nothing (Cranelift materializes only referenced
        // ones), and a region body may contain any instruction the block gate
        // admits — so gating these on a scan of the region would be a second,
        // weaker copy of the admission decision, and the failure mode of getting
        // it wrong is a lowering refusal on the *first* guest run.
        //
        // `string` / `host_span` / UCRT refs stay `None` on purpose: formation
        // refuses string ops and calls, so those imports are unreachable here by
        // construction rather than by omission.
        let load_ref = Some(eng.module.declare_func_in_func(eng.load_id, bcx.func));
        let store_ref = Some(eng.module.declare_func_in_func(eng.store_id, bcx.func));
        let f32_ref = Some(eng.module.declare_func_in_func(eng.f32_id, bcx.func));
        let f64_ref = Some(eng.module.declare_func_in_func(eng.f64_id, bcx.func));
        let sse_int_ref = Some(eng.module.declare_func_in_func(eng.sse_int_id, bcx.func));
        let sse_shift_ref = Some(eng.module.declare_func_in_func(eng.sse_shift_id, bcx.func));
        let sse_pshufb_lo_ref = Some(
            eng.module
                .declare_func_in_func(eng.sse_pshufb_lo_id, bcx.func),
        );
        let sse_pshufb_hi_ref = Some(
            eng.module
                .declare_func_in_func(eng.sse_pshufb_hi_id, bcx.func),
        );
        let sse_fp_unop_ref = Some(
            eng.module
                .declare_func_in_func(eng.sse_fp_unop_id, bcx.func),
        );
        let sse_fp_binop_ref = Some(
            eng.module
                .declare_func_in_func(eng.sse_fp_binop_id, bcx.func),
        );
        let sse_cvt_ref = Some(eng.module.declare_func_in_func(eng.sse_cvt_id, bcx.func));
        let div64_ref = Some(eng.module.declare_func_in_func(eng.div64_id, bcx.func));
        let lookup_ref = eng.module.declare_func_in_func(eng.lookup_id, bcx.func);
        let block_sig_ref = bcx.import_signature(eng.block_sig.clone());

        // Every block is created before any is filled, so an edge can name its
        // target wherever the target sits in the region.
        let mut layout = Layout {
            state: Vec::with_capacity(nblocks),
            exits: Vec::with_capacity(region.exit_targets().len()),
            back_edges: region.back_edges().to_vec(),
            entry: region.entry(),
        };
        for _ in 0..nblocks {
            layout.state.push(state_block(&mut bcx));
        }
        // One exit block per *distinct target*: two exits landing on the same
        // guest RIP run the same store-and-chain sequence, so they share a block.
        for &va in &region.exit_targets() {
            let blk = state_block(&mut bcx);
            layout.exits.push((va, blk));
        }
        // The shared store-and-return block: the fault target (`MemEnv::exit`) and
        // the chain-table-miss target of every exit. It does **not** touch `rip` —
        // a fault's helper has already written the faulting guest IP, and a chain
        // miss has already had its target stored.
        let store_exit = state_block(&mut bcx);

        // ---- entry: load the whole guest file, once ------------------------
        let rflags_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_RFLAGS));
        let mut entry_gpr = [bcx.ins().iconst(types::I64, 0); 16];
        for (i, value) in entry_gpr.iter_mut().enumerate() {
            let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
            let p = bcx.ins().iadd_imm(ctx_ptr, off);
            *value = bcx.ins().load(types::I64, flags, p, 0);
        }
        let entry_rflags = bcx.ins().load(types::I64, flags, rflags_ptr, 0);

        // Memory descriptors, hoisted **once per region entry** instead of once
        // per hop — the same hoist `compile_block` performs, for the same reason
        // (they are invariant for the whole `run_compiled`).
        //
        // No super (bare-host-mem) path: that optimisation proves one block-wide
        // stack range on entry, and a region's blocks are entered from each other,
        // so "one block-wide guard" has no well-defined range. Regions use the
        // ordinary refill/pin probes for every operand.
        let (stack_pin, data_pins, refill) = if JitConfig::get().mem_inline_enabled() {
            let stack = Some(hoist_pin_slot(&mut bcx, ctx_ptr, flags, 0));
            let refill = Some(hoist_refill(&mut bcx, ctx_ptr, flags));
            let mut data = Vec::new();
            if JitConfig::get().mem_pin_enabled() {
                const IR_DATA_PIN_SLOTS: usize = 2;
                let end = 1_usize
                    .saturating_add(IR_DATA_PIN_SLOTS)
                    .min(super::PIN_SLOTS);
                for slot in 1..end {
                    data.push(hoist_pin_slot(&mut bcx, ctx_ptr, flags, slot));
                }
            }
            (stack, data, refill)
        } else {
            (None, Vec::new(), None)
        };

        let mut mem_env = MemEnv {
            ctx_ptr,
            load_ref,
            store_ref,
            string_ref: None,
            host_span_ref: None,
            f32_ref,
            f64_ref,
            sse_int_ref,
            sse_shift_ref,
            sse_pshufb_lo_ref,
            sse_pshufb_hi_ref,
            sse_fp_unop_ref,
            sse_fp_binop_ref,
            sse_cvt_ref,
            div64_ref,
            flags,
            guest_flags,
            exit: store_exit,
            ucrt_refs: [None; 7],
            stack_pin,
            data_pins,
            refill,
            super_stack: None,
        };

        // Guard chaining on the same knob the block path uses: with
        // `WIE_JIT_CHAIN=0` no chain edge exists anywhere, so the region's back
        // edge keeps codegen identical to that arm too.
        let inv_guard = JitConfig::get().chain_enabled();

        let header = layout.state[0];
        bcx.ins().jump(header, &exit_args(&entry_gpr, entry_rflags));

        // ---- one CLIF block per region block ------------------------------
        for (index, block_plan) in plan.iter().enumerate() {
            bcx.switch_to_block(layout.state[index]);
            let (mut gpr, mut rflags) = state_params(&bcx, layout.state[index]);
            let flag_state = emit_body(
                &mut bcx,
                block_plan,
                ctx_ptr,
                flags,
                &mut mem_env,
                &mut gpr,
                &mut rflags,
            )?;
            emit_term(
                &mut bcx,
                block_plan,
                index,
                &layout,
                ctx_ptr,
                flags,
                &gpr,
                rflags,
                rflags_ptr,
                flag_state.as_ref(),
                &trip,
                inv_guard,
            )?;
        }

        // ---- one exit block per distinct target ---------------------------
        //
        // The whole guest file is written back (module docs), then the exit is the
        // *same* edge a block terminator emits: `emit_chain_or_exit` stores `rip`,
        // runs the invalidation and depth guards, and either calls the compiled
        // successor or returns to the dispatcher.
        let exits = std::mem::take(&mut layout.exits);
        for &(va, blk) in &exits {
            bcx.switch_to_block(blk);
            let (gpr, rflags) = state_params(&bcx, blk);
            let href = chain
                .chain_func(va)
                .map(|fid| eng.module.declare_func_in_func(fid, bcx.func));
            let exit_rip = iconst_u64(&mut bcx, va);
            emit_chain_or_exit(
                &mut bcx,
                ctx_ptr,
                flags,
                &gpr,
                &ALL_GPRS,
                None,
                rflags,
                rflags_ptr,
                true,
                store_exit,
                exit_rip,
                href,
                lookup_ref,
                block_sig_ref,
                inv_guard,
                &trip,
            );
        }
        layout.exits = exits;

        // ---- the shared store-and-return block ----------------------------
        bcx.switch_to_block(store_exit);
        trip.flush(&mut bcx, ctx_ptr, flags);
        bcx.seal_block(store_exit);
        let (exit_gpr, exit_rflags) = state_params(&bcx, store_exit);
        for (i, value) in exit_gpr.iter().enumerate() {
            let off = i64::try_from(i.saturating_mul(8)).unwrap_or(0);
            let p = bcx.ins().iadd_imm(ctx_ptr, off);
            bcx.ins().store(flags, *value, p, 0);
        }
        bcx.ins().store(flags, exit_rflags, rflags_ptr, 0);
        bcx.ins().return_(&[]);

        // Sealed last: the back edge is emitted while the header still has a
        // predecessor to come, so no block may be sealed before its last edge
        // exists. `seal_all_blocks` is what `compile_block` ends with too.
        bcx.seal_all_blocks();
        bcx.finalize();
    }

    // The shape, taken while the function still exists: `define_function` +
    // `clear_context` below discard it, and with `opt_level = none` these counts
    // *are* the emitted code's op count (the emitter is the optimiser).
    #[cfg(test)]
    tests::record_emit_shape(eng.ctx.func.dfg.num_blocks(), eng.ctx.func.dfg.num_insts());
    tracing::debug!(
        start = format_args!("{:#x}", region.entry()),
        clif_blocks = eng.ctx.func.dfg.num_blocks(),
        clif_insts = eng.ctx.func.dfg.num_insts(),
        "jit loop region emitted"
    );

    dump_ir_if_requested("region", region.entry(), region.insn_count(), &eng.ctx.func);

    eng.module
        .define_function(func_id, &mut eng.ctx)
        .map_err(|e| LowerError::verifier(e.to_string()))?;
    eng.module.clear_context(&mut eng.ctx);
    eng.module
        .finalize_definitions()
        .map_err(|e| LowerError::verifier(e.to_string()))?;

    let code = eng.module.get_finalized_function(func_id);
    dump_map_if_requested(code as usize, region.entry(), region.insn_count(), tier);
    let func = unsafe { std::mem::transmute::<*const u8, unsafe extern "C" fn(*mut JitCtx)>(code) };

    // The guest byte range this function owns: the union of its blocks' extents.
    // A region is one *function* covering several blocks, so the range must be the
    // widest the compiled code can claim — range-selective invalidation on
    // `mem_write` uses it to choose which translations to drop, and a range that
    // stopped at the entry block would leave a loop compiled against patched bytes.
    let guest_end = region
        .blocks()
        .iter()
        .enumerate()
        .filter_map(|(i, _)| region.block_insns(i).last().map(|last| last.insn.next_ip()))
        .max()
        .unwrap_or(region.entry());

    Ok(CompiledBlock {
        func,
        func_id: Some(func_id),
        tier,
        insn_count: u32::try_from(region.insn_count()).unwrap_or(u32::MAX),
        guest_start: region.entry(),
        guest_end,
        inv_gen: inv_gen_baked,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    //! The emitted shape for **DOOM Retro's real hot loop**.
    //!
    //! `region_tests` pins what formation discovers about `0x140066570` (two
    //! blocks, 36 guest instructions, seven GPR live-ins, two exits, back edge
    //! `(1,0)`). This module pins the other half: that the *emitter* turns exactly
    //! that description into one Cranelift function, with the back edge as a
    //! `jump` and one exit block per recorded exit.
    //!
    //! It is the only place the emitter is driven on a real guest's real bytes
    //! rather than on a fixture — a live DOOM run compiles that loop too late to
    //! observe on every host (the guest exits during start-up here), so this test
    //! is how the claim "the shape the next round consumes" gets checked against
    //! the shape it produces.
    //!
    //! With `WIE_JIT_OPT=none` the emitter *is* the optimiser and nothing
    //! downstream removes a redundant op, which is why the assertions below count
    //! CLIF ops rather than trusting the intent.

    use super::*;
    use crate::RwxPerms;
    use crate::mem::GuestMemory;
    use std::cell::Cell;

    thread_local! {
        /// `(clif blocks, clif insts)` of the last region this *thread* emitted.
        ///
        /// Thread-local rather than a static because the wie-cpu suite runs tests in
        /// parallel and more than one of them emits a region; a plain static would
        /// make this assertion read another test's shape. Test-only: nothing on the
        /// production path needs the count (the `tracing::debug!` line carries it).
        static EMIT_SHAPE: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
    }

    /// Record the shape of the region just emitted (called from `compile_region`).
    pub(super) fn record_emit_shape(blocks: usize, insts: usize) {
        EMIT_SHAPE.with(|slot| slot.set(Some((blocks, insts))));
    }

    fn last_emit_shape() -> (usize, usize) {
        EMIT_SHAPE
            .with(std::cell::Cell::get)
            .expect("a region was emitted")
    }

    /// Guest base of the window these fixtures map.
    const BASE: u64 = 0x1_4006_6000;
    /// DOOM Retro's hot inner loop (`dec`/`lea` fixed-point column walk).
    const ENTRY: u64 = 0x1_4006_6570;
    /// Its second block.
    const SECOND: u64 = 0x1_4006_65C4;

    /// The PE bytes at guest VAs, for the one section that covers this window.
    ///
    /// A deliberately minimal reader: section headers only, no relocations, no
    /// imports. `wie-cpu` does not depend on `wie-pe` (one fixture is not worth the
    /// edge), and the full reader in `region_tests` would be a duplicate — this
    /// maps the single section containing `ENTRY` and nothing else.
    fn doom_window() -> Option<GuestMemory> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../real_exes/doomretro/doomretro.exe");
        let file = std::fs::read(&path).ok()?;
        let lfanew = u32::from_le_bytes(file.get(0x3C..0x40)?.try_into().ok()?) as usize;
        let coff = lfanew.checked_add(4)?;
        if file.get(lfanew..coff)? != b"PE\0\0" {
            return None;
        }
        let nsec = u16::from_le_bytes(file.get(coff + 2..coff + 4)?.try_into().ok()?);
        let size_opt = u16::from_le_bytes(file.get(coff + 16..coff + 18)?.try_into().ok()?);
        let opt = coff + 20;
        if u16::from_le_bytes(file.get(opt..opt + 2)?.try_into().ok()?) != 0x20B {
            return None;
        }
        let image_base = u64::from_le_bytes(file.get(opt + 24..opt + 32)?.try_into().ok()?);
        let sh = opt + usize::from(size_opt);

        let mut mem = GuestMemory::new();
        let size = 0x4000_usize;
        mem.map(BASE, size, RwxPerms::ALL)
            .expect("map the PE window");
        let mut bytes = vec![0_u8; size];
        for i in 0..usize::from(nsec) {
            let at = sh + i * 40;
            let vsize = u32::from_le_bytes(file.get(at + 8..at + 12)?.try_into().ok()?);
            let vaddr = u32::from_le_bytes(file.get(at + 12..at + 16)?.try_into().ok()?);
            let raw_size = u32::from_le_bytes(file.get(at + 16..at + 20)?.try_into().ok()?);
            let raw_ptr = u32::from_le_bytes(file.get(at + 20..at + 24)?.try_into().ok()?);
            let sec_start = image_base + u64::from(vaddr);
            let sec_span = u64::from(vsize.max(raw_size));
            let lo = sec_start.max(BASE);
            let hi = (sec_start + sec_span).min(BASE + size as u64);
            let Some(len) = hi.checked_sub(lo).and_then(|n| usize::try_from(n).ok()) else {
                continue;
            };
            if len == 0 {
                continue;
            }
            let in_section = usize::try_from(lo - sec_start).unwrap_or(usize::MAX);
            let file_at = usize::try_from(raw_ptr).ok()?.saturating_add(in_section);
            let dst_at = usize::try_from(lo - BASE).unwrap_or(usize::MAX);
            let (Some(dst), Some(src)) = (bytes.get_mut(dst_at..), file.get(file_at..)) else {
                continue;
            };
            let n = len
                .min(
                    usize::try_from(raw_size)
                        .unwrap_or(0)
                        .saturating_sub(in_section),
                )
                .min(src.len())
                .min(dst.len());
            dst[..n].copy_from_slice(&src[..n]);
        }
        mem.write(BASE, &bytes).expect("write the PE window");
        Some(mem)
    }

    /// DOOM Retro's loop, emitted: one function, two state blocks, two exits, one
    /// native back edge, and a block-parameter file rather than `JitCtx` traffic.
    #[test]
    fn doom_hot_loop_emits_one_function_with_a_native_back_edge() {
        let Some(mem) = doom_window() else {
            eprintln!("skip: real_exes/doomretro/doomretro.exe not present");
            return;
        };
        let region = form_region(&mem, None, ENTRY).expect("DOOM's hot loop forms a region");

        // The description this emitter is handed (also pinned by `region_tests`).
        assert_eq!(region.entry(), ENTRY);
        let starts: Vec<u64> = region.blocks().iter().map(|b| b.start()).collect();
        assert_eq!(starts, vec![ENTRY, SECOND], "two blocks, entry first");
        assert_eq!(region.insn_count(), 36, "25 + 11 guest instructions");
        assert_eq!(
            region.live_in().reg_count(),
            7,
            "RBX RBP RSI RDI R11 R14 R15"
        );
        assert_eq!(region.live_in().xmms().count(), 0, "no XMM live-in");
        assert!(
            !region.live_in().rflags(),
            "the `test` defines flags inside"
        );
        assert_eq!(region.back_edges(), &[(1, 0)]);
        assert_eq!(
            region.exit_targets(),
            vec![0x1_4006_65F0, 0x1_4006_65E5],
            "the `je`'s taken side and the `jl`'s fallthrough"
        );

        let mut eng = IsaEngine::new(OptTier::Base.opt_level()).expect("cranelift engine");
        let compiled = compile_region(&mut eng, &region, &|_| None, 0, OptTier::Base)
            .expect("DOOM's loop must be emittable, not merely discoverable");

        assert_eq!(compiled.insn_count, 36);
        assert_eq!(compiled.guest_start, ENTRY);
        assert_eq!(
            compiled.guest_end, 0x1_4006_65E5,
            "the region owns both blocks' bytes — up to the end of the `jl` that is \
             the second block's last instruction, which is past every byte of the \
             entry block — not just the entry block's 0x1_4006_65C4"
        );

        // Shape of the emitted function. Counted from the CLIF rather than
        // asserted from intent, because `opt_level = none` means the emitter is
        // the optimiser: 36 guest instructions stay 36 instructions' worth of
        // lowering, and the structural claims are the block count and the edges.
        let (blocks, insts) = last_emit_shape();
        eprintln!(
            "doom region ({ENTRY:#x}): clif blocks={blocks} clif_insts={insts} \
             guest_insns={} live_in={} exits={} back_edges={}",
            region.insn_count(),
            region.live_in().reg_count(),
            region.exit_targets().len(),
            region.back_edges().len()
        );
        // Structural floor: 1 entry + 2 region blocks + 2 exit blocks + 1 shared
        // store/return block = 6, plus one landing block for the `jl` back edge (it
        // is one side of a `jcc`, so its trip charge and the invalidation guard need
        // a block of their own) = 7. One block per region block and one per exit is
        // the shape; there is no splitting per guest instruction.
        assert!(
            blocks >= 7,
            "a 2-block loop with 2 exits and one native back edge is at least 7 \
             CLIF blocks; got {blocks}"
        );
        // Measured: 69 blocks / 1562 CLIF ops for the 36 guest instructions. The
        // block count is dominated by the *memory probe* CFG (each operand emits a
        // hit/miss stage chain, exactly as on the block path), not by the region
        // structure — which is why the ceilings below are generous and the point of
        // the assertions is regression detection, not a performance claim:
        //
        // * a translation that reintroduced one function per block would have to
        //   duplicate the whole 36-instruction lowering, blowing `insts`;
        // * duplicated exits (one per *exit* rather than one per *distinct target*,
        //   or a copy per leaving block) would show up as 17 more stores each.
        //
        // `opt_level = none` means these are real emitted ops, so a regression here
        // is a code-size and dispatch-cost regression, not just a tidy-up.
        assert!(
            blocks <= 80,
            "measured 69 CLIF blocks for DOOM's loop; {blocks} means the region \
             structure grew"
        );
        assert!(
            insts <= 1800,
            "measured 1562 CLIF ops for 36 guest instructions; {insts} means new \
             per-instruction or per-exit work was added"
        );
    }
}
