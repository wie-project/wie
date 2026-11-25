//! A call counter and wall-time accumulator for one stage of a host handler.
//!
//! Written for the `LoadLibrary*` attribution and reused for `BitBlt`: the
//! question "this handler costs 397 µs per call, which stage owns it?" has no
//! answer from an export-level total, and the wrong fix is easy to write once
//! you have guessed. Every stage the handler can be split into gets one of
//! these, and the profile prints calls, total cost and per-call cost for each.
//!
//! Counts are process-wide and never reset: this is diagnostic output for one
//! run, the same convention as [`crate::sdl2`]'s export stats.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// One stage's call count and accumulated wall time.
#[derive(Debug, Default)]
pub(crate) struct Stage {
    calls: AtomicU64,
    ns: AtomicU64,
}

impl Stage {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            ns: AtomicU64::new(0),
        }
    }

    /// Record one completed call that took `elapsed` since `start`.
    fn record(&self, start: Instant) {
        let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let _count = self.calls.fetch_add(1, Ordering::Relaxed);
        let _time = self.ns.fetch_add(elapsed, Ordering::Relaxed);
    }

    /// Record one completed call whose elapsed time the caller measured itself.
    ///
    /// For a stage that is a *region* of a handler (with early returns inside)
    /// rather than a closure call.
    pub(crate) fn record_since(&self, start: Instant) {
        self.record(start);
    }

    /// `(calls, ns)`.
    pub(crate) fn read(&self) -> (u64, u128) {
        (
            self.calls.load(Ordering::Relaxed),
            u128::from(self.ns.load(Ordering::Relaxed)),
        )
    }

    /// Zero the counters — tests only.
    #[cfg(all(test, debug_assertions))]
    pub(crate) fn reset(&self) {
        self.calls.store(0, Ordering::Relaxed);
        self.ns.store(0, Ordering::Relaxed);
    }
}

/// A bare counter for questions that are about *how often*, not how long.
///
/// `Stage` always accumulates nanoseconds; asking "were these reads served from
/// the mirrored arena or from the host path" wants counts and byte totals, and a
/// duration there would be noise around zero.
#[derive(Debug, Default)]
pub(crate) struct Counter(AtomicU64);

impl Counter {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub(crate) fn add(&self, n: u64) {
        let _previous = self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Time `f` against `stage`.
pub(crate) fn timed<T>(stage: &Stage, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = f();
    stage.record(start);
    value
}

/// `name=calls/total/per-call` for one stage.
pub(crate) fn field(name: &str, stage: &Stage) -> String {
    let (calls, ns) = stage.read();
    let per_call = if calls == 0 {
        0.0
    } else {
        ns as f64 / calls as f64 / 1000.0
    };
    format!(
        "{name}={calls}/{:.2}ms/{per_call:.2}us",
        ns as f64 / 1_000_000.0
    )
}
