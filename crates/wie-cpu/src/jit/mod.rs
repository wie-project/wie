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
mod trampolines;

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
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
