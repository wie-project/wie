//! Opt-level tiers: the vocabulary a single block compilation is recorded under.
//!
//! The persistent ledger needs one fact more than the process-wide
//! [`JitConfig::opt_level`] can supply: with per-block tiering a single process
//! compiles the same guest bytes at **two different Cranelift opt levels** (a
//! block that earns it is compiled at [`TIER_OPT_LEVEL`], everything else at
//! the configured base level). A ledger record therefore has to name WHICH
//! level produced it, or a later run will replay one level's verdict as
//! known-good for the other.
//!
//! [`OptTier`] is that vocabulary, and it is deliberately two-valued and
//! stable: its `u8` code is persisted in the on-disk ledger, so widening it is
//! a [`FORMAT_VERSION`](super::cache_persist) bump, not a refactor.
//!
//! This module also holds the tier-up *policy* ([`TierPlan`]): which blocks
//! earn [`TIER_OPT_LEVEL`], decided **pre-compile from block shape** and
//! memoised once per guest VA, plus the [`ChainTarget`] tag that keeps direct
//! chaining inside one module.

use super::config::JitConfig;
use ahash::HashMap;
use cranelift_module::FuncId;

/// The opt level a tier-up compile runs at.
///
/// `speed` (not `speed_and_size`) is the tier: the block has already proven it
/// is hot, so emitted-code quality matters and code size does not. The base
/// tier is whatever `WIE_JIT_OPT` says, which is `none` by default — see the
/// measured trade-off on [`JitConfig::from_env`].
pub(super) const TIER_OPT_LEVEL: &str = "speed";

/// Default per-run cap on tier-up decisions (`WIE_JIT_TIER_BUDGET`).
///
/// Sized from measurement, not taste. A tier compile costs ~1 ms (that is the
/// whole point of the `none` default), and a self-loop repays it only by
/// retiring ≳10^7 instructions, which a pre-compile shape signal cannot see.
/// So the budget is the only bound on the miss:
///
/// - `micro-exes/long_loop` needs **1** (its inner loop is the single
///   self-loop) and repays 1.37x — the whole point of the mechanism;
/// - 7-Zip Extra's `i` has **18** self-loops and repays **nothing**
///   (`emu_ms` is flat cold, +2.5 % warm — the tier compiles it never uses),
///   because its visit-hot set contains no self-loops at all.
///
/// Eight therefore buys a compute-bound guest its hot-loop set with room to
/// spare, while bounding what a real tool can spend on speculative `speed`
/// compiles to ~8 ms per run instead of ~18. `WIE_JIT_TIER_BUDGET` retunes it;
/// `WIE_JIT_TIER=0` turns the mechanism off entirely.
pub(super) const TIER_BUDGET_DEFAULT: usize = 8;

/// Which opt level one block compilation runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OptTier {
    /// The process-wide configured level (`WIE_JIT_OPT`, default `none`).
    Base,
    /// The tier-up level ([`TIER_OPT_LEVEL`]), paid for only by blocks that
    /// earn it.
    Speed,
}

impl OptTier {
    /// Cranelift `opt_level` string this tier compiles at.
    ///
    /// Not a `const fn`: the base level is process state, and the honest
    /// mapping is what the engine is actually configured with. In a
    /// `WIE_JIT_OPT=speed` process this returns `"speed"` for BOTH tiers —
    /// which is exactly right, because there the two tiers coincide and
    /// tiering is inert.
    #[must_use]
    pub(super) fn opt_level(self) -> &'static str {
        match self {
            Self::Base => JitConfig::get().opt_level(),
            Self::Speed => TIER_OPT_LEVEL,
        }
    }

    /// Stable on-disk code for the ledger record field.
    #[must_use]
    pub(super) const fn code(self) -> u8 {
        match self {
            Self::Base => 0,
            Self::Speed => 1,
        }
    }

    /// Inverse of [`Self::code`]. `None` for any unknown code, so a corrupt or
    /// hand-edited record is *rejected* rather than silently read as a tier it
    /// was not.
    #[must_use]
    pub(super) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Base),
            1 => Some(Self::Speed),
            _ => None,
        }
    }
}

/// A direct-chaining target: the `FuncId` **plus the module that declared it**.
///
/// The tag is load-bearing, not bookkeeping. A `cranelift_module::FuncId` is
/// an index into the *declaring* module's function table, so
/// `Module::declare_func_in_func` on another module either panics or silently
/// names a different function and emits a call to the wrong address. Direct
/// chaining is therefore SAME-TIER-ONLY (see
/// [`TierPlan`]'s users in `shared.rs`), and the tag is what makes that
/// decidable: a cross-tier edge is simply not offered to
/// `declare_func_in_func` and falls back to the late-bound chain hop /
/// dispatcher, both of which are already supported and tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ChainTarget {
    pub(super) func_id: FuncId,
    pub(super) tier: OptTier,
}

/// The earn signal: pre-compile block SHAPE, not a post-compile visit count.
/// The supporting measurement (7-Zip's visit-hot set holds no self-loops) is
/// what makes shape, rather than visit count, the only honest signal here.
///
/// A block whose terminator branches to its own entry multiplies emitted-code
/// quality by its iteration count; every other shape amortises the very same
/// work over at most one pass plus dispatch overhead, so a non-loop block
/// never earns `speed` however many times it is entered. This is the
/// correction the plan's hotness counter needed: the counter is destroyed at
/// promotion, so "exceeded the threshold by a clear margin" is not expressible
/// on it at all, and the measured `long_loop` case retires 1.1e9 instructions
/// inside a *single* dispatcher entry — there is no post-compile observation
/// before it has retired everything it ever will.
///
/// Supporting measurement: 7-Zip's hot set has no self-loops, so this signal
/// classifies none of its blocks, which is the point.
#[must_use]
pub(super) fn earns_tier(armed: bool, is_self_loop: bool, budget_left: usize) -> OptTier {
    if armed && is_self_loop && budget_left > 0 {
        OptTier::Speed
    } else {
        OptTier::Base
    }
}

/// Per-run tier-up ledger: **one memoised decision per guest VA**, plus the
/// run's remaining budget.
///
/// The memo is what makes the anti-thrash guardrails structural rather than
/// best-effort:
///
/// - **tier-up only** — a memoised entry is returned verbatim and never
///   upgraded later, so no block can oscillate across the line;
/// - **at most one tier-up per block per run** — the decision is taken once,
///   before the single compile it governs, so there is no recompile at all and
///   the compile cost Task 2 removed is never paid back;
/// - **budget consumed at decision time**, not at compile time, so a decision
///   that is never compiled still costs its slot (the honest accounting: the
///   budget bounds the number of blocks that *may* be compiled at `speed`).
///
/// The one permitted rewrite is [`Self::downgrade_after_reject`]: a tier
/// compile the verifier rejects falls back to `Base` and stays there. It can
/// only move DOWN, and only after a rejection, so it cannot oscillate.
///
/// A decision survives cache invalidation of the block. That is deliberate:
/// the value is a pure function of the block's shape, and a stale entry can
/// only be conservative (a block that used to be a self-loop keeps paying
/// `speed`), never a correctness problem.
pub(super) struct TierPlan {
    /// Tiering is armed: the knob is on, the budget is non-zero, and a
    /// tier-up module exists. Disarmed ⇒ every decision is `Base`, which is
    /// the pre-tiering behaviour.
    armed: bool,
    budget_left: usize,
    /// Guest VA → decided tier. Written once, then read-only (bar a rejection
    /// downgrade).
    decided: HashMap<u64, OptTier>,
    /// Tier-up decisions made (blocks that will compile at `speed`).
    tier_ups: u64,
    /// Tier compiles rejected by the verifier and retried at base.
    tier_rejects: u64,
}

/// Read-only view of the tier ledger for the profile report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TierCounters {
    pub(super) tier_ups: u64,
    pub(super) tier_rejects: u64,
    pub(super) budget_left: usize,
    pub(super) armed: bool,
}

impl TierPlan {
    pub(super) fn new(armed: bool, budget: usize) -> Self {
        Self {
            armed,
            budget_left: budget,
            decided: HashMap::default(),
            tier_ups: 0,
            tier_rejects: 0,
        }
    }

    /// The tier `rip` compiles at. Memoised: the first call for a VA decides,
    /// every later call replays the same answer.
    pub(super) fn decide(&mut self, rip: u64, is_self_loop: bool) -> OptTier {
        if let Some(&tier) = self.decided.get(&rip) {
            return tier;
        }
        let tier = earns_tier(self.armed, is_self_loop, self.budget_left);
        if tier == OptTier::Speed {
            self.budget_left = self.budget_left.saturating_sub(1);
            self.tier_ups = self.tier_ups.saturating_add(1);
        }
        self.decided.insert(rip, tier);
        tier
    }

    /// Record that the tier compile for `rip` was rejected: re-decide it as
    /// `Base`, permanently. Returns whether this call was the one that did it.
    pub(super) fn downgrade_after_reject(&mut self, rip: u64) -> bool {
        match self.decided.get(&rip) {
            Some(OptTier::Speed) => {
                self.decided.insert(rip, OptTier::Base);
                self.tier_rejects = self.tier_rejects.saturating_add(1);
                true
            }
            _ => false,
        }
    }

    pub(super) fn counters(&self) -> TierCounters {
        TierCounters {
            tier_ups: self.tier_ups,
            tier_rejects: self.tier_rejects,
            budget_left: self.budget_left,
            armed: self.armed,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn tier_codes_round_trip() {
        for tier in [OptTier::Base, OptTier::Speed] {
            assert_eq!(OptTier::from_code(tier.code()), Some(tier));
        }
    }

    #[test]
    fn unknown_tier_code_is_rejected_not_guessed() {
        // A corrupt/hand-edited ledger record must not be read as some tier it
        // was never compiled at.
        for code in [2_u8, 42, 255] {
            assert_eq!(OptTier::from_code(code), None, "code {code}");
        }
    }

    #[test]
    fn codes_are_distinct_and_stable() {
        // Persisted bytes: these values must not be reshuffled without a
        // FORMAT_VERSION bump.
        assert_eq!(OptTier::Base.code(), 0);
        assert_eq!(OptTier::Speed.code(), 1);
        assert_ne!(OptTier::Base.code(), OptTier::Speed.code());
    }

    #[test]
    fn base_tier_is_the_configured_level_and_speed_tier_is_fixed() {
        // The unit suite runs at the cheap-compile default, so the two tiers
        // resolve to different strings here — which is the precondition for
        // the record-level rejection in the ledger to have anything to reject.
        assert_eq!(OptTier::Base.opt_level(), JitConfig::get().opt_level());
        assert_eq!(OptTier::Speed.opt_level(), "speed");
        assert_eq!(TIER_OPT_LEVEL, "speed");
    }

    // --- earn signal (pre-compile shape) ---

    /// Only a self-loop earns `speed`, and only while budget remains. Every
    /// other shape amortises the emitted code over at most one pass, so the
    /// post-compile visit count that the plan originally proposed cannot and
    /// does not apply to it.
    #[test]
    fn only_a_self_loop_with_budget_left_earns_the_tier() {
        assert_eq!(earns_tier(true, true, 1), OptTier::Speed);
        assert_eq!(earns_tier(true, false, 64), OptTier::Base, "one-shot shape");
        assert_eq!(earns_tier(true, true, 0), OptTier::Base, "budget spent");
        assert_eq!(earns_tier(false, true, 64), OptTier::Base, "disarmed");
    }

    /// A memoised decision is replayed verbatim and never consumes budget
    /// twice: this is what makes "tier up only" and "at most one tier-up per
    /// block per run" structural.
    #[test]
    fn decision_is_memoised_and_budget_is_spent_once() {
        let mut plan = TierPlan::new(true, 2);
        assert_eq!(plan.decide(0x1000, true), OptTier::Speed);
        assert_eq!(plan.decide(0x1000, true), OptTier::Speed);
        assert_eq!(plan.decide(0x1000, true), OptTier::Speed);
        assert_eq!(plan.counters().budget_left, 1, "one decision, one slot");
        assert_eq!(plan.counters().tier_ups, 1);
    }

    /// A block decided at base while the budget was spent is never upgraded
    /// later, even though it is the same self-loop: no oscillation, and no
    /// recompile to recover from one.
    #[test]
    fn base_decision_is_never_upgraded_even_when_budget_returns() {
        let mut plan = TierPlan::new(true, 0);
        assert_eq!(plan.decide(0x3000, true), OptTier::Base, "no budget");
        // Budget reappears mid-run (a later decision spends it) — the memoised
        // base verdict must not be re-litigated.
        plan.budget_left = 8;
        assert_eq!(plan.decide(0x3000, true), OptTier::Base);
        assert_eq!(plan.counters().tier_ups, 0);
    }

    /// The budget bounds the run: the Nth self-loop past the cap compiles at
    /// base, permanently.
    #[test]
    fn budget_caps_the_number_of_tier_ups() {
        let mut plan = TierPlan::new(true, 3);
        for i in 0..10_u64 {
            let want = if i < 3 { OptTier::Speed } else { OptTier::Base };
            assert_eq!(plan.decide(0x4000 + i * 0x10, true), want, "block {i}");
        }
        assert_eq!(plan.counters().tier_ups, 3);
        assert_eq!(plan.counters().budget_left, 0);
    }

    /// A rejected tier compile falls back to base and STAYS there: the single
    /// permitted rewrite only moves down, and only once.
    #[test]
    fn verifier_rejection_downgrades_once_and_never_oscillates() {
        let mut plan = TierPlan::new(true, 4);
        assert_eq!(plan.decide(0x5000, true), OptTier::Speed);
        assert!(
            plan.downgrade_after_reject(0x5000),
            "first rejection downgrades"
        );
        assert!(!plan.downgrade_after_reject(0x5000), "second is a no-op");
        assert_eq!(plan.decide(0x5000, true), OptTier::Base);
        assert_eq!(plan.counters().tier_ups, 1, "the decision is not re-spent");
        assert_eq!(plan.counters().tier_rejects, 1);
    }

    /// A rejection on a block that was never tiered changes nothing.
    #[test]
    fn downgrade_of_a_base_block_is_a_no_op() {
        let mut plan = TierPlan::new(true, 4);
        assert_eq!(plan.decide(0x6000, false), OptTier::Base);
        assert!(!plan.downgrade_after_reject(0x6000));
        assert_eq!(plan.counters().tier_rejects, 0);
    }

    /// Disarmed (`WIE_JIT_TIER=0`, zero budget, or no tier module) means every
    /// single block compiles at base — the pre-tiering behaviour, exactly.
    #[test]
    fn disarmed_plan_is_exactly_the_pre_tiering_behaviour() {
        let mut plan = TierPlan::new(false, TIER_BUDGET_DEFAULT);
        for i in 0..1000_u64 {
            assert_eq!(plan.decide(i, true), OptTier::Base);
        }
        assert_eq!(plan.counters().tier_ups, 0);
        assert_eq!(
            plan.counters().budget_left,
            TIER_BUDGET_DEFAULT,
            "untouched"
        );
        assert!(!plan.counters().armed);
    }
}
