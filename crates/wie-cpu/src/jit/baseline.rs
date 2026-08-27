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

#![allow(
    unsafe_code, // finalized Cranelift fn pointers are unsafe
    private_interfaces
)]

use super::block::BlockKind;
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
