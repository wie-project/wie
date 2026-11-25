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
//! * **The caller names the tier, and it names the one the block path would have
//!   picked.** `pipeline.rs` asks `JitShared::decide_tier` for the RIP, whose rule
//!   is "a self-loop earns `speed`", so a region over a *one-block* loop folds a
//!   `speed` block into a `speed` region and a multi-block loop's region compiles
//!   at the base tier. That matters because the `speed` tier has a measurement
//!   behind it (`long_loop` repays it 1.37x) and a `Base` region replacing such a
//!   block would be trading measured code for unmeasured code. This module still
//!   never decides a tier for itself — it does not know which of its blocks is a
//!   loop header and which is a body.
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
use super::flags::{FlagState, decompose_packed, iconst_u64, pack_once};
use super::insn::{PendingFlags, flush_pending_deferred, lower_insn};
use super::mem::{hoist_pin_slot, hoist_refill};
use super::region::{
    GuestInsn, LoopRegion, RegionLimits, RegionRefusal, form_loop_region, form_self_loop_region,
};
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
/// block: `gpr[0..16]`, in [`exit_args`]' order, so [`writeback_gprs`] — written
/// for the block path's exit block — serves the region's exits unchanged.
const GPR_PARAMS: usize = 16;

/// The block-parameter arity of a state block: the sixteen GPRs, the
/// architectural RFLAGS word, and — only when [`JitConfig::ssa_flags_enabled`] —
/// the six flag fields.
///
/// # Why the word *and* the fields
///
/// The word is not the flags carrier any more, but it is still needed: when SSA
/// flags are on, `FlagState::pack` takes DF, IF and the reserved bit from a word
/// (`FlagState::old`), and nothing inside a region writes those bits, so one
/// carried word is exact for every pack the region can perform. When SSA flags are
/// off the word *is* the carrier, which is why the six field parameters are
/// conditional rather than unconditional: `WIE_JIT_SSA_FLAGS=0` must produce the
/// same function as before this change.
///
/// # Why the six fields, and not a packed word
///
/// Measured, not argued. The region used to hand its successor a *packed word*:
/// `pack_once` at every region-internal edge, then `decompose` at the target's
/// entry. On a one-block loop that is a pack and a decode **per iteration**, and
/// it was the whole of the region's per-iteration deficit against the block path
/// (171 host instructions against 156 on DOOM's scaled blit `0x1800d8fb0`, with
/// the pack built inline at `0x1559405ec`-`0x155940620`). The block path never
/// pays it because its exit pack happens once per *exit*; the region was paying it
/// once per back edge. Carrying the six fields as parameters removes both halves:
/// nothing inside the region builds a word, and no block decodes one. The fields
/// already exist as SSA values at the end of every body
/// (`flush_pending_deferred` assigns them), so the edge costs no instructions —
/// only parameters.
const fn state_param_count(ssa_flags: bool) -> usize {
    if ssa_flags {
        GPR_PARAMS + 1 + 6
    } else {
        GPR_PARAMS + 1
    }
}

/// The region's state, as one edge hands it to the next block.
///
/// `fields` is `[zf, sf, cf, of, pf, af]` — [`FlagState`]'s own field order — and
/// they are **materialised**: `pf` is `FlagState::pf_value`'s result, not the
/// deferred recipe, because a block parameter cannot carry a recipe.
#[derive(Clone, Copy)]
struct Carry {
    gpr: [Value; 16],
    /// The architectural word. With SSA flags this is a DF/IF/reserved carrier
    /// (`FlagState::old`), *not* the six tracked bits; without SSA flags it is the
    /// carrier.
    word: Value,
    fields: Option<[Value; 6]>,
}

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
///
/// Two formations, in this order:
///
/// 1. [`form_loop_region`] — the SCC fold, which is the *general* answer and must
///    get first refusal. A block whose terminator comes back to itself can still
///    sit on a bigger foldable cycle (`B: jne C; C: jmp B` folds as the two-block
///    region `{B, C}`), and formation's rule that a region must not shrink makes
///    the bigger fold the right one whenever it exists. Trying the one-block form
///    first would silently take `{B}` and leave `C` on the block path.
/// 2. [`form_self_loop_region`] — the rescue, for exactly the shapes the SCC fold
///    *cannot* express. A self-loop nested inside a larger loop has, by
///    construction, a cycle in its component that does not pass through the entry,
///    so the SCC fold refuses `Irreducible`. That is the shape of the two hottest
///    blocks in the corpus: DOOM's scaled blit `0x1800d8fb0` (54.1 % of guest
///    time, refused `Irreducible { at: 0x1800d8fca }`) and `long_loop`'s inner
///    loop `0x140001040` (refused `Irreducible { at: 0x140001075 }`).
///
/// `allow_self_loop` gates a one-block region (`WIE_JIT_REGION_SELF_LOOP`,
/// default **off**) — the *shape*, whichever formation produced it. A one-block
/// self-loop carries a per-iteration tax the shape cannot amortise and **loses on
/// the hot block**: DOOM's blit `0x1800d8fb0` is 171 host instructions per
/// iteration as a region against 156 as a block, at 54.1 % of guest time. It is
/// neutral within noise on `long_loop` (median ~323 armed vs ~330 gated, quiet
/// host); the 4.5x at `4991913` and 6.3x before its probe hoist are superseded
/// builds, not this one. So it is opt-in while multi-block regions — which are
/// neutral to 1.45x *faster* — are not. With the gate closed this function is
/// what it was before the one-block form existed.
///
/// Which refusal gets reported when both fail: the self-loop form's, unless it
/// answered [`RegionRefusal::NoBackEdge`] — "this block does not branch to
/// itself" is a precondition failure and says nothing about the loop, so the SCC
/// fold's answer is what the caller wants. Every other self-loop refusal comes
/// from the *same* checks the SCC form runs ([`check_block`], the same bounds) and
/// names the actual defect: a self-loop with a call inside reports
/// [`RegionRefusal::Call`], where the SCC fold would only get as far as
/// `Irreducible` about the enclosing loop.
pub(super) fn form_region(
    mem: &GuestMemory,
    hooks: Option<&HookWindow>,
    rip: u64,
    allow_self_loop: bool,
) -> Result<LoopRegion, RegionRefusal> {
    let limits = RegionLimits::default();
    let region = match form_loop_region(mem, hooks, rip, &limits) {
        Ok(region) => region,
        Err(scc) => match form_self_loop_region(mem, hooks, rip, &limits) {
            Ok(region) => region,
            // Not a self-loop: the precondition, not a defect, and the SCC fold's
            // answer is the diagnosis of the loop as a whole.
            Err(RegionRefusal::NoBackEdge) => return Err(scc),
            // A self-loop refused for a real reason. It reached checks the SCC
            // fold never got to, so its refusal is the specific one.
            Err(reason) => return Err(reason),
        },
    };
    // The one-block shape is gated on the **shape**, not on which formation
    // produced it — `WIE_JIT_REGION_SELF_LOOP` off (the default) refuses it
    // whichever way it was found.
    //
    // That distinction is not pedantic: the SCC fold already accepts a *pure*
    // self-loop (`block_count == 1 && succ[0].contains(0)`), so gating only the
    // new formation would leave the trap half-open. Measured on a bounded DOOM
    // run with the gate closed but keyed on the formation: 116 of 200 regions
    // were still one-block. A one-block region carries a per-iteration flag
    // round trip (~32 CLIF ops: `decompose_packed` at every block entry,
    // `flush_pending` at every block exit) and the register allocator's spill
    // traffic for the 17-wide `STATE_PARAMS`; a loop whose body *is* one block
    // has nothing to amortise that over, and on the block that matters it loses:
    // DOOM's blit `0x1800d8fb0` is 171 host instructions per iteration as a
    // region against 156 as a block. (`long_loop` is neutral within noise —
    // median ~323 ms armed vs ~330 gated on a quiet host; the 4.5x at `4991913`
    // and 6.3x before that commit's probe hoist are superseded builds.)
    // Multi-block regions are untouched — `chain_hops` is 1.45x *faster*.
    if !allow_self_loop && region.blocks().len() == 1 {
        return Err(RegionRefusal::SelfLoopGatedOff { at: region.entry() });
    }
    Ok(region)
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
    for _ in 0..GPR_PARAMS + 1 {
        bcx.append_block_param(blk, types::I64);
    }
    if JitConfig::get().ssa_flags_enabled() {
        // The six fields are `FlagState`'s own type: a Cranelift `i1` is an I8
        // holding 0 or 1, and `select_flag` widens it at the pack. Declaring them
        // I64 would be a signature mismatch on every edge that passes one.
        for _ in 0..6 {
            bcx.append_block_param(blk, types::I8);
        }
    }
    blk
}

/// A fresh CLIF block in the **word** convention — `gpr[0..16]` then the
/// architectural RFLAGS word, which is `exit_args`' arity.
///
/// This is the shape `emit.rs` insists on: `emit_chain_or_exit` ends in
/// `jump_exit`, which passes exactly seventeen arguments to the block a region
/// designates as its store-and-return target. So the exit blocks and that shared
/// target keep the block path's convention unchanged, and the six carried flag
/// fields exist only *inside* the region. The pack therefore happens on the edge
/// into an exit block — once per region exit, which is the block path's count.
fn word_block(bcx: &mut FunctionBuilder<'_>) -> Block {
    let blk = bcx.create_block();
    for _ in 0..GPR_PARAMS + 1 {
        bcx.append_block_param(blk, types::I64);
    }
    blk
}

/// The parameters of a [`word_block`]: the file and the architectural word.
fn word_params(bcx: &FunctionBuilder<'_>, blk: Block) -> ([Value; 16], Value) {
    let params = bcx.block_params(blk);
    let mut gpr = [params[0]; GPR_PARAMS];
    gpr.copy_from_slice(&params[..GPR_PARAMS]);
    (gpr, params[GPR_PARAMS])
}

/// The state block parameters of `blk`, as a [`Carry`].
fn state_params(bcx: &FunctionBuilder<'_>, blk: Block) -> Carry {
    let params = bcx.block_params(blk);
    let mut gpr = [params[0]; GPR_PARAMS];
    gpr.copy_from_slice(&params[..GPR_PARAMS]);
    let word = params[GPR_PARAMS];
    let fields = if JitConfig::get().ssa_flags_enabled() {
        let f = &params[GPR_PARAMS + 1..];
        Some([f[0], f[1], f[2], f[3], f[4], f[5]])
    } else {
        None
    };
    Carry { gpr, word, fields }
}

/// The block arguments that hand `carry` to a state block.
fn carry_args(carry: &Carry) -> Vec<BlockArg> {
    let mut args = Vec::with_capacity(state_param_count(carry.fields.is_some()));
    args.extend(carry.gpr.iter().map(|&v| BlockArg::Value(v)));
    args.push(BlockArg::Value(carry.word));
    if let Some(f) = carry.fields {
        args.extend(f.iter().map(|&v| BlockArg::Value(v)));
    }
    args
}

impl Carry {
    /// The SSA flag state these parameters represent, or `None` when the word is
    /// the only authority (`WIE_JIT_SSA_FLAGS=0`).
    ///
    /// `packed: None` is the point: these fields are *values*, not a memo of a
    /// word, so a pack has to rebuild — which is exactly what every pack in the
    /// region already did for a block whose entry decode narrowed the mask.
    fn state(self) -> Option<FlagState> {
        self.fields.map(|f| FlagState {
            zf: f[0],
            sf: f[1],
            cf: f[2],
            of: f[3],
            pf: f[4],
            af: f[5],
            pf_src: None,
            old: self.word,
            packed: None,
        })
    }

    /// Turn a body's leaving state into the parameters its edges pass.
    ///
    /// Costs no instructions for the GPRs and none for five of the six flags: the
    /// values already exist. It costs the PF ladder when the last writer deferred
    /// PF (`FlagState::pf_src`), because a deferred parity recipe cannot be a
    /// block parameter — a pack would have paid the same ladder, so this is a move
    /// rather than an addition, and it is why the arithmetic below does not change.
    fn out(
        bcx: &mut FunctionBuilder<'_>,
        gpr: &[Value; 16],
        rflags: Value,
        state: Option<&FlagState>,
    ) -> Self {
        match state {
            None => Self {
                gpr: *gpr,
                word: rflags,
                fields: None,
            },
            Some(fs) => Self {
                gpr: *gpr,
                // `old` is the word these fields were last consistent with, so it
                // carries DF, IF, bit 1 and the reserved bits for every pack
                // downstream — and nothing inside a region changes those except
                // `cld`/`std`/`popfq`, which `FlagState::resync` re-seats.
                word: fs.old,
                fields: Some([fs.zf, fs.sf, fs.cf, fs.of, fs.pf_value(bcx), fs.af]),
            },
        }
    }
}

/// The architectural word for a carry that has to leave the SSA state: an exit
/// writeback, the shared store-and-return block, or the invalidation bailout on a
/// back edge.
///
/// One pack, on a path that runs once per region *exit* — the same count the block
/// path pays.
fn carry_word(bcx: &mut FunctionBuilder<'_>, carry: &Carry) -> Value {
    match carry.state() {
        None => carry.word,
        Some(fs) => {
            let mut cache = None;
            pack_once(bcx, &mut cache, Some(&fs), carry.word)
        }
    }
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
#[allow(clippy::too_many_arguments)]
fn emit_body(
    bcx: &mut FunctionBuilder<'_>,
    plan: &BlockPlan<'_>,
    ctx_ptr: Value,
    flags: MemFlagsData,
    mem: &mut MemEnv,
    carry: &Carry,
    gpr: &mut [Value; 16],
    rflags: &mut Value,
) -> Result<Option<FlagState>, LowerError> {
    // The flag state a block starts with comes from the edge that entered it —
    // six block parameters, not a word to decode.
    //
    // This is where the region's per-iteration flag round trip used to live. It
    // was an unconditional `decompose_packed` at every block entry, and a
    // narrowed-mask variant of it (the same predicate the block path applies with
    // `analysis::entry_decode_mask`) for as long as the mask was a per-block
    // question: both only ever mattered because the *predecessor* had packed a
    // word the block then had to take apart again. The mask narrowing was measured
    // on `0x1800d8fb0` and refuted — the block ends with a `shr`, and
    // `entry_decode_mask` bails to the full six bits for any writer `defines_all_six`
    // does not admit, so the narrowed decode was byte-identical to the wide one.
    // With the fields carried as parameters neither shape exists: the only
    // decode in a region is the one in the prologue, once per region *entry*.
    //
    // `None` (the `WIE_JIT_SSA_FLAGS=0` arm) means the word is the authority, and
    // `carry.word` is that word.
    let mut flag_state = carry.state();
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
    // Assign any deferred ALU flags to the state — but **do not pack**. The
    // carrier is materialised by the edge that actually needs a word
    // (`emit_term`'s `pack_once`), which is the discipline `emit.rs` adopted in
    // `a2c499f` and the region path had kept the old eager shape of.
    //
    // Why the old shape cost per iteration: `flush_pending` here built the word
    // out of the six flag bits at the end of *every* block body, and then the
    // block's own `jcc` terminator — the usual consumer — read those same six
    // bits from the state, not from the word. So a self-looping region packed a
    // word per iteration that the iteration never read, and the next iteration's
    // entry did the inverse decode. Measured on DOOM's scaled blit
    // (`0x1800d8fb0`): 171 host instructions per iteration as a region against 156
    // as a block, with the pack built inline at `0x1559405ec`-`0x155940620`.
    //
    // Deferring is safe for the same reason it is safe on the block path: every
    // place a word is observable (the exit writeback, the invalidation bailout,
    // the back-edge hand-off) goes through `pack_once`, and the fault edge already
    // carries the *state* (`MemEnv::fault_flags` + `fault_pending`) and packs it
    // in the fault block, where a fault actually lands.
    flush_pending_deferred(bcx, rflags, &mut pending, &mut flag_state);
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
    carry: &Carry,
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
        // dispatcher at the loop header and leave. This is the one place on a back
        // edge that needs a *word*, so it is the one place that builds one — and
        // it is cold (a generation bump, not an iteration).
        let word = carry_word(bcx, carry);
        writeback_gprs(
            bcx, ctx_ptr, flags, &carry.gpr, &ALL_GPRS, None, word, rflags_ptr, true,
        );
        let rip_ptr = bcx.ins().iadd_imm(ctx_ptr, i64::from(OFF_RIP));
        let rip = iconst_u64(bcx, layout.entry);
        bcx.ins().store(flags, rip, rip_ptr, 0);
        trip.flush(bcx, ctx_ptr, flags);
        bcx.ins().return_(&[]);

        bcx.switch_to_block(cont_blk);
        bcx.seal_block(cont_blk);
    }
    bcx.ins().jump(header, &carry_args(carry));
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
    carry: &Carry,
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
        /// The target is an **exit** block, which takes the word convention
        /// (`gpr`, `rflags`) rather than the region's state parameters.
        to_exit: bool,
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
                    to_exit: false,
                }
            }
            Dest::Leave(_) => Edge {
                branch_to: target,
                work: None,
                to_exit: true,
            },
            Dest::Inside(_) => Edge {
                branch_to: target,
                work: None,
                to_exit: false,
            },
        })
    }

    /// The arguments one edge passes: nothing through a landing block (it reads
    /// the values this block leaves, which dominate it), the six carried fields to
    /// a region block, and a packed word to an exit block — the one place in the
    /// region a word is built, once per exit.
    fn args_for(bcx: &mut FunctionBuilder<'_>, carry: &Carry, e: &Edge) -> Vec<BlockArg> {
        if e.work.is_some() {
            Vec::new()
        } else if e.to_exit {
            exit_args(&carry.gpr, carry_word(bcx, carry)).to_vec()
        } else {
            carry_args(carry)
        }
    }

    // `leave` fills a landing block: charge, guard, then jump to the header.
    fn work(
        bcx: &mut FunctionBuilder<'_>,
        landing: Block,
        header: Block,
        ctx_ptr: Value,
        flags: MemFlagsData,
        carry: &Carry,
        rflags_ptr: Value,
        trip: &TripCounter,
        inv_guard: bool,
        layout: &Layout,
    ) {
        bcx.switch_to_block(landing);
        bcx.seal_block(landing);
        emit_back_edge(
            bcx, ctx_ptr, flags, carry, rflags_ptr, trip, inv_guard, layout, header,
        );
    }

    // **No pack on an internal edge.** The body leaves the six flag fields as SSA
    // values, and the edge passes them as block parameters instead of building a
    // word the target would immediately take apart again.
    //
    // The pack lives where a word is actually observable, and only there: an exit
    // block (`carry_word`, once per region exit), the shared store-and-return
    // block, and the cold invalidation bailout on a back edge. That is the block
    // path's shape and its cost; the region used to pay a pack *per iteration* on
    // top of it, which is what made the scaled blit 171 host instructions per
    // iteration as a region against 156 as a block.
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
                        bcx, landing, header, ctx_ptr, flags, carry, rflags_ptr, trip, inv_guard,
                        layout,
                    );
                }
                None => {
                    let args = args_for(bcx, carry, &e);
                    bcx.ins().jump(e.branch_to, &args);
                }
            }
        }
        Some(BlockTerm::Jcc { mnemonic, .. }) => {
            // The branch reads the *state* (`flag_cond_fs`); no word is built for
            // it, and none is built for either side.
            let cond = emit_cond(bcx, flag_state, mnemonic, rflags)?;
            // Both sides carry the identical state: a `jcc` changes no register.
            let taken = edge(bcx, plan.dests[0], index, layout)?;
            let not_taken = edge(bcx, plan.dests[1], index, layout)?;
            let t_args = args_for(bcx, carry, &taken);
            let n_args = args_for(bcx, carry, &not_taken);
            let (t_branch, n_branch) = (taken.branch_to, not_taken.branch_to);
            bcx.ins().brif(cond, t_branch, &t_args, n_branch, &n_args);
            for e in [taken, not_taken] {
                if let Some((landing, header)) = e.work {
                    work(
                        bcx, landing, header, ctx_ptr, flags, carry, rflags_ptr, trip, inv_guard,
                        layout,
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
// One argument per thing the caller already holds: the engine, the guest memory
// guard, the hook window, the RIP, the chain lookup, the generation bake, the
// tier and the self-loop gate. Bundling them would be a struct built at the one
// call site, and every one of them is already borrowed there — same shape and the
// same allowance as `emit_term` / `lower_term`.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit) fn try_compile_region(
    eng: &mut IsaEngine,
    mem: &GuestMemory,
    hooks: Option<&HookWindow>,
    rip: u64,
    chain: &dyn ChainLookup,
    inv_gen_baked: u64,
    tier: OptTier,
    allow_self_loop: bool,
) -> Option<CompiledBlock> {
    let region = match form_region(mem, hooks, rip, allow_self_loop) {
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
                tier = ?tier,
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
            let blk = word_block(&mut bcx);
            layout.exits.push((va, blk));
        }
        // The shared store-and-return block: the fault target (`MemEnv::exit`) and
        // the chain-table-miss target of every exit. It does **not** touch `rip` —
        // a fault's helper has already written the faulting guest IP, and a chain
        // miss has already had its target stored.
        let store_exit = word_block(&mut bcx);

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
            fault_flags: None,
            fault_pending: PendingFlags::None,
        };

        // Guard chaining on the same knob the block path uses: with
        // `WIE_JIT_CHAIN=0` no chain edge exists anywhere, so the region's back
        // edge keeps codegen identical to that arm too.
        let inv_guard = JitConfig::get().chain_enabled();

        let header = layout.state[0];
        // The one decode a region performs: the architectural word into the six
        // carried fields, **once per region entry** rather than once per block
        // entry or per back edge. `decompose_packed` is the full six-bit cone and
        // the right shape here — the entry word is authoritative and nothing has
        // been written yet, so it is also a valid `packed` memo.
        let entry_carry = if JitConfig::get().ssa_flags_enabled() {
            let fs = decompose_packed(&mut bcx, entry_rflags);
            Carry {
                gpr: entry_gpr,
                word: entry_rflags,
                fields: Some([fs.zf, fs.sf, fs.cf, fs.of, fs.pf, fs.af]),
            }
        } else {
            Carry {
                gpr: entry_gpr,
                word: entry_rflags,
                fields: None,
            }
        };
        bcx.ins().jump(header, &carry_args(&entry_carry));

        // ---- one CLIF block per region block ------------------------------
        for (index, block_plan) in plan.iter().enumerate() {
            bcx.switch_to_block(layout.state[index]);
            let carry = state_params(&bcx, layout.state[index]);
            let mut gpr = carry.gpr;
            let mut rflags = carry.word;
            let flag_state = emit_body(
                &mut bcx,
                block_plan,
                ctx_ptr,
                flags,
                &mut mem_env,
                &carry,
                &mut gpr,
                &mut rflags,
            )?;
            // The state this block hands on: `flush_pending_deferred` left the
            // six fields as SSA values, so an internal edge costs parameters and
            // no instructions.
            let out = Carry::out(&mut bcx, &gpr, rflags, flag_state.as_ref());
            emit_term(
                &mut bcx,
                block_plan,
                index,
                &layout,
                ctx_ptr,
                flags,
                &out,
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
            let (gpr, rflags) = word_params(&bcx, blk);
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
        let (exit_gpr, exit_rflags) = word_params(&bcx, store_exit);
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
    #[cfg(test)]
    tests::record_emit_dump(&format!("{}", eng.ctx.func.display()));
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

    thread_local! {
        /// The CLIF text of the last region this thread emitted.
        ///
        /// Same reason as `EMIT_SHAPE`: per-thread, test-only, and the only way a
        /// test can attribute ops to *blocks* rather than to a function total. A
        /// region's total op count mixes three very different costs — the entry
        /// decode (once per region entry), the loop body (per iteration) and the
        /// exit/bailout packs (per exit) — and a change that moves work between
        /// them can raise the total while making the loop faster. That is exactly
        /// what the flag-carrier change does, so the attribution has to exist.
        static EMIT_DUMP: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }

    pub(super) fn record_emit_dump(text: &str) {
        EMIT_DUMP.with(|slot| slot.borrow_mut().clear());
        EMIT_DUMP.with(|slot| slot.borrow_mut().push_str(text));
    }

    /// Per-block CLIF op counts of the last region emitted, in block order.
    ///
    /// Counts instructions only: Cranelift's display also prints the
    /// `v123 -> v456` block-argument bookkeeping lines, which are not ops and
    /// which inflated the first version of this measurement by 17-28 per block.
    fn last_emit_per_block() -> Vec<(String, usize)> {
        let text = EMIT_DUMP.with(|slot| slot.borrow().clone());
        let mut out: Vec<(String, usize)> = Vec::new();
        for line in text.lines() {
            if line.starts_with("block") {
                let name = line.split('(').next().unwrap_or("?").trim_end_matches(':');
                out.push((name.to_string(), 0));
            } else if line.starts_with("    ")
                && !line.contains(" -> ")
                && let Some(last) = out.last_mut()
            {
                last.1 += 1;
            }
        }
        out
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
        let region = form_region(&mem, None, ENTRY, true).expect("DOOM's hot loop forms a region");

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
        // Attribution, not a total: the two region blocks are the per-iteration
        // path, `block0` is the entry decode (once per region entry) and the exit
        // blocks run once per exit. A flag-carrier change that moves a decode out
        // of the loop and into the entry *raises* `clif_insts` while making the
        // loop cheaper, so the total cannot be the assertion.
        let per_block = last_emit_per_block();
        eprintln!(
            "doom region per-block ops: {:?}  (iteration path = first {} region blocks)",
            per_block,
            region.blocks().len()
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
