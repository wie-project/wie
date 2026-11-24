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
//!
//! # One host read per snapshot
//!
//! Every slot except the wall-clock `FILETIME` is a function of a single
//! monotonic reading: `tick_count = nanos / 1e6`, `qpc = nanos / 100`
//! (100 ns ticks at the 10 MHz frequency). `snapshot_with` therefore takes
//! **one** `Instant::now()` and derives all of them, instead of each slot
//! re-reading the host clock. It used to issue three reads per call (two
//! `Instant` for the tick/QPC pair plus one `SystemTime` for the filetime); on a
//! 60 s DOOM Retro run `clock_table_values` is called **2.8 M times**, so the
//! per-call read count is the whole cost.
//!
//! The wall clock is a *different* host source (realtime, not monotonic) and
//! cannot be derived from that reading, so the whole snapshot is memoized for
//! `WIE_CLOCK_MEMO_US` microseconds (default **250**). Inside one window a call
//! costs one `Instant::now()` to test the window and nothing else: no
//! `SystemTime::now()` — the expensive read, which reaches `mach_absolute_time`
//! through `clock_gettime` — and no slot arithmetic. See `snapshot_with` for
//! the monotonicity argument and the knob's RUNBOOK row for the rationale
//! behind 250 µs.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::{
    FIXED_PERFORMANCE_COUNTER, FIXED_PERFORMANCE_FREQUENCY, FIXED_SYSTEM_FILETIME, FIXED_TICK_COUNT,
};

/// Session epoch, shared by every clock API.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Default [`snapshot`] memo window in microseconds.
///
/// SDL2 recomputes its timer delta per event-poll and per tic; DOOM Retro polls
/// at >40 kHz of emulated work on this host, so a window of a few hundred µs
/// collapses those bursts without being visible to a 35 Hz / 60 Hz frame
/// limiter (250 µs is <1 % of a 16.7 ms frame and 1/140th of a 35 Hz tic).
const DEFAULT_MEMO_US: u64 = 250;

/// Upper clamp on `WIE_CLOCK_MEMO_US`.
///
/// A guest that reads `GetTickCount64` twice and expects movement (a spin-wait,
/// a watchdog, `Sleep`-loop pacing) stalls if the window is longer than the
/// interval it polls at. One second bounds that to "a very slow poll", far below
/// the multi-day freeze a raw `u64` from the environment could ask for.
const MAX_MEMO_US: u64 = 1_000_000;

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

/// Memo window in microseconds; `0` disables the memo (every call re-reads).
///
/// Unparseable or negative values keep the default rather than silently
/// freezing the clock; an over-long window is clamped to [`MAX_MEMO_US`].
fn memo_window_us() -> u64 {
    static WINDOW: OnceLock<u64> = OnceLock::new();
    *WINDOW.get_or_init(|| {
        std::env::var("WIE_CLOCK_MEMO_US")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map_or(DEFAULT_MEMO_US, |us| us.min(MAX_MEMO_US))
    })
}

/// Public gate for the B5 guest clock table: when the clock is frozen, the
/// runtime skips per-stop table refreshes (it was written once at session init).
#[must_use]
pub fn clock_is_fixed() -> bool {
    fixed_clock()
}

/// Highest monotonic reading ever handed out.
///
/// Every guest-visible monotonic slot derives from the value returned by
/// [`monotonic_nanos`], so a reading taken on one guest thread can never be
/// published below a reading another thread already published: `fetch_max` is a
/// single atomic read-modify-write, so each call returns the running maximum,
/// and the maximum only grows. `Relaxed` is sufficient — the value is a
/// monotone counter, not a publication of other memory.
static LAST_NANOS: AtomicU64 = AtomicU64::new(0);

// The memoized snapshot: the reading it was taken at, plus the six slots.
//
// Thread-local so the hot path needs no atomic and no lock. Each thread keeps
// its own window; monotonicity does *not* depend on this cache having a global
// view, because `taken_nanos` is always a value that already came out of the
// process-wide `monotonic_nanos` clamp, and `table` was derived from it.
thread_local! {
    static MEMO: Cell<Option<Snapshot>> = const { Cell::new(None) };
}

/// One memoized clock snapshot (see [`MEMO`]).
#[derive(Clone, Copy)]
struct Snapshot {
    /// The clamped monotonic reading this snapshot was derived from.
    taken_nanos: u64,
    /// The six slots as derived at that reading.
    table: [u64; 6],
}

/// Nanoseconds elapsed since the session epoch — **the** host clock read.
fn elapsed_nanos() -> u64 {
    let epoch = EPOCH.get_or_init(Instant::now);
    u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Clamp a fresh reading up to the highest one ever returned.
fn monotonic_nanos(raw: u64) -> u64 {
    LAST_NANOS.fetch_max(raw, Ordering::Relaxed).max(raw)
}

/// Milliseconds since session start, for `GetTickCount64`.
#[must_use]
pub fn tick_count_64() -> u64 {
    if fixed_clock() {
        return FIXED_TICK_COUNT;
    }
    snapshot_with(memo_window_us())[1]
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
    snapshot_with(memo_window_us())[0]
}

/// Wall-clock `FILETIME` (100 ns since 1601-01-01 UTC) for
/// `GetSystemTimeAsFileTime`. Advances with the host clock; pinned to
/// [`FIXED_SYSTEM_FILETIME`] under `WIE_FIXED_CLOCK=1`.
///
/// Memoized for the same window as the monotonic slots, so a burst of calls
/// inside one window reads the host wall clock once.
pub(crate) fn system_time_filetime() -> u64 {
    if fixed_clock() {
        return FIXED_SYSTEM_FILETIME;
    }
    snapshot_with(memo_window_us())[3]
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
    snapshot_with(memo_window_us())[4]
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

/// The frozen snapshot `WIE_FIXED_CLOCK=1` pins every clock API to.
fn fixed_table() -> [u64; 6] {
    [
        FIXED_TICK_COUNT & u64::from(u32::MAX),
        FIXED_TICK_COUNT,
        FIXED_TICK_COUNT & u64::from(u32::MAX),
        FIXED_SYSTEM_FILETIME,
        FIXED_PERFORMANCE_COUNTER,
        FIXED_PERFORMANCE_FREQUENCY,
    ]
}

/// Pure slot derivation from one monotonic reading plus one wall reading.
///
/// This is the units contract in one place: `nanos` is nanoseconds since the
/// session epoch, `filetime` is 100 ns since 1601-01-01, and every derived slot
/// keeps exactly the units the legacy per-slot readers produced.
fn derive_table(nanos: u64, filetime: u64) -> [u64; 6] {
    let tick = nanos / 1_000_000;
    let tick32 = tick & u64::from(u32::MAX);
    // 10 MHz => 100 ns per counter tick (`1_000_000_000 / 10_000_000`).
    let qpc = nanos
        .saturating_div(1_000_000_000 / FIXED_PERFORMANCE_FREQUENCY)
        .saturating_add(FIXED_PERFORMANCE_COUNTER);
    [
        tick32,
        tick,
        tick32,
        filetime,
        qpc,
        FIXED_PERFORMANCE_FREQUENCY,
    ]
}

/// One memoized snapshot, with the window supplied explicitly.
///
/// # Why one read
///
/// `nanos` is the only host clock read on the monotonic side and it feeds slots
/// 0, 1, 2 and 4, so they cannot disagree about how much time has passed. The
/// wall clock is a *different* host source (realtime vs. monotonic), so it
/// cannot be derived from `nanos`; it is memoized instead, which removes that
/// read — the expensive one, since it reaches `mach_absolute_time` through
/// `clock_gettime` — entirely inside one window.
///
/// # Stability
///
/// A hit returns the memoized array *unchanged*, including slots 0/1/2/4. An
/// earlier revision re-derived them from the fresh reading on a hit, which left
/// the snapshot drifting inside its own window and would have made the window
/// pointless for the pacing loop it exists for: a frame limiter comparing two
/// reads 100 µs apart wants them equal, not 100 µs apart.
///
/// # Monotonicity
///
/// Two rules, both cheap:
///
/// 1. `nanos` comes from [`monotonic_nanos`], a process-wide running maximum,
///    and a snapshot is only ever derived from such a reading. A thread that
///    was descheduled therefore cannot create a snapshot below one already
///    created.
/// 2. The memo is consulted only when its reading is within the window *and*
///    not ahead of the fresh reading (`taken_nanos <= nanos`, which the clamp
///    makes unconditional but is asserted anyway). A hit returns values derived
///    from a reading that is a *prefix* of the current one, and a miss derives
///    from a strictly later reading — so per thread the returned tick and QPC
///    are non-decreasing, never decreasing.
///
/// The one ordering this does not serialise is two guest threads publishing the
/// table concurrently: thread A may compute a value, thread B publish a later
/// one, and A publish its now-stale value afterwards. That inversion is bounded
/// by the memo window (250 µs by default, i.e. sub-frame) and its *kind*
/// predates the memo — the refresh writes a lock-free guest VA from whichever
/// thread runs.
fn snapshot_with(window_us: u64) -> [u64; 6] {
    let nanos = monotonic_nanos(elapsed_nanos());
    let window_ns = window_us.saturating_mul(1_000);

    if window_us != 0
        && let Some(memo) = MEMO.with(Cell::get)
        && memo.taken_nanos <= nanos
        && nanos.saturating_sub(memo.taken_nanos) < window_ns
    {
        return memo.table;
    }

    let table = derive_table(nanos, wall_filetime());
    if window_us != 0 {
        MEMO.with(|memo| {
            memo.set(Some(Snapshot {
                taken_nanos: nanos,
                table,
            }));
        });
    }
    table
}

/// `FILETIME` for "now", read from the host wall clock.
///
/// Same units and epoch as it has always used: 100 ns ticks since
/// 1601-01-01 UTC, the UNIX epoch offset added in seconds.
fn wall_filetime() -> u64 {
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

/// Pure core of [`clock_table_values`], parameterized by the fixed-clock kill
/// switch so tests can exercise both paths without mutating the environment.
fn clock_table_values_inner(fixed: bool) -> [u64; 6] {
    if fixed {
        return fixed_table();
    }
    snapshot_with(memo_window_us())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A memo over a synthetic clock: drives `snapshot_with`'s rules without
    /// touching the process-wide epoch, the environment, or the wall clock, so
    /// the assertions cannot flake against a parallel test on another thread.
    #[derive(Default)]
    struct TestMemo {
        memo: Option<Snapshot>,
        wall_reads: u32,
        last_wall: u64,
    }

    impl TestMemo {
        fn observe(&mut self, nanos: u64, window_us: u64) -> [u64; 6] {
            let window_ns = window_us.saturating_mul(1_000);
            if window_us != 0
                && let Some(memo) = self.memo
                && memo.taken_nanos <= nanos
                && nanos.saturating_sub(memo.taken_nanos) < window_ns
            {
                return memo.table;
            }
            self.wall_reads += 1;
            self.last_wall = self.last_wall.saturating_add(1_000_000);
            let table = derive_table(nanos, self.last_wall);
            if window_us != 0 {
                self.memo = Some(Snapshot {
                    taken_nanos: nanos,
                    table,
                });
            }
            table
        }
    }

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

        // tick_count_64 (slot 1) advances with the monotonic epoch. 10 ms is
        // two orders of magnitude wider than the default memo window, so the
        // memo must not be what answers here.
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

    /// **Units and epoch are unchanged by the fusion.** One monotonic reading
    /// must produce exactly what the three separate readers used to: tick =
    /// ms since epoch, qpc = 10 MHz ticks since epoch (+ the non-zero base),
    /// timeGetTime = low 32 bits of the same tick.
    #[test]
    fn derive_table_keeps_legacy_units_and_epoch() {
        let nanos = 12_345_678_901_u64; // 12.345678901 s
        let wall = 133_000_000_000_000_000_u64;
        let table = derive_table(nanos, wall);

        assert_eq!(table[1], 12_345, "tick_count_64 is milliseconds");
        assert_eq!(table[0], 12_345, "tick_count is the low 32 bits");
        assert_eq!(table[2], 12_345, "timeGetTime is the same milliseconds");
        assert_eq!(
            table[4],
            nanos / 100 + FIXED_PERFORMANCE_COUNTER,
            "qpc is 100 ns ticks above the non-zero base"
        );
        assert_eq!(table[3], wall, "filetime passes through untouched");
        assert_eq!(table[5], FIXED_PERFORMANCE_FREQUENCY);
        // The QPC pair must stay exactly convertible: ticks / freq == seconds.
        let seconds =
            (table[4] - FIXED_PERFORMANCE_COUNTER) as f64 / FIXED_PERFORMANCE_FREQUENCY as f64;
        assert!(
            (seconds - 12.345_678_9).abs() < 1e-6,
            "qpc/freq must reproduce the monotonic reading: {seconds}"
        );
    }

    /// The memo's core promise: inside the window the snapshot is *identical*,
    /// and the filetime costs no host read.
    #[test]
    fn memo_holds_snapshot_stable_inside_the_window() {
        let mut memo = TestMemo::default();
        let first = memo.observe(5_000_000_000, DEFAULT_MEMO_US);
        // 100 µs later — inside the 250 µs window.
        let second = memo.observe(5_000_100_000, DEFAULT_MEMO_US);

        assert_eq!(
            first, second,
            "a call inside the memo window must return the same snapshot"
        );
        assert_eq!(
            memo.wall_reads, 1,
            "the window must read the wall clock once"
        );

        // Crossing the window refreshes the filetime.
        let third = memo.observe(5_000_300_000, DEFAULT_MEMO_US);
        assert_eq!(memo.wall_reads, 2, "crossing the window re-reads the clock");
        assert!(third[3] > second[3], "the refreshed filetime advances");
    }

    /// **Monotonic through the memo.** Sample across several windows with a
    /// jittery (but never decreasing) reading; no slot may ever go backwards,
    /// including the steps that are answered from the memo.
    #[test]
    fn memo_never_goes_backwards_across_windows() {
        let mut memo = TestMemo::default();
        let mut previous: Option<[u64; 6]> = None;
        // 0.5 ms steps over 20 ms, i.e. many window crossings, with the
        // occasional duplicate reading (a coarse host clock).
        let mut nanos = 0_u64;
        for step in 0..40_u64 {
            nanos += if step % 7 == 0 { 0 } else { 500_000 };
            let now = memo.observe(nanos, DEFAULT_MEMO_US);
            if let Some(prev) = previous {
                for slot in [0_usize, 1, 2, 3, 4] {
                    assert!(
                        now[slot] >= prev[slot],
                        "slot {slot} went backwards at step {step}: {:?} -> {:?}",
                        prev,
                        now
                    );
                }
                assert_eq!(now[5], prev[5], "frequency is constant");
            }
            previous = Some(now);
        }
        assert!(memo.wall_reads > 1, "the loop must cross windows");
    }

    /// `WIE_CLOCK_MEMO_US=0` disables the memo: every call takes a fresh wall
    /// reading and the snapshot tracks the reading, with no stale repeats.
    #[test]
    fn zero_window_disables_the_memo() {
        let mut memo = TestMemo::default();
        let first = memo.observe(1_000_000_000, 0);
        let second = memo.observe(1_000_050_000, 0);

        assert_eq!(memo.wall_reads, 2, "a zero window must never memoize");
        assert!(second[1] >= first[1], "still monotonic with the memo off");
        assert!(
            second[3] > first[3],
            "the filetime must be re-read, not reused, with the memo off"
        );
        assert!(
            memo.memo.is_none(),
            "a zero window must not store a memo entry"
        );
    }

    /// A raw reading that is behind the running maximum is clamped up, so a
    /// descheduled thread cannot publish an older value than one already seen.
    #[test]
    fn monotonic_nanos_clamps_a_stale_reading() {
        let ahead = monotonic_nanos(elapsed_nanos());
        assert!(
            monotonic_nanos(0) >= ahead,
            "a stale reading must be clamped up to the running maximum"
        );
    }
}
