//! Hybrid Cranelift block JIT + iced interpreter fallback.
//!
//! **Strategy:** decode a lowerable block at RIP (GPR, mem, ALU, shift, call/ret,
//! jcc, SSE, bulk string); if hot enough, compile once and cache by guest entry VA.
//! Complex forms / cold sites → iced `step`.
//!
//! **Fast UCRT path:** hot CRT imports (`malloc`/`memcpy`/…) are Cranelift imports;
//! `call` to those fake-API VAs is lowered in-place (no host-stop).
//! **Block chaining:** self-loops, direct `call` to known successors, and late-bound
//! open-addressing chain-table lookups keep control in native code (no dispatcher).
//! **Shadow return stack:** `call` pushes guest return VA; `ret` validates and
//! chain-lookups the target for better call/ret prediction.

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

mod baseline;
mod block;
mod cache_persist;
mod config;
mod cpu_engine;
mod diag;
mod engine;
mod fast_api;
mod gen_tlb;
mod lower;
mod pipeline;
mod profile;
mod shared;
mod tier;
mod trampolines;

pub use cache_persist::jit_cache_pe_hash;
/// Dump mem-path histogram / profile report lines (`WIE_JIT_MEM_TRACE=1` …).
pub use diag::{dump_mem_path_stats, jit_profile_report_lines};
pub(crate) use engine::JitEngine;
pub use fast_api::{FastApiKind, JitFastPathConfig, JitHeapLayout};
pub use profile::{BgCompileProfile, JitProfile, PROFILE_BUCKETS, TimeBuckets};
pub use shared::{JitShared, PerThreadJitState};

use lower::CompiledBlock;
use shared::BgWaitCell;
use std::sync::Arc;

/// All 16 dirty bits set (GPR or XMM bank): the "everything is dirty" sentinel
/// for trampolines / fault paths that cannot track individual registers.
pub(super) const ALL_DIRTY_BITS: u16 = u16::MAX;

/// Hybrid CPU: Cranelift for hot pure-GPR blocks, iced for everything else.
///
/// Holds a shared compilation cache + guest memory (Arc) and per-thread
/// execution state (registers, TLB, chain table, shadow stack).
pub struct JitCpu {
    /// Shared compilation cache + guest memory (one per process).
    pub(crate) shared: Arc<JitShared>,
    /// Per-thread execution state (registers, TLB, chain table, etc.).
    pub(crate) thread: PerThreadJitState,
    /// Fake-API VA → fast UCRT kind (per-thread; configured once during init).
    pub(crate) fast_api: Vec<(u64, FastApiKind)>,
    /// Diagnostic counters (per-thread).
    pub(crate) stats: JitStats,
    /// Previous `GuestMemory::generation` at last `run_compiled` (diag).
    pub(crate) last_mem_gen: u64,
    /// Last `JitShared::cache_epoch` this thread re-synced its chain table at.
    /// `u64::MAX` means "dirty — resync on next dispatch" (set after the table
    /// is cleared).
    pub(crate) chain_sync_epoch: u64,
    /// How far into [`JitShared::recent_installs`] this thread has consumed.
    /// Delta-resync (G5): link only new installs instead of walking the whole
    /// Ready cache on every epoch advance.
    pub(crate) chain_watermark: usize,
    /// Last [`JitShared::invalidate_gen`] this thread observed. A mismatch
    /// means some thread dropped compiled code from the shared cache; the
    /// dispatch loop then arms the `u64::MAX` [`JitCpu::chain_sync_epoch`]
    /// sentinel so the next resync fully rebuilds the (possibly stale) chain
    /// table.
    pub(crate) seen_invalidate_gen: u64,
}

// SAFETY: Arc<JitShared> is Send + Sync (via unsafe impl above).
// PerThreadJitState is Send (raw pointers owned by one thread).
#[expect(unsafe_code)]
unsafe impl Send for JitCpu {}

#[doc(hidden)]
#[derive(Clone)]
/// Shared cache entry for one guest entry VA (`Ready` / `Never` / `Hot` / `Queued`).
pub enum CacheEntry {
    /// Native block ready to run.
    Ready(CompiledBlock),
    /// Do not retry decode/compile at this VA (cold fail or non-pure).
    Never,
    /// Visit counter + compile threshold (threshold fixed on first sight so we
    /// do not re-decode for UCRT peek on every warmup visit). Also doubles as
    /// the cooldown state: a wait timeout re-inserts the entry with
    /// `visits: 0` and a doubled `thr` (hysteresis, capped), so a stalled
    /// block keeps interpreting instead of triggering an inline double-compile.
    Hot { visits: u32, thr: u32 },
    /// Enqueued for background compilation. The [`BgWaitCell`] wakes guest
    /// threads waiting specifically for this entry (the worker calls
    /// `notify_all` when it installs the Ready block or gives up).
    Queued(Arc<BgWaitCell>),
}
/// Cranelift `FuncId`s for direct UCRT host calls.
#[derive(Clone, Copy)]
pub(super) struct UcrtImportIds {
    pub malloc: cranelift_module::FuncId,
    pub free: cranelift_module::FuncId,
    pub memcpy: cranelift_module::FuncId,
    pub strlen: cranelift_module::FuncId,
    pub iob: cranelift_module::FuncId,
    pub fwrite: cranelift_module::FuncId,
    pub fflush: cranelift_module::FuncId,
}

impl UcrtImportIds {
    pub(super) fn for_kind(self, kind: FastApiKind) -> cranelift_module::FuncId {
        match kind {
            FastApiKind::Malloc => self.malloc,
            FastApiKind::Free => self.free,
            FastApiKind::Memcpy => self.memcpy,
            FastApiKind::Strlen => self.strlen,
            FastApiKind::AcrtIobFunc => self.iob,
            FastApiKind::Fwrite => self.fwrite,
            FastApiKind::Fflush => self.fflush,
        }
    }
}

/// Execution-path counters: what retired and how the block cache fed it.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExecStats {
    /// Instructions retired via native blocks.
    pub jit_insns: u64,
    /// Instructions retired via iced fallback.
    pub iced_insns: u64,
    /// Cache hits (native run).
    pub cache_hits: u64,
    /// Selective code-cache invalidations (SMC / X-loss / unmap).
    pub code_invs: u64,
}

/// Inline compilation counters on this engine.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompileStats {
    /// Successful block compiles.
    pub compiles: u64,
    /// Block decode declined or cold skip.
    pub compile_skip: u64,
}

/// Background-worker compilation and the guest-visible waiting it causes.
#[derive(Debug, Default, Clone, Copy)]
pub struct BgCompileStats {
    /// Blocks compiled on the background worker (shared counter; merged into
    /// per-thread snapshots by [`JitCpu::stats`]).
    pub compiles: u64,
    /// Times this thread waited for a background compile (any resolution).
    pub waits: u64,
    /// Total wall µs this thread spent waiting on background compiles.
    pub wait_us: u64,
    /// Inline compilations after a wait gave up (worker dead/unavailable).
    pub inline_fallbacks: u64,
    /// Background worker-pool size at spawn (shared counter; merged into
    /// per-thread snapshots by [`JitCpu::stats`]). Zero while no pool was
    /// ever started for this `JitShared`.
    pub workers: u64,
}

/// Per-enqueue resolution of the background-promotion pipeline (G5/P2).
#[derive(Debug, Default, Clone, Copy)]
pub struct PromoLedger {
    /// Guest arrived while queued and got the compiled block after waiting.
    pub hit_ready: u64,
    /// Guest arrived, waited, and the block was already installed on recheck.
    pub stalled_ok: u64,
    /// Guest waited out the full budget without resolution.
    pub timed_out: u64,
    /// Timed-out entries re-armed as cooldown instead of inline compiling.
    pub cooled_down: u64,
    /// Promotions deferred outright (backpressure / skip-depth policy).
    pub deferred: u64,
}

/// Direct-chaining health: how often thread chain tables refresh and how
/// wide each refresh is (G5).
#[derive(Debug, Default, Clone, Copy)]
pub struct ChainStats {
    /// Chain-table resyncs on this thread (one per observed cache-epoch
    /// advance; each is an O(Ready-cache) walk).
    pub resyncs: u64,
    /// Ready entries inserted across all resyncs on this thread
    /// (`resync_entries / resyncs` = average resync width).
    pub resync_entries: u64,
    /// Direct chain-table inserts from inline compiles on this thread.
    pub inline_inserts: u64,
    /// Shared cache-epoch advances (installs + invalidations), merged from
    /// [`JitShared::chain_epoch_bumps`] by [`JitCpu::stats`].
    pub epoch_bumps: u64,
}

/// Host helper mem-path breakdown for generated-code accesses.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemPathStats {
    /// Calls into host `wie_jit_load` (TLB hit or miss).
    pub load_calls: u64,
    /// Calls into host `wie_jit_store` (TLB hit or miss).
    pub store_calls: u64,
    /// Resolved by the single-page sticky TLB.
    pub sticky_hit: u64,
    /// Resolved by the multi-page TLB.
    pub multi_hit: u64,
    /// Resolved by region-direct pin.
    pub pin_hit: u64,
    /// Resolved by page-walk.
    pub walk_hit: u64,
    /// Cross-page access split across two resolves.
    pub cross_page: u64,
    /// Full slow-path resolve (no fast structure hit).
    pub slow: u64,
    /// Sticky miss: key mismatch.
    pub sticky_miss_key: u64,
    /// Sticky miss: generation mismatch.
    pub sticky_miss_gen: u64,
    /// Sticky miss: protection mismatch.
    pub sticky_miss_prot: u64,
    /// Sticky TLB entry swaps.
    pub sticky_swaps: u64,
    /// Accesses resolved by stack pin.
    pub addr_stack_pin: u64,
    /// Accesses resolved by heap pin.
    pub addr_heap_pin: u64,
    /// Accesses outside any pinned region.
    pub addr_outside: u64,
    /// Memory-generation bumps (protection churn).
    pub gen_bumps: u64,
    /// Peak live memory generations.
    pub gen_peak: u64,
    /// Stack pin bytes.
    pub pin_stack_bytes: u64,
    /// Heap pin bytes.
    pub pin_heap_bytes: u64,
    /// Allow-bits pin bytes.
    pub pin_allow_bits: u64,
}

/// Lightweight counters for \`WIE_CPU=jit\` diagnostics, grouped by concern.
#[derive(Debug, Default, Clone, Copy)]
pub struct JitStats {
    /// Execution throughput and block-cache feeding.
    pub exec: ExecStats,
    /// Inline compilation counters.
    pub compile: CompileStats,
    /// Background-worker compilation and guest-visible waits.
    pub bg: BgCompileStats,
    /// Per-enqueue promotion outcomes.
    pub promo: PromoLedger,
    /// Direct-chaining health (G5).
    pub chain: ChainStats,
    /// Host helper mem-path breakdown for generated-code accesses.
    pub mem: MemPathStats,
    /// Decision + compile-timing diagnostics.
    pub profile: JitProfile,
}

/// Fields [`JitCpu::stats`] fills from the **shared** `JitShared` rather than
/// from this engine's own counters.
///
/// Every engine's snapshot already contains these totals, so folding two
/// snapshots must NOT add them or the shared work is counted once per engine.
/// [`JitStats::merge_engine`] skips exactly these; the process-wide fold
/// therefore starts from one engine's complete snapshot (which carries the
/// shared part exactly once) and merges the rest per-thread.
const SHARED_DERIVED_STATS: [&str; 5] = [
    "bg.compiles",
    "bg.workers",
    "chain.epoch_bumps",
    "profile.compile_us",
    "profile.compile_by_insns",
];

impl JitStats {
    /// Fold another engine's snapshot into this one.
    ///
    /// This is the cross-thread aggregation primitive: a multithreaded guest
    /// gives every guest thread its own [`JitCpu`], and each engine's counters
    /// were previously dropped on the floor, so `cpu_stats()` reported only
    /// the primary engine's work (measured on `micro-exes/cpp_threads`: 46
    /// reported against 180 interpreted instructions).
    ///
    /// Semantics per field:
    /// - monotonic counters **add** (saturating),
    /// - the shared-derived fields listed in [`SHARED_DERIVED_STATS`] are
    ///   **skipped** (see that constant),
    /// - gauges/peaks take the **max**: `mem.gen_peak` is a high-water mark,
    ///   and `mem.pin_*_bytes` / `pin_allow_bits` describe the *last* block a
    ///   thread ran, so summing them would be meaningless.
    pub fn merge_engine(&mut self, other: &JitStats) {
        // Tripwire: every field listed here must be skipped below, and any
        // field `JitCpu::stats` starts folding from `JitShared` must be listed.
        debug_assert_eq!(SHARED_DERIVED_STATS.len(), 5, "shared-derived set changed");
        let add = |a: &mut u64, b: u64| *a = a.saturating_add(b);
        let max = |a: &mut u64, b: u64| *a = (*a).max(b);

        // exec
        add(&mut self.exec.jit_insns, other.exec.jit_insns);
        add(&mut self.exec.iced_insns, other.exec.iced_insns);
        add(&mut self.exec.cache_hits, other.exec.cache_hits);
        add(&mut self.exec.code_invs, other.exec.code_invs);
        // compile
        add(&mut self.compile.compiles, other.compile.compiles);
        add(&mut self.compile.compile_skip, other.compile.compile_skip);
        // bg (compiles / workers are shared-derived: skipped)
        add(&mut self.bg.waits, other.bg.waits);
        add(&mut self.bg.wait_us, other.bg.wait_us);
        add(&mut self.bg.inline_fallbacks, other.bg.inline_fallbacks);
        // promo
        add(&mut self.promo.hit_ready, other.promo.hit_ready);
        add(&mut self.promo.stalled_ok, other.promo.stalled_ok);
        add(&mut self.promo.timed_out, other.promo.timed_out);
        add(&mut self.promo.cooled_down, other.promo.cooled_down);
        add(&mut self.promo.deferred, other.promo.deferred);
        // chain (epoch_bumps is shared-derived: skipped)
        add(&mut self.chain.resyncs, other.chain.resyncs);
        add(&mut self.chain.resync_entries, other.chain.resync_entries);
        add(&mut self.chain.inline_inserts, other.chain.inline_inserts);
        // mem: counters add, gauges/peaks take the max
        add(&mut self.mem.load_calls, other.mem.load_calls);
        add(&mut self.mem.store_calls, other.mem.store_calls);
        add(&mut self.mem.sticky_hit, other.mem.sticky_hit);
        add(&mut self.mem.multi_hit, other.mem.multi_hit);
        add(&mut self.mem.pin_hit, other.mem.pin_hit);
        add(&mut self.mem.walk_hit, other.mem.walk_hit);
        add(&mut self.mem.cross_page, other.mem.cross_page);
        add(&mut self.mem.slow, other.mem.slow);
        add(&mut self.mem.sticky_miss_key, other.mem.sticky_miss_key);
        add(&mut self.mem.sticky_miss_gen, other.mem.sticky_miss_gen);
        add(&mut self.mem.sticky_miss_prot, other.mem.sticky_miss_prot);
        add(&mut self.mem.sticky_swaps, other.mem.sticky_swaps);
        add(&mut self.mem.addr_stack_pin, other.mem.addr_stack_pin);
        add(&mut self.mem.addr_heap_pin, other.mem.addr_heap_pin);
        add(&mut self.mem.addr_outside, other.mem.addr_outside);
        add(&mut self.mem.gen_bumps, other.mem.gen_bumps);
        max(&mut self.mem.gen_peak, other.mem.gen_peak);
        max(&mut self.mem.pin_stack_bytes, other.mem.pin_stack_bytes);
        max(&mut self.mem.pin_heap_bytes, other.mem.pin_heap_bytes);
        max(&mut self.mem.pin_allow_bits, other.mem.pin_allow_bits);
        // profile (compile_us / compile_by_insns are shared-derived: skipped)
        add(
            &mut self.profile.eager_compiles,
            other.profile.eager_compiles,
        );
        add(&mut self.profile.hot_compiles, other.profile.hot_compiles);
        add(&mut self.profile.bg_enqueues, other.profile.bg_enqueues);
        add(&mut self.profile.bg_wait_hits, other.profile.bg_wait_hits);
        add(
            &mut self.profile.bg_wait_timeouts,
            other.profile.bg_wait_timeouts,
        );
        add(
            &mut self.profile.inline_compiles,
            other.profile.inline_compiles,
        );
        add(
            &mut self.profile.iced_fallbacks,
            other.profile.iced_fallbacks,
        );
        add(&mut self.profile.never_marks, other.profile.never_marks);
        add(
            &mut self.profile.warm_ledger_hits,
            other.profile.warm_ledger_hits,
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod stats_merge_tests {
    use super::*;

    /// Two engines' per-thread counters add, and the process-wide fold of a
    /// multithreaded guest's stats therefore covers every worker. This is the
    /// regression guard for the dropped-worker bug: before the merge, only the
    /// primary engine was ever read.
    #[test]
    fn merge_engine_adds_per_thread_counters() {
        let mut primary = JitStats::default();
        primary.exec.jit_insns = 1000;
        primary.exec.iced_insns = 40;
        primary.exec.cache_hits = 7;
        primary.compile.compiles = 3;
        primary.bg.waits = 1;
        primary.promo.hit_ready = 2;
        primary.chain.resyncs = 5;
        primary.mem.load_calls = 900;
        primary.profile.eager_compiles = 4;

        let mut worker = JitStats::default();
        worker.exec.jit_insns = 20_000;
        worker.exec.iced_insns = 1_400;
        worker.exec.cache_hits = 90;
        worker.compile.compiles = 30;
        worker.bg.waits = 12;
        worker.promo.hit_ready = 20;
        worker.chain.resyncs = 50;
        worker.mem.load_calls = 9_000;
        worker.profile.eager_compiles = 40;

        let mut agg = primary;
        agg.merge_engine(&worker);

        assert_eq!(agg.exec.jit_insns, 21_000);
        assert_eq!(agg.exec.iced_insns, 1_440);
        assert_eq!(agg.exec.cache_hits, 97);
        assert_eq!(agg.compile.compiles, 33);
        assert_eq!(agg.bg.waits, 13);
        assert_eq!(agg.promo.hit_ready, 22);
        assert_eq!(agg.chain.resyncs, 55);
        assert_eq!(agg.mem.load_calls, 9_900);
        assert_eq!(agg.profile.eager_compiles, 44);
    }

    /// Gauges and peaks must take the max: summing `gen_peak` (a high-water
    /// mark) or the last-block pin sizes across engines would report a
    /// generation or a pin span no thread ever saw.
    #[test]
    fn merge_engine_takes_max_for_peaks_and_gauges() {
        let mut a = JitStats::default();
        a.mem.gen_peak = 9;
        a.mem.pin_stack_bytes = 0x8000;
        a.mem.pin_heap_bytes = 0x100;
        a.mem.pin_allow_bits = 0b1010;
        let mut b = JitStats::default();
        b.mem.gen_peak = 4;
        b.mem.pin_stack_bytes = 0x4000;
        b.mem.pin_heap_bytes = 0x900;
        b.mem.pin_allow_bits = 0b0101;

        a.merge_engine(&b);

        assert_eq!(a.mem.gen_peak, 9);
        assert_eq!(a.mem.pin_stack_bytes, 0x8000);
        assert_eq!(a.mem.pin_heap_bytes, 0x900);
        assert_eq!(a.mem.pin_allow_bits, 0b1010);
    }

    /// The shared-derived fields are already present in *every* engine's
    /// snapshot (they come from the one `JitShared`), so folding a second
    /// snapshot must leave them untouched — otherwise a process with N guest
    /// threads reports background compiles and chain-epoch bumps N times.
    ///
    /// This test is the tripwire for [`SHARED_DERIVED_STATS`]: it enumerates
    /// the list, so a field that `JitCpu::stats` starts folding from `JitShared`
    /// must be added to the list in the same change.
    #[test]
    fn merge_engine_never_double_counts_shared_derived_fields() {
        // One engine's complete snapshot (primary thread).
        let mut agg = JitStats::default();
        agg.bg.compiles = 11;
        agg.bg.workers = 4;
        agg.chain.epoch_bumps = 17;
        agg.profile.compile_us = 900;
        agg.profile.compile_by_insns.record(1, 100);
        agg.profile.compile_by_insns.record(1, 200);

        // A second engine's snapshot of the SAME shared state.
        let mut other = JitStats::default();
        other.bg.compiles = 11;
        other.bg.workers = 4;
        other.chain.epoch_bumps = 17;
        other.profile.compile_us = 900;
        other.profile.compile_by_insns.record(1, 100);
        other.profile.compile_by_insns.record(1, 200);

        agg.merge_engine(&other);

        assert_eq!(agg.bg.compiles, 11, "bg.compiles is shared");
        assert_eq!(agg.bg.workers, 4, "bg.workers is shared");
        assert_eq!(agg.chain.epoch_bumps, 17, "chain.epoch_bumps is shared");
        assert_eq!(agg.profile.compile_us, 900, "compile_us is shared");
        assert_eq!(agg.profile.compile_by_insns.count(0), 2);
        assert_eq!(agg.profile.compile_by_insns.total_us(0), 300);

        // The list is the contract; assert it names what the test just proved.
        for field in SHARED_DERIVED_STATS {
            assert!(!field.is_empty());
        }
        assert_eq!(SHARED_DERIVED_STATS.len(), 5);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
