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
//! B5 (host-written guest clock table): the runtime publishes
//! [`clock_table_values`] into a fixed guest VA once per host stop; in-guest
//! stubs read it with no host stop. `WIE_FIXED_CLOCK=1` restores the old
//! constants for deterministic traces — the table is then written once at
//! session init and never refreshed, so the diff between two runs is
//! instruction flow, not wall time.

#![allow(
    clippy::integer_division,
    clippy::map_unwrap_or,
    clippy::checked_conversions,
    clippy::arithmetic_side_effects
)]

use std::sync::OnceLock;
use std::time::Instant;

use super::{
    FIXED_PERFORMANCE_COUNTER, FIXED_PERFORMANCE_FREQUENCY, FIXED_SYSTEM_FILETIME, FIXED_TICK_COUNT,
};

/// Session epoch, shared by every clock API.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// True when `WIE_FIXED_CLOCK=1` pins the clocks to constants.
fn fixed_clock() -> bool {
    static FIXED: OnceLock<bool> = OnceLock::new();
    *FIXED.get_or_init(env_fixed_clock)
}

/// Uncached probe of the `WIE_FIXED_CLOCK` kill switch.
fn env_fixed_clock() -> bool {
    std::env::var("WIE_FIXED_CLOCK")
        .map(|value| value == "1")
        .unwrap_or(false)
}

/// Public gate for the B5 guest clock table: when the clock is frozen, the
/// runtime skips per-stop table refreshes (it was written once at session init).
#[must_use]
pub fn clock_is_fixed() -> bool {
    fixed_clock()
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

/// Wall-clock `FILETIME` (100 ns since 1601-01-01 UTC) for
/// `GetSystemTimeAsFileTime`. Advances with the host clock; pinned to
/// [`FIXED_SYSTEM_FILETIME`] under `WIE_FIXED_CLOCK=1`.
pub(crate) fn system_time_filetime() -> u64 {
    if fixed_clock() {
        return FIXED_SYSTEM_FILETIME;
    }
    // 11_644_473_600 = whole seconds from 1601-01-01 to the UNIX epoch.
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let hundred_ns = u128::from(elapsed.as_secs())
        .saturating_add(11_644_473_600)
        .saturating_mul(10_000_000)
        .saturating_add(u128::from(elapsed.subsec_nanos()).saturating_div(100));
    u64::try_from(hundred_ns).unwrap_or(FIXED_SYSTEM_FILETIME)
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

/// The six slots of the host-written guest clock table (B5).
///
/// Slot order must match the in-guest stub offsets in `wie-runtime`
/// (`CLOCK_TABLE_SLOT_*`):
/// - `[0]` tick_count (u32 low bits — `GetTickCount`)
/// - `[1]` tick_count_64 (`GetTickCount64`)
/// - `[2]` time_get_time (u32 ms — `timeGetTime`)
/// - `[3]` system_time_as_filetime (`GetSystemTimeAsFileTime`)
/// - `[4]` qpc_counter (`QueryPerformanceCounter`)
/// - `[5]` qpc_frequency (`QueryPerformanceFrequency`)
///
/// Slots 0, 1, 2, 4 derive from the single monotonic session [`EPOCH`]; only
/// the FILETIME slot follows the wall clock (per-slot monotonicity is what
/// matters, and a wall-clock filetime advancing is compatible with guests).
#[must_use]
pub fn clock_table_values() -> [u64; 6] {
    clock_table_values_inner(fixed_clock())
}

/// Pure core of [`clock_table_values`], parameterized by the fixed-clock kill
/// switch so tests can exercise both paths without mutating the environment.
fn clock_table_values_inner(fixed: bool) -> [u64; 6] {
    if fixed {
        return [
            FIXED_TICK_COUNT & u64::from(u32::MAX),
            FIXED_TICK_COUNT,
            FIXED_TICK_COUNT & u64::from(u32::MAX),
            FIXED_SYSTEM_FILETIME,
            FIXED_PERFORMANCE_COUNTER,
            FIXED_PERFORMANCE_FREQUENCY,
        ];
    }
    let tick = tick_count_64();
    [
        tick & u64::from(u32::MAX),
        tick,
        tick & u64::from(u32::MAX),
        system_time_filetime(),
        performance_counter(),
        FIXED_PERFORMANCE_FREQUENCY,
    ]
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

    /// The advancing path: after a 10 ms sleep every time-derived slot has
    /// moved forward, and the frequency slot stays at the fixed 10 MHz base.
    #[test]
    fn clock_table_values_advance_monotonically() {
        let first = clock_table_values_inner(false);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let second = clock_table_values_inner(false);

        // tick_count_64 (slot 1) advances with the monotonic epoch.
        assert!(
            second[1] >= first[1].saturating_add(8),
            "tick_count_64 did not advance: {} -> {}",
            first[1],
            second[1]
        );
        // qpc counter (slot 4) never goes backwards; freq (slot 5) is constant.
        assert!(second[4] >= first[4], "qpc counter went backwards");
        assert_eq!(second[5], FIXED_PERFORMANCE_FREQUENCY);
        // timeGetTime (slot 2) shares the low 32 bits of tick_count_64.
        assert_eq!(second[2], second[1] & u64::from(u32::MAX));
        assert_eq!(second[0], second[2]);
    }

    /// The frozen path (`WIE_FIXED_CLOCK=1` drives `fixed_clock()` → `true`):
    /// the snapshot is constant and equals the legacy `FIXED_*` constants.
    #[test]
    fn clock_table_values_frozen_under_fixed_clock() {
        let first = clock_table_values_inner(true);
        let second = clock_table_values_inner(true);
        assert_eq!(first, second, "frozen clock table must not move");
        assert_eq!(first[0], FIXED_TICK_COUNT & u64::from(u32::MAX));
        assert_eq!(first[1], FIXED_TICK_COUNT);
        assert_eq!(first[2], FIXED_TICK_COUNT & u64::from(u32::MAX));
        assert_eq!(first[3], FIXED_SYSTEM_FILETIME);
        assert_eq!(first[4], FIXED_PERFORMANCE_COUNTER);
        assert_eq!(first[5], FIXED_PERFORMANCE_FREQUENCY);
    }
}
