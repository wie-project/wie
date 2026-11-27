//! Loop-region formation: from decoded guest blocks to one compilable loop region.
//!
//! Today the JIT emits **one Cranelift function per basic block** and chains
//! them: every hop loads the guest registers the successor reads out of
//! [`JitCtx`](super::JitCtx), runs a handful of instructions, and stores the live
//! registers back. DOOM Retro's hottest loop is two blocks (25 + 11 guest
//! instructions at `0x140066570` / `0x1400665c4`) that hop once per iteration,
//! and the measured cost is **39.8 host instructions per guest instruction** —
//! the per-hop load-in/store-back and the chain bookkeeping dominate the loop
//! body itself.
//!
//! This module is the layer underneath region compilation: it answers *which
//! blocks form one loop, what does that region read and write, and where does it
//! leave to*. It emits no IR: [`super::region_emit`] consumes the [`LoopRegion`]
//! it describes and emits one Cranelift function from it, behind
//! `WIE_JIT_REGION` (default off).
//!
//! # Two formations, one shape
//!
//! [`form_loop_region`] folds the **strongly connected component** of the start —
//! the general answer, and the one to reach for first, because it takes the whole
//! loop. [`form_self_loop_region`] folds the **single block** whose own terminator
//! comes back to its entry, and exists for exactly the shapes the SCC fold cannot
//! express: a self-loop nested inside a larger loop has a cycle in its component
//! that does not pass through the entry, so an SCC fold refuses it `Irreducible`.
//! Both are refused-safe by construction — neither can return a partial region —
//! and `region_emit::form_region` tries them in that order (see its docs for why
//! the SCC fold must get first refusal: a block that branches to itself can also
//! sit on a bigger foldable cycle, and a region must not shrink).
//!
//! # What a region is
//!
//! Given the guest VA of a hot block, [`form_loop_region`] explores the blocks
//! reachable from it — following only successors that decode as lowerable
//! [`BlockKind::Pure`] blocks — and then keeps the **strongly connected component
//! containing the start**: the intersection of everything reachable from the
//! start with everything that can reach it. That set is the candidate region:
//!
//! * every edge *inside* it is region-internal, so the set is closed under
//!   execution: once control enters the region only region blocks run, until an
//!   exit edge fires;
//! * a non-trivial SCC (or a self-loop) contains at least one cycle, hence at
//!   least one back edge — the thing that makes it a *loop* region and not just a
//!   set of blocks;
//! * every edge leaving the set is an **exit**, recorded with the guest RIP it
//!   lands on ([`RegionExit`]);
//! * the entry is the requested start VA, and every cycle in the set returns
//!   through it (soundness rule 4 below).
//!
//! Two consequences worth stating. Edges into the start from blocks the
//! exploration never saw are not a problem: they enter the region at its entry,
//! which is where the region compiler establishes entry state. And an explored
//! block *outside* the region cannot enter it at all — that follows from the SCC
//! definition (proof under rule 4 below), so it is asserted rather than checked.
//!
//! # Bounded by construction
//!
//! Exploration and the region itself are both bounded by [`RegionLimits`]: blocks
//! explored, blocks in the region, guest instructions in the region, and distinct
//! exit targets. Hitting any limit **refuses** the region ([`RegionRefusal`])
//! instead of truncating it. A partial region would be a miscompile, and a
//! truncation that happens to compile is the worst possible outcome.
//!
//! # Soundness rules (each enforced here, each pinned by a test)
//!
//! A region is refused when it would contain, or depend on, any of:
//!
//! 1. **A call or a return** ([`RegionRefusal::Call`] / [`RegionRefusal::Return`]).
//!    Both leave the native frame: a call pushes a return address and runs an
//!    unknown target, a return pops a runtime RIP. A region's premise is that
//!    guest state lives in SSA values across the back edge, which a call breaks.
//!    A `call` block is explored through its `return_ip` edge precisely so a call
//!    sitting *inside* a cycle is refused by name, instead of silently shrinking
//!    the loop into a set with no back edge.
//!    Pinned by `a_region_containing_a_call_is_refused`.
//! 2. **A string op** ([`RegionRefusal::StringOp`]): `MOVS`/`STOS`/`LODS`/`SCAS`/
//!    `CMPS` need the host helper for a dynamic exit RIP and an atomic bulk
//!    transfer. They are a whole-block feature, not a region body instruction.
//!    Pinned by `a_region_containing_a_string_op_is_refused`.
//! 3. **A memory operand whose address the refill probe cannot guard**
//!    ([`RegionRefusal::UnguardableAddress`]). The emitted probe
//!    ([`refill_tlb_probe`](super::mem::refill_tlb_probe)) guards *any* correctly
//!    computed 64-bit effective address: a miss, a cross-page access or a denied
//!    page falls through to `wie_jit_load` / `wie_jit_store`, which report the
//!    fault. What it cannot guard is an address the emitter computes *wrongly*,
//!    and today that is exactly one shape: an **address-size override** (a 32-bit
//!    base or index in 64-bit mode), because
//!    [`effective_addr`](super::gpr::effective_addr) adds the full 64-bit register
//!    values — so the probe would be handed, and would happily "guard", a
//!    different address than the guest meant. Inside a single block that is a
//!    pre-existing hazard outside this lane; here it is a named refusal. Pinned by
//!    `a_region_with_an_unguardable_register_operand_is_refused`.
//! 4. **A cycle inside the region that does not pass through the entry**
//!    ([`RegionRefusal::Irreducible`]). The region is one compilation unit with one
//!    entry and one set of entry live-ins, and that is only a loop *fold* if every
//!    iteration comes back around through the entry. A cycle that avoids the entry
//!    means the region is either an irreducible control-flow graph (several
//!    headers, no dominant header — the classic `A→{B,C}`, `{B,C}→D`, `D→{B,C}`
//!    shape) or a nested loop whose header is not our entry. Both are refused
//!    *by this formation*; the second is what [`form_self_loop_region`] exists for
//!    when the entry is itself the nested loop's header. Pinned by
//!    `an_irreducible_cfg_is_refused` and
//!    `a_nested_self_loop_is_irreducible_for_the_scc_fold_and_a_region_for_the_self_loop_form`.
//!
//!    The closely related rule from the brief — *no edge from outside the region
//!    into a non-entry block* — is **provable rather than checkable** here, and
//!    therefore asserted instead of checked. Given the region is the SCC of the
//!    start inside the start's forward closure: if an explored block `X` had an
//!    edge into a region block `r`, then `start →* X` (explored), `X → r`, and
//!    `r →* start` (SCC), so `X` sits on a cycle through `start` and is inside the
//!    SCC — a contradiction. What this cannot see is an edge from a block that is
//!    **not** reachable from the start (a sibling predecessor of an interior
//!    block); it is not an entry into this compilation unit, because the emitted
//!    region is keyed by its entry VA and the dispatcher never enters it
//!    mid-region, but it does mean the same guest code can also be compiled
//!    separately at that VA. `the_region_has_no_entry_but_its_own` pins the
//!    provable half over every fixture in the suite.
//!
//! **Not** checked here, deliberately: **self-modifying code and invalidation**.
//! Those are the caller's problem, but here is what the region compiler will have
//! to check, because a region lives far longer than a block:
//!
//! * **At region entry**: the bytes of *every* region block still decode to the
//!   instructions the region was compiled from. A block-level compile gets this
//!   from the `invalidate_gen` snapshot taken before its bytes are read; a region
//!   spans several blocks, so one generation check at entry has to cover all of
//!   them, and a mismatch means re-forming the region, not re-entering it.
//! * **At region entry**: the per-block guards the block path emits today — the
//!   block-wide stack pin range ([`emit_block_wide_stack_guard`]) and the `mem_gen`
//!   a [`RefillSlot`](super::RefillSlot) tag was resolved under. A pin or slot that
//!   goes stale mid-region cannot be caused by the guest from inside a call-free
//!   region, but a host helper can cause it, so the exit path must re-stamp
//!   `mem_gen` the way the block path does.
//! * **At region exit**: store back exactly the registers the exit's live-out set
//!   needs, set `JitCtx::rip` to the exit target, and charge `JitCtx::insn_acc`
//!   for the instructions actually retired. Skipping any of the three
//!   is silently wrong, not slow.
//!
//! # How a region becomes ONE Cranelift function
//!
//! This is the shape the next round emits, written down here because the
//! description below is exactly what that emitter consumes:
//!
//! ```text
//!  entry block:                      ; reached only at `region.entry()`
//!    load live-ins from JitCtx       ; region.live_in() — one load each, once
//!    br loop_header                  ; (RFLAGS only if live_in.rflags())
//!
//!  loop_header:                      ; one CLIF block per region block, in
//!    [block 0 body]                  ;   region.blocks() order; guest GPR/XMM
//!    brif cond, loop_header, exit_N  ;   state is SSA `Value`s here
//!
//!  exit_N:                           ; one CLIF block per region exit
//!    store live-out to JitCtx        ; only what that exit's successors read
//!    set rip = exit.target()
//!    jump to the chain/dispatcher    ; the exit edge, unchanged from today
//! ```
//!
//! The win is structural: `live_in()` is loaded **once per region entry** and the
//! live-out stored **once per region exit**, instead of once per block hop. For
//! DOOM's two-block loop, block 1's load-in/store-back disappears entirely — and
//! that per-hop cost is what makes the loop 39.8 host instructions per guest
//! instruction today.
//!
//! ## What has to be true for the native back edge to be safe
//!
//! A back edge that stays inside the native frame means the guest register file in
//! `JitCtx` is **stale for the duration of the loop**. That is sound only if:
//!
//! 1. **Nothing inside the region reads `JitCtx`'s GPR/XMM file.** Every read is
//!    served from SSA values, which is what `live_in` plus the must-defined
//!    analysis guarantee: a register is either loaded at entry or written earlier
//!    on *every* path to the read. A register missing from `live_in` that the body
//!    reads is a miscompile with no fault, which is why the analysis intersects
//!    over predecessors instead of unioning reads.
//! 2. **Every exit stores back everything it must.** A live-out that misses a
//!    register hands the next block a stale value; the exit's live-out is the set
//!    read before written by whatever the exit lands on.
//! 3. **RFLAGS are materialized consistently at the header.** The flag carrier
//!    ([`FlagState`](super::flags::FlagState)) at the back edge must hold the same
//!    deferred state invariant the block path maintains at a block exit, so a
//!    deopt, a trap or a host-visible `pushfq` inside the loop sees architectural
//!    flags rather than a half-updated carrier.
//! 4. **Nothing can leave or be interrupted without going through an exit block.**
//!    Rules 1–2 above (no call, no `ret`, no string op) are what make that true,
//!    together with every faulting path routing through `set_fault` and out to an
//!    exit rather than continuing the loop.
//! 5. **Guest RSP stays exact per instruction**, including through the shadow
//!    return stack, so an exit taken mid-region leaves the same `JitCtx` a
//!    block-level exit would.
//!
//! # Status
//!
//! Live but off by default (`WIE_JIT_REGION`): [`form_loop_region`] and
//! [`form_self_loop_region`] are called from the pipeline at three seams (a hot
//! `try_compile`, a latch's `backward_targets`, and the boot/baseline promotion of
//! a loop header), and [`super::region_emit`] compiles what they return. With the
//! knob off no formation is attempted and the per-block path is untouched. The
//! default is off because the frame measurement is still inconclusive on this
//! host — the compile-time effect (4x fewer host compiles per DOOM run) is
//! measured, the frame effect is not.

use super::super::block::{BlockKind, BlockTerm, DecodedInsn, decode_pure_gpr_block};
use super::opclass;
use crate::exec::HookWindow;
use crate::mem::GuestMemory;
use ahash::HashMap;
use ahash::HashMapExt;
use iced_x86::{Instruction, InstructionInfoFactory, Mnemonic, OpAccess, OpKind, Register};
use std::collections::VecDeque;

/// Guest GPR slots in a region's register mask (index = `gpr::reg_index`).
pub(super) const GPR_SLOTS: usize = 16;
/// Guest XMM slots in a region's register mask (XMM0..XMM15).
pub(super) const XMM_SLOTS: usize = 16;

/// GPR index → register, in `reg_index` order.
const GPR_ORDER: [Register; GPR_SLOTS] = [
    Register::RAX,
    Register::RCX,
    Register::RDX,
    Register::RBX,
    Register::RSP,
    Register::RBP,
    Register::RSI,
    Register::RDI,
    Register::R8,
    Register::R9,
    Register::R10,
    Register::R11,
    Register::R12,
    Register::R13,
    Register::R14,
    Register::R15,
];

/// Bounds on region formation. Hitting any of them refuses the region.
///
/// The defaults are sized from the one loop we have measured — DOOM Retro's
/// two-block, 36-instruction loop at `0x140066570` — with room to spare and no
/// evidence yet for anything larger. They are a **bomb shelter, not a cost
/// model**: nothing here claims a 192-instruction region compiles in a sensible
/// time, only that nothing larger is formed at all.
#[derive(Clone, Copy, Debug)]
pub(super) struct RegionLimits {
    /// Blocks that may end up in the region.
    pub(super) max_blocks: usize,
    /// Guest instructions that may end up in the region (terminators included).
    pub(super) max_insns: usize,
    /// Distinct guest RIPs the region may exit to.
    pub(super) max_exit_targets: usize,
    /// Blocks exploration may decode before giving up.
    ///
    /// Separate from `max_blocks` because exploration is not the region: the
    /// forward closure of a loop header includes every block it can fall into,
    /// while the region is only the part of that closure which leads back to the
    /// header. The "no external entry into a non-entry block" check needs the
    /// *whole* closure to be seen, so when the closure is wider than this the
    /// region is refused rather than guessed at. That is the limitation to relax
    /// first if real guest code is refused here (see the module docs).
    pub(super) max_explore_blocks: usize,
}

impl Default for RegionLimits {
    fn default() -> Self {
        Self {
            max_blocks: 32,
            max_insns: 512,
            max_exit_targets: 4,
            max_explore_blocks: 512,
        }
    }
}

/// One guest instruction inside a region, with the RIP it was decoded at.
#[derive(Debug, Clone)]
pub(super) struct GuestInsn {
    /// Guest RIP of the instruction.
    pub(super) rip: u64,
    /// The decoded instruction.
    pub(super) insn: Instruction,
}

/// What a decoded block does with control flow, in the two-slot form every
/// consumer here wants: at most two static successors.
///
/// A `call` reports its `return_ip` — where execution continues once the callee
/// returns. Exploring through it is what makes a call sitting inside a cycle
/// visible to the soundness scan (which then refuses the region) instead of
/// silently turning the loop into a set with no back edge. A `ret` has no static
/// successor: its RIP comes off the guest stack at run time.
fn block_successors(term: Option<BlockTerm>, end_rip: u64) -> ([u64; 2], usize) {
    match term {
        Some(BlockTerm::Jmp { target }) => ([target, 0], 1),
        Some(BlockTerm::Jcc {
            taken, not_taken, ..
        }) => ([taken, not_taken], 2),
        Some(BlockTerm::Call { return_ip, .. } | BlockTerm::CallDyn { return_ip }) => {
            ([return_ip, 0], 1)
        }
        Some(BlockTerm::Ret) => ([0, 0], 0),
        None => ([end_rip, 0], 1),
    }
}

/// A decoded block as exploration sees it.
struct ExploredBlock {
    /// Guest VA the block starts at.
    start: u64,
    /// Decoded body, **terminator included** (`decode_pure_gpr_block` pushes the
    /// terminator into the body before ending the block).
    insns: Vec<DecodedInsn>,
    /// Decoded terminator, if the block has one.
    term: Option<BlockTerm>,
    /// Fallthrough RIP when the block has no terminator, or a `jcc`'s
    /// not-taken side.
    end_rip: u64,
}

impl ExploredBlock {
    fn successors(&self) -> ([u64; 2], usize) {
        block_successors(self.term, self.end_rip)
    }

    /// Guest instructions in the block, terminator included.
    fn insn_count(&self) -> usize {
        self.insns.len()
    }
}

/// One block of a region: its start VA, its slice of [`LoopRegion::insns`], and
/// its decoded terminator.
#[derive(Debug, Clone)]
pub(super) struct RegionBlock {
    start: u64,
    /// Index of the block's first instruction in [`LoopRegion::insns`].
    first_insn: usize,
    /// Guest instructions in the block, terminator included.
    insn_count: usize,
    term: Option<BlockTerm>,
    end_rip: u64,
}

impl RegionBlock {
    /// Guest VA the block starts at.
    pub(super) fn start(&self) -> u64 {
        self.start
    }

    /// Decoded terminator, if any.
    pub(super) fn term(&self) -> Option<BlockTerm> {
        self.term
    }

    /// Fallthrough RIP when the block has no terminator, or a `jcc`'s
    /// not-taken side.
    // Read by `region_tests` (the fallthrough a `jcc`'s not-taken side and a
    // terminator-less block land on). Not read by the emitter: a `jcc`'s
    // fallthrough reaches it through `successors()`, and the guest byte extent of
    // a block comes from its last instruction's `next_ip()` — `end_rip` is the
    // *jump target* for an unconditional `jmp` and would be the wrong answer here.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(super) fn end_rip(&self) -> u64 {
        self.end_rip
    }

    /// Guest instructions this block contributes (terminator included, because a
    /// `jcc` is a guest instruction too).
    pub(super) fn insn_count(&self) -> usize {
        self.insn_count
    }

    /// Guest instructions in the body, *excluding* the terminator.
    ///
    /// This is the count the emitter walks with the block lowerer: the block
    /// path compiles `insns` and then its terminator, and a region must hand it
    /// the same split.
    pub(super) fn body_len(&self) -> usize {
        self.insn_count - usize::from(self.term.is_some())
    }

    /// Static successors, in `(taken, fallthrough)` order, as guest VAs.
    pub(super) fn successors(&self) -> ([u64; 2], usize) {
        block_successors(self.term, self.end_rip)
    }
}

/// An edge leaving the region, and the guest RIP it lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RegionExit {
    target: u64,
    from: usize,
    taken: bool,
}

impl RegionExit {
    /// Guest RIP this exit lands on.
    pub(super) fn target(&self) -> u64 {
        self.target
    }

    /// Region-local index of the block the edge leaves from.
    pub(super) fn from(&self) -> usize {
        self.from
    }

    /// Whether the exit is the branch target of the leaving block (`true`) or a
    /// fallthrough (`false`): a `jmp`'s target and a `jcc`'s taken side are
    /// branch targets, a `jcc`'s not-taken side and a terminator-less block's
    /// fallthrough are not.
    pub(super) fn is_taken(&self) -> bool {
        self.taken
    }
}

/// Guest registers a region reads before writing along *some* path from its entry
/// — the set the emitter loads from `JitCtx` once, at region entry.
///
/// This is a **must**-analysis result: a register is in here when a path from the
/// region entry reaches a read of it without an intervening write. A register
/// that one arm writes and another arm reads before writing is therefore
/// included, which is the only sound answer for a single load at entry — the
/// cheap `reads \ writes` up-set would drop exactly that register and compile the
/// read against whatever SSA value a previous block happened to leave.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct LiveIns {
    /// Bitmask over [`GPR_SLOTS`], indexed by `gpr::reg_index`.
    gpr: u16,
    /// Bitmask over [`XMM_SLOTS`], indexed by register number.
    xmm: u16,
    /// Whether RFLAGS is read before written (a `jcc` whose producer is outside
    /// the region, or a `cmov`/`adc` in the first block).
    rflags: bool,
}

impl LiveIns {
    /// GPRs read before written, in `reg_index` order.
    pub(super) fn gprs(&self) -> impl Iterator<Item = Register> + '_ {
        let mask = self.gpr;
        GPR_ORDER
            .into_iter()
            .enumerate()
            .filter(move |(i, _)| mask & (1 << i) != 0)
            .map(|(_, reg)| reg)
    }

    /// XMM register numbers read before written, ascending.
    pub(super) fn xmms(&self) -> impl Iterator<Item = usize> + '_ {
        (0..XMM_SLOTS).filter(move |i| self.xmm & (1 << i) != 0)
    }

    /// Whether `reg` is in the GPR live-in set (any width of the same register).
    // Read by `region_tests`, which pins the live-in set register by register.
    // The emitter itself loads all 16 GPRs at entry rather than narrowing to this
    // set (see `region_emit`'s module docs, point 1), and reports the set through
    // `gprs()`.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(super) fn contains_gpr(&self, reg: Register) -> bool {
        super::gpr::reg_index(reg).is_ok_and(|i| self.gpr & (1 << i) != 0)
    }

    /// Whether XMM register number `xmm` is in the live-in set.
    // Read by `region_tests`; the emitter reports XMM liveness through `xmms()`
    // and needs no per-register predicate (XMM is write-through, so no decision
    // here depends on it).
    #[cfg_attr(not(test), expect(dead_code))]
    pub(super) fn contains_xmm(&self, xmm: usize) -> bool {
        xmm < XMM_SLOTS && self.xmm & (1 << xmm) != 0
    }

    /// Whether the region reads RFLAGS before writing it.
    pub(super) fn rflags(&self) -> bool {
        self.rflags
    }

    /// Number of GPRs + XMMs in the set (RFLAGS excluded).
    pub(super) fn reg_count(&self) -> usize {
        self.gpr.count_ones() as usize + self.xmm.count_ones() as usize
    }
}

/// A formed loop region: the unit region compilation would turn into one native
/// function.
///
/// Self-describing on purpose — the emitter needs nothing but this value plus the
/// existing per-block lowerer: ordering, entry live-ins, exits and the exact guest
/// instructions are all here, and none of them is recoverable from the others
/// without repeating the same decode.
#[derive(Debug, Clone)]
pub(super) struct LoopRegion {
    entry: u64,
    blocks: Vec<RegionBlock>,
    insns: Vec<GuestInsn>,
    live_in: LiveIns,
    exits: Vec<RegionExit>,
    back_edges: Vec<(usize, usize)>,
}

impl LoopRegion {
    /// Guest VA of the region entry (the VA the caller asked about).
    pub(super) fn entry(&self) -> u64 {
        self.entry
    }

    /// Blocks in reverse postorder, entry first: `blocks()[0].start() == entry()`.
    pub(super) fn blocks(&self) -> &[RegionBlock] {
        &self.blocks
    }

    /// The guest instructions of region block `index`, terminator included.
    pub(super) fn block_insns(&self, index: usize) -> &[GuestInsn] {
        let Some(block) = self.blocks.get(index) else {
            return &[];
        };
        let end = block.first_insn + block.insn_count;
        self.insns.get(block.first_insn..end).unwrap_or(&[])
    }

    /// Every guest instruction of the region, in `blocks` order.
    pub(super) fn insns(&self) -> &[GuestInsn] {
        &self.insns
    }

    /// Guest instructions in the region, terminators included.
    pub(super) fn insn_count(&self) -> usize {
        self.insns.len()
    }

    /// Registers the region reads before writing (loaded once, at entry).
    pub(super) fn live_in(&self) -> LiveIns {
        self.live_in
    }

    /// Edges leaving the region, one per leaving edge (not deduped).
    pub(super) fn exits(&self) -> &[RegionExit] {
        &self.exits
    }

    /// Distinct guest RIPs the region exits to, in first-seen order.
    pub(super) fn exit_targets(&self) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::with_capacity(self.exits.len());
        for exit in &self.exits {
            if !out.contains(&exit.target) {
                out.push(exit.target);
            }
        }
        out
    }

    /// Internal edges that close a cycle, as region-local `(from, to)` pairs.
    ///
    /// At least one always exists — it is what [`form_loop_region`] demanded of
    /// the block set, and it is the edge the emitter turns into a native back
    /// edge instead of a dispatcher round trip.
    pub(super) fn back_edges(&self) -> &[(usize, usize)] {
        &self.back_edges
    }
}

/// Why a candidate region was refused.
///
/// Refusal is the normal outcome — most hot blocks are not loop headers, and most
/// loops contain something a region cannot hold — so each variant names the rule
/// it broke and the VA that broke it, which is what lets a refusal report say
/// something more useful than "no".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegionRefusal {
    /// The requested start block does not decode as a lowerable block.
    EntryNotLowerable,
    /// The explored closure exceeded [`RegionLimits::max_explore_blocks`].
    ExplorationTooLarge,
    /// The start block is not on a cycle: no back edge, so nothing to fold.
    NoBackEdge,
    /// The region contains a `call` (direct or indirect); `at` is the call.
    Call { at: u64 },
    /// The region contains a `ret`; `at` is the `ret`.
    Return { at: u64 },
    /// The region contains a string op; `at` is that instruction.
    StringOp { at: u64 },
    /// A memory operand whose address the emitted probe cannot be trusted with;
    /// `at` is the instruction, `why` names the shape (for example `addr32`).
    UnguardableAddress { at: u64, why: &'static str },
    /// More blocks than [`RegionLimits::max_blocks`].
    TooManyBlocks { blocks: usize, limit: usize },
    /// More guest instructions than [`RegionLimits::max_insns`].
    TooManyInsns { insns: usize, limit: usize },
    /// More distinct exit targets than [`RegionLimits::max_exit_targets`].
    TooManyExitTargets { targets: usize, limit: usize },
    /// A cycle inside the region does not pass through its entry (`at` is the
    /// cycle's target): an irreducible loop, or a nested loop whose header is not
    /// this region's entry. Either way there is no single header to fold.
    Irreducible { at: u64 },
    /// The region formed, but it is a **one-block self-loop** and that shape is
    /// gated off (`WIE_JIT_REGION_SELF_LOOP`, default off).
    ///
    /// A policy refusal, not a soundness one: the emitted region is correct, and
    /// where it has been timed cleanly it is **neutral within noise** —
    /// `long_loop`, 100 M iterations, `emu_ms` median ~323 armed against ~330
    /// gated, quiet host. What keeps the gate closed is the shape that matters:
    /// DOOM's scaled blit `0x1800d8fb0` emits 171 host instructions per iteration
    /// as a region against 156 as a block and is 54.1 % of guest time
    /// (`docs/architecture/region-regalloc.md`), because a one-block body has
    /// nothing to amortise the per-block flag round trip and the 17-wide
    /// `STATE_PARAMS` traffic over. Two superseded ratios describe older builds,
    /// not this one: 4.5x on the tree at `4991913` and 6.3x before that commit's
    /// memory-probe hoist, both closed by that hoist and by `c0e6934`. `at` is the
    /// header.
    SelfLoopGatedOff { at: u64 },
}

/// One instruction's effect on guest state, as bitmasks.
#[derive(Debug, Clone, Copy, Default)]
struct Effect {
    gpr_r: u16,
    gpr_w: u16,
    xmm_r: u16,
    xmm_w: u16,
    flags_r: bool,
    flags_w: bool,
}

/// Guest registers `instr` reads and writes, from iced's instruction info.
///
/// Using `InstructionInfoFactory` rather than the block path's
/// [`mark_insn_gprs`](super::analysis::mark_insn_gprs) is deliberate: that helper
/// answers "which registers must this block have *loaded*", so it marks written
/// registers live too. That is right for a block that loads and then stores, and
/// exactly wrong for a region's live-in set, where a register the region writes
/// before reading must **not** be loaded.
fn insn_effect(factory: &mut InstructionInfoFactory, instr: &Instruction) -> Effect {
    let mut effect = Effect::default();
    let info = factory.info(instr);
    for used in info.used_registers() {
        let reg = used.register();
        let access = used.access();
        let reads = matches!(
            access,
            OpAccess::Read | OpAccess::CondRead | OpAccess::ReadWrite | OpAccess::ReadCondWrite
        );
        let writes = matches!(
            access,
            OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite | OpAccess::ReadCondWrite
        );
        if !reads && !writes {
            continue;
        }
        if reg.is_xmm() {
            let n = reg.number();
            if n < XMM_SLOTS {
                if reads {
                    effect.xmm_r |= 1 << n;
                }
                if writes {
                    effect.xmm_w |= 1 << n;
                }
            }
        } else if let Ok(idx) = super::gpr::reg_index(reg) {
            if reads {
                effect.gpr_r |= 1 << idx;
            }
            if writes {
                effect.gpr_w |= 1 << idx;
            }
        }
    }
    // RFLAGS effect comes off the `Instruction` (the iced `instr_info` tables),
    // not off `InstructionInfo`, which covers registers only.
    effect.flags_r = instr.rflags_read() != 0;
    effect.flags_w = instr.rflags_written() != 0 || instr.rflags_undefined() != 0;
    effect
}

/// Why `instr`'s memory operand (if it has one) cannot be guarded, or `None` when
/// it can.
///
/// The emitted probe guards *any* address the emitter computes correctly: it
/// compares the page key folded with the generation and otherwise falls through
/// to the host helper, which re-resolves and reports faults. So the refusal is
/// not about the address being dynamic — a register-based address is the normal
/// case and is fine — it is about the emitter being unable to compute the
/// architectural address at all. Today that is the address-size override: with a
/// 32-bit base or index the architectural effective address wraps at 2^32, while
/// [`effective_addr`](super::gpr::effective_addr) adds the full 64-bit register
/// values, so the probe would "guard" a page the guest never addressed.
fn unguardable_address(instr: &Instruction) -> Option<&'static str> {
    let mut has_mem = instr.mnemonic() == Mnemonic::Lea;
    for i in 0..instr.op_count() {
        if instr.op_kind(i) == OpKind::Memory {
            has_mem = true;
        }
    }
    if !has_mem {
        return None;
    }
    if instr.memory_base().is_gpr32() || instr.memory_index().is_gpr32() {
        return Some("addr32");
    }
    // `memory_segment()` reports the *effective* segment (DS/SS by default), so
    // the question is whether a prefix was actually encoded: only FS/GS survive
    // as segment bases in 64-bit mode, and `effective_addr` reads the TEB base
    // from `JitCtx` for those. Any other encoded prefix would be silently
    // dropped, so refuse it if iced ever reports one.
    let prefix = instr.segment_prefix();
    if prefix != Register::None && prefix != Register::FS && prefix != Register::GS {
        return Some("segment");
    }
    None
}

/// The explored closure, in discovery order.
struct Explored {
    blocks: Vec<ExploredBlock>,
    /// VA → index into `blocks`, or `None` for a VA that is reachable but does
    /// not decode as a lowerable block (an exit into code the JIT refuses).
    index: HashMap<u64, Option<usize>>,
}

/// Explore the lowerable closure of `start`, bounded by
/// [`RegionLimits::max_explore_blocks`].
fn explore(
    mem: &GuestMemory,
    hooks: Option<&HookWindow>,
    start: u64,
    limits: &RegionLimits,
) -> Result<Explored, RegionRefusal> {
    let mut blocks: Vec<ExploredBlock> = Vec::new();
    let mut index: HashMap<u64, Option<usize>> = HashMap::new();
    let mut queue: VecDeque<u64> = VecDeque::from([start]);
    index.insert(start, None);

    while let Some(va) = queue.pop_front() {
        if index.get(&va).is_some_and(Option::is_some) {
            continue;
        }
        let BlockKind::Pure {
            insns,
            end_rip,
            bytes_len: _,
            term,
        } = decode_pure_gpr_block(mem, hooks, va)
        else {
            // Reachable, but not a block the JIT can lower: it stays `None` in
            // the index, which makes every edge into it an exit.
            if va == start {
                return Err(RegionRefusal::EntryNotLowerable);
            }
            continue;
        };
        if blocks.len() >= limits.max_explore_blocks {
            return Err(RegionRefusal::ExplorationTooLarge);
        }
        let idx = blocks.len();
        index.insert(va, Some(idx));
        let block = ExploredBlock {
            start: va,
            insns,
            term,
            end_rip,
        };
        let (slots, len) = block.successors();
        blocks.push(block);
        for &succ in &slots[..len] {
            if let std::collections::hash_map::Entry::Vacant(slot) = index.entry(succ) {
                slot.insert(None);
                queue.push_back(succ);
            }
        }
    }

    Ok(Explored { blocks, index })
}

/// Reverse postorder of the region from `root`, plus the DFS back edges.
///
/// Iterative on purpose: depth would otherwise be derived from guest-controlled
/// input. A back edge is an edge to a node currently on the DFS stack; the SCC
/// guarantees at least one exists.
fn postorder(
    in_region: &[bool],
    succ: &[Vec<usize>],
    root: usize,
) -> (Vec<usize>, Vec<(usize, usize)>) {
    let mut state = vec![0_u8; in_region.len()]; // 0 = new, 1 = on stack, 2 = done
    let mut finish: Vec<usize> = Vec::new();
    let mut back_edges: Vec<(usize, usize)> = Vec::new();
    let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
    state[root] = 1;

    while !stack.is_empty() {
        let depth = stack.len() - 1;
        let (node, next) = stack[depth];
        let Some(&cand) = succ[node].get(next) else {
            state[node] = 2;
            finish.push(node);
            stack.pop();
            continue;
        };
        stack[depth].1 += 1;
        if !in_region[cand] {
            continue;
        }
        match state[cand] {
            0 => {
                state[cand] = 1;
                stack.push((cand, 0));
            }
            1 if !back_edges.contains(&(node, cand)) => back_edges.push((node, cand)),
            _ => {}
        }
    }
    finish.reverse();
    (finish, back_edges)
}

/// Enforce the instruction-level soundness rules on one region block.
fn check_block(body: &[DecodedInsn], term: Option<BlockTerm>) -> Result<(), RegionRefusal> {
    for decoded in body {
        let instr = &decoded.instr;
        if opclass::is_string_op(instr) {
            return Err(RegionRefusal::StringOp { at: instr.ip() });
        }
        if let Some(why) = unguardable_address(instr) {
            return Err(RegionRefusal::UnguardableAddress {
                at: instr.ip(),
                why,
            });
        }
    }
    // The terminator is the last instruction of the decoded body, so its own IP
    // is what a report should point at.
    let term_ip = body.last().map_or(0, |d| d.instr.ip());
    match term {
        Some(BlockTerm::Call { .. } | BlockTerm::CallDyn { .. }) => {
            Err(RegionRefusal::Call { at: term_ip })
        }
        Some(BlockTerm::Ret) => Err(RegionRefusal::Return { at: term_ip }),
        _ => Ok(()),
    }
}

/// Explore, keep the strongly connected component containing the start, and
/// describe it as a [`LoopRegion`].
///
/// Returns `Err` — never a partial or best-effort region — when any bound is hit
/// or any soundness rule in the module docs is broken. The error names the rule
/// and the VA, so a refusal report can say why a hot block stays on the block path.
pub(super) fn form_loop_region(
    mem: &GuestMemory,
    hooks: Option<&HookWindow>,
    start: u64,
    limits: &RegionLimits,
) -> Result<LoopRegion, RegionRefusal> {
    let explored = explore(mem, hooks, start, limits)?;
    let node_count = explored.blocks.len();
    if node_count == 0 {
        return Err(RegionRefusal::EntryNotLowerable);
    }

    // Forward and reverse edges over the explored closure. A successor that is
    // not a lowerable block has no index: edges into it are exits.
    let local = |va: u64| -> Option<usize> { explored.index.get(&va).copied().flatten() };
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    let mut pred: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    for (i, block) in explored.blocks.iter().enumerate() {
        let (slots, len) = block.successors();
        for &va in &slots[..len] {
            if let Some(j) = local(va) {
                succ[i].push(j);
                pred[j].push(i);
            }
        }
    }

    // The region is the SCC of the start: everything reachable from it that can
    // also reach it (`pred` gives the second half directly, because every
    // explored block is forward-reachable by construction).
    let mut in_region = vec![false; node_count];
    in_region[0] = true;
    let mut queue: VecDeque<usize> = VecDeque::from([0]);
    while let Some(node) = queue.pop_front() {
        for &p in &pred[node] {
            if !in_region[p] {
                in_region[p] = true;
                queue.push_back(p);
            }
        }
    }
    let block_count = in_region.iter().filter(|&&m| m).count();
    if block_count == 1 && !succ[0].contains(&0) {
        return Err(RegionRefusal::NoBackEdge);
    }

    // Region order (reverse postorder from the entry) and the edges that close a
    // cycle, in one walk.
    let (order, back_edges) = postorder(&in_region, &succ, 0);
    debug_assert_eq!(order.len(), block_count, "SCC is reachable from its entry");
    debug_assert_eq!(order.first().copied(), Some(0), "entry must order first");

    // Soundness rule 4: every cycle must come back through the entry.
    if let Some(&(_, target)) = back_edges.iter().find(|(_, to)| *to != 0) {
        return Err(RegionRefusal::Irreducible {
            at: explored.blocks[target].start,
        });
    }

    // The "no edge from outside the region into a non-entry block" rule is
    // provable for an SCC of the start's forward closure (module docs, rule 4),
    // so it is asserted rather than checked: a block that enters the region from
    // inside the closure would sit on a cycle through the entry, and would
    // therefore be *in* the region.
    debug_assert!(
        explored.blocks.iter().enumerate().all(|(i, block)| {
            if in_region[i] {
                return true;
            }
            let (slots, len) = block.successors();
            slots[..len]
                .iter()
                .all(|&va| local(va).is_none_or(|j| !in_region[j]))
        }),
        "an explored block outside the region cannot enter it"
    );

    if block_count > limits.max_blocks {
        return Err(RegionRefusal::TooManyBlocks {
            blocks: block_count,
            limit: limits.max_blocks,
        });
    }
    let mut insn_count = 0_usize;
    for (i, block) in explored.blocks.iter().enumerate() {
        if in_region[i] {
            insn_count += block.insn_count();
        }
    }
    if insn_count > limits.max_insns {
        return Err(RegionRefusal::TooManyInsns {
            insns: insn_count,
            limit: limits.max_insns,
        });
    }

    // Soundness rules 1–3, in discovery order then instruction order, so a
    // refusal report is deterministic.
    for (i, block) in explored.blocks.iter().enumerate() {
        if in_region[i] {
            check_block(&block.insns, block.term)?;
        }
    }

    // Exit set: every edge from a region block that does not stay inside.
    let mut exits: Vec<RegionExit> = Vec::new();
    for (i, block) in explored.blocks.iter().enumerate() {
        if !in_region[i] {
            continue;
        }
        let (slots, len) = block.successors();
        for (slot, &va) in slots[..len].iter().enumerate() {
            if local(va).is_some_and(|j| in_region[j]) {
                continue;
            }
            exits.push(RegionExit {
                target: va,
                from: i,
                // Slot 0 of a terminated block is its branch target; slot 1 is a
                // `jcc` fallthrough, and slot 0 of a terminator-less block is its
                // fallthrough too.
                taken: slot == 0 && block.term.is_some(),
            });
        }
    }
    let mut targets: Vec<u64> = Vec::new();
    for exit in &exits {
        if !targets.contains(&exit.target) {
            targets.push(exit.target);
        }
    }
    if targets.len() > limits.max_exit_targets {
        return Err(RegionRefusal::TooManyExitTargets {
            targets: targets.len(),
            limit: limits.max_exit_targets,
        });
    }

    // Re-index the region blocks: `order` is region-local order, `local_of` maps
    // back to the explored-closure index.
    let mut local_of = vec![usize::MAX; node_count];
    for (pos, &global) in order.iter().enumerate() {
        local_of[global] = pos;
    }

    let mut blocks: Vec<RegionBlock> = Vec::with_capacity(order.len());
    let mut insns: Vec<GuestInsn> = Vec::with_capacity(insn_count);
    for &global in &order {
        let block = &explored.blocks[global];
        let first_insn = insns.len();
        for decoded in &block.insns {
            insns.push(GuestInsn {
                rip: decoded.instr.ip(),
                insn: decoded.instr,
            });
        }
        blocks.push(RegionBlock {
            start: block.start,
            first_insn,
            insn_count: block.insn_count(),
            term: block.term,
            end_rip: block.end_rip,
        });
    }

    let mut region = LoopRegion {
        entry: start,
        blocks,
        insns,
        live_in: LiveIns::default(),
        exits: exits
            .into_iter()
            .map(|exit| RegionExit {
                target: exit.target,
                from: local_of[exit.from],
                taken: exit.taken,
            })
            .collect(),
        back_edges: back_edges
            .into_iter()
            .map(|(from, to)| (local_of[from], local_of[to]))
            .collect(),
    };
    region.live_in = analyze_live_ins(&region);
    Ok(region)
}

/// Form the **one-block** region at `start`, when `start`'s own terminator
/// branches back to `start`'s entry.
///
/// This is [`form_loop_region`]'s sibling, not a loosening of it. The SCC form
/// folds "the strongly connected component containing the start", and refuses
/// ([`RegionRefusal::Irreducible`]) as soon as that component holds a cycle which
/// does not pass through the entry — which is exactly what a **self-loop inside
/// any larger loop** looks like, because the block's fallthrough leads into the
/// rest of the loop and the rest of the loop comes back. Two of the hottest
/// blocks in the corpus are that shape and are therefore refused by an SCC fold
/// that is otherwise perfectly correct:
///
/// * DOOM Retro's scaled-blit block `0x1800d8fb0` (54.1 % of guest time) — refused
///   as `Irreducible { at: 0x1800d8fca }`;
/// * `micro-exes/long_loop`'s inner loop `0x140001040` — refused as
///   `Irreducible { at: 0x140001075 }`.
///
/// Their **component** is not a foldable loop. The **block** is: its terminator
/// marches straight back to its own entry, so the native back edge is that block's
/// own branch, and every other successor slot is an exit. Nothing else is needed —
/// no exploration, no SCC, no reverse postorder, and therefore no way to be
/// irreducible.
///
/// # What it does *not* relax
///
/// Every per-block soundness rule still applies, through the same
/// [`check_block`] the SCC form uses: a string op, an address the refill probe
/// cannot be trusted with, a `call` terminator, a `ret`. The bounds are the same
/// [`RegionLimits`] fields, read for a one-block region (`max_insns`, and
/// `max_exit_targets` against the at most two slots a terminator has).
///
/// # No exit at all
///
/// `jmp $` — a block whose only successor is itself — forms a region with an
/// **empty** exit set. That is not a special case in the emitter: the frame's only
/// return is the back edge's invalidation guard, which is the same escape the block
/// path's in-frame self-loop has (`emit::lower_self_loop_term` charges the trip and
/// checks the generation, then jumps). What that means for a guest is that such a
/// loop can only be stopped by invalidation or by the host, in either path — and
/// that is what `a_no_exit_self_loop_region_is_stopped_by_invalidation` pins.
pub(super) fn form_self_loop_region(
    mem: &GuestMemory,
    hooks: Option<&HookWindow>,
    start: u64,
    limits: &RegionLimits,
) -> Result<LoopRegion, RegionRefusal> {
    let BlockKind::Pure {
        insns,
        end_rip,
        bytes_len: _,
        term,
    } = decode_pure_gpr_block(mem, hooks, start)
    else {
        // Not a lowerable block, or an address the guest asked to stop at: the
        // same answer the SCC form gives, for the same reason.
        return Err(RegionRefusal::EntryNotLowerable);
    };

    // The whole question: does this block's own terminator come back to it?
    // `block_successors` is the same slot order every other consumer uses, so
    // "slot 0 is the branch target, slot 1 a fallthrough" holds here too.
    let (slots, len) = block_successors(term, end_rip);
    if !slots[..len].contains(&start) {
        // Not a self-loop: the precondition, not a defect. `form_region` falls
        // through to the SCC form on exactly this answer.
        return Err(RegionRefusal::NoBackEdge);
    }

    if insns.len() > limits.max_insns {
        return Err(RegionRefusal::TooManyInsns {
            insns: insns.len(),
            limit: limits.max_insns,
        });
    }
    // Soundness rules 1–3 for the one block, through the SCC form's own check.
    check_block(&insns, term)?;

    let mut exits: Vec<RegionExit> = Vec::new();
    for (slot, &va) in slots[..len].iter().enumerate() {
        if va == start {
            continue;
        }
        exits.push(RegionExit {
            target: va,
            from: 0,
            taken: slot == 0 && term.is_some(),
        });
    }
    let mut targets: Vec<u64> = Vec::new();
    for exit in &exits {
        if !targets.contains(&exit.target) {
            targets.push(exit.target);
        }
    }
    if targets.len() > limits.max_exit_targets {
        return Err(RegionRefusal::TooManyExitTargets {
            targets: targets.len(),
            limit: limits.max_exit_targets,
        });
    }

    let mut region = LoopRegion {
        entry: start,
        blocks: vec![RegionBlock {
            start,
            first_insn: 0,
            insn_count: insns.len(),
            term,
            end_rip,
        }],
        insns: insns
            .into_iter()
            .map(|decoded| GuestInsn {
                rip: decoded.instr.ip(),
                insn: decoded.instr,
            })
            .collect(),
        live_in: LiveIns::default(),
        exits,
        // The cycle this region is: the block's own back edge, which is the edge
        // the emitter turns into a native `jump` instead of a dispatcher round
        // trip. Formation guarantees it exists (the check above).
        back_edges: vec![(0, 0)],
    };
    region.live_in = analyze_live_ins(&region);
    Ok(region)
}

/// Must-defined dataflow over the region, then the live-in set.
///
/// `in[b]` is the set of registers written on **every** path from the region
/// entry to `b`'s first instruction: empty at the entry, the intersection of the
/// predecessors' `out` sets elsewhere, with `out[b] = in[b] ∪ writes[b]`. A read
/// of a register outside `in[b]` there is a read of the region's entry value, so
/// that register belongs in [`LiveIns`]. Intersecting (rather than unioning
/// reads) is the whole point: a register one arm writes and the other reads
/// before writing must still be loaded at entry.
fn analyze_live_ins(region: &LoopRegion) -> LiveIns {
    let mut factory = InstructionInfoFactory::new();
    let n = region.blocks.len();
    let mut local: HashMap<u64, usize> = HashMap::new();
    for (i, block) in region.blocks.iter().enumerate() {
        local.insert(block.start(), i);
    }
    let mut pred: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, block) in region.blocks.iter().enumerate() {
        let (slots, len) = block.successors();
        for &va in &slots[..len] {
            if let Some(&j) = local.get(&va) {
                pred[j].push(i);
            }
        }
    }

    // Per-block transfer: what the block cannot fail to define. Terminators are
    // inside the body slice, so a `jcc`'s RFLAGS read is covered by the same
    // walk.
    let mut writes: Vec<Effect> = Vec::with_capacity(n);
    for i in 0..n {
        let mut effect = Effect::default();
        for guest in region.block_insns(i) {
            let insn = insn_effect(&mut factory, &guest.insn);
            effect.gpr_w |= insn.gpr_w;
            effect.xmm_w |= insn.xmm_w;
            effect.flags_w |= insn.flags_w;
        }
        writes.push(effect);
    }

    const TOP: u16 = u16::MAX;
    let mut out_gpr = vec![TOP; n];
    let mut out_xmm = vec![TOP; n];
    let mut out_flags = vec![true; n];
    out_gpr[0] = writes[0].gpr_w;
    out_xmm[0] = writes[0].xmm_w;
    out_flags[0] = writes[0].flags_w;
    // `out` only shrinks from `TOP`, so this converges; in practice in two or
    // three sweeps over a single-entry region of at most `max_blocks` blocks.
    loop {
        let mut changed = false;
        for (b, preds) in pred.iter().enumerate().skip(1) {
            let mut in_gpr = TOP;
            let mut in_xmm = TOP;
            let mut in_flags = true;
            for &p in preds {
                in_gpr &= out_gpr[p];
                in_xmm &= out_xmm[p];
                in_flags &= out_flags[p];
            }
            let new_gpr = in_gpr | writes[b].gpr_w;
            let new_xmm = in_xmm | writes[b].xmm_w;
            let new_flags = in_flags || writes[b].flags_w;
            if new_gpr != out_gpr[b] || new_xmm != out_xmm[b] || new_flags != out_flags[b] {
                out_gpr[b] = new_gpr;
                out_xmm[b] = new_xmm;
                out_flags[b] = new_flags;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut live = LiveIns::default();
    for (b, preds) in pred.iter().enumerate() {
        let (mut gpr, mut xmm, mut flags) = if b == 0 {
            (0, 0, false)
        } else {
            let mut in_gpr = TOP;
            let mut in_xmm = TOP;
            let mut in_flags = true;
            for &p in preds {
                in_gpr &= out_gpr[p];
                in_xmm &= out_xmm[p];
                in_flags &= out_flags[p];
            }
            (in_gpr, in_xmm, in_flags)
        };
        for guest in region.block_insns(b) {
            let effect = insn_effect(&mut factory, &guest.insn);
            live.gpr |= effect.gpr_r & !gpr;
            live.xmm |= effect.xmm_r & !xmm;
            if effect.flags_r && !flags {
                live.rflags = true;
            }
            gpr |= effect.gpr_w;
            xmm |= effect.xmm_w;
            flags |= effect.flags_w;
        }
    }
    live
}
