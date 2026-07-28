//! Monotonic guest clocks.
//!
//! `GetTickCount`, `GetTickCount64`, and `QueryPerformanceCounter` used to
//! return frozen constants. That is fine for a batch tool but fatal for
//! anything that paces itself: a frame loop computing `dt = now - last` gets
//! zero forever, so animation never advances and a frame limiter spins at
//! 100% CPU without ever yielding.
//!
//! All three now run off one process-wide [`Instant`] captured on first use, so
//! they share an epoch and cannot disagree about how much time has passed.
//!
//! `WIE_FIXED_CLOCK=1` restores the old constants for deterministic traces —
//! the diff between two runs should be instruction flow, not wall time.

#![allow(
    clippy::integer_division,
    clippy::map_unwrap_or,
    clippy::checked_conversions,
    clippy::arithmetic_side_effects
)]

use std::sync::OnceLock;
use std::time::Instant;

use super::{FIXED_PERFORMANCE_COUNTER, FIXED_PERFORMANCE_FREQUENCY, FIXED_TICK_COUNT};

/// Session epoch, shared by every clock API.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// True when `WIE_FIXED_CLOCK=1` pins the clocks to constants.
fn fixed_clock() -> bool {
    static FIXED: OnceLock<bool> = OnceLock::new();
    *FIXED.get_or_init(|| {
        std::env::var("WIE_FIXED_CLOCK")
            .map(|value| value == "1")
            .unwrap_or(false)
    })
}

/// Nanoseconds elapsed since the session epoch.
fn elapsed_nanos() -> u128 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos()
}

/// Milliseconds since session start, for `GetTickCount64`.
#[must_use]
pub fn tick_count_64() -> u64 {
    if fixed_clock() {
        return FIXED_TICK_COUNT;
    }
    let millis = elapsed_nanos() / 1_000_000;
    u64::try_from(millis).unwrap_or(u64::MAX)
}

/// Milliseconds since session start, truncated to 32 bits.
///
/// The wrap at 2^32 ms (~49.7 days) is the documented Windows behaviour and the
/// reason `GetTickCount64` exists.
#[must_use]
pub fn tick_count_32() -> u64 {
    if fixed_clock() {
        return FIXED_TICK_COUNT;
    }
    tick_count_64() & u64::from(u32::MAX)
}

/// Performance-counter reading, in [`FIXED_PERFORMANCE_FREQUENCY`] units.
///
/// The frequency is 10 MHz, so one tick is 100 ns — the same resolution
/// Windows reports, which keeps a guest's tick-to-seconds division exact.
#[must_use]
pub fn performance_counter() -> u64 {
    if fixed_clock() {
        return FIXED_PERFORMANCE_COUNTER;
    }
    let ticks = elapsed_nanos() / (1_000_000_000 / u128::from(FIXED_PERFORMANCE_FREQUENCY));
    // Start above zero: a guest that divides by its first reading, or treats 0
    // as "counter not started", then behaves sanely.
    u64::try_from(ticks)
        .unwrap_or(u64::MAX)
        .saturating_add(FIXED_PERFORMANCE_COUNTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performance_counter_advances_monotonically() {
        if fixed_clock() {
            return;
        }
        let first = performance_counter();
        let second = performance_counter();
        assert!(
            second >= first,
            "counter went backwards: {second} < {first}"
        );
    }

    #[test]
    fn tick_count_32_stays_inside_u32() {
        assert!(tick_count_32() <= u64::from(u32::MAX));
    }
}
