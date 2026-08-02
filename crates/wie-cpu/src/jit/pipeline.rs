//! Per-thread JIT execution pipeline: dispatch, compile, run, invalidate.
//!
//! Extracted verbatim from `jit/mod.rs`. The hot path
//! (`step_one`, `try_compile`, `finish_compiled`, `run_compiled`,
//! `invalidate_code_range`) moves byte-for-byte. Methods called from
//! `cpu_engine.rs` or `mod.rs` tests are `pub(super)`.

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

use super::JitStats;
use super::block::{self, BlockKind, decode_pure_gpr_block, pure_is_self_loop};
use super::config::JitConfig;
use super::fast_api::{FastApiKind, JitFastPathConfig, install_heap_layout};
use super::lower::{
    self, CompiledBlock, JitCtx, MemPathSlice, MemPin, PIN_SLOTS, STICKY_WAYS, TLB_EMPTY, TLB_SETS,
    XmmSlot, chain_table_clear, chain_table_insert, empty_tlb_aux, empty_tlb_bucket,
};
use super::shared::{BgEnqueueOutcome, BgWaitCell, BgWaitState, JitShared, PerThreadJitState};
use super::{CacheEntry, JitCpu};
use crate::CpuError;
use crate::exec::{self, StepResult};
use crate::mem::{self, GuestMemory, PAGE_SIZE, PAGE_SIZE_USIZE};
use crate::regs::Rflags;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

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
        }
    }

    /// Snapshot of JIT diagnostics counters (baselines).
    ///
    /// Merges the shared background-compile counter into the per-thread
    /// snapshot so `WIE_RUNTIME_PROFILE` sees background work.
    #[must_use]
    pub fn stats(&self) -> JitStats {
        let mut s = self.stats;
        s.bg_compiles = s
            .bg_compiles
            .saturating_add(self.shared.bg_compiles.load(Ordering::Relaxed));
        s
    }

    /// Install UCRT/heap fast-path config (called once after fake-API table build).
    pub fn configure_fast_path(&mut self, cfg: JitFastPathConfig) {
        install_heap_layout(cfg.heap);
        let pairs = cfg.pairs.clone();
        self.fast_api = cfg.pairs;
        // Mirror the pairs so the background worker lowers UCRT calls exactly
        // like the inline path (same `call_fast` → same emitted code).
        *self.shared.bg_fast_api.lock().unwrap() = pairs;
        self.clear_compiled();
        self.invalidate_chain_and_shadow();
    }

    // `fast_api_kind` was the on-demand helper used by the old peek loop;
    // `block_kind_ends_in_fast_ucrt` inlines the same lookup on the miss path.
    // Kept accessible for future callers (e.g. non-Pure fast-API detection).
    #[allow(dead_code)]
    #[inline]
    fn fast_api_kind(&self, va: u64) -> Option<FastApiKind> {
        self.fast_api
            .iter()
            .find_map(|&(k, kind)| (k == va).then_some(kind))
    }

    pub(super) fn insert_ready(&mut self, rip: u64, compiled: CompiledBlock) {
        self.shared.insert_ready(rip, compiled);
    }

    pub(super) fn clear_compiled(&mut self) {
        self.shared.cache.write().unwrap().clear();
        self.shared.chain_ids.write().unwrap().clear();
        self.shared.code_pages.lock().unwrap().clear();
    }

    pub(super) fn invalidate_code_range(&mut self, addr: u64, len: usize) {
        {
            let cache = self.shared.cache.read().unwrap();
            if cache.is_empty() || len == 0 {
                return;
            }
        }
        if !self.code_pages_overlap(addr, len) {
            return;
        }
        let write_end = addr.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        let to_drop: Vec<u64> = {
            let cache = self.shared.cache.read().unwrap();
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
            let mut cache = self.shared.cache.write().unwrap();
            if let Some(CacheEntry::Ready(c)) = cache.remove(va) {
                drop(cache);
                self.shared
                    .code_pages_remove_range(c.guest_start, c.guest_end);
            }
            self.shared.chain_ids.write().unwrap().remove(va);
        }
        self.stats.code_invs = self.stats.code_invs.saturating_add(1);
        self.invalidate_chain_and_shadow();
        if JitConfig::get().chain_enabled() {
            let cache = self.shared.cache.read().unwrap();
            for (va, entry) in &*cache {
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
            if !self.shared.cache.read().unwrap().is_empty() {
                self.clear_compiled();
                self.invalidate_chain_and_shadow();
                self.stats.code_invs = self.stats.code_invs.saturating_add(1);
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
            self.shared.cache.read().unwrap().get(&rip),
            Some(CacheEntry::Ready(_))
        )
    }

    /// Returns `(result, guest_insns_retired)` for budget accounting.
    pub(super) fn step_one(&mut self) -> Result<(StepResult, usize), CpuError> {
        let rip = self.thread.regs.rip;
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
            // Read-first pattern: acquire read lock, clone entry, drop lock, then act.
            let entry = {
                let cache = self.shared.cache.read().unwrap();
                cache.get(&rip).cloned()
            };
            if let Some(entry) = entry {
                match entry {
                    CacheEntry::Ready(compiled) => {
                        self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
                        let meta = CompiledRunMeta::from(&compiled);
                        return Ok(self.finish_compiled(rip, meta));
                    }
                    CacheEntry::Never => { /* fall through to iced */ }
                    CacheEntry::Queued(notify) => {
                        // About to execute the entry the worker is compiling:
                        // block only for this entry, and only briefly. On timeout
                        // (queue backlog / dead worker) fall back to inline.
                        if let Some(compiled) = self.wait_bg_ready(rip, &notify) {
                            let meta = CompiledRunMeta::from(&compiled);
                            return Ok(self.finish_compiled(rip, meta));
                        }
                        if let Some(compiled) = self.try_compile(rip) {
                            let meta = CompiledRunMeta::from(&compiled);
                            self.insert_ready(rip, compiled);
                            return Ok(self.finish_compiled(rip, meta));
                        }
                        self.shared
                            .cache
                            .write()
                            .unwrap()
                            .insert(rip, CacheEntry::Never);
                    }
                    CacheEntry::Hot { visits, thr } => {
                        let next = visits.saturating_add(1);
                        if thr > 0 && next < thr {
                            self.shared
                                .cache
                                .write()
                                .unwrap()
                                .insert(rip, CacheEntry::Hot { visits: next, thr });
                        } else {
                            // Threshold crossed. Prefer the background worker:
                            // enqueue and keep executing on iced this visit; the
                            // compiled block lands in the cache for the next one.
                            // Inline compilation is only the fallback.
                            let kind = {
                                let mem = self.shared.mem.read().unwrap();
                                block::decode_pure_gpr_block(&mem, self.thread.hooks.as_ref(), rip)
                            };
                            match self.enqueue_bg(rip, &kind) {
                                BgEnqueueOutcome::Queued(_) | BgEnqueueOutcome::Ready => {
                                    // Continue on iced this visit.
                                }
                                BgEnqueueOutcome::Unavailable => {
                                    if let Some(compiled) = self.try_compile_from_kind(rip, kind) {
                                        let meta = CompiledRunMeta::from(&compiled);
                                        self.insert_ready(rip, compiled);
                                        return Ok(self.finish_compiled(rip, meta));
                                    }
                                    self.shared
                                        .cache
                                        .write()
                                        .unwrap()
                                        .insert(rip, CacheEntry::Never);
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
                let thr = if is_ucrt {
                    2
                } else if is_loop {
                    JitConfig::get().pure_loop_hotness()
                } else {
                    JitConfig::get().hotness_threshold()
                };
                if thr == 0 || is_ucrt {
                    // Eager compile: the entry is required NOW (there may be no
                    // revisit). Prefer the background worker and block briefly
                    // on this entry only; inline compile is the fallback.
                    match self.enqueue_bg(rip, &kind) {
                        BgEnqueueOutcome::Queued(notify) => {
                            if let Some(compiled) = self.wait_bg_ready(rip, &notify) {
                                let meta = CompiledRunMeta::from(&compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            // Deadline missed — compile inline (replaces Queued).
                            if let Some(compiled) = self.try_compile_from_kind(rip, kind) {
                                let meta = CompiledRunMeta::from(&compiled);
                                self.insert_ready(rip, compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            self.shared
                                .cache
                                .write()
                                .unwrap()
                                .insert(rip, CacheEntry::Never);
                        }
                        BgEnqueueOutcome::Ready => {
                            // Worker beat us: the cache already holds Ready.
                            let compiled = {
                                let cache = self.shared.cache.read().unwrap();
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
                            if let Some(compiled) = self.try_compile_from_kind(rip, kind) {
                                let meta = CompiledRunMeta::from(&compiled);
                                self.insert_ready(rip, compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            self.shared
                                .cache
                                .write()
                                .unwrap()
                                .insert(rip, CacheEntry::Never);
                        }
                    }
                } else {
                    self.shared
                        .cache
                        .write()
                        .unwrap()
                        .insert(rip, CacheEntry::Hot { visits: 1, thr });
                }
            }
        }

        // Iced does not maintain the shadow return stack — drop prediction.
        self.thread.shadow_sp = 0;
        self.stats.iced_insns = self.stats.iced_insns.saturating_add(1);
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

    /// Hand a block to the background compiler.
    ///
    /// Queues the exact decoded block so the worker compiles the same bytes the
    /// guest classified. The cache entry transitions to `Queued` only after the
    /// queue slot is reserved (a full queue must never strand a Queued entry).
    pub(super) fn enqueue_bg(&mut self, rip: u64, kind: &BlockKind) -> BgEnqueueOutcome {
        if !self.shared.bg_enabled_here() || !self.shared.engine_ready.load(Ordering::Relaxed) {
            return BgEnqueueOutcome::Unavailable;
        }
        self.shared.ensure_bg_worker();
        if !self.shared.bg_alive.load(Ordering::Relaxed) {
            return BgEnqueueOutcome::Unavailable;
        }
        if matches!(kind, BlockKind::NotPure) {
            self.shared
                .cache
                .write()
                .unwrap()
                .insert(rip, CacheEntry::Never);
            return BgEnqueueOutcome::Unavailable;
        }
        let tx_guard = self.shared.bg_tx.lock().unwrap();
        let Some(tx) = tx_guard.as_ref() else {
            return BgEnqueueOutcome::Unavailable;
        };
        if tx.try_send((rip, kind.clone())).is_err() {
            return BgEnqueueOutcome::Unavailable;
        }
        // Transition the entry (only from Hot/absent; never clobber Ready/Never).
        let mut cache = self.shared.cache.write().unwrap();
        match cache.get(&rip) {
            None | Some(CacheEntry::Hot { .. }) => {
                let cell = BgWaitCell::new();
                cache.insert(rip, CacheEntry::Queued(Arc::clone(&cell)));
                BgEnqueueOutcome::Queued(cell)
            }
            Some(CacheEntry::Queued(_) | CacheEntry::Never) => BgEnqueueOutcome::Unavailable,
            Some(CacheEntry::Ready(_)) => BgEnqueueOutcome::Ready,
        }
    }

    /// Wait (bounded) for the background worker to resolve `rip`.
    ///
    /// The guest only reaches this when it is about to execute the entry. The
    /// wait is per-entry (the cell from the Queued entry, not the whole queue)
    /// and time-boxed by [`JitConfig::bg_wait_timeout`]; on timeout the caller falls back
    /// to inline compilation so a worker stall can never deadlock the guest.
    /// Returns the Ready block once installed.
    pub(super) fn wait_bg_ready(&mut self, rip: u64, cell: &BgWaitCell) -> Option<CompiledBlock> {
        if !self.shared.bg_alive.load(Ordering::Relaxed) {
            return None; // worker gone: inline fallback
        }
        let budget = JitConfig::get().bg_wait_timeout();
        let start = Instant::now();
        loop {
            let state = {
                let cache = self.shared.cache.read().unwrap();
                match cache.get(&rip) {
                    Some(CacheEntry::Ready(c)) => Some(BgWaitState::Ready(*c)),
                    Some(CacheEntry::Never) => Some(BgWaitState::Never),
                    Some(CacheEntry::Queued(_)) => None,
                    // Re-decided or invalidated while we waited: inline fallback.
                    Some(CacheEntry::Hot { .. }) | None => return None,
                }
            };
            match state {
                Some(BgWaitState::Ready(c)) => {
                    self.stats.compile_stalls = self.stats.compile_stalls.saturating_add(1);
                    self.stats.compile_stall_us = self.stats.compile_stall_us.saturating_add(
                        u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX),
                    );
                    return Some(c);
                }
                Some(BgWaitState::Never) => return None, // worker failed → iced
                None => {}
            }
            let elapsed = start.elapsed();
            if elapsed >= budget {
                self.stats.compile_stalls = self.stats.compile_stalls.saturating_add(1);
                self.stats.compile_stall_us = self
                    .stats
                    .compile_stall_us
                    .saturating_add(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX));
                self.stats.compile_stall_fallback =
                    self.stats.compile_stall_fallback.saturating_add(1);
                return None;
            }
            // Chunked wait: a notification that races with our re-check (or a
            // spurious wakeup) costs at most one 1 ms chunk, never the whole
            // budget. The loop re-checks the cache after each wake.
            cell.wait_timeout(budget.saturating_sub(elapsed).min(Duration::from_millis(1)));
        }
    }

    /// Re-insert every Ready block into this thread's late-bound chain table.
    ///
    /// Runs only when the shared `cache_epoch` advanced (background installs),
    /// so worker-compiled blocks chain from compiled code exactly like inline
    /// compiles would have.
    pub(super) fn resync_chain_table(&mut self) {
        if !JitConfig::get().chain_enabled() {
            return;
        }
        let cache = self.shared.cache.read().unwrap();
        for (va, entry) in &*cache {
            if let CacheEntry::Ready(c) = entry {
                let fn_ptr = c.func as usize as u64;
                chain_table_insert(self.thread.chain_slots.as_mut(), *va, fn_ptr);
            }
        }
    }

    pub(super) fn try_compile(&mut self, rip: u64) -> Option<CompiledBlock> {
        let kind = {
            let mem_guard = self.shared.mem.read().unwrap();
            decode_pure_gpr_block(&mem_guard, self.thread.hooks.as_ref(), rip)
        };
        self.try_compile_from_kind(rip, kind)
    }

    /// Compile a block from an already-decoded [`BlockKind`], skipping the
    /// full iced-decode pass that would otherwise repeat previous work.
    ///
    /// Shares the lowering with the background worker ([`JitShared::compile_from_kind_shared`]);
    /// this wrapper adds the per-thread side effects: compile stats and the
    /// thread-local chain-table entry.
    fn try_compile_from_kind(&mut self, rip: u64, result: BlockKind) -> Option<CompiledBlock> {
        let compiled = self
            .shared
            .compile_from_kind_shared(&self.fast_api, rip, result);
        let Some(compiled) = compiled else {
            self.stats.compile_skip = self.stats.compile_skip.saturating_add(1);
            return None;
        };
        self.stats.compiles = self.stats.compiles.saturating_add(1);
        if JitConfig::get().chain_enabled() {
            let fn_ptr = compiled.func as usize as u64;
            chain_table_insert(self.thread.chain_slots.as_mut(), rip, fn_ptr);
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
            self.stats.jit_insns = self
                .stats
                .jit_insns
                .saturating_add(u64::from(meta.insn_count));
            (
                StepResult::Continue,
                usize::try_from(meta.insn_count).unwrap_or(1),
            )
        }
    }

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
            self.stats.mem_gen_bumps = self
                .stats
                .mem_gen_bumps
                .saturating_add(mem_gen.saturating_sub(self.last_mem_gen));
        }
        self.last_mem_gen = mem_gen;
        if mem_gen > self.stats.mem_gen_peak {
            self.stats.mem_gen_peak = mem_gen;
        }
        {
            let stack = self.thread.pins[0];
            self.stats.pin_stack_bytes = if stack.is_empty() {
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
            self.stats.pin_heap_bytes = data_bytes;
            self.stats.pin_allow_bits = bits;
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
        // Pure GPR blocks skip the XMM bank copy on both sides of the call.
        // SSE blocks load only live XMMs (Track A live mask).
        let mut xmm = [XmmSlot::ZERO; 16];
        if meta.uses_sse {
            let mut m = meta.xmm_live_mask;
            // If mask is empty but uses_sse (fp-only edge), load all.
            if m == 0 {
                m = 0xffff;
            }
            let mut i = 0_usize;
            while m != 0 {
                if m & 1 != 0 {
                    let v = regs.xmm_at(i);
                    if let Some(slot) = xmm.get_mut(i) {
                        *slot = XmmSlot::from_u128(v);
                    }
                }
                m >>= 1;
                i = i.saturating_add(1);
            }
        }
        let mut ctx = JitCtx {
            gpr,
            rflags: u64::from(regs.rflags),
            rip: entry_rip,
            mem: mem_ptr,
            fault: 0,
            fault_addr: 0,
            fault_size: 0,
            fault_access: 0,
            tlb_sets: self.thread.tlb_sets,
            tlb_aux: self.thread.tlb_aux,
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
        };
        drop(mem_guard); // GuestMemory read lock already released; compiled block runs on TLB/pins.
        // SAFETY: func is a finalized Cranelift block; TLB/pins resolve to stable mmap pointers.
        unsafe {
            (meta.func)(std::ptr::from_mut(&mut ctx));
        }
        self.stats.load_calls = self.stats.load_calls.saturating_add(ctx.load_calls);
        self.stats.store_calls = self.stats.store_calls.saturating_add(ctx.store_calls);
        {
            let m = &ctx.mem_path;
            let s = &mut self.stats;
            s.mem_sticky_hit = s.mem_sticky_hit.saturating_add(m.sticky_hit);
            s.mem_multi_hit = s.mem_multi_hit.saturating_add(m.multi_hit);
            s.mem_pin_hit = s.mem_pin_hit.saturating_add(m.pin_hit);
            s.mem_walk_hit = s.mem_walk_hit.saturating_add(m.walk_hit);
            s.mem_cross_page = s.mem_cross_page.saturating_add(m.cross_page);
            s.mem_slow = s.mem_slow.saturating_add(m.slow);
            s.mem_sticky_miss_key = s.mem_sticky_miss_key.saturating_add(m.sticky_miss_key);
            s.mem_sticky_miss_gen = s.mem_sticky_miss_gen.saturating_add(m.sticky_miss_gen);
            s.mem_sticky_miss_prot = s.mem_sticky_miss_prot.saturating_add(m.sticky_miss_prot);
            s.mem_sticky_swaps = s.mem_sticky_swaps.saturating_add(m.sticky_swaps);
            s.mem_addr_stack_pin = s.mem_addr_stack_pin.saturating_add(m.addr_in_stack_pin);
            s.mem_addr_heap_pin = s.mem_addr_heap_pin.saturating_add(m.addr_in_heap_pin);
            s.mem_addr_outside = s.mem_addr_outside.saturating_add(m.addr_outside_pins);
        }
        // Guest stores via `GuestMemory::write` leave a pending range;
        // apply selective code invalidation only after the native frame returns.
        // Persist per-thread execution state from JitCtx.
        self.thread.tlb_sets = ctx.tlb_sets;
        self.thread.tlb_aux = ctx.tlb_aux;
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
            0xffff_u16
        } else {
            ctx.gpr_dirty_bits as u16
        };
        if dirty == 0xffff {
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
        if meta.uses_sse {
            // Cranelift blocks skip the per-def `xmm_dirty_bits` RMW — the static
            // `xmm_may_def_mask` covers them. Trampolines still set dirty from Rust,
            // so we always OR both so trampoline-only writes and Cranelift writes
            // are both covered.
            let dirty = if ctx.fault != 0 {
                u16::try_from(ctx.xmm_dirty_bits).unwrap_or(0xffff)
            } else {
                u16::try_from(ctx.xmm_dirty_bits).unwrap_or(0)
            };
            let mut mask = dirty | meta.xmm_may_def_mask;
            if mask == 0 {
                mask = meta.xmm_live_mask;
            }
            let mut i = 0_usize;
            let mut m = mask;
            while m != 0 {
                if m & 1 != 0
                    && let Some(slot) = ctx.xmm.get(i)
                {
                    regs.set_xmm_at(i, slot.to_u128());
                }
                m >>= 1;
                i = i.saturating_add(1);
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
        self.thread.tlb_sets = [empty_tlb_bucket(); TLB_SETS];
        self.thread.tlb_aux = [empty_tlb_aux(); TLB_SETS];
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
    uses_sse: bool,
    /// XMMi referenced in the block (selective entry load).
    xmm_live_mask: u16,
    /// XMMi that may be defined (conservative exit writeback on fault).
    xmm_may_def_mask: u16,
}

impl From<&CompiledBlock> for CompiledRunMeta {
    fn from(c: &CompiledBlock) -> Self {
        Self {
            func: c.func,
            insn_count: c.insn_count,
            uses_sse: c.uses_sse,
            xmm_live_mask: c.xmm_live_mask,
            xmm_may_def_mask: c.xmm_may_def_mask,
        }
    }
}

/// Half-open range overlap: `[a0, a1)` vs `[b0, b1)`.
#[inline]
pub(super) fn ranges_overlap(a0: u64, a1: u64, b0: u64, b1: u64) -> bool {
    a0 < b1 && b0 < a1
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
