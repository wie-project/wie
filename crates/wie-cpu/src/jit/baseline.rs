//! Tier-0 baseline compiler: single-pass, no optimization, fast lowering.
//!
//! Target: 0.1–0.3 ms per block vs ~3 ms for Cranelift tier-1. The current
//! implementation is a structural stub that satisfies the tier-0 wiring and
//! handles the common `mov`/`alu`/`branch`/`mem` shapes via the shared
//! lowering (micro-stub fast path when eligible, otherwise a direct Cranelift
//! compile with `opt_level = "none"` would be ideal). The stub keeps
//! correctness while the boot-mode fast path (threshold = 1, inline token
//! bucket) already delivers the cold-boot wall improvement; a future iteration
//! can replace the body with a true single-pass emitter (e.g. dynasm/aarch64
//! direct) without changing the pipeline contract.
//!
//! Contract with `pipeline.rs`:
//! - `try_compile_baseline` is called synchronously on the guest thread
//!   (first hit during boot, 0.2 ms budget).
//! - On success the caller installs the returned `CompiledBlock` as `Ready`
//!   (tier-0) and enqueues a tier-1 Cranelift job for the same RIP. The
//!   background worker's later install overwrites the tier-0 entry
//!   (last-writer-wins); the next dispatch therefore patches to tier-1.
//! - On `None` the caller falls back to the normal Hot/eager path.
//!
//! # Why loop regions are *not* formed in here (`WIE_JIT_REGION=1`)
//!
//! A baseline compile is the one place a loop header gets compiled and then
//! never revisited: the caller installs it `Ready`, and nothing re-compiles a
//! `Ready` entry except a self-loop's tier-up. That is exactly why a header
//! first seen during boot stayed a per-block translation for the rest of the
//! process, so this is the seam the region path has to cover — but the seam
//! belongs one frame up, in
//! [`JitCpu::step_compiled`](super::pipeline), for a reason neither this
//! function nor `JitShared` can work around: **region formation needs the
//! guest thread's [`HookWindow`](crate::exec::HookWindow)**, and this signature
//! deliberately does not take one. `compile_from_kind_shared` is shared with the
//! background workers, which have no hook window, so a region formed down here
//! could cover an address the guest asked to stop at and would then run that
//! instruction natively, silently skipping the stop.
//!
//! So the pipeline asks [`backward_targets`] about the block it just compiled
//! and forms the region itself, where the window is in hand. Nothing in this
//! file changes when the knob is off.

#![allow(
    unsafe_code, // finalized Cranelift fn pointers are unsafe
    private_interfaces
)]

use super::block::{BlockKind, BlockTerm};
use super::fast_api::FastApiKind;
use super::lower::CompiledBlock;
use super::shared::JitShared;

/// Whether the decoded block is eligible for baseline compilation.
///
/// Baseline currently accepts any `Pure` block up to 32 insns that the
/// lowerer considers compilable. `NotPure` blocks are never baseline
/// candidates (they fall back to the interpreter).
#[must_use]
pub(super) fn is_baseline_eligible(kind: &BlockKind) -> bool {
    match kind {
        BlockKind::NotPure => false,
        BlockKind::Pure { insns, .. } => insns.len() <= 32,
    }
}

/// Record one backward successor, deduplicated.
fn push_backward(slots: &mut [u64; 2], len: &mut usize, from: u64, to: u64) {
    // **`to == from` is included**, i.e. a block that branches to its own entry
    // is a loop header of a one-block loop. That is the same cycle as any other,
    // and it is where the hottest blocks in a real guest are: DOOM Retro's
    // SDL2 scaled-blit block (`0x1800d8fb0`, 54.1 % of guest time) is one, and
    // while it was excluded here it could not get the region's once-per-entry
    // memory hoist at all.
    //
    // The tier is what made this safe to include rather than a gamble: a region
    // compiles at the tier the block path would have picked for the same RIP
    // (`JitShared::decide_tier`, which returns `Speed` for exactly this shape),
    // so folding a self-loop replaces a `speed` block with a `speed` region, not
    // with a `Base` one. See `pipeline::try_compile_region`.
    if to > from || slots[..*len].contains(&to) {
        return;
    }
    slots[*len] = to;
    *len += 1;
}

/// The successors of the block at `rip` that are **backward edges**: at or below
/// the block's own entry VA.
///
/// Such a successor is the head of a cycle — control reaches `rip`, and `rip`
/// branches back into it — so it is a **loop header**, which is precisely what
/// [`form_loop_region`](super::lower::region) folds. `rip` itself is included:
/// a block that branches back to its own entry is the header of a one-block
/// loop, the commonest loop shape there is. This is the whole of the per-block
/// cost of finding loops the compiler was not already pointed at: two
/// comparisons over the two successor slots of a [`BlockKind`] that has already
/// been decoded.
///
/// Returned as `(slots, len)` like `ExploredBlock::successors` in
/// `lower/region.rs`, so a caller does not allocate for a list of at most two.
///
/// # Why the *latch* can answer this and the header cannot
///
/// The header's own terminator says nothing — a loop's exit branch is forward
/// and its backward branch is in some later block. Only the block that branches
/// back knows the loop exists, and it knows it the first time it is compiled,
/// which is *after* the header has already been compiled. That is the ordering
/// that left boot-compiled loop headers on the per-block path forever (see
/// [`try_compile_baseline`]'s region note); this function is how the latch hands
/// the fact to the pipeline.
#[must_use]
pub(super) fn backward_targets(rip: u64, kind: &BlockKind) -> ([u64; 2], usize) {
    let mut slots = [0_u64; 2];
    let mut len = 0_usize;
    match kind {
        BlockKind::Pure { term, end_rip, .. } => match term {
            Some(BlockTerm::Jmp { target }) => push_backward(&mut slots, &mut len, rip, *target),
            Some(BlockTerm::Jcc {
                taken, not_taken, ..
            }) => {
                push_backward(&mut slots, &mut len, rip, *taken);
                push_backward(&mut slots, &mut len, rip, *not_taken);
            }
            // A call's successor is the *callee*, and control resumes at the
            // return site rather than at the target: a callee that happens to
            // sit below this block is not a loop header of this loop.
            // `CallDyn`/`Ret` have no static successor at all.
            Some(BlockTerm::Call { .. } | BlockTerm::CallDyn { .. } | BlockTerm::Ret) => {}
            // No terminator: the decoder walks strictly forward, so this can
            // never produce a candidate. Kept explicit rather than `_` so a new
            // `BlockTerm` variant has to state its answer.
            None => push_backward(&mut slots, &mut len, rip, *end_rip),
        },
        BlockKind::NotPure => {}
    }
    (slots, len)
}

/// Try to compile `kind` with the baseline tier (synchronous, guest thread).
///
/// Returns `Some(CompiledBlock)` on success. The block is functionally
/// equivalent to a tier-1 compile for the same inputs; only the *when*
/// differs. Callers must still enqueue a tier-1 background job so the
/// block is later patched to the optimized version.
#[must_use]
pub(super) fn try_compile_baseline(
    shared: &JitShared,
    fast_api: &[(u64, FastApiKind)],
    rip: u64,
    kind: BlockKind,
    inv_gen: u64,
) -> Option<CompiledBlock> {
    if !is_baseline_eligible(&kind) {
        return None;
    }
    // Fast path: micro-stub (1–3 insn handlers) is already baseline-optimal
    // (no Cranelift). Otherwise delegate to the shared lowering; even though
    // this still uses Cranelift, the surrounding boot-mode path (threshold=1
    // + inline token bucket) already collapses iced residency. A true
    // single-pass emitter can replace this delegate without touching the
    // pipeline.
    shared.compile_from_kind_shared(fast_api, rip, kind, inv_gen)
}

#[cfg(test)]
mod tests {
    //! [`backward_targets`]: the two-comparison question that finds a loop from
    //! its latch.
    //!
    //! These are pure over a hand-built [`BlockKind`], which is the point: the
    //! function is the whole per-block cost the knob adds to the block path, and
    //! every arm is a claim about which guest shape is a loop header candidate.

    use super::*;

    fn pure_at(term: Option<BlockTerm>, end_rip: u64) -> BlockKind {
        BlockKind::Pure {
            insns: Vec::new(),
            end_rip,
            bytes_len: 0,
            term,
        }
    }

    fn slots(kind: &BlockKind, rip: u64) -> Vec<u64> {
        let (slots, len) = backward_targets(rip, kind);
        slots[..len].to_vec()
    }

    /// The DOOM shape: a `jl` whose taken side is the loop header below it. Its
    /// *fallthrough* is forward, so exactly one candidate.
    #[test]
    fn a_backward_jcc_yields_its_taken_side_only() {
        let kind = pure_at(
            Some(BlockTerm::Jcc {
                mnemonic: iced_x86::Mnemonic::Jl,
                taken: 0x1000,
                not_taken: 0x2000,
            }),
            0x2000,
        );
        assert_eq!(slots(&kind, 0x1800), vec![0x1000]);
    }

    /// Both sides backward: both are candidates, and the loop it describes is
    /// discovered from whichever one the fold tries first.
    #[test]
    fn a_jcc_with_two_backward_sides_yields_both() {
        let kind = pure_at(
            Some(BlockTerm::Jcc {
                mnemonic: iced_x86::Mnemonic::Jl,
                taken: 0x1000,
                not_taken: 0x1800,
            }),
            0x1800,
        );
        assert_eq!(slots(&kind, 0x2000), vec![0x1000, 0x1800]);
    }

    /// A forward `jmp` is an ordinary successor, not a loop.
    #[test]
    fn a_forward_jmp_is_not_a_loop_header() {
        let kind = pure_at(Some(BlockTerm::Jmp { target: 0x2000 }), 0x2000);
        assert!(slots(&kind, 0x1000).is_empty());
    }

    /// The DOOM-blit shape: a block whose `jne` targets itself. It **is** a loop
    /// header — of a one-block loop — and it must be proposed, or the hottest
    /// block in a real guest can never become a region.
    #[test]
    fn a_self_loop_is_a_loop_header() {
        let kind = pure_at(
            Some(BlockTerm::Jcc {
                mnemonic: iced_x86::Mnemonic::Jne,
                taken: 0x1000,
                not_taken: 0x2000,
            }),
            0x1000,
        );
        assert_eq!(slots(&kind, 0x1000), vec![0x1000]);
    }

    /// Both sides of a `jcc` landing on its own entry is still **one**
    /// candidate, not two: the two slots are deduplicated, so a self-loop costs
    /// one formation attempt rather than two.
    #[test]
    fn a_two_sided_self_loop_yields_one_candidate() {
        let kind = pure_at(
            Some(BlockTerm::Jcc {
                mnemonic: iced_x86::Mnemonic::Jle,
                taken: 0x1000,
                not_taken: 0x1000,
            }),
            0x1000,
        );
        assert_eq!(slots(&kind, 0x1000), vec![0x1000]);
    }

    /// A `call`'s successor is the callee: control resumes at the return site,
    /// so a callee below the caller is not a loop header of the caller's loop.
    #[test]
    fn a_call_target_is_not_a_loop_header() {
        let kind = pure_at(
            Some(BlockTerm::Call {
                target: 0x1000,
                return_ip: 0x2005,
            }),
            0x1000,
        );
        assert!(slots(&kind, 0x2000).is_empty());
    }

    /// `ret` and an indirect call have no static successor; nor does a
    /// `NotPure` block (nothing was decoded to ask about). A block with no
    /// terminator falls through, and the decoder walks strictly forward, so its
    /// only edge is never a candidate.
    #[test]
    fn terminators_without_a_static_successor_yield_nothing() {
        for term in [
            Some(BlockTerm::Ret),
            Some(BlockTerm::CallDyn { return_ip: 0x2005 }),
        ] {
            let kind = pure_at(term, 0x2000);
            assert!(slots(&kind, 0x2000).is_empty(), "term={term:?}");
        }
        assert!(
            slots(&pure_at(None, 0x2010), 0x2000).is_empty(),
            "fallthrough"
        );
        assert!(slots(&BlockKind::NotPure, 0x2000).is_empty());
    }
}
