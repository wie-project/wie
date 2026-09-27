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

use super::config::JitConfig;

/// The opt level a tier-up compile runs at.
///
/// `speed` (not `speed_and_size`) is the tier: the block has already proven it
/// is hot, so emitted-code quality matters and code size does not. The base
/// tier is whatever `WIE_JIT_OPT` says, which is `none` by default — see the
/// measured trade-off on [`JitConfig::from_env`].
pub(super) const TIER_OPT_LEVEL: &str = "speed";

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
}
