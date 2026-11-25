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
use super::baseline;
use super::block::{self, BlockKind, decode_pure_gpr_block, pure_is_self_loop};
use super::config::{
    BG_QUEUE_CAP, COOLDOWN_THRESHOLD_CAP, JitConfig, WORK_THRESHOLD_CEILING, WORK_THRESHOLD_FLOOR,
};
use super::fast_api::{FastApiKind, JitFastPathConfig};
use super::gen_tlb::GenTlb;
use super::lower::{
    self, CompiledBlock, JitCtx, MemPathSlice, MemPin, PIN_SLOTS, STICKY_WAYS, TLB_EMPTY, XmmSlot,
    chain_table_clear, chain_table_insert, refill_clear,
};
use super::shared::{BgEnqueueOutcome, BgWaitCell, BgWaitState, JitShared, PerThreadJitState};
use super::{CacheEntry, JitCpu};
use crate::CpuError;
use crate::exec::{self, StepResult};
use crate::lock::{lock_unpoisoned, read_unpoisoned};
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

/// Failed region-formation attempts allowed per invalidation epoch on one
/// thread (`WIE_JIT_REGION=1`).
///
/// Formation is not free: it decodes up to `RegionLimits::max_explore_blocks`
/// (512) blocks to prove the loop has no other entry, and measured 42 µs
/// (a debug build, on DOOM Retro's hot loop). Almost every backward branch in a
/// real guest is a loop *we cannot fold* — a call in the body, a string op, an
/// irreducible shape — so the attempts that find nothing are the ones that need
/// a bound, while the attempts that find a region are the ones worth paying for
/// and stay uncapped. 128 misses is ~5 ms of formation on this host per epoch,
/// once, and a candidate past the cap keeps the per-block path it would have
/// had anyway: a missed fold, never a wrong one.
const REGION_PROBE_MISSES_PER_EPOCH: u32 = 128;

/// Per-thread memory of which loop headers have already been offered to region
/// formation (`WIE_JIT_REGION=1`).
///
/// Nothing on the block path reads this; it exists so the *extra* work the knob
/// turns on stays proportional to the number of loops rather than to the number
/// of block compiles.
#[derive(Default)]
pub(crate) struct RegionProbe {
    /// [`JitShared::invalidate_gen`] the fields below were built under.
    ///
    /// A mismatch means guest code bytes, or the hook window that formation
    /// decodes against, may have changed since a verdict was reached, so both
    /// memories are dropped rather than trusted. Invalidation is already the
    /// trigger for dropping compiled code, so this costs one comparison on a
    /// path that is about to do a compile.
    epoch: u64,
    /// Loop headers formation has already been asked about at `epoch`.
    ///
    /// `pub(crate)` only so `jit::tests` can assert that a self-loop header was
    /// actually *proposed* to formation — the one observable difference between
    /// the two paths when both produce a single block.
    pub(crate) tried: std::collections::HashSet<u64>,
    /// Formation attempts that produced no region, this epoch.
    pub(crate) misses: u32,
}

/// Queue depth at or above which waiting for this entry's compile is futile:
/// the worker is several block-latencies behind, so the wait budget would
/// expire unserved. At sustained backlog nearly every full-budget stall timed
/// out (measured 65 %), so past this depth we skip straight to cooldown
/// hysteresis and keep interpreting instead of blocking the guest thread.
const BG_WAIT_SKIP_DEPTH: u64 = 4;

/// Visits a block may spend interpreted while its background compile is in
/// flight before the dispatch path insists on the bounded wait.
///
/// This is a **liveness backstop, not the policy**. The policy is "never block
/// the guest thread on a compile that is already queued" — a queued compile
/// finishes at the same wall-clock moment whether the guest sleeps or works, so
/// sleeping is pure loss. What is not guaranteed is that a queued job is always
/// *processed*: a worker that panics mid-compile leaves its job unprocessed
/// while its peers keep `bg_alive` true, and that entry would stay `Queued`
/// forever. Past the cap the wait path is reached, its timeout inline-compiles
/// the block, and the entry resolves.
///
/// Counted in interpreted visits rather than elapsed time because the failure
/// mode this must *not* trigger on is a deep queue: at session init the runtime
/// queues one speculative compile per planted guest stub (124 on `crt_hello`),
/// so a job can be tens of ms old on a perfectly healthy pool. 1 024 visits is
/// far past any healthy resolution (a ≤96-insn block compiles in ~½ ms, during
/// which a guest reaches a given entry a handful of times) and still bounds a
/// lost job to about a millisecond of extra interpretation. Not a knob: a
/// per-run tuning value for a pathology nobody has observed is a knob nobody
/// would ever learn to turn.
pub(super) const NO_STALL_SKIP_CAP: u32 = 1_024;

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
        let shared = Arc::new(JitShared::new());
        Self {
            thread: PerThreadJitState::new(&shared),
            shared,
            fast_api: Vec::new(),
            stats: JitStats::default(),
            last_mem_gen: 0,
            chain_sync_epoch: 0,
            chain_watermark: 0,
            seen_invalidate_gen: 0,
            region_probe: RegionProbe::default(),
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
            thread: PerThreadJitState::new(&shared),
            shared,
            fast_api: Vec::new(),
            stats: JitStats::default(),
            last_mem_gen: 0,
            chain_sync_epoch: 0,
            chain_watermark: 0,
            seen_invalidate_gen: 0,
            region_probe: RegionProbe::default(),
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
        // Tier-up ledger lives on the one `JitShared` (one decision per VA), so
        // it is read — never summed — into every engine's snapshot.
        let tier = self.shared.tier_counters();
        s.profile.tier_compiles = tier.tier_ups;
        s.profile.tier_rejects = tier.tier_rejects;
        s.profile.tier_budget_left = u64::try_from(tier.budget_left).unwrap_or(u64::MAX);
        // Code-cache counters live on the one `JitShared` that owns the on-disk
        // cache — a restore happens there, not on a guest thread's dispatch path
        // — so they are read (never summed) into every engine's snapshot, exactly
        // like the tier ledger above. One snapshot of relaxed atomic loads per
        // call, and `stats()` is only taken on the profile path.
        let cc = self.shared.code_cache_counters();
        s.profile.code_cache_restored = cc.restored;
        s.profile.code_cache_refused = cc.refused();
        s.profile.code_cache_unpersistable = cc.skipped_unpersistable;
        s
    }

    /// Install UCRT/heap fast-path config (called once after fake-API table build).
    ///
    /// The heap layout is published on the SHARED state, not a process global:
    /// two `RuntimeSession`s in one process (or any reconfigure after execution
    /// starts) would otherwise leave every engine's `malloc`/`free` reading the
    /// last-installed layout, so session A would allocate from session B's heap.
    /// `run_compiled` copies these into the `JitCtx` it owns, which is what the
    /// UCRT helpers actually read.
    pub fn configure_fast_path(&mut self, cfg: JitFastPathConfig) {
        self.shared.heap[0].store(cfg.heap.ctrl_va, Ordering::Release);
        self.shared.heap[1].store(cfg.heap.base, Ordering::Release);
        self.shared.heap[2].store(cfg.heap.end, Ordering::Release);
        // New session → discard this ENGINE's stale large freelist entries
        // (VAs are heap-region relative). Scoped to `self.thread`, so it cannot
        // reach another engine's live blocks; the list did not exist at all
        // before, and a fresh engine starts empty.
        self.thread.large_free.clear();
        let pairs = cfg.pairs.clone();
        self.fast_api = cfg.pairs;
        // Mirror the pairs so the background worker lowers UCRT calls exactly
        // like the inline path (same `call_fast` → same emitted code). The Arc
        // hands the worker a shared immutable table (no per-job Vec clone).
        *lock_unpoisoned(&self.shared.bg_fast_api) = Arc::from(pairs);
        // Init-time clear: keep the persistent ledger — this runs before any
        // guest execution, so loaded ledger facts stay valid (probes are
        // hash-validated anyway).
        self.clear_compiled_keep_ledger();
        self.invalidate_chain_and_shadow();
    }

    pub(super) fn insert_ready(&mut self, rip: u64, compiled: CompiledBlock) {
        self.shared.insert_ready(rip, compiled);
    }

    /// Mark `rip` as `Never` (cold / non-pure) and count it for diagnostics.
    /// Also records a negative ledger entry (`WIE_JIT_CACHE`) so warm boots
    /// skip re-decoding blocks that structurally cannot compile.
    fn mark_never(&mut self, rip: u64) {
        self.shared.cache.pin().insert(rip, CacheEntry::Never);
        self.shared.persist_record_never(rip);
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
    ///
    /// Guest-triggered full flushes also purge the persistent disk ledger
    /// (full rewrite at next flush): guest-written code means cached facts
    /// about those bytes are stale by definition.
    pub(super) fn clear_compiled(&mut self) {
        self.clear_compiled_inner(true);
    }

    /// Same as [`Self::clear_compiled`] but preserves the persistent ledger:
    /// used for init-time clears that run BEFORE any guest execution
    /// (`configure_fast_path`, `install_runtime_hooks`). Every ledger
    /// consumption is hash-validated against current guest bytes, so loaded
    /// facts cannot go stale unnoticed.
    pub(super) fn clear_compiled_keep_ledger(&mut self) {
        self.clear_compiled_inner(false);
    }

    fn clear_compiled_inner(&mut self, purge_ledger: bool) {
        self.shared.cache.pin().clear();
        self.shared.chain_ids.pin().clear();
        lock_unpoisoned(&self.shared.code_pages).clear();
        if purge_ledger {
            self.shared.persist_clear(true);
        }
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
        // Persistent-ledger tombstones for the invalidated byte range (drops
        // in-memory entries + marks the file for rewrite).
        self.shared.persist_invalidate_range(addr, write_end);
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
        // Fast path: nothing has been queued since this engine's last drain, so
        // there is neither a page to invalidate nor an overflow to honour. One
        // relaxed load replaces a mutex acquisition on every dispatch entry.
        let pending_gen = self.shared.pending_code_gen.load(Ordering::Relaxed);
        if pending_gen == self.thread.drained_pending_gen {
            return;
        }
        self.thread.drained_pending_gen = pending_gen;
        let overflow = self
            .shared
            .pending_code_overflow
            .swap(false, Ordering::Relaxed);
        let pages = std::mem::take(&mut *lock_unpoisoned(&self.shared.pending_code_writes));
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
        // Published before the set is touched: a drainer that observes this
        // generation must also observe the push that follows it, and one that
        // observes the push always observes a generation it has not drained.
        self.shared.pending_code_gen.fetch_add(1, Ordering::Relaxed);
        let len_u = u64::try_from(len).unwrap_or(u64::MAX);
        let end = address.saturating_add(len_u);
        if end <= address && len > 0 {
            self.shared
                .pending_code_overflow
                .store(true, Ordering::Relaxed);
            lock_unpoisoned(&self.shared.pending_code_writes).clear();
            return;
        }
        let mut guard = lock_unpoisoned(&self.shared.pending_code_writes);
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
            return read_unpoisoned(&self.shared.mem).allocation_span_at_base(addr);
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
        // Purge on *observation*, here, before anything below can run compiled
        // code. `run_until_stop` does the same on its own loop, but that covers
        // only the path through it: the emitted invalidation guard and
        // `chain_tail` reason that every `Ready` block is either chain-compatible
        // or purged, and that argument holds only if a generation change is
        // noticed before *every* dispatch. Reusing the value already loaded
        // above makes that unconditional and costs one compare.
        if inv_gen != self.seen_invalidate_gen {
            self.seen_invalidate_gen = inv_gen;
            self.invalidate_chain_and_shadow();
        }
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
                        // **No-stall rule** (see [`NO_STALL_SKIP_CAP`]): the
                        // compile is already queued and its completion time
                        // does not depend on whether this thread blocks, so
                        // blocking converts guest progress into nothing.
                        // Interpret this visit and re-check the cache on the
                        // next dispatch instead.
                        //
                        // Safe by construction rather than by argument:
                        // `enqueue_bg` inserts `Queued` only **after**
                        // `BgPool::push` accepted the job, and every resolution
                        // of a queued job replaces or removes the entry
                        // (`bg_install_ready` → `Ready`, `bg_install_never` →
                        // `Never`, `bg_install_drop` → *absent*, so a dropped
                        // job re-enters the ordinary miss path). The `iced`
                        // fall-through this takes is the same one the old code
                        // already took when a wait resolved to nothing, so no
                        // new dispatch shape is being introduced.
                        //
                        // Dead worker first: a `Queued` entry whose pool is
                        // gone can only be resolved by compiling it here, and
                        // nothing is gained by interpreting up to the bound
                        // first.
                        if !self.shared.bg_alive.load(Ordering::Relaxed) {
                            self.stats.bg.inline_fallbacks =
                                self.stats.bg.inline_fallbacks.saturating_add(1);
                            if let Some(compiled) = self.try_compile(rip) {
                                let meta = CompiledRunMeta::from(&compiled);
                                self.insert_ready(rip, compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            self.mark_never(rip);
                        } else if notify.note_skip() {
                            // Skip budget spent on a live pool: the job was
                            // lost, not slow. Take the bounded wait, whose
                            // timeout inline-compiles and resolves the entry.
                            // This is the *only* path that still stalls.
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
                        }
                        // Fall through to iced this visit: the block is either
                        // in flight on a live pool (interpret, re-check next
                        // dispatch), or resolved to Never/absent above.
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
                                // Boot mode: bounded inline instead of doubling.
                                if self.shared.is_boot_mode()
                                    && self.shared.try_acquire_boot_inline_token()
                                {
                                    self.stats.bg.inline_fallbacks =
                                        self.stats.bg.inline_fallbacks.saturating_add(1);
                                    let kind = {
                                        let mem = read_unpoisoned(&self.shared.mem);
                                        block::decode_pure_gpr_block(
                                            &mem,
                                            self.thread.hooks.as_ref(),
                                            rip,
                                        )
                                    };
                                    if let Some(compiled) =
                                        self.try_compile_from_kind(rip, kind, inv_gen)
                                    {
                                        let meta = CompiledRunMeta::from(&compiled);
                                        self.insert_ready(rip, compiled);
                                        return Ok(self.finish_compiled(rip, meta));
                                    }
                                    self.mark_never(rip);
                                } else {
                                    self.defer_promotion(rip, thr);
                                }
                            } else {
                                let kind = {
                                    let mem = read_unpoisoned(&self.shared.mem);
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
                    let mem = read_unpoisoned(&self.shared.mem);
                    block::decode_pure_gpr_block(&mem, self.thread.hooks.as_ref(), rip)
                };
                // Warm-boot fast path (`WIE_JIT_CACHE`): a byte-validated
                // known-good ledger hit skips the Hot visit-threshold warmup
                // entirely — the block goes straight to compilation. (`Never`
                // negatives never reach here; attach-time seeding covers them.)
                let mut persist_eager = false;
                if self.shared.persist_active() {
                    let probe = {
                        let mem = read_unpoisoned(&self.shared.mem);
                        self.shared.restore_or_probe(&mem, rip)
                    };
                    if probe.is_some() {
                        persist_eager = true;
                        self.stats.profile.warm_ledger_hits =
                            self.stats.profile.warm_ledger_hits.saturating_add(1);
                    }
                }
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
                    cfg!(test),
                    JitConfig::get().target_work(),
                );
                // Boot mode deliberately does NOT lower the threshold. It used
                // to force `thr = 1` for the first `BOOT_MODE_INSNS` (5 M)
                // guest instructions, i.e. compile every block on first sight,
                // on the theory that a warm engine pays for its compiles later.
                // Measured against simply leaving the threshold alone, on the
                // calibrated corpus with interleaved pairs (median of per-pair
                // ratios, 5/5 pairs, load 2.1-2.2):
                //
                //   `7za h` 0.811 (1 197 -> 969 ms), compiles 336 -> 183
                //   `7za x` 0.897 (3 583 -> 3 210 ms), compiles 668 -> 472
                //
                // Retired guest instructions were unchanged (5.79 M / 120.9 M
                // both arms) and `iced` rose by only 0.02-0.03 % of them, so
                // the saving is almost entirely compile work that was never
                // repaid. 45 % / 29 % of the compiles were buying nothing.
                //
                // What remains of boot mode is the part that measured neutral
                // or better: the bounded inline-compile token bucket below
                // (`try_acquire_boot_inline_token`) and the tier-0 attempt, both
                // of which exist to keep a compile from blocking the guest.
                // Tier-0 baseline: during boot, attempt a fast inline baseline
                // compile (0.2 ms) and enqueue tier-1 Cranelift bg for same RIP.
                // On next execution the Ready entry will be patched to tier-1.
                // Ledger-known-good blocks skip it: they get one direct
                // compile, not tier-0 + tier-1 double work.
                // **No synchronous first-sight compile** (this is the second
                // half of the `crt_hello` startup cost, see the no-stall rule in
                // this function's `Queued` arm). The branch below used to call
                // `baseline::try_compile_baseline` on the *first* sighting of
                // every `Pure` block, which is the eager-by-size rule the
                // RUNBOOK records as deleted: a one-shot 96-instruction block
                // costs ~9 µs interpreted against ~1 ms compiled, so paying to
                // compile one that never revisits is a ~100x loss. Measured on
                // `micro-exes/out/crt_hello.exe`, which retires ~450 guest
                // instructions: 105 synchronous Cranelift compiles at ~0.85 ms
                // each dominated an ~80 ms emulation phase once the stalls were
                // gone. The arithmetic in the RUNBOOK row is the whole argument
                // — break-even for a *single* visit needs ~900 block
                // instructions and `decode_pure_gpr_block` caps out at 96 — so
                // the only block shape a first-sight compile can pay for is one
                // where a single entry runs the block many times: a self-loop.
                //
                // Those keep the synchronous compile (they are also exactly the
                // blocks `earns_tier` promotes to `Speed`, so the
                // `long_loop` tier parity this file pins is unchanged).
                // Everything else falls through to the ordinary work-weighted
                // promotion: interpret to the threshold, compile on the worker,
                // keep interpreting while it lands.
                //
                // The region hook stays here and that is deliberate: a `Ready`
                // entry is never re-compiled, so the first sighting of a latch
                // is the only chance to fold its loop header (`WIE_JIT_REGION`).
                if self.shared.is_boot_mode()
                    && !persist_eager
                    && baseline::is_baseline_eligible(&kind)
                {
                    // Enqueue a background tier-1 job only for a block that can
                    // actually tier up. `earns_tier` (`jit/tier.rs`) returns
                    // `Speed` only for `is_self_loop`, and the "baseline" compile
                    // below — `baseline::try_compile_baseline` — is a full
                    // `compile_from_kind_shared` at exactly the tier this job
                    // would decide. For every non-self-loop block the enqueue was
                    // therefore a **duplicate compile of the RIP we install two
                    // lines later**: same input, same tier, same output, twice the
                    // Cranelift work, and while it ran the worker held the engine
                    // mutex the guest thread needs for its own inline compile.
                    //
                    // Measured on the `scripts/real-guest-perf.sh` corpus with
                    // interleaved pairs (median of per-pair ratios, calm host):
                    // `7za h` 1.57x (3.28 s -> 2.07 s, 5/5 pairs) and `7za x`
                    // 1.62x (10.04 s -> 6.19 s, 5/5), with `bg_compiles`
                    // 2139 -> 336 / 5951 -> 668 and compile CPU roughly halved.
                    // The guest's instruction count, `iced` residue and `eager`
                    // promotions are unchanged (9115 vs 9132; 1920 vs 1920),
                    // which is the proof that nothing but the duplicate compile
                    // was removed. `long_loop` is flat (median ratio 1.000, 1/5),
                    // i.e. the self-loop tier-up path is intact.
                    // `WIE_JIT_REGION=1`: this block is a latch of every loop it
                    // branches back into, and this is the **only** look those
                    // headers get. A baseline compile installs `Ready` on the
                    // first sighting of a RIP and nothing re-compiles a `Ready`
                    // entry, so a header compiled here would stay a per-block,
                    // per-iteration hop for the rest of the process. Fold the
                    // loop now, while the guest thread's hook window (which
                    // formation decodes against) is in hand.
                    //
                    // **Before** the tier-up enqueue and the block install, and that
                    // ordering is load-bearing: for a self-loop the candidate *is*
                    // `rip`, so folding it and then installing the block would
                    // overwrite the region on the spot, and the non-urgent tier-up
                    // job would overwrite it again when it landed. When the fold
                    // takes this RIP, `Some(region)` comes back and the region is
                    // what runs.
                    if let Some(region) = self.promote_loop_headers(
                        rip,
                        baseline::backward_targets(rip, &kind),
                        inv_gen,
                    ) {
                        let meta = CompiledRunMeta::from(&region);
                        super::diag::count_eager_site(super::diag::EagerSite::RegionFold);
                        self.stats.profile.eager_compiles =
                            self.stats.profile.eager_compiles.saturating_add(1);
                        return Ok(self.finish_compiled(rip, meta));
                    }
                    if is_loop && super::diag::profile_gate_on() {
                        // Diagnostic attribution, gated so it costs a relaxed
                        // `OnceLock` load when the profile is off. Recording the
                        // attempt *before* the compile is deliberate: an attempt
                        // that fails to compile also re-decides on the next visit,
                        // and only the attempt count shows that.
                        let present = {
                            let cache = self.shared.cache.pin();
                            cache.get(&rip).is_some()
                        };
                        super::diag::note_loop_baseline_attempt(rip, present);
                    }
                    if is_loop
                        && let Some(baseline) = baseline::try_compile_baseline(
                            &self.shared,
                            &self.fast_api,
                            rip,
                            kind.clone(),
                            inv_gen,
                        )
                    {
                        drop(self.enqueue_bg(rip, &kind, inv_gen, false));
                        let meta = CompiledRunMeta::from(&baseline);
                        self.insert_ready(rip, baseline);
                        super::diag::note_loop_baseline_compile();
                        self.stats.profile.eager_compiles =
                            self.stats.profile.eager_compiles.saturating_add(1);
                        return Ok(self.finish_compiled(rip, meta));
                    }
                }
                if eager || persist_eager {
                    // Eager compile: the entry is required NOW (there may be no
                    // revisit). Prefer the background worker and block briefly
                    // on this entry only. On timeout the wait attempts an inline
                    // compile on the guest thread instead of cooling down.
                    super::diag::count_eager_site(super::diag::EagerSite::EagerBranch);
                    self.stats.profile.eager_compiles =
                        self.stats.profile.eager_compiles.saturating_add(1);
                    if self.bg_queue_too_deep() {
                        // Boot mode uses bounded inline instead of deferral.
                        if self.shared.is_boot_mode() && self.shared.try_acquire_boot_inline_token()
                        {
                            self.stats.bg.inline_fallbacks =
                                self.stats.bg.inline_fallbacks.saturating_add(1);
                            if let Some(compiled) =
                                self.try_compile_from_kind(rip, kind.clone(), inv_gen)
                            {
                                let meta = CompiledRunMeta::from(&compiled);
                                self.insert_ready(rip, compiled);
                                return Ok(self.finish_compiled(rip, meta));
                            }
                            self.mark_never(rip);
                        } else {
                            self.defer_promotion(rip, thr);
                        }
                    } else {
                        self.thread.pending_promote_thr = thr;
                        // Urgent lane: this job is wanted *now*, so it is
                        // queued ahead of every normal job. What it is **not**
                        // is worth blocking on — see the no-stall rule
                        // ([`NO_STALL_SKIP_CAP`]) and the identical decision in
                        // the `Queued` arm of [`Self::step_one`].
                        //
                        // This is the site that made `crt_hello` slow, and it
                        // is the *only* one that mattered, per the 105
                        // `stalled_ok` compiles that `hit_ready=0` forced. The
                        // reason *this* branch was reached that often is no
                        // longer the fast-UCRT arm: work-gating it
                        // (`select_hot_threshold`) moved `eager` from 368 to 365
                        // on the same fixture with `iced_fb=446` unchanged, so
                        // the traffic that arrived here came from elsewhere — the
                        // boot-mode self-loop baseline branch below is the
                        // remaining candidate, count unproven. Either way the
                        // no-stall rule is what fixed it, so a 433-instruction
                        // hello-world queued and then slept on 105 compiles —
                        // `stalled_ok=105`, 45-63 ms of a 47-68 ms emulation
                        // phase, with `hit_ready=0`. Fixing the `Queued` arm
                        // alone changed nothing on that fixture (0 iced
                        // fall-throughs still), which is how this site was
                        // found.
                        match self.enqueue_bg(rip, &kind, inv_gen, true) {
                            BgEnqueueOutcome::Queued(notify) => {
                                // Dead worker: nothing to wait for and nothing
                                // will ever resolve the entry but us.
                                if !self.shared.bg_alive.load(Ordering::Relaxed) {
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
                                } else if notify.note_skip() {
                                    // Skip budget spent on a live pool: the job
                                    // is lost rather than slow. Take the bounded
                                    // wait, whose timeout inline-compiles.
                                    if let Some(compiled) = self.wait_bg_ready(rip, &notify) {
                                        let meta = CompiledRunMeta::from(&compiled);
                                        return Ok(self.finish_compiled(rip, meta));
                                    }
                                    if !self.shared.bg_alive.load(Ordering::Relaxed) {
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
                                }
                                // Live pool, budget left: interpret this visit
                                // and let the worker's install resolve the
                                // entry for the next one.
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
            let mem = read_unpoisoned(&self.shared.mem);
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
        // No lock: the guest-memory address is captured once per engine and
        // cannot move or be replaced (see `PerThreadJitState::guest_mem`), the
        // same argument `run_compiled` uses. This path still took the memory
        // read lock per *interpreted instruction* — a guard acquire plus a
        // release — which is the 3.47 ns the dispatcher entry stopped paying
        // when the frame became persistent. It is the residue that runs when a
        // block is not compiled yet, i.e. exactly a startup/spin loop.
        //
        // SAFETY: `guest_mem` points into the `Arc<JitShared>` payload that
        // this engine holds a share of, and the pointee is never moved or
        // replaced for the life of the session. The borrow does not alias
        // `thread.regs`, which is the only mutable access below.
        #[expect(unsafe_code)]
        let mem: &GuestMemory = unsafe { &*self.thread.guest_mem };
        let result = exec::step(mem, &mut self.thread.regs, hook)?;
        if matches!(result, StepResult::Continue) {
            self.thread.iced_steps = self.thread.iced_steps.saturating_add(1);
            self.shared.record_guest_insns(1);
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
        // `WIE_JIT_REGION=1`: a hot loop header compiles as **one function for the
        // whole loop**, not one per block.
        //
        // This is the hook that makes the knob mean anything on a real guest. The
        // `Hot`-threshold branch and the eager branch both decide a promotion by
        // calling here, so without this the *dominant* promotion path (compile on
        // the background worker) would never form a region and only the inline
        // fallbacks would — measured: `long_loop` and DOOM Retro formed none.
        //
        // Three reasons it belongs here rather than inside the worker:
        //
        // * **The hook window lives on the guest thread.** Region formation decodes
        //   up to `RegionLimits::max_explore_blocks` blocks to prove the loop has no
        //   other entry, and `decode_pure_gpr_block` needs `hooks` to refuse an
        //   address the guest asked to stop at. A pool thread has no hooks, so
        //   forming a region there would compile over a host-stop address — the
        //   region would then run the hooked instruction natively and the stop would
        //   never fire.
        // * **One attempt per promotion.** `urgent` is `false` only for the
        //   speculative boot-mode prefetch, which must stay a plain block compile.
        // * **The region is the thing that costs more to compile**, so it should be
        //   attempted where the block's compile was about to happen anyway.
        //
        // `Ready` is the outcome that already means "the cache holds this entry":
        // both callers' `Ready` arms look it up and run (or defer to) it, which is
        // exactly right. The one counter whose reading shifts is `promo.hit_ready`
        // ("the worker beat us to it"), which now also counts a promotion resolved
        // on the spot — it is a diagnostic, and the knob is off by default.
        if urgent
            && let Some(compiled) =
                self.try_compile_region(rip, inv_gen, pure_is_self_loop(kind, rip))
        {
            self.insert_ready(rip, compiled);
            return BgEnqueueOutcome::Ready;
        }
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
            // Boot mode: bounded inline on deep-queue (avoid thrash, no doubling).
            if self.shared.is_boot_mode() && self.shared.try_acquire_boot_inline_token() {
                self.stats.bg.inline_fallbacks = self.stats.bg.inline_fallbacks.saturating_add(1);
                if let Some(compiled) = self.try_compile(rip) {
                    self.insert_ready(rip, compiled);
                    return Some(compiled);
                }
                self.mark_never(rip);
                return None;
            }
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
        // Boot mode uses bounded inline instead of cooldown hysteresis.
        if self.shared.is_boot_mode() && self.shared.try_acquire_boot_inline_token() {
            self.stats.bg.inline_fallbacks = self.stats.bg.inline_fallbacks.saturating_add(1);
            if let Some(compiled) = self.try_compile(rip) {
                self.insert_ready(rip, compiled);
                // Caller will handle Ready via cache re-check; for wait
                // path we return after install via wait_bg_ready inline.
                return;
            }
            self.mark_never(rip);
            return;
        }
        let thr = if self.shared.is_boot_mode() {
            1
        } else {
            next_cooldown_thr(prev_thr)
        };
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
        // Boot mode: bounded inline instead of doubling.
        if self.shared.is_boot_mode() && self.shared.try_acquire_boot_inline_token() {
            self.stats.bg.inline_fallbacks = self.stats.bg.inline_fallbacks.saturating_add(1);
            if let Some(compiled) = self.try_compile(rip) {
                self.insert_ready(rip, compiled);
                return;
            }
            self.mark_never(rip);
            return;
        }
        let thr = if self.shared.is_boot_mode() {
            1
        } else {
            next_cooldown_thr(prev_thr)
        };
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
            let mem_guard = read_unpoisoned(&self.shared.mem);
            decode_pure_gpr_block(&mem_guard, self.thread.hooks.as_ref(), rip)
        };
        // A hot RIP may be a **loop header**: try the whole loop as one function
        // before compiling a single block (`WIE_JIT_REGION=1`). `None` — not a
        // loop, refused at formation, or refused at lower time — falls through to
        // the per-block path below, which is the same code this function ran
        // before regions existed.
        //
        // The block is decoded *before* the attempt (it used to be decoded after)
        // so the region is compiled at the tier this RIP would be: `decide_tier`
        // is a question about the block's own terminator, i.e. whether it is a
        // self-loop. One decode is 1–2 µs against formation's 42 µs, and the
        // region still precedes the *compile* either way.
        if let Some(compiled) = self.try_compile_region(rip, inv_gen, pure_is_self_loop(&kind, rip))
        {
            return Some(compiled);
        }
        self.try_compile_from_kind(rip, kind, inv_gen)
    }

    /// Attempt loop-region compilation for `rip` (`WIE_JIT_REGION=1`).
    ///
    /// Returns `None` — leaving every counter untouched — when the knob is off,
    /// when `rip` is not a loop header, when the region is refused at formation,
    /// or when an instruction in it is refused at lower time. That last case is
    /// the reason this is a `None` and not a `Result`: a refusal here is routine
    /// and its only effect is that the per-block path compiles the same RIP, so
    /// it must not change what the *caller* does with the RIP.
    ///
    /// # The tier a region compiles at
    ///
    /// The same one the per-block path would pick for this RIP,
    /// [`JitShared::decide_tier`]: `Speed` for a block that is a **self-loop**
    /// (when `WIE_JIT_TIER` is armed and the run's budget allows), `Base`
    /// otherwise. `self_loop` is the caller's answer for `rip`, because the
    /// caller has the decoded block.
    ///
    /// Deciding it here rather than defaulting to `Base` is what makes folding a
    /// self-loop safe rather than a gamble: `earns_tier` gives a self-loop the
    /// whole tier-up budget because `micro-exes/long_loop` repays it 1.37x, so a
    /// region that replaced such a block at `Base` would be trading a measured
    /// `speed` compile for an unmeasured `none` one. A region on a *multi-block*
    /// loop is not a self-loop, gets `Base`, and charges nothing.
    ///
    /// This costs no extra budget in the common case: `decide_tier` is memoised
    /// per guest VA, and the RIP being folded has usually been decided already by
    /// the block compile that made it hot. When it has not (a header folded before
    /// anything compiled it), the decision is the one that block would have taken
    /// — one slot, for one function that replaces it.
    ///
    /// # Locks
    ///
    /// Held at once: the guest-memory read guard and the engine mutex. See
    /// [`lower::try_compile_region`]'s doc for why that order cannot cycle (no
    /// path in this crate holds a `mem` guard across another lock acquisition).
    /// `decide_tier` is taken **before** either, so the tier ledger's lock never
    /// nests inside the engine mutex.
    fn try_compile_region(
        &mut self,
        rip: u64,
        inv_gen: u64,
        self_loop: bool,
    ) -> Option<CompiledBlock> {
        if !self.shared.region_enabled_here() {
            return None;
        }
        let tier = self.shared.decide_tier(rip, self_loop);
        let start = Instant::now();
        let compiled = {
            let mem_guard = read_unpoisoned(&self.shared.mem);
            let mut eng_guard = lock_unpoisoned(&self.shared.engine);
            let chain = |va: u64| self.shared.chain_func_for(tier, va);
            eng_guard
                .as_mut()
                .and_then(|eng| eng.module_for(tier))
                .and_then(|eng| {
                    lower::try_compile_region(
                        eng,
                        &mem_guard,
                        self.thread.hooks.as_ref(),
                        rip,
                        &chain,
                        inv_gen,
                        tier,
                        self.shared.region_self_loop_here(),
                    )
                })
        };
        let compiled = compiled?;
        // Same per-thread bookkeeping `try_compile_from_kind` performs for a
        // block: a region compile is an inline compile, and its `insn_count` is
        // the whole loop, so `compile_by_insns` records the region's real size.
        let us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
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
        Some(compiled)
    }

    /// Fold the loop that the block just compiled at `rip` is the latch of
    /// (`WIE_JIT_REGION=1`).
    ///
    /// `backward` is [`baseline::backward_targets`] of that block: its
    /// successors at or below it, each of which is by definition a loop header —
    /// including `latch` itself, which makes a block that branches to its own
    /// entry the header of a one-block loop. For every candidate this asks
    /// [`form_loop_region`] to describe the loop and compiles the answer as one
    /// native function, then installs it over whatever translation the header had.
    ///
    /// # The return value
    ///
    /// `Some` only when the fold took the **latch's own** RIP — i.e. a self-loop,
    /// where the caller is holding a translation of the same instructions the
    /// region now covers. The caller must install *that* instead of the block it
    /// just compiled: installing the block afterwards would overwrite the region on
    /// the spot, and a queued tier-up for the same RIP would overwrite it again
    /// when it landed. Folds of *other* headers change no RIP the caller holds, so
    /// they return `None` and the caller's block install is unaffected.
    ///
    /// # The gap this closes
    ///
    /// The other two region hooks (`try_compile` and `enqueue_bg`) fire on the
    /// RIP **being promoted**. That covers a hot block reached through the Hot
    /// threshold or an urgent background job — but not a loop header first
    /// touched during boot, which `baseline::try_compile_baseline` compiles and
    /// installs `Ready` on its *first* sighting. Nothing re-compiles a `Ready`
    /// entry except a self-loop's tier-up, so before this the header of every
    /// loop entered during boot stayed a per-block translation for the rest of
    /// the process — measured on DOOM Retro, whose entire hot set is in that
    /// window, which is why the knob had never had a region to measure.
    ///
    /// The latch is compiled *after* the header it closes the loop back to, so
    /// this is the first moment the pipeline can know a loop exists, and it
    /// knows it on the guest thread with the hook window formation needs.
    ///
    /// # Why overwriting a `Ready` entry is allowed here
    ///
    /// The header is usually already `Ready` (that is the whole problem), so this
    /// replaces it — the same last-writer-wins patch
    /// [`JitShared::insert_ready`] performs when a background tier-1 compile
    /// lands on a tier-0 entry. Two properties make that safe:
    ///
    /// * the superseded function stays mapped and stays *correct* (code pages are
    ///   only ever unmapped on a full flush), so another guest thread whose chain
    ///   slot still points at it runs the same guest instructions, only slower,
    ///   until its next resync;
    /// * the region's `[guest_start, guest_end)` covers the block it replaces, so
    ///   range-selective invalidation still drops the region for any write to
    ///   either block's bytes.
    ///
    /// # Cost
    ///
    /// One call, two comparisons and one atomic load when the knob is off — the
    /// [`RegionProbe`] check runs inside. When it is on, one formation per
    /// distinct loop header per invalidation epoch, with
    /// [`REGION_PROBE_MISSES_PER_EPOCH`] bounding the attempts that find nothing.
    fn promote_loop_headers(
        &mut self,
        latch: u64,
        backward: ([u64; 2], usize),
        inv_gen: u64,
    ) -> Option<CompiledBlock> {
        let (slots, len) = backward;
        if len == 0 || !self.shared.region_enabled_here() {
            return None;
        }
        if self.region_probe.epoch != inv_gen {
            self.region_probe.epoch = inv_gen;
            self.region_probe.tried.clear();
            self.region_probe.misses = 0;
        }
        for &header in &slots[..len] {
            if !self.region_probe.tried.insert(header) {
                continue;
            }
            if self.already_folded(header) {
                // Debug, not a counter: this is the answer to "why is this loop
                // still hopping?", and it is routine (most loops are folded once
                // and then have every other backward edge into them skipped).
                tracing::debug!(
                    header = format_args!("{header:#x}"),
                    latch = format_args!("{latch:#x}"),
                    "loop header already inside a compiled unit — not folding"
                );
                continue;
            }
            if self.region_probe.misses >= REGION_PROBE_MISSES_PER_EPOCH {
                // Loud the first time only: past here, loops keep the per-block
                // path for the rest of the epoch, which is a missed fold and never
                // a wrong one. The line exists because "why is this loop still
                // hopping?" has two very different answers — refused at formation,
                // or out of probe budget.
                tracing::debug!(
                    attempts = self.region_probe.tried.len(),
                    misses = self.region_probe.misses,
                    "region probe budget exhausted for this invalidation epoch"
                );
                return None;
            }
            // Whether the candidate is a **self-loop** decides its tier, and it has
            // to be *asked*, not assumed. `decide_tier` memoises one decision per
            // guest VA, so answering `false` for a header that is really a
            // self-loop would not merely compile this region at the base tier: it
            // would record `Base` for that VA and downgrade the **block path's**
            // `speed` compile for the rest of the run. Measured on `long_loop`
            // before this was fixed: the hot self-loop block went from `tier=Speed`
            // to `tier=Base` under `WIE_JIT_REGION=1`, which is the one thing a
            // region must never cost.
            //
            // `header == latch` is exact without a decode: a candidate equal to the
            // block being compiled means that block's own terminator branches to its
            // own entry. Otherwise the header is a different block, and its own
            // terminator is the answer.
            let self_loop = if header == latch {
                true
            } else {
                let mem = read_unpoisoned(&self.shared.mem);
                pure_is_self_loop(
                    &block::decode_pure_gpr_block(&mem, self.thread.hooks.as_ref(), header),
                    header,
                )
            };
            match self.try_compile_region(header, inv_gen, self_loop) {
                Some(compiled) => {
                    tracing::debug!(
                        header = format_args!("{header:#x}"),
                        latch = format_args!("{latch:#x}"),
                        insns = compiled.insn_count,
                        "jit loop region folded from its latch"
                    );
                    // Handed back only when the loop *is* this block, because then
                    // the caller is holding a translation of the same RIP that the
                    // region supersedes — installing the block afterwards would
                    // overwrite the region, and enqueueing a tier-up for it would
                    // overwrite it later.
                    let at_latch = (header == latch).then_some(compiled);
                    self.insert_ready(header, compiled);
                    if at_latch.is_some() {
                        return at_latch;
                    }
                }
                None => self.region_probe.misses = self.region_probe.misses.saturating_add(1),
            }
        }
        None
    }

    /// Whether `header` already sits inside a **folded region**, which makes a
    /// region entry there a second copy of guest code that is already one
    /// function.
    ///
    /// Two shapes reach this, and both are a fold worth **not** making: the loop
    /// was already folded at some block of it — by one of the other two region
    /// hooks, by this one on an earlier epoch, or by this one just now at a
    /// *different* block of the same loop (a loop with two backward branches into
    /// it has several candidate headers) — and folding it again compiles the same
    /// guest bytes twice. Both copies would be correct; the second one is dead
    /// code that cost a region compile to produce.
    ///
    /// # Why this is not just a span test
    ///
    /// "Inside some translation" is too weak: a block that **falls through** into a
    /// loop header already owns the header's bytes, because its span is every byte
    /// it decoded and the decoder walks forward until a terminator. A plain span
    /// test therefore refuses to fold almost every loop whose header follows
    /// straight-line code — measured on `cpp_dtor`, where it took the folds from 2
    /// to 1 and skipped a real loop.
    ///
    /// A per-block translation's span is *exactly* the byte length of the block at
    /// its own entry ([`compile_block`](super::lower::compile_block) sets
    /// `guest_end = start_rip + bytes_len`), so one decode of the covering unit's
    /// entry separates the two: a span wider than that block cannot be one block.
    /// The entry **at** `header` is not consulted (`va != header`) — that one is the
    /// boot block this hook exists to replace, and a re-attempt at a header already
    /// folded into a region is stopped by `tried` for the epoch, which is as long as
    /// an unchanged-code verdict is worth keeping.
    /// If the bytes no longer decode (the unit was compiled from instructions the
    /// decoder now refuses), the wider answer is taken, which costs a fold rather
    /// than risking a duplicate.
    fn already_folded(&self, header: u64) -> bool {
        let spans: Vec<(u64, u64)> = {
            let cache = self.shared.cache.pin();
            cache
                .iter()
                .filter_map(|(&va, entry)| match entry {
                    CacheEntry::Ready(c)
                        if va != header && c.guest_start <= header && header < c.guest_end =>
                    {
                        Some((c.guest_start, c.guest_end))
                    }
                    _ => None,
                })
                .collect()
        };
        if spans.is_empty() {
            return false;
        }
        let mem = read_unpoisoned(&self.shared.mem);
        spans.into_iter().any(|(start, end)| {
            let one_block =
                match block::decode_pure_gpr_block(&mem, self.thread.hooks.as_ref(), start) {
                    BlockKind::Pure { bytes_len, .. } => Some(u64::from(bytes_len)),
                    BlockKind::NotPure => None,
                };
            one_block.is_none_or(|bytes| end.saturating_sub(start) > bytes)
        })
    }

    /// Compile a block from an already-decoded [`BlockKind`], skipping the
    /// full iced-decode pass that would otherwise repeat previous work.
    ///
    /// Shares the lowering with the background worker ([`JitShared::compile_from_kind_shared`]);
    /// this wrapper adds the per-thread side effects: compile stats and the
    /// thread-local chain-table entry. `inv_gen` must be the generation
    /// snapshot taken before `kind`'s bytes were decoded.
    ///
    /// `pub(super)` so `jit::tests` can compile a block the way boot mode does
    /// and pin that the *latch* of a loop folds its header
    /// ([`Self::promote_loop_headers`]) without needing a live boot run.
    pub(super) fn try_compile_from_kind(
        &mut self,
        rip: u64,
        result: BlockKind,
        inv_gen: u64,
    ) -> Option<CompiledBlock> {
        // Compile timing lives at the rare compile seam, so it is always
        // recorded (no cost-model gate needed).
        let start = Instant::now();
        // Read the loop-header candidates off the decoded block before it is
        // moved into the lowering; see `promote_loop_headers`.
        let backward = baseline::backward_targets(rip, &result);
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
        // This block is a latch of any loop its backward edges lead into. If the
        // loop is this block (a self-loop), the region supersedes the block that
        // was just compiled — return it so the caller installs and runs the region
        // rather than the block it would otherwise overwrite it with.
        if let Some(region) = self.promote_loop_headers(rip, backward, inv_gen) {
            return Some(region);
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
        let (fault, dynamic_insns) = self.run_compiled(entry_rip, meta);
        // `dynamic_insns` was already charged to `jit_insns` / `guest_insns` by
        // `run_compiled`; carry it out as the step's retired count so the
        // engine loop's instruction budget counts real work, not block entries.
        let retired = usize::try_from(dynamic_insns).unwrap_or(usize::MAX);
        match fault {
            Some(inv) => (StepResult::InvalidMemory(inv), retired),
            None => (StepResult::Continue, retired),
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
    ///   (`chain_slots`), live for the call, and persist back
    ///   into `self.thread` from `JitCtx` on return; pending code writes are
    ///   drained (SMC invalidation) before returning.
    ///
    /// Returns `(Some(InvalidMem), dynamic_insns)` when a host mem helper
    /// faulted, else `(None, dynamic_insns)`. `dynamic_insns` is what the
    /// native frame (this block plus every chained successor and micro-stub)
    /// retired, read from `JitCtx::insn_acc` and already charged to
    /// `jit_insns` / `guest_insns` here.
    #[allow(clippy::type_complexity)]
    fn run_compiled(
        &mut self,
        entry_rip: u64,
        meta: CompiledRunMeta,
    ) -> (Option<exec::InvalidMem>, u64) {
        super::diag::count_block_entry(entry_rip);
        // Refresh pins only when GuestMemory generation changes (map/protect/free).
        // Rebuilding VAD-ranked pins every block was measurable on 7za.
        let mem_gen = self.shared.mem_gen.load(Ordering::Acquire);
        if self.thread.pins_gen != mem_gen {
            // Scoped so the read guard is released before the `&mut self`
            // diagnostics call below; the guard must not span it.
            {
                let mem = read_unpoisoned(&self.shared.mem);
                let infos = mem.jit_region_pins();
                self.thread.ctx.pins = std::array::from_fn(|i| MemPin::from_info(infos[i]));
            }
            self.thread.pins_gen = mem_gen;
            // The inline refill table caches `(page_key ^ mem_gen, host base,
            // prot)` per slot. Clearing it on the generation change means no
            // slot outlives the mapping it was resolved under, which is what
            // makes the emitted probe's folded tag compare *exact* rather than
            // merely improbable (see `RefillSlot`). Same event and the same
            // cost class as the pin rebuild above — a full table is 8 KiB.
            refill_clear(&mut self.thread.ctx);
            // The pin *shape* stats are a pure function of `ctx.pins`, which
            // only changes on the line above (`invalidate_tlb` forces the same
            // rebuild by setting `pins_gen = u64::MAX`). They are read by the
            // `WIE_RUNTIME_PROFILE` report and by nothing else, so recomputing
            // them here yields the identical numbers while taking ~8 pin
            // probes and three `JitStats` stores off every dispatch entry.
            self.record_pin_shape_stats();
        }
        // Generation-bump diagnostics: one compare per entry, and both stores
        // are skipped unless the generation actually moved.
        if mem_gen != self.last_mem_gen {
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
        }
        // No lock here: the guest-memory address is captured once per engine
        // (see `PerThreadJitState::guest_mem`) because it is a field of the
        // `Arc<JitShared>` payload and cannot move or be replaced. Reading its
        // *contents* still takes the guard at each site that needs it.
        let mem_ptr = self.thread.guest_mem;
        let ctx = &mut self.thread.ctx;
        let regs = &mut self.thread.regs;
        // Entry state, and *only* entry state. This used to build a whole
        // `JitCtx` by value — 3,744 B in all (`tlb` 2.3 KB + `pins`
        // + 16 GPRs + 16 XMM slots) copied in here and copied back out below, on
        // every dispatcher entry (≈8.9% of the guest thread on `7za a -mx9`
        // came from `_platform_memmove` alone). The frame is now owned by this
        // engine, so the state that must persist between entries (`tlb`,
        // `pins`, `sticky_*`, `edge_ic_*`, `shadow_*`, `tlb_hot_*`) is simply
        // still there and nothing is copied.
        //
        // Full GPR snapshot on entry: late-bound chaining reloads live regs from
        // JitCtx, so every architectural GPR must be valid for successors.
        for (i, slot) in ctx.gpr.iter_mut().enumerate() {
            *slot = regs.gpr(i);
        }
        // Full xmm snapshot: a chained successor may load any live xmm from
        // JitCtx even when THIS block is pure-GPR (its segment can still chain
        // into an SSE block). The old `uses_sse`-gated live-mask load zeroed
        // ctx.xmm for GPR blocks, so a chained SSE successor read zeros — e.g.
        // notepad's CRLF expansion `movd [rcx], xmm0` wrote 0x0000 pairs and
        // the loaded file truncated at the first such NUL.
        for i in 0..16 {
            let v = regs.xmm_at(i);
            if let Some(slot) = ctx.xmm.get_mut(i) {
                *slot = XmmSlot::from_u128(v);
            }
        }
        ctx.rflags = u64::from(regs.rflags);
        ctx.rip = entry_rip;
        ctx.gs_base = regs.gs_base();
        // Every raw pointer in the frame is re-stamped per entry, so no field
        // can outlive a move of the owning engine with a stale target.
        ctx.mem = mem_ptr;
        ctx.chain_slots = self.thread.chain_slots.as_mut_ptr();
        ctx.large_free = &raw mut self.thread.large_free;
        // Emitted guards + chain_tail compare the live shared generation
        // against the block's compile-time bake; any bump observed here
        // sends control back to this dispatcher (purge + fresh decode).
        ctx.inv_gen_ptr = std::ptr::from_ref(&self.shared.invalidate_gen);
        ctx.inv_gen_baked = meta.inv_gen;
        ctx.mem_gen = mem_gen;
        // Per-run fields: these MUST be reset, because the frame now outlives a
        // run. Everything above them is either re-stamped here or meant to
        // persist. A stale `fault` would turn the next entry into a spurious
        // access violation; a stale dirty mask would make the host skip
        // writeback of GPRs/XMMs the run really did change.
        ctx.fault = 0;
        ctx.fault_addr = 0;
        ctx.fault_size = 0;
        ctx.fault_access = 0;
        // 0 = Cranelift path (host falls back to full writeback);
        // trampolines OR their dirty bits; chain sets 0xffff.
        ctx.gpr_dirty_bits = 0;
        ctx.xmm_dirty_bits = 0;
        ctx.load_calls = 0;
        ctx.store_calls = 0;
        ctx.mem_path = MemPathSlice::default();
        // Fresh dispatcher entry always starts a new host-chain budget.
        ctx.chain_depth = 0;
        // Blocks (and chained successors / micro-stubs) fold their dynamic
        // instruction count in here; read back below.
        ctx.insn_acc = 0;
        // Chain edges taken by this frame and the GPR stores they did.
        // Folded into `stats.chain` below, like `insn_acc`.
        ctx.chain_hops = 0;
        ctx.chain_store_ops = 0;
        // UCRT malloc/free fast path: this session's heap layout, published
        // from the shared config at init. Plain copies — this frame's owner
        // holds it exclusively, so no atomicity is needed on the read side
        // (the Acquire on the publish is what orders the init writes).
        ctx.heap_ctrl_va = self.shared.heap[0].load(Ordering::Acquire);
        ctx.heap_base = self.shared.heap[1].load(Ordering::Acquire);
        ctx.heap_end = self.shared.heap[2].load(Ordering::Acquire);
        // SAFETY: func is a finalized Cranelift block, and `ctx` is this
        // engine's one persistent frame: `&mut` here is the only outstanding
        // borrow for the duration of the call, the engine is owned by this host
        // thread (`PerThreadJitState` is `Send` but not `Sync`), and every
        // pointer the frame carries was stamped above and resolves into mmap
        // arenas that outlive the call. Chain edges and micro-stubs hand the
        // same pointer to successors, so no second copy of the frame exists.
        unsafe {
            (meta.func)(std::ptr::from_mut(ctx));
        }
        // Charge what the native frame ACTUALLY retired, not the block's static
        // length: a self-looping block bumps the emitted trip counter once per
        // back edge, so this is exact (see `lower::emit::TripCounter`).
        let dynamic_insns = ctx.insn_acc;
        self.stats.exec.jit_insns = self.stats.exec.jit_insns.saturating_add(dynamic_insns);
        self.shared.record_guest_insns(dynamic_insns);
        // Chain-edge accounting. Additive per frame: `chain_hops` counts native
        // transfers to a compiled successor, `chain_store_ops` the GPR slots
        // their writebacks touched. Never derived from block entries — a
        // self-looping block takes zero chain hops no matter how many trips it
        // retires, which is exactly what makes the pair a measurement of the
        // chain-edge cost rather than of execution volume.
        self.stats.chain.hops = self.stats.chain.hops.saturating_add(ctx.chain_hops);
        self.stats.chain.store_ops = self
            .stats
            .chain
            .store_ops
            .saturating_add(ctx.chain_store_ops);
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
        // Nothing is persisted from `ctx` here: the TLB, pins, sticky ways,
        // edge IC, shadow stack and hot page ARE the frame, which this engine
        // owns. This used to be ~25 field copies back into `self.thread` plus
        // the whole 2.3 KB `tlb`, on every entry.
        //
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
        // Safe point: guest stores via `GuestMemory::write` leave a pending
        // range, so drop Ready blocks overlapping any store from this block —
        // but only after the native frame has returned.
        self.drain_pending_code_writes();
        (fault, dynamic_insns)
    }

    /// Snapshot the pin-shape stats printed by the `WIE_RUNTIME_PROFILE` report
    /// (`mem.pin_stack_bytes` / `pin_heap_bytes` / `pin_allow_bits`).
    ///
    /// Called from the pin-rebuild path — the only place `ctx.pins` can change —
    /// rather than from every dispatch entry. `pins` is rebuilt when the
    /// `GuestMemory` generation moves and nowhere else, so a per-entry walk
    /// recomputed the same three numbers until the next rebuild: pure overhead
    /// on the hot path for a diagnostic no one reads outside the report.
    fn record_pin_shape_stats(&mut self) {
        let stack = self.thread.ctx.pins[0];
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
        for (i, pin) in self.thread.ctx.pins.iter().enumerate().skip(1) {
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

    pub(super) fn invalidate_tlb(&mut self) {
        // The TLB/pins/sticky ways live in the engine's persistent frame.
        let ctx = &mut self.thread.ctx;
        ctx.tlb = GenTlb::new();
        ctx.tlb_hot_page = TLB_EMPTY;
        ctx.tlb_hot_ptr = std::ptr::null_mut();
        ctx.tlb_hot_prot = 0;
        ctx.tlb_hot_gen = 0;
        ctx.sticky_page = [TLB_EMPTY; STICKY_WAYS];
        ctx.sticky_ptr = [std::ptr::null_mut(); STICKY_WAYS];
        ctx.sticky_prot = [0; STICKY_WAYS];
        ctx.sticky_gen = [0; STICKY_WAYS];
        ctx.sticky_rr = 0;
        ctx.pins = [MemPin::EMPTY; PIN_SLOTS];
        // The inline fast path is a frame-side cache of the same translations;
        // a thread switch (or a protect/free that routed here) must not leave a
        // page resolvable through the refill table that the TLB no longer has.
        refill_clear(ctx);
        self.thread.pins_gen = u64::MAX;
    }

    pub(super) fn invalidate_chain_and_shadow(&mut self) {
        chain_table_clear(self.thread.chain_slots.as_mut());
        let ctx = &mut self.thread.ctx;
        ctx.edge_ic_va = [0; lower::EDGE_IC_SLOTS];
        ctx.edge_ic_fn = [0; lower::EDGE_IC_SLOTS];
        ctx.edge_ic_rr = 0;
        // The table is empty now; force a full re-sync from the cache on the
        // next dispatch (picks up worker-installed Ready blocks too).
        self.chain_sync_epoch = u64::MAX;
    }
}

/// Snapshot of a Ready block needed to run it without holding a cache borrow.
///
/// Deliberately carries no instruction count: the retired count is read back
/// from `JitCtx::insn_acc` after the native frame returns, which is the only
/// source that knows how many times the block actually looped.
#[derive(Clone, Copy)]
pub(super) struct CompiledRunMeta {
    func: unsafe extern "C" fn(*mut JitCtx),
    /// Invalidate-generation baked into the block's guards (also compared by
    /// Rust-side trampolines via `JitCtx::inv_gen_baked`).
    inv_gen: u64,
}

impl From<&CompiledBlock> for CompiledRunMeta {
    fn from(c: &CompiledBlock) -> Self {
        Self {
            func: c.func,
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
/// Fast-UCRT blocks promote after two visits but are not eager: the shape says
/// the block ends in an inlined call, not that it will be entered again.
/// Self-loops use the dedicated loop
/// hotness — a back-edge is the cheapest possible hotness signal, because one
/// entry can run the block a million times (HotSpot draws the same distinction
/// between an invocation counter and a back-edge counter). Otherwise promotion
/// is **work-weighted**: a block of N instructions promotes after
/// `clamp(target_work / N, FLOOR, CEILING)` visits — accumulated interpreted
/// work exceeding predicted compile cost × margin — so a 9-insn block keeps
/// ≈100 visits (the historical flat behavior) while a 90-insn block needs
/// only ~10. `forced_eager` (true under `cfg(test)`) means "compile everything
/// eagerly on first sight" and bypasses the size model entirely.
///
/// Returns `(threshold, eager)`. `eager` is true when the block must compile
/// on its first visit.
#[must_use]
pub(super) fn select_hot_threshold(
    is_ucrt: bool,
    is_loop: bool,
    pure_insns: usize,
    loop_hotness: u32,
    forced_eager: bool,
    target_work: u64,
) -> (u32, bool) {
    if is_ucrt {
        // Work-gated, not eager: a block ending in a fast UCRT call is a shape,
        // not evidence of a revisit. The 2-visit threshold keeps promotion cheap
        // for blocks that do repeat while removing the compile whose native code
        // is never entered.
        //
        // **Measured, and not the answer to `crt_hello`.** This arm is worth
        // 3 eager compiles, not the bulk: with it eager, `crt_hello` reported
        // `eager=368 bg_enq=368`; with it work-gated, `eager=365 bg_enq=365`
        // and `iced_fb=446` unchanged, i.e. the 365 came from somewhere else.
        // The other two `eager_compiles` sites cannot account for them either:
        // the region fold needs `WIE_JIT_REGION` (`promote_loop_headers` returns
        // `None` first), and the self-loop baseline branch needs one visit per
        // decision, while the same run reports `block_entries=5`. So the counter
        // is not tracking the policy's notion of an eager compile; see the note
        // on `eager_compiles` in the RUNBOOK before reading that number as a
        // count of first-sight compiles.
        (2, false)
    } else if is_loop {
        (loop_hotness, loop_hotness == 0)
    } else if forced_eager {
        // Unit-suite / forced-eager regime: byte-for-byte deterministic.
        (0, true)
    } else {
        (work_weighted_threshold(pure_insns, target_work), false)
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
    let mem = read_unpoisoned(&shared.mem);
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
    use crate::lock::read_unpoisoned;
    use crate::mem::protect;
    use crate::mem::{MEM_COMMIT, MEM_RESERVE};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    const TARGET: u64 = 900; // mirrored default; kept explicit so tests are self-contained

    // --- Work-weighted promotion math ---

    /// A 9-insn block keeps ≈100 visits — the historical flat threshold.
    #[test]
    fn nine_insn_block_keeps_historical_threshold() {
        assert_eq!(work_weighted_threshold(9, TARGET), 100);
        let (thr, eager) = select_hot_threshold(false, false, 9, 8, false, TARGET);
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
        let (thr, eager) = select_hot_threshold(false, false, 96, 8, false, TARGET);
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
    fn ucrt_blocks_promote_cheap_but_not_eager() {
        // A UCRT-ending block is a shape, not evidence of a revisit. Measured on
        // `crt_hello` (472 guest instructions retired): this arm accounted for
        // `eager=368 bg_enq=368` while only 28 blocks compiled and 4 were ever
        // entered as compiled code. The 2-visit threshold keeps promotion cheap
        // for the blocks that do repeat; first sight no longer compiles.
        for forced in [false, true] {
            let (thr, eager) = select_hot_threshold(true, false, 1, 8, forced, TARGET);
            assert_eq!(thr, 2, "UCRT promotion stays cheap (forced_eager={forced})");
            assert!(!eager, "UCRT blocks are work-gated (forced_eager={forced})");
        }
        // The self-loop arm is the one that must stay synchronous: one entry
        // there covers the whole run, so there is no second visit to learn from.
        assert!(select_hot_threshold(false, true, 8, 0, false, TARGET).1);
    }

    /// Self-loops use the dedicated loop hotness, not the size model; with a
    /// nonzero loop hotness they are not eager.
    #[test]
    fn self_loop_uses_loop_hotness_not_body_rule() {
        let (thr, eager) = select_hot_threshold(false, true, 10_000, 8, false, TARGET);
        assert_eq!(thr, 8);
        assert!(
            !eager,
            "self-loop eagerness comes from loop hotness, not body size"
        );
    }

    /// The forced-eager regime (unit-suite value) compiles everything eagerly,
    /// independent of body size — this is the byte-for-byte deterministic
    /// path the unit suite relies on.
    #[test]
    fn forced_eager_regardless_of_size() {
        assert_eq!(
            select_hot_threshold(false, false, 4, 8, true, TARGET),
            (0, true)
        );
        assert_eq!(
            select_hot_threshold(false, false, 10_000, 8, true, TARGET),
            (0, true)
        );
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
            let mem = read_unpoisoned(&cpu.shared.mem);
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
            let mem = read_unpoisoned(&cpu.shared.mem);
            record_opcode_sample(&mem, base.wrapping_add(5)); // ud2 — valid but distinct
        }
        assert!(OPCODE_SAMPLES.load(Ordering::Relaxed) > bad);
    }

    // --- Compile-cost floor (`--run-ignored`) --------------------------------
    //
    // The DOOM warm-up's compile histogram puts a one-instruction block at
    // 2 061 us (547 of them, 1.13 s of aggregate compile CPU), and a two-point
    // fit over the bucket means puts ~1.65 ms of every compile in a
    // block-size-independent constant — the bucket means are the facts, the fit
    // is an estimate. This harness splits that constant into the parts a change
    // could actually touch: `JITModule` construction, the first function in a
    // fresh module, the functions after it in the same module, the CLIF verifier,
    // and the tier-0 baseline path for the identical block.
    //
    // A benchmark, not an assertion, so it is ignored by default. Run it with:
    //   cargo nextest run -p wie-cpu -E 'test(compile_cost_floor)' \
    //       --run-ignored --no-capture
    // and repeat with `--release`, and with `WIE_JIT_VERIFIER=0` / `=1` pinned to
    // isolate the verifier. Each nextest test runs in its own process, so the
    // configuration is read exactly once per run and the arms do not contaminate
    // each other.

    /// The block every arm compiles — `mov rax,rbx; mov rax,rcx; add rax,rcx;
    /// inc rax; ret`. Five instructions, i.e. the histogram's dominant 4-8
    /// bucket, a `Ret` terminator so the decode is unambiguous, and above the
    /// 1-3 instruction shape that `match_micro_stub` turns into hand-written
    /// code without ever reaching Cranelift.
    const FLOOR_CODE: [u8; 13] = [
        0x48, 0x89, 0xd8, // mov rax, rbx
        0x48, 0x89, 0xc8, // mov rax, rcx
        0x48, 0x01, 0xc8, // add rax, rcx
        0x48, 0xff, 0xc0, // inc rax
        0xc3, // ret
    ];
    const FLOOR_BASE: u64 = 0x4000_0000;

    fn floor_median(v: &mut [f64]) -> f64 {
        if v.is_empty() {
            return f64::NAN;
        }
        v.sort_by(|a, b| a.partial_cmp(b).expect("timings are finite"));
        v[v.len() / 2]
    }

    /// Map, write and expose the floor block at `FLOOR_BASE`.
    fn floor_map(cpu: &mut JitCpu) {
        cpu.virtual_alloc(
            FLOOR_BASE,
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc floor page");
        cpu.mem_write(FLOOR_BASE, &FLOOR_CODE).expect("write code");
    }

    fn floor_decode(cpu: &JitCpu) -> crate::jit::block::BlockKind {
        let mem = read_unpoisoned(&cpu.shared.mem);
        crate::jit::block::decode_pure_gpr_block(&mem, None, FLOOR_BASE)
    }

    /// Print the compile-cost floor, split by factor. See the note above.
    #[test]
    #[ignore = "benchmark: measures the compile-cost floor; assertions live in the report, not here"]
    fn compile_cost_floor_split() {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "compile_cost_floor: profile={} verifier={}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            super::JitConfig::get().verifier_enabled()
        );

        // (a) A whole fresh shared engine: both modules are built inside
        // `JitShared::new`, so this is the per-module cost end to end.
        let mut module_us = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            let cpu = JitCpu::open_x86_64();
            module_us.push(t.elapsed().as_secs_f64() * 1e6);
            drop(cpu);
        }
        let _ = writeln!(
            out,
            "compile_cost_floor: (a) fresh JitShared (all modules)      median={:8.1} us",
            floor_median(&mut module_us)
        );

        // (b) First function in a fresh module vs (c) the functions after it in
        // the same module. If the constant is per-module, (b) >> (c) and the
        // saving is amortisation; if (b) ~= (c), it is per-function and the only
        // lever is fewer functions.
        let mut first_us = Vec::new();
        let mut second_us = Vec::new();
        let mut rest_us = Vec::new();
        let mut trend: Vec<f64> = Vec::new();
        let mut insn_count = 0_u32;
        let mut shape_kind = crate::jit::block::BlockKind::NotPure;
        for _ in 0..3 {
            let mut cpu = JitCpu::open_x86_64();
            floor_map(&mut cpu);
            let kind = floor_decode(&cpu);
            shape_kind = kind.clone();
            let inv_gen = cpu.shared.invalidate_gen.load(Ordering::Acquire);
            let t = Instant::now();
            let compiled = cpu.shared.compile_from_kind_shared(
                &cpu.fast_api,
                FLOOR_BASE,
                kind.clone(),
                inv_gen,
            );
            first_us.push(t.elapsed().as_secs_f64() * 1e6);
            if let Some(c) = compiled {
                insn_count = c.insn_count;
            }
            for i in 1..32_u64 {
                let t = Instant::now();
                let r = cpu.shared.compile_from_kind_shared(
                    &cpu.fast_api,
                    FLOOR_BASE + i * 64,
                    kind.clone(),
                    inv_gen,
                );
                let dt = t.elapsed().as_secs_f64() * 1e6;
                if i == 1 {
                    second_us.push(dt);
                }
                if i < 7 {
                    trend.push(dt);
                }
                if r.is_some() && i > 1 {
                    rest_us.push(dt);
                }
            }
        }
        let shape = match &shape_kind {
            crate::jit::block::BlockKind::Pure { insns, term, .. } => {
                format!("Pure insns={} term={term:?}", insns.len())
            }
            crate::jit::block::BlockKind::NotPure => "NotPure".to_string(),
        };
        let _ = writeln!(
            out,
            "compile_cost_floor: decoded {shape} compiled_instrs={insn_count}"
        );
        let _ = writeln!(
            out,
            "compile_cost_floor: (b) 1st function, fresh module          median={:8.1} us  n={}",
            floor_median(&mut first_us),
            first_us.len()
        );
        let _ = writeln!(
            out,
            "compile_cost_floor: (c) 2nd function, same module           median={:8.1} us  n={}",
            floor_median(&mut second_us),
            second_us.len()
        );
        let _ = writeln!(
            out,
            "compile_cost_floor: (d) 3rd..32nd function, same module     median={:8.1} us  n={}",
            floor_median(&mut rest_us),
            rest_us.len()
        );
        let shown: Vec<String> = trend.iter().map(|v| format!("{v:.0}")).collect();
        let _ = writeln!(
            out,
            "compile_cost_floor: (e) first 18 functions in order, us: {}",
            shown.join(" ")
        );

        // (f) The tier-0 baseline path for the identical block: the cheapest
        // compile the engine has, and the floor below which no Cranelift change
        // can go without changing what runs.
        let mut cpu = JitCpu::open_x86_64();
        floor_map(&mut cpu);
        let kind = floor_decode(&cpu);
        let inv_gen = cpu.shared.invalidate_gen.load(Ordering::Acquire);
        let mut baseline_us = Vec::new();
        for i in 0..16_u64 {
            let t = Instant::now();
            let r = super::baseline::try_compile_baseline(
                &cpu.shared,
                &cpu.fast_api,
                FLOOR_BASE + i * 64,
                kind.clone(),
                inv_gen,
            );
            let dt = t.elapsed().as_secs_f64() * 1e6;
            if r.is_some() {
                baseline_us.push(dt);
            }
        }
        let _ = writeln!(
            out,
            "compile_cost_floor: (f) tier-0 baseline, same block         median={:8.1} us  n={}",
            floor_median(&mut baseline_us),
            baseline_us.len()
        );
        eprint!("{out}");
    }
}
