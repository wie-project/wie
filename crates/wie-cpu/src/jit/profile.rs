//! JIT decision + compile-timing instrumentation.
//!
//! Accumulates decision counters (eager / hot / background / iced-fallback /
//! never) and compile-time histograms for diagnostics. Compile timing lives at
//! the rare compile seam and is always recorded; there is no hot-path timing.
//! Background-compile timing uses lock-free atomics so the worker thread can
//! write without contending with guest threads.

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

use std::sync::atomic::{AtomicU64, Ordering};

/// Number of fixed buckets in each timing histogram.
pub const PROFILE_BUCKETS: usize = 8;

/// Fixed-bucket accumulator for `(count, total_us)` pairs keyed by a
/// non-negative integer (e.g. block instruction count).
///
/// Bucket `i` covers keys in `[2^i, 2^(i+1))`; keys at or above
/// `2^(PROFILE_BUCKETS-1)` saturate into the last bucket. Averages are
/// `total_us / count` (0 when a bucket is empty).
#[derive(Debug, Clone, Copy, Default)]
pub struct TimeBuckets {
    counts: [u64; PROFILE_BUCKETS],
    total_us: [u64; PROFILE_BUCKETS],
}

impl TimeBuckets {
    /// Bucket index for `key` (floor log2, saturated at the last bucket).
    #[must_use]
    pub fn bucket_for(key: u64) -> usize {
        if key == 0 {
            return 0;
        }
        let idx = 63_u32.saturating_sub(key.leading_zeros());
        usize::try_from(idx).unwrap_or(0).min(PROFILE_BUCKETS - 1)
    }

    /// Record one sample of `us` microseconds under bucket `key`.
    pub fn record(&mut self, key: u64, us: u64) {
        let i = Self::bucket_for(key);
        self.counts[i] = self.counts[i].saturating_add(1);
        self.total_us[i] = self.total_us[i].saturating_add(us);
    }

    /// Sample count in bucket `i` (0 when out of range).
    #[must_use]
    pub fn count(&self, i: usize) -> u64 {
        self.counts.get(i).copied().unwrap_or(0)
    }

    /// Total microseconds in bucket `i` (0 when out of range).
    #[must_use]
    pub fn total_us(&self, i: usize) -> u64 {
        self.total_us.get(i).copied().unwrap_or(0)
    }

    /// Average microseconds per sample in bucket `i` (0 when empty).
    #[must_use]
    pub fn avg_us(&self, i: usize) -> u64 {
        self.total_us(i).checked_div(self.count(i)).unwrap_or(0)
    }

    /// Fold another histogram's samples into this one (per-thread merge of a
    /// shared background snapshot).
    pub fn merge(&mut self, other: &TimeBuckets) {
        for i in 0..PROFILE_BUCKETS {
            self.merge_bucket(i, other.count(i), other.total_us(i));
        }
    }

    /// Add `count` samples / `us` microseconds into bucket `i` (no-op when out
    /// of range). Used to fold lock-free shared atomics into a per-thread copy.
    pub fn merge_bucket(&mut self, i: usize, count: u64, us: u64) {
        if let (Some(c), Some(t)) = (self.counts.get_mut(i), self.total_us.get_mut(i)) {
            *c = c.saturating_add(count);
            *t = t.saturating_add(us);
        }
    }
}

/// Per-thread JIT instrumentation (embedded in [`super::JitStats`]).
///
/// Timing fields are wall microseconds. `compile_*` is always recorded (rare
/// compile seam); there is no hot-path timing.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitProfile {
    /// Wall µs spent compiling blocks inline.
    pub compile_us: u64,
    /// Compile-time histogram keyed by block instruction count.
    pub compile_by_insns: TimeBuckets,
    /// Eager compile decisions (`thr == 0` / fast-UCRT).
    pub eager_compiles: u64,
    /// Hot-threshold-crossed compile decisions.
    pub hot_compiles: u64,
    /// Blocks successfully queued to the background worker.
    pub bg_enqueues: u64,
    /// Background waits that resolved to a Ready block.
    pub bg_wait_hits: u64,
    /// Background waits that timed out (inline fallback).
    pub bg_wait_timeouts: u64,
    /// Blocks compiled inline.
    pub inline_compiles: u64,
    /// Times execution fell through to the iced interpreter.
    pub iced_fallbacks: u64,
    /// Blocks marked `Never` (cold / non-pure).
    pub never_marks: u64,
    /// Persistent-ledger warm hits (`WIE_JIT_CACHE`): known-good blocks that
    /// skipped the Hot visit-threshold warmup and compiled immediately.
    pub warm_ledger_hits: u64,
}

/// Lock-free accumulator for background-compile timing. The worker thread
/// writes; per-thread [`super::JitCpu::stats`] snapshots read and fold into the
/// per-thread [`JitProfile`].
#[derive(Debug, Default)]
pub struct BgCompileProfile {
    /// Total wall µs spent compiling on the worker.
    pub compile_us: AtomicU64,
    /// Total blocks compiled on the worker.
    pub compile_count: AtomicU64,
    /// Per-bucket total wall µs (indexed like [`TimeBuckets`]).
    pub bucket_us: [AtomicU64; PROFILE_BUCKETS],
    /// Per-bucket sample counts.
    pub bucket_counts: [AtomicU64; PROFILE_BUCKETS],
}

impl BgCompileProfile {
    /// Record one worker compile of `us` µs for a block of `insns` instructions.
    pub fn record(&self, insns: u64, us: u64) {
        self.compile_us.fetch_add(us, Ordering::Relaxed);
        self.compile_count.fetch_add(1, Ordering::Relaxed);
        let i = TimeBuckets::bucket_for(insns);
        if let (Some(u), Some(c)) = (self.bucket_us.get(i), self.bucket_counts.get(i)) {
            u.fetch_add(us, Ordering::Relaxed);
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Fold this shared accumulator into a per-thread [`TimeBuckets`] histogram.
    pub fn fold_into(&self, buckets: &mut TimeBuckets) {
        for i in 0..PROFILE_BUCKETS {
            let c = self.bucket_counts[i].load(Ordering::Relaxed);
            let u = self.bucket_us[i].load(Ordering::Relaxed);
            buckets.merge_bucket(i, c, u);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn bucket_for_maps_log2_and_saturates() {
        assert_eq!(TimeBuckets::bucket_for(0), 0);
        assert_eq!(TimeBuckets::bucket_for(1), 0);
        assert_eq!(TimeBuckets::bucket_for(2), 1);
        assert_eq!(TimeBuckets::bucket_for(3), 1);
        assert_eq!(TimeBuckets::bucket_for(4), 2);
        assert_eq!(TimeBuckets::bucket_for(127), 6);
        assert_eq!(TimeBuckets::bucket_for(128), 7);
        assert_eq!(TimeBuckets::bucket_for(u64::MAX), 7);
    }

    #[test]
    fn time_buckets_record_and_average() {
        let mut b = TimeBuckets::default();
        // Bucket 0 ([1,2)): two samples of 10 and 30 µs.
        b.record(1, 10);
        b.record(1, 30);
        // Bucket 2 ([4,8)): one sample of 20 µs.
        b.record(5, 20);
        assert_eq!(b.count(0), 2);
        assert_eq!(b.total_us(0), 40);
        assert_eq!(b.avg_us(0), 20);
        assert_eq!(b.count(2), 1);
        assert_eq!(b.avg_us(2), 20);
        // Empty bucket averages to 0.
        assert_eq!(b.avg_us(1), 0);
        // Out-of-range reads are 0.
        assert_eq!(b.count(PROFILE_BUCKETS), 0);
        assert_eq!(b.total_us(PROFILE_BUCKETS), 0);
    }

    #[test]
    fn time_buckets_merge_folds_samples() {
        let mut a = TimeBuckets::default();
        a.record(1, 10);
        let mut b = TimeBuckets::default();
        b.record(1, 30);
        b.record(5, 20);
        a.merge(&b);
        assert_eq!(a.count(0), 2);
        assert_eq!(a.total_us(0), 40);
        assert_eq!(a.count(2), 1);
        assert_eq!(a.total_us(2), 20);
    }

    #[test]
    fn bg_compile_profile_folds_into_buckets() {
        let bg = BgCompileProfile::default();
        bg.record(1, 10);
        bg.record(1, 30);
        bg.record(5, 20);
        assert_eq!(bg.compile_us.load(Ordering::Relaxed), 60);
        assert_eq!(bg.compile_count.load(Ordering::Relaxed), 3);
        let mut buckets = TimeBuckets::default();
        bg.fold_into(&mut buckets);
        assert_eq!(buckets.count(0), 2);
        assert_eq!(buckets.total_us(0), 40);
        assert_eq!(buckets.count(2), 1);
        assert_eq!(buckets.total_us(2), 20);
    }

    #[test]
    fn jit_profile_accumulates() {
        let mut p = JitProfile::default();
        p.compile_us = p.compile_us.saturating_add(100);
        p.eager_compiles = p.eager_compiles.saturating_add(1);
        p.hot_compiles = p.hot_compiles.saturating_add(2);
        p.bg_enqueues = p.bg_enqueues.saturating_add(3);
        p.bg_wait_hits = p.bg_wait_hits.saturating_add(4);
        p.bg_wait_timeouts = p.bg_wait_timeouts.saturating_add(5);
        p.inline_compiles = p.inline_compiles.saturating_add(6);
        p.iced_fallbacks = p.iced_fallbacks.saturating_add(7);
        p.never_marks = p.never_marks.saturating_add(8);
        assert_eq!(p.compile_us, 100);
        assert_eq!(p.eager_compiles, 1);
        assert_eq!(p.hot_compiles, 2);
        assert_eq!(p.bg_enqueues, 3);
        assert_eq!(p.bg_wait_hits, 4);
        assert_eq!(p.bg_wait_timeouts, 5);
        assert_eq!(p.inline_compiles, 6);
        assert_eq!(p.iced_fallbacks, 7);
        assert_eq!(p.never_marks, 8);
    }
}
