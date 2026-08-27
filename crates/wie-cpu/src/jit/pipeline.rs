//! Per-thread JIT execution pipeline: dispatch, compile, run, invalidate.
//!
//! The hot path (`step_one`, `try_compile`, `finish_compiled`, `run_compiled`,
//! `invalidate_code_range`) runs here; methods called from `cpu_engine.rs` or
//! `mod.rs` tests are `pub(super)`.

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

use super::ALL_DIRTY_BITS;
use super::JitStats;
use super::block::{self, BlockKind, decode_pure_gpr_block, pure_is_self_loop};
use super::config::{
    BG_QUEUE_CAP, COOLDOWN_THRESHOLD_CAP, JitConfig, WORK_THRESHOLD_CEILING, WORK_THRESHOLD_FLOOR,
};
use super::fast_api::{FastApiKind, JitFastPathConfig, install_heap_layout};
use super::gen_tlb::GenTlb;
use super::lower::{
    self, CompiledBlock, JitCtx, MemPathSlice, MemPin, PIN_SLOTS, STICKY_WAYS, TLB_EMPTY, XmmSlot,
    chain_table_clear, chain_table_insert,
};
use super::shared::{BgEnqueueOutcome, BgWaitCell, BgWaitState, JitShared, PerThreadJitState};
use super::{CacheEntry, JitCpu};
use crate::CpuError;
use crate::exec::{self, StepResult};
use crate::mem::{self, GuestMemory, PAGE_SIZE, PAGE_SIZE_USIZE};
use crate::regs::Rflags;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Sample every Nth iced step into the opcode histogram.
pub(super) const OPCODE_SAMPLE_EVERY: u32 = 64;

/// Sampled iced-residue opcode histogram, keyed by the iced-x86 mnemonic
/// discriminant (`WIE_JIT_OPCODE_HISTO=1` to record + emit). Recording is
/// sampled (1/64 steps) and env-gated so the hot path pays one relaxed load
/// when off; the bucket write is a relaxed fetch_add.
pub(super) static OPCODE_HISTO: LazyLock<Box<[AtomicU64]>> = LazyLock::new(|| {
    std::iter::repeat_with(|| AtomicU64::new(0))
        .take(2048)
        .collect()
});
/// Total samples taken (for the dump header).
pub(super) static OPCODE_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Park quantum for a co-waiter whose one-shot token receiver was already
/// taken by another thread (rare: two guest threads executing the same
/// not-yet-compiled block). The primary waiter wakes instantly; co-waiters
/// re-check the cache at this cadence until the budget runs out.
const BG_COWAIT_QUANTUM: Duration = Duration::from_micros(200);

/// Queue depth at or above which waiting for this entry's compile is futile:
/// the worker is several block-latencies behind, so the wait budget would
/// expire unserved. At sustained backlog nearly every full-budget stall timed
/// out (measured 65 %), so past this depth we skip straight to cooldown
/// hysteresis and keep interpreting instead of blocking the guest thread.
const BG_WAIT_SKIP_DEPTH: u64 = 4;

/// Decode the instruction at `rip` and bucket its mnemonic. Runs on the
/// sampling seam only (every [`OPCODE_SAMPLE_EVERY`]th iced step).
fn record_opcode_sample(mem: &GuestMemory, rip: u64) {
    let mut fetch_buf = [0_u8; 15];
    let Ok(n) = mem.fetch_into(rip, &mut fetch_buf) else {
        return;
    };
    let Some(bytes) = fetch_buf.get(..n) else {
        return;
    };
    let mut decoder = iced_x86::Decoder::with_ip(64, bytes, rip, iced_x86::DecoderOptions::NONE);
    let instr = decoder.decode();
    if instr.is_invalid() {
        return;
    }
    let m = instr.mnemonic() as usize;
    OPCODE_SAMPLES.fetch_add(1, Ordering::Relaxed);
    if let Some(bucket) = OPCODE_HISTO.get(m) {
        bucket.fetch_add(1, Ordering::Relaxed);
    }
}

impl JitCpu {
    /// Open hybrid JIT on the host ISA (ARM64 on Apple Silicon).
    #[must_use]
    pub fn open_x86_64() -> Self {
        let shared = JitShared::new();
        Self {
            shared: Arc::new(shared),
            thread: PerThreadJitState::new(),
            fast_api: Vec::new(),
            stats: JitStats::default(),
            last_mem_gen: 0,
            chain_sync_epoch: 0,
            chain_watermark: 0,
            seen_invalidate_gen: 0,
        }
    }

    /// Borrow the shared JIT state (compilation cache + guest memory).
    #[must_use]
    pub fn shared_jit(&self) -> &Arc<JitShared> {
        &self.shared
    }

    /// Create a per-thread engine sharing the compilation cache + guest memory.
    #[must_use]
    pub fn new_shared(shared: Arc<JitShared>) -> Self {
        Self {
            shared,
            thread: PerThreadJitState::new(),
            fast_api: Vec::new(),
            stats: JitStats::default(),
            last_mem_gen: 0,
            chain_sync_epoch: 0,
            chain_watermark: 0,
            seen_invalidate_gen: 0,
        }
    }

    /// Snapshot of JIT diagnostic counters.
    ///
    /// Merges the shared background-compile counter into the per-thread
    /// snapshot so `WIE_RUNTIME_PROFILE` sees background work.
    #[must_use]
    pub fn stats(&self) -> JitStats {
        let mut s = self.stats;
        s.bg.compiles =
            s.bg.compiles
                .saturating_add(self.shared.bg_compiles.load(Ordering::Relaxed));
        s.bg.workers = u64::try_from(self.shared.bg_workers_spawned.load(Ordering::Relaxed))
            .unwrap_or(u64::MAX);
        s.chain.epoch_bumps = s
            .chain
            .epoch_bumps
            .saturating_add(self.shared.chain_epoch_bumps.load(Ordering::Relaxed));
        // Fold the worker's lock-free compile timing into the per-thread
        // snapshot so `WIE_RUNTIME_PROFILE` sees background work.
        let bg = &self.shared.bg_compile;
        s.profile.compile_us = s
            .profile
            .compile_us
            .saturating_add(bg.compile_us.load(Ordering::Relaxed));
        bg.fold_into(&mut s.profile.compile_by_insns);
        s
    }

    /// Install UCRT/heap fast-path config (called once after fake-API table build).
    pub fn configure_fast_path(&mut self, cfg: JitFastPathConfig) {
        install_heap_layout(cfg.heap);
        let pairs = cfg.pairs.clone();
        self.fast_api = cfg.pairs;
        // Mirror the pairs so the background worker lowers UCRT calls exactly
        // like the inline path (same `call_fast` → same emitted code). The Arc
        // hands the worker a shared immutable table (no per-job Vec clone).
        *self.shared.bg_fast_api.lock().unwrap() = Arc::from(pairs);
        self.clear_compiled();
        self.invalidate_chain_and_shadow();
    }

    pub(super) fn insert_ready(&mut self, rip: u64, compiled: CompiledBlock) {
        self.shared.insert_ready(rip, compiled);
    }

    /// Mark `rip` as `Never` (cold / non-pure) and count it for diagnostics.
    fn mark_never(&mut self, rip: u64) {
        self.shared.cache.pin().insert(rip, CacheEntry::Never);
        self.stats.profile.never_marks = self.stats.profile.never_marks.saturating_add(1);
    }

    /// Drop every Ready block from the shared cache (full-flush paths).
    ///
    /// Repairs only the shared tables plus THIS thread's view; every other
    /// guest thread's chain table may still map the dropped VAs to stale fn
    /// pointers. Bumps [`JitShared::invalidate_gen`] so those threads force a
    /// full chain-table rebuild on their next dispatch. Covers all callers:
    /// `configure_fast_path`, `install_runtime_hooks`, the
    /// `FlushInstructionCache` size==0 flush, and the
    /// `drain_pending_code_writes` overflow flush.
    pub(super) fn clear_compiled(&mut self) {
        self.shared.cache.pin().clear();
        self.shared.chain_ids.pin().clear();
        self.shared.code_pages.lock().unwrap().clear();
        // Release: pairs with the dispatcher's Acquire load (see below).
        self.shared.invalidate_gen.fetch_add(1, Ordering::Release);
    }

    pub(super) fn invalidate_code_range(&mut self, addr: u64, len: usize) {
        {
            let cache = self.shared.cache.pin();
            if cache.is_empty() || len == 0 {
                return;
            }
        }
        if !self.code_pages_overlap(addr, len) {
            return;
        }
        let write_end = addr.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        let to_drop: Vec<u64> = {
            let cache = self.shared.cache.pin();
            cache
                .iter()
                .filter_map(|(va, entry)| match entry {
                    CacheEntry::Ready(c)
                        if ranges_overlap(c.guest_start, c.guest_end, addr, write_end) =>
                    {
                        Some(*va)
                    }
                    _ => None,
                })
                .collect()
        };
        if to_drop.is_empty() {
            return;
        }
        for va in &to_drop {
            let cache = self.shared.cache.pin();
            if let Some(CacheEntry::Ready(c)) = cache.remove(va).cloned() {
                drop(cache);
                self.shared
                    .code_pages_remove_range(c.guest_start, c.guest_end);
            }
            self.shared.chain_ids.pin().remove(va);
        }
        // Shared drop signal: other threads' chain tables still map these VAs
        // to the removed fn pointers and would chain into stale code forever —
        // this method repairs only the invalidating thread's own state.
        // Release: pairs with the dispatcher's Acquire load — a thread that
        // observes this bump must also observe every drop made before it.
        self.shared.invalidate_gen.fetch_add(1, Ordering::Release);
        self.stats.exec.code_invs = self.stats.exec.code_invs.saturating_add(1);
        self.invalidate_chain_and_shadow();
        if JitConfig::get().chain_enabled() {
            let cache = self.shared.cache.pin();
            for (va, entry) in cache.iter() {
                if let CacheEntry::Ready(c) = entry {
                    let fn_ptr = c.func as usize as u64;
                    chain_table_insert(self.thread.chain_slots.as_mut(), *va, fn_ptr);
                }
            }
        }
    }

    #[inline]
    pub(super) fn code_pages_overlap(&self, addr: u64, len: usize) -> bool {
        self.shared.code_pages_overlap(addr, len)
    }

    pub(super) fn drain_pending_code_writes(&mut self) {
        let overflow = self
            .shared
            .pending_code_overflow
            .swap(false, Ordering::Relaxed);
        let pages = std::mem::take(&mut *self.shared.pending_code_writes.lock().unwrap());
        if overflow {
            if !self.shared.cache.pin().is_empty() {
                self.clear_compiled();
                self.invalidate_chain_and_shadow();
                self.stats.exec.code_invs = self.stats.exec.code_invs.saturating_add(1);
            }
            return;
        }
        for page in pages {
            self.invalidate_code_range(page << 12, PAGE_SIZE_USIZE);
        }
    }

    const PENDING_WRITE_PAGE_CAP: usize = 256;

    pub(super) fn note_code_write(&self, address: u64, len: usize) {
        if len == 0 || self.shared.pending_code_overflow.load(Ordering::Relaxed) {
            return;
        }
        let len_u = u64::try_from(len).unwrap_or(u64::MAX);
        let end = address.saturating_add(len_u);
        if end <= address && len > 0 {
            self.shared
                .pending_code_overflow
                .store(true, Ordering::Relaxed);
            self.shared.pending_code_writes.lock().unwrap().clear();
            return;
        }
        let mut guard = self.shared.pending_code_writes.lock().unwrap();
        let mut page = address >> 12;
        let last = end.saturating_sub(1) >> 12;
        while page <= last {
            if guard.len() >= Self::PENDING_WRITE_PAGE_CAP {
                self.shared
                    .pending_code_overflow
                    .store(true, Ordering::Relaxed);
                guard.clear();
                return;
            }
            if guard.last().copied() != Some(page) {
                guard.push(page);
            }
            page = page.saturating_add(1);
        }
    }

    pub(super) fn code_inv_span_for_free(
        &self,
        addr: u64,
        size: usize,
        free_type: u32,
    ) -> Option<(u64, usize)> {
        if (free_type & mem::MEM_RELEASE) != 0 {
            return self
                .shared
                .mem
                .read()
                .unwrap()
                .allocation_span_at_base(addr);
        }
        if (free_type & mem::MEM_DECOMMIT) != 0 {
            if size == 0 {
                return None;
            }
            let page_base = addr & !(PAGE_SIZE - 1);
            let end = addr.saturating_add(u64::try_from(size).unwrap_or(u64::MAX));
            let page_end = end
                .saturating_add(PAGE_SIZE - 1)
                .wrapping_div(PAGE_SIZE)
                .saturating_mul(PAGE_SIZE);
            let n = usize::try_from(page_end.saturating_sub(page_base)).unwrap_or(0);
            if n == 0 {
                return None;
            }
            return Some((page_base, n));
        }
        None
    }

    #[cfg(test)]
    #[must_use]
    pub(super) fn has_ready_at(&self, rip: u64) -> bool {
        matches!(
            self.shared.cache.pin().get(&rip),
            Some(CacheEntry::Ready(_))
        )
    }

    /// Returns `(result, guest_insns_retired)` for budget accounting.
    pub(super) fn step_one(&mut self) -> Result<(StepResult, usize), CpuError> {
        let rip = self.thread.regs.rip;
        // Bake-before-decode: snapshot the shared invalidation generation
        // BEFORE any guest bytes are decoded for compilation. A block built
        // from bytes fetched after this point can only be stale if an
        // invalidation landed later — which bumps the generation past this
        // bake, so the emitted guards / chain_tail catch it. Baking after
        // the decode could instead stamp a NEWER generation over
        // pre-invalidation bytes and hide a stale block forever.
        let inv_gen = self.shared.invalidate_gen.load(Ordering::Acquire);
        if let Some(hook) = self.thread.hooks.as_ref()
            && hook.should_host_stop(rip)
        {
            return Ok((
                StepResult::HostStop {
                    address: rip,
                    size: 1,
                },
                0,
            ));
        }

        if self.shared.engine_ready.load(Ordering::Relaxed) {
            // Read-first pattern: acquire a pin, clone entry, drop pin, then act.
            let entry = {
                let cache = self.shared.cache.pin();
                cache.get(&rip).cloned()
            };
            if let Some(entry) = entry {
                match entry {
                    CacheEntry::Ready(compiled) => {
                        self.stats.exec.cache_hits = self.stats.exec.cache_hits.saturating_add(1);
                        let meta = CompiledRunMeta::from(&compiled);
                        return Ok(self.finish_compiled(rip, meta));
                    }
                    CacheEntry::Never => { /* fall through to iced */ }
                    CacheEntry::Queued(notify) => {
                        // About to execute the entry the worker is compiling:
                        // block only for this entry, and only briefly. On
                        // timeout the wait attempts an inline compile on the
                        // guest thread (installing Ready on success or Never
                        // on failure) — the queued Bg job still races but
                        // last-writer-wins on the cache. Inline fallback here
                        // is only for a dead worker that never timed out.
                        if let Some(compiled) = self.wait_bg_ready(rip, &notify) {
                            let meta = CompiledRunMeta::from(&compiled);
                            return Ok(self.finish_compiled(rip, meta));
                        }
                        if !self.shared.bg_alive.load(Ordering::Relaxed) {
                            self.stats.bg.inline_fallbacks =
                                self.stats.bg.inline_fallbacks.saturating_add(1);
                            if let Some(compiled) = self.try_compile(rip) {
                                let meta = CompiledRunMeta::from(&compiled);
                                self.insert_ready(rip, compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            self.mark_never(rip);
                        }
                        // Worker alive: either inline compile succeeded and we
                        // already returned, or it failed and we marked Never,
                        // or the entry resolved/re-decided mid-wait — fall
                        // through to iced this visit.
                    }
                    CacheEntry::Hot { visits, thr } => {
                        let next = visits.saturating_add(1);
                        if thr > 0 && next < thr {
                            self.shared
                                .cache
                                .pin()
                                .insert(rip, CacheEntry::Hot { visits: next, thr });
                        } else {
                            // Threshold crossed. Prefer the background worker:
                            // enqueue and keep executing on iced this visit; the
                            // compiled block lands in the cache for the next one.
                            // Inline compilation is only the fallback.
                            self.stats.profile.hot_compiles =
                                self.stats.profile.hot_compiles.saturating_add(1);
                            // Backpressure: a deep compile queue means guests
                            // would stall past their wait budget anyway. Defer
                            // promotion (doubled threshold) instead of feeding
                            // the backlog; the block re-promotes after it.
                            if self.bg_queue_too_deep() {
                                self.defer_promotion(rip, thr);
                            } else {
                                let kind = {
                                    let mem = self.shared.mem.read().unwrap();
                                    block::decode_pure_gpr_block(
                                        &mem,
                                        self.thread.hooks.as_ref(),
                                        rip,
                                    )
                                };
                                self.thread.pending_promote_thr = thr;
                                // Urgent lane: this thread keeps interpreting
                                // THIS block now and re-enters the Queued
                                // entry within one loop iteration, blocking
                                // on its cell then — the compile latency is
                                // on this guest thread's critical path.
                                match self.enqueue_bg(rip, &kind, inv_gen, true) {
                                    BgEnqueueOutcome::Queued(_) => {
                                        // Continue on iced this visit; the wait
                                        // (if any) happens when this thread is
                                        // about to execute the entry.
                                    }
                                    BgEnqueueOutcome::Ready => {
                                        self.stats.promo.hit_ready =
                                            self.stats.promo.hit_ready.saturating_add(1);
                                    }
                                    BgEnqueueOutcome::Unavailable
                                        if self.shared.entry_queued(rip)
                                            && self.shared.bg_alive.load(Ordering::Relaxed) =>
                                    {
                                        // Another thread already queued this exact
                                        // block: never build it twice — keep iced.
                                        self.stats.promo.deferred =
                                            self.stats.promo.deferred.saturating_add(1);
                                    }
                                    BgEnqueueOutcome::Unavailable => {
                                        // Worker dead / disabled / queue send failed:
                                        // inline compilation is permitted here.
                                        if let Some(compiled) =
                                            self.try_compile_from_kind(rip, kind, inv_gen)
                                        {
                                            let meta = CompiledRunMeta::from(&compiled);
                                            self.insert_ready(rip, compiled);
                                            return Ok(self.finish_compiled(rip, meta));
                                        }
                                        self.mark_never(rip);
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                // Miss: decode the block once and route the same BlockKind through
                // fast-UCRT / self-loop / try_compile. Was three iced-decode passes
                // over the same up-to-96-insn body before.
                let kind = {
                    let mem = self.shared.mem.read().unwrap();
                    block::decode_pure_gpr_block(&mem, self.thread.hooks.as_ref(), rip)
                };
                let is_ucrt = block_kind_ends_in_fast_ucrt(&self.shared, &self.fast_api, &kind);
                let is_loop = pure_is_self_loop(&kind, rip);
                let pure_insns = match &kind {
                    BlockKind::Pure { insns, .. } => insns.len(),
                    BlockKind::NotPure => 0,
                };
                let (thr, eager) = select_hot_threshold(
                    is_ucrt,
                    is_loop,
                    pure_insns,
                    JitConfig::get().pure_loop_hotness(),
                    JitConfig::get().hotness_threshold(),
                    JitConfig::get().target_work(),
                    JitConfig::get().eager_block_insns(),
                );
                if eager {
                    // Eager compile: the entry is required NOW (there may be no
                    // revisit). Prefer the background worker and block briefly
                    // on this entry only. On timeout the wait attempts an inline
                    // compile on the guest thread instead of cooling down.
                    self.stats.profile.eager_compiles =
                        self.stats.profile.eager_compiles.saturating_add(1);
                    if self.bg_queue_too_deep() {
                        // Backpressure: don't feed the backlog; re-promote soon
                        // via the (doubled) visit threshold instead.
                        self.defer_promotion(rip, thr);
                    } else {
                        self.thread.pending_promote_thr = thr;
                        // Urgent lane: the caller blocks on this job's cell
                        // immediately below (`wait_bg_ready`).
                        match self.enqueue_bg(rip, &kind, inv_gen, true) {
                            BgEnqueueOutcome::Queued(notify) => {
                                if let Some(compiled) = self.wait_bg_ready(rip, &notify) {
                                    let meta = CompiledRunMeta::from(&compiled);
                                    return Ok(self.finish_compiled(rip, meta));
                                }
                                if !self.shared.bg_alive.load(Ordering::Relaxed) {
                                    // Deadline missed AND worker gone: inline
                                    // compile replaces the Queued entry.
                                    self.stats.bg.inline_fallbacks =
                                        self.stats.bg.inline_fallbacks.saturating_add(1);
                                    if let Some(compiled) =
                                        self.try_compile_from_kind(rip, kind, inv_gen)
                                    {
                                        let meta = CompiledRunMeta::from(&compiled);
                                        self.insert_ready(rip, compiled);
                                        return Ok(self.finish_compiled(rip, meta));
                                    }
                                    self.mark_never(rip);
                                }
                                // Worker alive: timeout already attempted an
                                // inline compile (installed Ready/Never) or the
                                // entry resolved/vanished mid-wait — fall
                                // through to iced this visit if not Ready.
                            }
                            BgEnqueueOutcome::Ready => {
                                // Worker beat us: the cache already holds Ready.
                                self.stats.promo.hit_ready =
                                    self.stats.promo.hit_ready.saturating_add(1);
                                let compiled = {
                                    let cache = self.shared.cache.pin();
                                    cache.get(&rip).and_then(|e| match e {
                                        CacheEntry::Ready(c) => Some(*c),
                                        _ => None,
                                    })
                                };
                                if let Some(compiled) = compiled {
                                    let meta = CompiledRunMeta::from(&compiled);
                                    return Ok(self.finish_compiled(rip, meta));
                                }
                                // Vanished (invalidated mid-flight) — fall through to iced.
                            }
                            BgEnqueueOutcome::Unavailable => {
                                if self.shared.entry_queued(rip)
                                    && self.shared.bg_alive.load(Ordering::Relaxed)
                                {
                                    // Dedup hit: another thread already queued this
                                    // exact block. Never build it twice — keep iced;
                                    // it resolves Ready shortly.
                                    self.stats.promo.deferred =
                                        self.stats.promo.deferred.saturating_add(1);
                                } else if let Some(compiled) =
                                    self.try_compile_from_kind(rip, kind, inv_gen)
                                {
                                    let meta = CompiledRunMeta::from(&compiled);
                                    self.insert_ready(rip, compiled);
                                    return Ok(self.finish_compiled(rip, meta));
                                } else {
                                    self.mark_never(rip);
                                }
                            }
                        }
                    }
                } else {
                    self.shared
                        .cache
                        .pin()
                        .insert(rip, CacheEntry::Hot { visits: 1, thr });
                }
            }
        }

        // Iced does not maintain the shadow return stack — drop prediction.
        self.thread.shadow_sp = 0;
        self.stats.exec.iced_insns = self.stats.exec.iced_insns.saturating_add(1);
        self.stats.profile.iced_fallbacks = self.stats.profile.iced_fallbacks.saturating_add(1);
        // Sampled opcode histogram over the interpreted residue (Phase-0
        // counter): bucket the mnemonic of every Nth step, only when enabled.
        // The sampling counter always advances so the interval is stable.
        self.thread.opcode_sample_i = self.thread.opcode_sample_i.wrapping_add(1);
        if self
            .thread
            .opcode_sample_i
            .is_multiple_of(OPCODE_SAMPLE_EVERY)
            && JitConfig::get().opcode_hist_enabled()
        {
            let mem = self.shared.mem.read().unwrap();
            record_opcode_sample(&mem, self.thread.regs.rip);
        }
        // Inline step_once_result: push RIP trace, call exec::step, update counters.
        {
            let rip = self.thread.regs.rip;
            let i = self.thread.rip_trace_i & 31;
            if let Some(slot) = self.thread.rip_trace.get_mut(i) {
                *slot = rip;
            }
            self.thread.rip_trace_i = self.thread.rip_trace_i.wrapping_add(1);
            if self.thread.rip_trace_n < 32 {
                self.thread.rip_trace_n = self.thread.rip_trace_n.saturating_add(1);
            }
        }
        let hook = self.thread.hooks.as_ref();
        let result = exec::step(
            &self.shared.mem.read().unwrap(),
            &mut self.thread.regs,
            hook,
        )?;
        if matches!(result, StepResult::Continue) {
            self.thread.iced_steps = self.thread.iced_steps.saturating_add(1);
        }
        self.drain_pending_code_writes();
        Ok((result, 1))
    }

    /// Hand a block to the background compiler pool.
    ///
    /// Queues the exact decoded block so a worker compiles the same bytes the
    /// guest classified. `inv_gen` must be the generation snapshot taken
    /// before those bytes were decoded (see the bake-before-decode note in
    /// [`Self::step_one`]). The cache entry transitions to `Queued` only after
    /// the queue slot is reserved (a full queue must never strand a Queued
    /// entry).
    ///
    /// Lane selection: `urgent == true` marks jobs whose producer blocks (or
    /// is about to block within one interpreted iteration) on this exact
    /// compile — both promotion sites below qualify. Speculative prefetches
    /// (`precompile_deferred_at`) pass `false`. Urgent jobs are processed
    /// before every normal job already queued (see `BgQueue`'s ordering
    /// contract).
    pub(super) fn enqueue_bg(
        &mut self,
        rip: u64,
        kind: &BlockKind,
        inv_gen: u64,
        urgent: bool,
    ) -> BgEnqueueOutcome {
        if !self.shared.bg_enabled_here() || !self.shared.engine_ready.load(Ordering::Relaxed) {
            return BgEnqueueOutcome::Unavailable;
        }
        self.shared.ensure_bg_worker();
        if !self.shared.bg_alive.load(Ordering::Relaxed) {
            return BgEnqueueOutcome::Unavailable;
        }
        if matches!(kind, BlockKind::NotPure) {
            self.mark_never(rip);
            return BgEnqueueOutcome::Unavailable;
        }
        let Some(pool) = self.shared.bg_pool_arc() else {
            return BgEnqueueOutcome::Unavailable;
        };
        if !pool.push(rip, kind.clone(), inv_gen, urgent) {
            return BgEnqueueOutcome::Unavailable;
        }
        // One in-flight item a worker has not processed yet.
        self.shared.bg_queue_depth.fetch_add(1, Ordering::Relaxed);
        // Transition the entry (only from Hot/absent; never clobber Ready/Never).
        let cache = self.shared.cache.pin();
        match cache.get(&rip) {
            None | Some(CacheEntry::Hot { .. }) => {
                let cell = BgWaitCell::new(self.thread.pending_promote_thr);
                cache.insert(rip, CacheEntry::Queued(Arc::clone(&cell)));
                self.stats.profile.bg_enqueues = self.stats.profile.bg_enqueues.saturating_add(1);
                BgEnqueueOutcome::Queued(cell)
            }
            Some(CacheEntry::Queued(_) | CacheEntry::Never) => BgEnqueueOutcome::Unavailable,
            Some(CacheEntry::Ready(_)) => BgEnqueueOutcome::Ready,
        }
    }

    /// Wait (bounded) for the background worker to resolve `rip`.
    ///
    /// The guest only reaches this when it is about to execute the entry. The
    /// wait is per-entry (the one-shot token cell from the Queued entry, not
    /// the whole queue) and time-boxed by [`JitConfig::bg_wait_timeout`].
    ///
    /// On budget exhaustion the guest thread attempts an inline compile
    /// instead of cooling down: if it succeeds the Ready block is installed
    /// and returned, otherwise the entry is marked Never. This keeps hot
    /// blocks from running interpreted for thousands of visits while the
    /// worker catches up (last-writer-wins if the worker later installs
    /// the same rip). A deep queue skips the wait entirely and takes the
    /// cooldown hysteresis path instead. Callers must not inline-compile
    /// separately while the worker is alive — the timeout path already
    /// did so.
    ///
    /// Returns the Ready block once installed (by worker or inline fallback).
    pub(super) fn wait_bg_ready(&mut self, rip: u64, cell: &BgWaitCell) -> Option<CompiledBlock> {
        if !self.shared.bg_alive.load(Ordering::Relaxed) {
            return None; // worker gone: caller handles inline fallback
        }
        if self.shared.bg_queue_depth.load(Ordering::Relaxed) >= BG_WAIT_SKIP_DEPTH {
            self.stats.promo.deferred = self.stats.promo.deferred.saturating_add(1);
            self.arm_cooldown(rip, cell.threshold());
            return None;
        }
        let budget = JitConfig::get().bg_wait_timeout();
        let start = Instant::now();
        loop {
            let state = {
                let cache = self.shared.cache.pin();
                match cache.get(&rip) {
                    Some(CacheEntry::Ready(c)) => Some(BgWaitState::Ready(*c)),
                    Some(CacheEntry::Never) => Some(BgWaitState::Never),
                    Some(CacheEntry::Queued(_)) => None,
                    // Re-decided or invalidated while we waited: no cooldown
                    // (the entry is gone / someone else owns its fate).
                    Some(CacheEntry::Hot { .. }) | None => return None,
                }
            };
            match state {
                Some(BgWaitState::Ready(c)) => {
                    self.stats.bg.waits = self.stats.bg.waits.saturating_add(1);
                    self.stats.bg.wait_us = self.stats.bg.wait_us.saturating_add(
                        u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX),
                    );
                    self.stats.profile.bg_wait_hits =
                        self.stats.profile.bg_wait_hits.saturating_add(1);
                    self.stats.promo.stalled_ok = self.stats.promo.stalled_ok.saturating_add(1);
                    return Some(c);
                }
                Some(BgWaitState::Never) => return None, // worker failed → iced
                None => {}
            }
            let elapsed = start.elapsed();
            if elapsed >= budget {
                self.stats.bg.waits = self.stats.bg.waits.saturating_add(1);
                self.stats.bg.wait_us = self
                    .stats
                    .bg
                    .wait_us
                    .saturating_add(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX));
                self.stats.profile.bg_wait_timeouts =
                    self.stats.profile.bg_wait_timeouts.saturating_add(1);
                self.stats.promo.timed_out = self.stats.promo.timed_out.saturating_add(1);
                // Timeout while worker is alive: make progress on the guest
                // thread instead of cooling down. The queued Bg job is still
                // in flight — last-writer-wins on the cache insert.
                self.stats.bg.inline_fallbacks = self.stats.bg.inline_fallbacks.saturating_add(1);
                if let Some(compiled) = self.try_compile(rip) {
                    self.insert_ready(rip, compiled);
                    return Some(compiled);
                }
                self.mark_never(rip);
                return None;
            }
            // One-shot token wait: block in recv_timeout for the remaining
            // budget. A token racing our cache re-check buffers in the
            // channel (no lost wakeup, no 1 ms polling floor). A co-waiter
            // whose receiver was already taken gets `false` and briefly parks
            // before re-checking the cache.
            if !cell.wait_timeout(budget.saturating_sub(elapsed)) {
                std::thread::sleep(BG_COWAIT_QUANTUM);
            }
        }
    }

    /// Re-arm a timed-out background compile as cooldown hysteresis.
    ///
    /// Inserts `Hot { visits: 0, thr: doubled }` so the block keeps running
    /// interpreted and re-promotes only after the raised threshold. Only ever
    /// replaces Hot/Queued/absent entries — never Ready or Never.
    fn arm_cooldown(&mut self, rip: u64, prev_thr: u32) {
        let thr = next_cooldown_thr(prev_thr);
        let inserted = {
            let cache = self.shared.cache.pin();
            match cache.get(&rip) {
                None | Some(CacheEntry::Hot { .. }) | Some(CacheEntry::Queued(_)) => {
                    cache.insert(rip, CacheEntry::Hot { visits: 0, thr });
                    true
                }
                _ => false,
            }
        };
        if inserted {
            tracing::debug!(
                start = format_args!("{rip:#x}"),
                prev_thr,
                thr,
                "jit bg wait timeout → cooldown"
            );
            self.stats.promo.cooled_down = self.stats.promo.cooled_down.saturating_add(1);
        }
    }

    /// Backpressure deferral: skip the enqueue entirely and raise the local
    /// promotion threshold (doubled) so the block re-promotes later instead of
    /// queueing behind a backlog guests would time out on anyway.
    fn defer_promotion(&mut self, rip: u64, prev_thr: u32) {
        let thr = next_cooldown_thr(prev_thr);
        {
            let cache = self.shared.cache.pin();
            match cache.get(&rip) {
                None | Some(CacheEntry::Hot { .. }) => {
                    cache.insert(rip, CacheEntry::Hot { visits: 0, thr });
                }
                _ => {}
            }
        }
        self.stats.promo.deferred = self.stats.promo.deferred.saturating_add(1);
    }

    /// Whether the compile queue is deep enough that a fresh enqueue would
    /// likely stall past the guest's wait budget (one relaxed atomic load).
    fn bg_queue_too_deep(&self) -> bool {
        self.shared.bg_queue_depth.load(Ordering::Relaxed) > BG_QUEUE_CAP as u64 / 2
    }

    /// Bring this thread's chain table up to date with the shared cache.
    ///
    /// Two modes:
    /// - after a hard invalidation (`chain_sync_epoch == u64::MAX`, table was
    ///   cleared) rebuild from the whole Ready cache — rare;
    /// - otherwise consume only installs since this thread's watermark from
    ///   [`JitShared::recent_installs`], validating each VA is still `Ready`
    ///   (an install later invalidated is skipped, never linked). This turns
    ///   the per-epoch O(cache) walk — one measured run did 5,130 walks ×
    ///   593-entry width on the guest thread — into O(new installs).
    ///
    /// `current_epoch` is the freshly loaded [`JitShared::cache_epoch`]. It is
    /// stored into `chain_sync_epoch` HERE, after the rebuild — never pre-
    /// stored by the caller, which would clobber the `u64::MAX` full-rebuild
    /// sentinel before this method reads it.
    pub(super) fn resync_chain_table(&mut self, current_epoch: u64) {
        if !JitConfig::get().chain_enabled() {
            // Record the epoch even with chaining off so the dispatcher stops
            // re-entering here on every block.
            self.chain_sync_epoch = current_epoch;
            return;
        }
        let mut inserted = 0_u64;
        if self.chain_sync_epoch == u64::MAX {
            let cache = self.shared.cache.pin();
            for (va, entry) in cache.iter() {
                if let CacheEntry::Ready(c) = entry {
                    let fn_ptr = c.func as usize as u64;
                    chain_table_insert(self.thread.chain_slots.as_mut(), *va, fn_ptr);
                    inserted += 1;
                }
            }
            self.chain_watermark = self.shared.recent_installs_len();
        } else {
            let (delta, watermark) = self.shared.installs_since(self.chain_watermark);
            self.chain_watermark = watermark;
            if !delta.is_empty() {
                // Validate before linking: an install that raced an
                // invalidation may no longer be Ready.
                let cache = self.shared.cache.pin();
                for va in delta {
                    if let Some(CacheEntry::Ready(c)) = cache.get(&va) {
                        let fn_ptr = c.func as usize as u64;
                        chain_table_insert(self.thread.chain_slots.as_mut(), va, fn_ptr);
                        inserted += 1;
                    }
                }
            }
        }
        // G5 counters: resync frequency and width quantify the per-epoch
        // cost every guest thread pays between block executions.
        self.stats.chain.resyncs = self.stats.chain.resyncs.saturating_add(1);
        self.stats.chain.resync_entries = self.stats.chain.resync_entries.saturating_add(inserted);
        // Consume the observed epoch only now: storing it earlier would erase
        // the `u64::MAX` sentinel before the full-rebuild branch could see it.
        self.chain_sync_epoch = current_epoch;
    }

    pub(super) fn try_compile(&mut self, rip: u64) -> Option<CompiledBlock> {
        // Bake-before-decode (see `step_one`): snapshot the generation before
        // the block's bytes are read.
        let inv_gen = self.shared.invalidate_gen.load(Ordering::Acquire);
        let kind = {
            let mem_guard = self.shared.mem.read().unwrap();
            decode_pure_gpr_block(&mem_guard, self.thread.hooks.as_ref(), rip)
        };
        self.try_compile_from_kind(rip, kind, inv_gen)
    }

    /// Compile a block from an already-decoded [`BlockKind`], skipping the
    /// full iced-decode pass that would otherwise repeat previous work.
    ///
    /// Shares the lowering with the background worker ([`JitShared::compile_from_kind_shared`]);
    /// this wrapper adds the per-thread side effects: compile stats and the
    /// thread-local chain-table entry. `inv_gen` must be the generation
    /// snapshot taken before `kind`'s bytes were decoded.
    fn try_compile_from_kind(
        &mut self,
        rip: u64,
        result: BlockKind,
        inv_gen: u64,
    ) -> Option<CompiledBlock> {
        // Compile timing lives at the rare compile seam, so it is always
        // recorded (no cost-model gate needed).
        let start = Instant::now();
        let compiled = self
            .shared
            .compile_from_kind_shared(&self.fast_api, rip, result, inv_gen);
        let us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let Some(compiled) = compiled else {
            self.stats.compile.compile_skip = self.stats.compile.compile_skip.saturating_add(1);
            return None;
        };
        self.stats.compile.compiles = self.stats.compile.compiles.saturating_add(1);
        self.stats.profile.inline_compiles = self.stats.profile.inline_compiles.saturating_add(1);
        self.stats.profile.compile_us = self.stats.profile.compile_us.saturating_add(us);
        self.stats
            .profile
            .compile_by_insns
            .record(u64::from(compiled.insn_count), us);
        if JitConfig::get().chain_enabled() {
            let fn_ptr = compiled.func as usize as u64;
            chain_table_insert(self.thread.chain_slots.as_mut(), rip, fn_ptr);
            self.stats.chain.inline_inserts = self.stats.chain.inline_inserts.saturating_add(1);
        }
        tracing::debug!(
            start = format_args!("{rip:#x}"),
            insns = compiled.insn_count,
            "jit compiled block"
        );
        Some(compiled)
    }

    pub(super) fn finish_compiled(
        &mut self,
        entry_rip: u64,
        meta: CompiledRunMeta,
    ) -> (StepResult, usize) {
        if let Some(inv) = self.run_compiled(entry_rip, meta) {
            (StepResult::InvalidMemory(inv), 0)
        } else {
            self.stats.exec.jit_insns = self
                .stats
                .exec
                .jit_insns
                .saturating_add(u64::from(meta.insn_count));
            (
                StepResult::Continue,
                usize::try_from(meta.insn_count).unwrap_or(1),
            )
        }
    }

    /// Run a compiled block against per-thread JIT state.
    ///
    /// Contract:
    /// - Entry: `meta` snapshots the Ready block, so no shared-cache borrow is
    ///   held while the native frame runs.
    /// - Pins refresh first when `GuestMemory` generation changed; TLB/pins
    ///   then resolve to stable mmap pointers for the whole call.
    /// - The `GuestMemory` read guard drops before native execution — the block
    ///   runs on the per-thread TLB/pins and `JitCtx` pointers only.
    /// - Chain-table slots and the shadow return stack are per-thread owned
    ///   (`chain_slots` / `shadow_ret`), live for the call, and persist back
    ///   into `self.thread` from `JitCtx` on return; pending code writes are
    ///   drained (SMC invalidation) before returning.
    ///
    /// Returns `Some(InvalidMem)` when a host mem helper faulted.
    fn run_compiled(&mut self, entry_rip: u64, meta: CompiledRunMeta) -> Option<exec::InvalidMem> {
        // Refresh pins only when GuestMemory generation changes (map/protect/free).
        // Rebuilding VAD-ranked pins every block was measurable on 7za.
        let mem_gen = self.shared.mem_gen.load(Ordering::Acquire);
        if self.thread.pins_gen != mem_gen {
            let mem = self.shared.mem.read().unwrap();
            let infos = mem.jit_region_pins();
            self.thread.pins = std::array::from_fn(|i| MemPin::from_info(infos[i]));
            self.thread.pins_gen = mem_gen;
        }
        // Gen-bump + pin-shape diagnostics (cheap; always on for stats).
        if self.last_mem_gen != 0 && mem_gen > self.last_mem_gen {
            self.stats.mem.gen_bumps = self
                .stats
                .mem
                .gen_bumps
                .saturating_add(mem_gen.saturating_sub(self.last_mem_gen));
        }
        self.last_mem_gen = mem_gen;
        if mem_gen > self.stats.mem.gen_peak {
            self.stats.mem.gen_peak = mem_gen;
        }
        {
            let stack = self.thread.pins[0];
            self.stats.mem.pin_stack_bytes = if stack.is_empty() {
                0
            } else {
                stack.span_bytes()
            };
            let mut data_bytes = 0_u64;
            let mut bits = 0_u64;
            if !stack.is_empty() {
                bits |= stack.allow & 0b11;
            }
            for (i, pin) in self.thread.pins.iter().enumerate().skip(1) {
                if pin.is_empty() {
                    continue;
                }
                data_bytes = data_bytes.saturating_add(pin.span_bytes());
                // Pack first two data pins' allow into bits 2..5 for compact dump.
                if i <= 2 {
                    let shift = (i.saturating_sub(1).saturating_add(1)) * 2;
                    bits |= (pin.allow & 0b11) << shift;
                }
            }
            self.stats.mem.pin_heap_bytes = data_bytes;
            self.stats.mem.pin_allow_bits = bits;
        }
        let mem_guard = self.shared.mem.read().unwrap();
        let mem_ptr = (&raw const *mem_guard).cast_mut();
        let regs = &mut self.thread.regs;
        // Full GPR snapshot on entry: late-bound chaining reloads live regs from
        // JitCtx, so every architectural GPR must be valid for successors.
        let mut gpr = [0_u64; 16];
        for (i, slot) in gpr.iter_mut().enumerate() {
            *slot = regs.gpr(i);
        }
        // Full xmm snapshot: a chained successor may load any live xmm from
        // JitCtx even when THIS block is pure-GPR (its segment can still chain
        // into an SSE block). The old `uses_sse`-gated live-mask load zeroed
        // ctx.xmm for GPR blocks, so a chained SSE successor read zeros — e.g.
        // notepad's CRLF expansion `movd [rcx], xmm0` wrote 0x0000 pairs and
        // the loaded file truncated at the first such NUL.
        let mut xmm = [XmmSlot::ZERO; 16];
        for i in 0..16 {
            let v = regs.xmm_at(i);
            if let Some(slot) = xmm.get_mut(i) {
                *slot = XmmSlot::from_u128(v);
            }
        }
        let mut ctx = JitCtx {
            gpr,
            rflags: u64::from(regs.rflags),
            rip: entry_rip,
            gs_base: regs.gs_base(),
            mem: mem_ptr,
            fault: 0,
            fault_addr: 0,
            fault_size: 0,
            fault_access: 0,
            tlb: self.thread.tlb,
            xmm,
            shadow_sp: self.thread.shadow_sp,
            shadow_ret: self.thread.shadow_ret,
            chain_slots: self.thread.chain_slots.as_mut_ptr(),
            tlb_hot_page: self.thread.tlb_hot_page,
            tlb_hot_ptr: self.thread.tlb_hot_ptr,
            // 0 = Cranelift path (host falls back to full writeback);
            // trampolines OR their dirty bits; chain sets 0xffff.
            gpr_dirty_bits: 0,
            load_calls: 0,
            store_calls: 0,
            tlb_hot_prot: self.thread.tlb_hot_prot,
            mem_gen,
            tlb_hot_gen: self.thread.tlb_hot_gen,
            pins: self.thread.pins,
            edge_ic_va: self.thread.edge_ic_va,
            edge_ic_fn: self.thread.edge_ic_fn,
            edge_ic_rr: self.thread.edge_ic_rr,
            xmm_dirty_bits: 0,
            mem_path: MemPathSlice::default(),
            sticky_page: self.thread.sticky_page,
            sticky_ptr: self.thread.sticky_ptr,
            sticky_prot: self.thread.sticky_prot,
            sticky_gen: self.thread.sticky_gen,
            sticky_rr: self.thread.sticky_rr,
            // Fresh dispatcher entry always starts a new host-chain budget.
            chain_depth: 0,
            // Emitted guards + chain_tail compare the live shared generation
            // against the block's compile-time bake; any bump observed here
            // sends control back to this dispatcher (purge + fresh decode).
            inv_gen_ptr: std::ptr::from_ref(&self.shared.invalidate_gen),
            inv_gen_baked: meta.inv_gen,
        };
        drop(mem_guard); // GuestMemory read lock already released; compiled block runs on TLB/pins.
        // SAFETY: func is a finalized Cranelift block; TLB/pins resolve to stable mmap pointers.
        unsafe {
            (meta.func)(std::ptr::from_mut(&mut ctx));
        }
        self.stats.mem.load_calls = self.stats.mem.load_calls.saturating_add(ctx.load_calls);
        self.stats.mem.store_calls = self.stats.mem.store_calls.saturating_add(ctx.store_calls);
        {
            let m = &ctx.mem_path;
            let s = &mut self.stats;
            s.mem.sticky_hit = s.mem.sticky_hit.saturating_add(m.sticky_hit);
            s.mem.multi_hit = s.mem.multi_hit.saturating_add(m.multi_hit);
            s.mem.pin_hit = s.mem.pin_hit.saturating_add(m.pin_hit);
            s.mem.walk_hit = s.mem.walk_hit.saturating_add(m.walk_hit);
            s.mem.cross_page = s.mem.cross_page.saturating_add(m.cross_page);
            s.mem.slow = s.mem.slow.saturating_add(m.slow);
            s.mem.sticky_miss_key = s.mem.sticky_miss_key.saturating_add(m.sticky_miss_key);
            s.mem.sticky_miss_gen = s.mem.sticky_miss_gen.saturating_add(m.sticky_miss_gen);
            s.mem.sticky_miss_prot = s.mem.sticky_miss_prot.saturating_add(m.sticky_miss_prot);
            s.mem.sticky_swaps = s.mem.sticky_swaps.saturating_add(m.sticky_swaps);
            s.mem.addr_stack_pin = s.mem.addr_stack_pin.saturating_add(m.addr_in_stack_pin);
            s.mem.addr_heap_pin = s.mem.addr_heap_pin.saturating_add(m.addr_in_heap_pin);
            s.mem.addr_outside = s.mem.addr_outside.saturating_add(m.addr_outside_pins);
        }
        // Guest stores via `GuestMemory::write` leave a pending range;
        // apply selective code invalidation only after the native frame returns.
        // Persist per-thread execution state from JitCtx.
        self.thread.tlb = ctx.tlb;
        self.thread.tlb_hot_page = ctx.tlb_hot_page;
        self.thread.tlb_hot_ptr = ctx.tlb_hot_ptr;
        self.thread.tlb_hot_prot = ctx.tlb_hot_prot;
        self.thread.tlb_hot_gen = ctx.tlb_hot_gen;
        self.thread.sticky_page = ctx.sticky_page;
        self.thread.sticky_ptr = ctx.sticky_ptr;
        self.thread.sticky_prot = ctx.sticky_prot;
        self.thread.sticky_gen = ctx.sticky_gen;
        self.thread.sticky_rr = ctx.sticky_rr;
        self.thread.edge_ic_va = ctx.edge_ic_va;
        self.thread.edge_ic_fn = ctx.edge_ic_fn;
        self.thread.edge_ic_rr = ctx.edge_ic_rr;
        self.thread.shadow_sp = ctx.shadow_sp;
        self.thread.shadow_ret = ctx.shadow_ret;
        // Prefer cumulative trampoline dirty bits (partial writeback when a
        // micro-stub does not chain). Cranelift leaves bits at 0 → full sync
        // (internal block chaining can dirty arbitrary GPRs).
        let dirty = if ctx.fault != 0 || ctx.gpr_dirty_bits == 0 {
            ALL_DIRTY_BITS
        } else {
            ctx.gpr_dirty_bits as u16
        };
        if dirty == ALL_DIRTY_BITS {
            for i in 0..16 {
                if let Some(&v) = ctx.gpr.get(i) {
                    regs.set_gpr(i, v);
                }
            }
        } else {
            let mut m = dirty;
            let mut i = 0_usize;
            while m != 0 {
                if m & 1 != 0
                    && let Some(&v) = ctx.gpr.get(i)
                {
                    regs.set_gpr(i, v);
                }
                m >>= 1;
                i = i.saturating_add(1);
            }
        }
        // XMM writeback is full, not mask-gated on the entry block's metadata.
        // `ctx.xmm` is write-through — every xmm def in the run (including in
        // chained successor blocks) lands in JitCtx immediately, so it is the
        // authoritative guest xmm snapshot for the whole run. The previous
        // masked writeback used the ENTRY block's `xmm_may_def_mask` / fallback
        // `xmm_live_mask`; a run that entered a pure-GPR block, chained through
        // an SSE block (def'ing xmm), then returned through a block whose mask
        // excluded xmm dropped the def — the engine kept a stale value and the
        // next dispatcher entry re-snapshotted it from the engine. Observed
        // on Doom Retro: `movups xmm0,[mem]` in one block and
        // `movdqu [obj],xmm0` in a later dispatcher block — the store wrote 0.
        // GPRs already write back fully on the Cranelift path
        // (`gpr_dirty_bits == 0` → ALL_DIRTY); make XMM consistent.
        for i in 0..16 {
            if let Some(slot) = ctx.xmm.get(i) {
                regs.set_xmm_at(i, slot.to_u128());
            }
        }
        regs.set_rflags_checked(Rflags::from(ctx.rflags));
        regs.rip = ctx.rip;
        let fault = if ctx.fault != 0 {
            Some(exec::InvalidMem {
                // `fault_access` holds the Unicorn-style code the fault path
                // reported (0 = read, 1 = write, 16 = fetch).
                access_type: match i32::try_from(ctx.fault_access).unwrap_or(0) {
                    1 => exec::AccessType::Write,
                    16 => exec::AccessType::Fetch,
                    _ => exec::AccessType::Read,
                },
                address: ctx.fault_addr,
                size: i32::try_from(ctx.fault_size).unwrap_or(0),
                value: 0,
            })
        } else {
            None
        };
        // Safe point: drop Ready blocks overlapping any stores from this block.
        self.drain_pending_code_writes();
        fault
    }

    pub(super) fn invalidate_tlb(&mut self) {
        self.thread.tlb = GenTlb::new();
        self.thread.tlb_hot_page = TLB_EMPTY;
        self.thread.tlb_hot_ptr = std::ptr::null_mut();
        self.thread.tlb_hot_prot = 0;
        self.thread.tlb_hot_gen = 0;
        self.thread.sticky_page = [TLB_EMPTY; STICKY_WAYS];
        self.thread.sticky_ptr = [std::ptr::null_mut(); STICKY_WAYS];
        self.thread.sticky_prot = [0; STICKY_WAYS];
        self.thread.sticky_gen = [0; STICKY_WAYS];
        self.thread.sticky_rr = 0;
        self.thread.pins = [MemPin::EMPTY; PIN_SLOTS];
        self.thread.pins_gen = u64::MAX;
    }

    pub(super) fn invalidate_chain_and_shadow(&mut self) {
        chain_table_clear(self.thread.chain_slots.as_mut());
        self.thread.edge_ic_va = [0; lower::EDGE_IC_SLOTS];
        self.thread.edge_ic_fn = [0; lower::EDGE_IC_SLOTS];
        self.thread.edge_ic_rr = 0;
        self.thread.shadow_sp = 0;
        self.thread.shadow_ret = [0; lower::SHADOW_DEPTH];
        // The table is empty now; force a full re-sync from the cache on the
        // next dispatch (picks up worker-installed Ready blocks too).
        self.chain_sync_epoch = u64::MAX;
    }
}

/// Snapshot of a Ready block needed to run it without holding a cache borrow.
#[derive(Clone, Copy)]
pub(super) struct CompiledRunMeta {
    func: unsafe extern "C" fn(*mut JitCtx),
    insn_count: u32,
    /// Invalidate-generation baked into the block's guards (also compared by
    /// Rust-side trampolines via `JitCtx::inv_gen_baked`).
    inv_gen: u64,
}

impl From<&CompiledBlock> for CompiledRunMeta {
    fn from(c: &CompiledBlock) -> Self {
        Self {
            func: c.func,
            insn_count: c.insn_count,
            inv_gen: c.inv_gen,
        }
    }
}

/// Half-open range overlap: `[a0, a1)` vs `[b0, b1)`.
#[inline]
pub(super) fn ranges_overlap(a0: u64, a1: u64, b0: u64, b1: u64) -> bool {
    a0 < b1 && b0 < a1
}

/// Select the visit threshold and eagerness for a decoded block.
///
/// Fast-UCRT blocks always compile eagerly. Self-loops use the dedicated loop
/// hotness. Otherwise promotion is **work-weighted**: a block of N
/// instructions promotes after `clamp(target_work / N, FLOOR, CEILING)`
/// visits — accumulated interpreted work exceeding predicted compile cost ×
/// margin — so a 9-insn block keeps ≈100 visits (the historical flat
/// behavior) while a 90-insn block needs only ~10. A `fixed_hotness` of `0`
/// (forced under `cfg(test)`) means "compile everything eagerly on first
/// sight" and bypasses the size model entirely. The eager-by-size rule
/// (`pure_insns >= eager_block_insns`, default off) additionally forces a
/// first-sight compile.
///
/// Returns `(threshold, eager)`. `eager` is true when the block must compile
/// on its first visit. `eager_block_insns == 0` disables the large-body rule.
#[must_use]
pub(super) fn select_hot_threshold(
    is_ucrt: bool,
    is_loop: bool,
    pure_insns: usize,
    loop_hotness: u32,
    fixed_hotness: u32,
    target_work: u64,
    eager_block_insns: usize,
) -> (u32, bool) {
    if is_ucrt {
        (2, true)
    } else if is_loop {
        (loop_hotness, loop_hotness == 0)
    } else if fixed_hotness == 0 {
        // Unit-suite / forced-eager regime: byte-for-byte deterministic.
        (0, true)
    } else {
        let thr = work_weighted_threshold(pure_insns, target_work);
        let eager = eager_block_insns > 0 && pure_insns >= eager_block_insns;
        (thr, eager)
    }
}

/// Size-aware visit threshold: `clamp(TARGET_WORK / max(insns, 1), FLOOR,
/// CEILING)` ([`WORK_THRESHOLD_FLOOR`] / [`WORK_THRESHOLD_CEILING`]).
#[must_use]
pub(super) fn work_weighted_threshold(insns: usize, target_work: u64) -> u32 {
    let n = insns.max(1) as u64;
    let raw = target_work / n;
    // Clamp in u64 space, then narrow: the ceiling keeps the cast total.
    raw.clamp(
        u64::from(WORK_THRESHOLD_FLOOR),
        u64::from(WORK_THRESHOLD_CEILING),
    ) as u32
}

/// Cooldown hysteresis: each wait timeout doubles the block's threshold,
/// capped at [`COOLDOWN_THRESHOLD_CAP`] so a pathological block cannot defer
/// its promotion forever. A zero base (unit-suite regime) stays zero, which
/// makes the very next visit re-promote immediately.
#[must_use]
pub(super) fn next_cooldown_thr(prev_thr: u32) -> u32 {
    prev_thr.saturating_mul(2).min(COOLDOWN_THRESHOLD_CAP)
}

/// Follow PE import thunks / short jumps to the final callee VA.
/// Predicate over an already-decoded [`BlockKind`]: does its terminator call
/// a registered UCRT fast-path API? Shared between `peek_fast_ucrt_call` and
/// the miss-path pre-decode so the block is decoded only once.
fn block_kind_ends_in_fast_ucrt(
    shared: &JitShared,
    fast_api: &[(u64, FastApiKind)],
    kind: &BlockKind,
) -> bool {
    if fast_api.is_empty() {
        return false;
    }
    let target = match kind {
        BlockKind::Pure {
            term: Some(block::BlockTerm::Call { target, .. }),
            ..
        } => *target,
        _ => return false,
    };
    let mem = shared.mem.read().unwrap();
    let final_va = resolve_thunk_va(&mem, target);
    fast_api.iter().any(|&(k, _)| k == final_va)
}

pub(super) fn resolve_thunk_va(mem: &GuestMemory, mut va: u64) -> u64 {
    let mut buf = [0_u8; 16];
    for _ in 0..4 {
        if mem.read(va, &mut buf).is_err() {
            return va;
        }
        if buf[0] == 0xff && buf[1] == 0x25 {
            let rel = i32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
            let iat = va
                .wrapping_add(6)
                .wrapping_add(i64::from(rel).cast_unsigned());
            let mut slot = [0_u8; 8];
            if mem.read(iat, &mut slot).is_ok() {
                va = u64::from_le_bytes(slot);
                continue;
            }
            return va;
        }
        if buf[0] == 0xe9 {
            let rel = i32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
            va = va
                .wrapping_add(5)
                .wrapping_add(i64::from(rel).cast_unsigned());
            continue;
        }
        if buf[0] == 0x48 && buf[1] == 0xb8 && buf[10] == 0xff && buf[11] == 0xe0 {
            va = u64::from_le_bytes([
                buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
            ]);
            continue;
        }
        if buf[0] == 0xeb {
            let rel = buf[1].cast_signed();
            va = va
                .wrapping_add(2)
                .wrapping_add(i64::from(rel).cast_unsigned());
            continue;
        }
        return va;
    }
    va
}

#[cfg(test)]
mod tests {
    use super::super::shared::BgWaitCell;
    use super::{
        JitCpu, OPCODE_HISTO, OPCODE_SAMPLES, next_cooldown_thr, record_opcode_sample,
        select_hot_threshold, work_weighted_threshold,
    };
    use crate::CpuEngine;
    use crate::mem::protect;
    use crate::mem::{MEM_COMMIT, MEM_RESERVE};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    const TARGET: u64 = 900; // mirrored default; kept explicit so tests are self-contained
    const CUTOFF: usize = 48;

    // --- Work-weighted promotion math ---

    /// A 9-insn block keeps ≈100 visits — the historical flat threshold.
    #[test]
    fn nine_insn_block_keeps_historical_threshold() {
        assert_eq!(work_weighted_threshold(9, TARGET), 100);
        let (thr, eager) = select_hot_threshold(false, false, 9, 8, 100, TARGET, 0);
        assert_eq!(thr, 100);
        assert!(!eager, "mid-size block stays visit-gated");
    }

    /// Tiny blocks clamp at the ceiling: however cheap a compile would be,
    /// the threshold never exceeds 10_000 visits (reached only via a large
    /// `target_work` override); `max(N,1)` guards division by zero.
    #[test]
    fn tiny_block_clamps_at_ceiling() {
        // Default target keeps a 1-insn block at its raw quotient.
        assert_eq!(work_weighted_threshold(1, TARGET), 900);
        assert_eq!(work_weighted_threshold(0, TARGET), 900); // max(N,1)
        // Ceiling engages when the quotient would exceed it.
        assert_eq!(work_weighted_threshold(1, 100_000), 10_000);
    }

    /// Huge blocks clamp at the floor: even a 96-insn (or larger) body
    /// promotes after at most 8 revisits.
    #[test]
    fn huge_block_clamps_at_floor() {
        assert_eq!(work_weighted_threshold(96, TARGET), 9);
        assert_eq!(work_weighted_threshold(usize::MAX, TARGET), 8);
        let (thr, eager) = select_hot_threshold(false, false, 96, 8, 100, TARGET, 0);
        assert_eq!(thr, 9);
        assert!(!eager);
    }

    /// Knob override: raising `target_work` raises every threshold
    /// proportionally (900 → 1800 doubles the 9-insn threshold).
    #[test]
    fn target_work_override_scales_thresholds() {
        assert_eq!(work_weighted_threshold(9, 1_800), 200);
        assert_eq!(work_weighted_threshold(90, 18_000), 200);
        // An override below the floor still clamps up to it.
        assert_eq!(work_weighted_threshold(4, 4), 8);
    }

    /// Fast-UCRT blocks always compile eagerly regardless of body length,
    /// cutoff, or regime.
    #[test]
    fn ucrt_always_eager() {
        assert!(select_hot_threshold(true, false, 1, 8, 100, TARGET, 0).1);
        assert!(select_hot_threshold(true, false, 1, 8, 0, TARGET, 48).1);
    }

    /// Self-loops use the dedicated loop hotness, not the size model; with a
    /// nonzero loop hotness they are not eager.
    #[test]
    fn self_loop_uses_loop_hotness_not_body_rule() {
        let (thr, eager) = select_hot_threshold(false, true, 10_000, 8, 100, TARGET, CUTOFF);
        assert_eq!(thr, 8);
        assert!(
            !eager,
            "self-loop eagerness comes from loop hotness, not body size"
        );
    }

    /// A zero regime switch (unit-suite value) compiles everything eagerly,
    /// independent of body size — this is the byte-for-byte deterministic
    /// path the unit suite relies on.
    #[test]
    fn zero_fixed_hotness_is_eager_regardless_of_size() {
        assert_eq!(
            select_hot_threshold(false, false, 4, 8, 0, TARGET, 0),
            (0, true)
        );
        assert_eq!(
            select_hot_threshold(false, false, 10_000, 8, 0, TARGET, 48),
            (0, true)
        );
    }

    /// The eager-by-size rule is off by default (`eager_block_insns == 0`)
    /// but still works when re-enabled via env: a large Pure body compiles
    /// eagerly with its work-weighted threshold as cooldown base.
    #[test]
    fn eager_by_size_rule_disabled_then_reenableable() {
        // Default: disabled — large bodies are visit-gated.
        let (thr, eager) = select_hot_threshold(false, false, 96, 8, 100, TARGET, 0);
        assert!(!eager);
        assert_eq!(thr, 9);
        // Re-enabled: same body goes eager on first sight.
        let (thr, eager) = select_hot_threshold(false, false, 96, 8, 100, TARGET, 48);
        assert!(eager, "large pure body must compile eagerly when enabled");
        assert_eq!(thr, work_weighted_threshold(96, TARGET));
    }

    // --- Cooldown hysteresis math ---

    /// Each timeout doubles the threshold; the cap holds at CEILING×4.
    #[test]
    fn cooldown_doubling_and_cap() {
        assert_eq!(next_cooldown_thr(100), 200);
        assert_eq!(next_cooldown_thr(200), 400);
        assert_eq!(next_cooldown_thr(40_000), 40_000, "cap respected");
        assert_eq!(
            next_cooldown_thr(30_000),
            40_000,
            "doubling saturates into cap"
        );
        // Zero base (unit-suite regime) stays zero → immediate re-promotion.
        assert_eq!(next_cooldown_thr(0), 0);
    }

    // --- One-shot completion token ---

    /// A token fired BEFORE the wait must resolve immediately: no 1 ms
    /// polling floor. The second `wait_timeout` finds the receiver taken and
    /// returns false right away (co-waiter contract: re-check the cache).
    #[test]
    fn bg_wait_cell_buffered_token_resolves_without_floor() {
        let cell = BgWaitCell::new(8);
        cell.notify_all();
        let start = Instant::now();
        assert!(cell.wait_timeout(Duration::from_secs(5)));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "buffered token must return promptly (generous CI bound)"
        );
        assert!(!cell.wait_timeout(Duration::from_millis(50)));
    }

    /// A token arriving mid-wait wakes the waiter well before a generous
    /// budget expires.
    #[test]
    fn bg_wait_cell_token_wakes_before_budget_expiry() {
        let cell = BgWaitCell::new(4);
        let c = Arc::clone(&cell);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            c.notify_all();
        });
        let start = Instant::now();
        assert!(
            cell.wait_timeout(Duration::from_secs(5)),
            "token must arrive"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(15),
            "woke before token was sent"
        );
        assert!(
            elapsed < Duration::from_millis(2_000),
            "token must wake the waiter promptly, not on chunk boundaries"
        );
    }

    /// An unfired token consumes exactly its budget (bounded stall).
    #[test]
    fn bg_wait_cell_silent_token_consumes_budget_then_times_out() {
        let cell = BgWaitCell::new(2);
        let start = Instant::now();
        assert!(!cell.wait_timeout(Duration::from_millis(30)));
        assert!(start.elapsed() >= Duration::from_millis(25));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    // --- Sampled opcode histogram ---

    /// Sampling buckets by mnemonic discriminant; names round-trip through
    /// `Mnemonic::try_from` for the dump.
    #[test]
    fn opcode_histogram_buckets_known_mnemonics() {
        let mut cpu = JitCpu::open_x86_64();
        let base = 0x1036_0000_u64;
        cpu.virtual_alloc(
            base,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc");
        // `mov eax, 0x2a; ud2`.
        cpu.mem_write(base, &[0xb8, 0x2a, 0x00, 0x00, 0x00, 0x0f, 0x0b])
            .expect("code");
        let mov_bucket = iced_x86::Mnemonic::Mov as usize;
        let before = OPCODE_HISTO[mov_bucket].load(Ordering::Relaxed);
        let samples_before = OPCODE_SAMPLES.load(Ordering::Relaxed);
        {
            let mem = cpu.shared.mem.read().unwrap();
            record_opcode_sample(&mem, base);
        }
        assert_eq!(
            OPCODE_HISTO[mov_bucket].load(Ordering::Relaxed),
            before + 1,
            "`mov` sample must land in its mnemonic bucket"
        );
        assert_eq!(OPCODE_SAMPLES.load(Ordering::Relaxed), samples_before + 1);
        // The dump resolves bucket indices back to names.
        assert_eq!(
            iced_x86::Mnemonic::try_from(mov_bucket).unwrap(),
            iced_x86::Mnemonic::Mov
        );
        // Invalid bytes are silently not sampled.
        let bad = OPCODE_SAMPLES.load(Ordering::Relaxed);
        {
            let mem = cpu.shared.mem.read().unwrap();
            record_opcode_sample(&mem, base.wrapping_add(5)); // ud2 — valid but distinct
        }
        assert!(OPCODE_SAMPLES.load(Ordering::Relaxed) > bad);
    }
}
