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

/// Lightweight counters for `WIE_CPU=jit` diagnostics.
#[derive(Debug, Default, Clone, Copy)]
pub struct JitStats {
    /// Instructions retired via native blocks.
    pub jit_insns: u64,
    /// Instructions retired via iced fallback.
    pub iced_insns: u64,
    /// Successful block compiles.
    pub compiles: u64,
    /// Block decode declined or cold skip.
    pub compile_skip: u64,
    /// Blocks compiled on the background worker (shared counter; merged into
    /// per-thread snapshots by [`JitCpu::stats`]).
    pub bg_compiles: u64,
    /// Times this thread waited for a background compile (any resolution).
    pub compile_stalls: u64,
    /// Total wall µs this thread spent waiting on background compiles.
    pub compile_stall_us: u64,
    /// Background waits that missed the deadline and fell back to inline
    /// compilation (worker backlog / dead worker visibility).
    pub compile_stall_fallback: u64,
    /// Cache hits (native run).
    pub cache_hits: u64,
    /// Calls into host `wie_jit_load` (TLB hit or miss).
    pub load_calls: u64,
    /// Calls into host `wie_jit_store` (TLB hit or miss).
    pub store_calls: u64,
    /// Selective code-cache invalidations (SMC / X-loss / unmap).
    pub code_invs: u64,
    /// Helper sticky hit after IR miss.
    pub mem_sticky_hit: u64,
    /// Helper multi-way TLB hit.
    pub mem_multi_hit: u64,
    /// Helper region-pin hit.
    pub mem_pin_hit: u64,
    /// Helper page-walk install hit.
    pub mem_walk_hit: u64,
    /// Helper cross-page (slow).
    pub mem_cross_page: u64,
    /// Helper full slow path (`GuestMemory::{read,write}`).
    pub mem_slow: u64,
    /// Sticky miss reason: wrong/empty page key.
    pub mem_sticky_miss_key: u64,
    /// Sticky miss reason: generation mismatch.
    pub mem_sticky_miss_gen: u64,
    /// Sticky miss reason: R/W denied.
    pub mem_sticky_miss_prot: u64,
    /// Sticky hot-page replacements.
    pub mem_sticky_swaps: u64,
    /// Helper VA inside stack pin.
    pub mem_addr_stack_pin: u64,
    /// Helper VA inside heap pin.
    pub mem_addr_heap_pin: u64,
    /// Helper VA outside both pins.
    pub mem_addr_outside: u64,
    /// Times `GuestMemory::generation` increased between `run_compiled` entries.
    pub mem_gen_bumps: u64,
    /// Peak `mem_gen` observed.
    pub mem_gen_peak: u64,
    /// Last stack pin guest span size (0 if empty).
    pub pin_stack_bytes: u64,
    /// Last heap pin guest span size (0 if empty).
    pub pin_heap_bytes: u64,
    /// Last pin allow bits: bit0 stack R, bit1 stack W, bit2 heap R, bit3 heap W.
    pub pin_allow_bits: u64,
    /// Adaptive-JIT cost-model instrumentation (timing + decision counters).
    pub profile: JitProfile,
    /// Promotion ledger: enqueues already `Ready` when handed to the worker
    /// (the worker beat us — pure bookkeeping, no wait).
    pub bg_promo_hit_ready: u64,
    /// Promotion ledger: waits that resolved to a Ready block within budget.
    pub bg_promo_stalled_ok: u64,
    /// Promotion ledger: waits that exhausted their budget.
    pub bg_promo_timed_out: u64,
    /// Promotion ledger: timeouts converted into a cooldown re-arm
    /// (`Hot { visits: 0, thr: doubled }`) instead of an inline compile.
    pub bg_promo_cooled_down: u64,
    /// Promotion ledger: crossings skipped because the compile queue was too
    /// deep (backpressure) or another thread already queued the same block.
    pub bg_promo_deferred: u64,
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
