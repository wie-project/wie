//! Process-wide compiled-code cache, guest memory, and the background compiler worker.
//!
//! One [`JitShared`] per process, shared via `Arc` across all per-thread
//! engines. Wait cells / enqueue outcomes / worker helpers used from
//! `pipeline.rs` or `mod.rs` tests are `pub(super)`.

#![allow(
    unsafe_code, // Cranelift finalized fn pointers + host mem helpers
    private_interfaces // JitShared/PerThreadJitState expose crate-private types
)]

use super::CacheEntry;
use super::block::{self, BlockKind};

// [verifier-rejection warn rate-limit] First REJECT_WARN_MAX rejections log at
// WARN; the tail logs at DEBUG so pathological guests don't spam the console.
const REJECT_WARN_MAX: usize = 50;
static REJECTION_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
use super::config::{BG_QUEUE_CAP, JitConfig};
use super::engine::JitEngine;
use super::fast_api::FastApiKind;
use super::gen_tlb::GenTlb;
use super::lower::{
    self, CHAIN_SLOTS, CompiledBlock, MemPin, PIN_SLOTS, STICKY_WAYS, TLB_EMPTY, TLB_SETS,
    TLB_WAYS_PER_SET, TlbValue, compile_block,
};
use super::pipeline::resolve_thunk_va;
use super::profile::BgCompileProfile;
use super::trampolines::match_micro_stub;
use crate::ConcurrentHashMap;
use crate::exec::HookWindow;
use crate::mem::GuestMemory;
use crate::regs::RegFile;
use ahash::HashMap;
use ahash::HashMapExt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

/// Per-entry completion token for background compiles.
///
/// One-shot `std::sync::mpsc` channel: the worker sends exactly once when it
/// resolves the entry (install / fail / drop), and a guest thread blocks in
/// `recv_timeout` for the remaining budget directly — no chunked condvar
/// polling, so there is no 1 ms latency floor. A token sent before the waiter
/// arrives buffers in the channel; `wait_timeout` still returns immediately.
///
/// The cell also carries the visit threshold that governed the promotion, so
/// a later wait timeout can re-arm cooldown hysteresis (doubling) even when
/// the waiting thread never computed the threshold itself (re-entry into an
/// already-Queued entry).
pub(super) struct BgWaitCell {
    /// Sender half; taken and fired once by [`Self::notify_all`].
    tx: Mutex<Option<mpsc::Sender<()>>>,
    /// Receiver half; taken once by the first waiter. Later waiters get
    /// `None` from [`Self::wait_timeout`] and fall back to cache re-checks.
    rx: Mutex<Option<mpsc::Receiver<()>>>,
    /// Visit threshold that governed this promotion (cooldown doubling base).
    thr: AtomicU32,
}

impl BgWaitCell {
    pub(super) fn new(thr: u32) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            rx: Mutex::new(Some(rx)),
            thr: AtomicU32::new(thr),
        })
    }

    /// Fire the one-shot completion token (worker install/fail/drop paths).
    ///
    /// Idempotent: a second call finds the sender taken and does nothing.
    /// `pub(super)` so JIT-module tests can fire tokens directly.
    pub(super) fn notify_all(&self) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            // A missing waiter is fine — the token buffers until recv.
            let _ = tx.send(());
        }
    }

    /// Block until the token fires or `timeout` elapses.
    ///
    /// Returns `true` when woken by the token; `false` when the budget ran
    /// out or another waiter already consumed the receiver (callers must
    /// re-check the cache state either way).
    pub(super) fn wait_timeout(&self, timeout: Duration) -> bool {
        let Some(rx) = self.rx.lock().unwrap().take() else {
            return false;
        };
        rx.recv_timeout(timeout).is_ok()
    }

    /// Threshold that governed this promotion (cooldown doubling base).
    pub(super) fn threshold(&self) -> u32 {
        self.thr.load(Ordering::Relaxed)
    }
}

/// Result of handing a block to the background worker.
pub(super) enum BgEnqueueOutcome {
    /// Entry transitioned to `Queued`; the caller may wait on this cell
    /// for the worker's Ready entry.
    Queued(Arc<BgWaitCell>),
    /// Already `Ready` in the cache (worker beat us); caller re-reads.
    Ready,
    /// Worker unavailable / queue full / block not compilable: caller falls
    /// back to inline compilation (or iced for NotPure).
    ///
    /// Invariant: this is the ONLY outcome that permits inline compilation
    /// while a worker might still be alive. A dedup hit (another thread
    /// already queued this exact rip) returns `Queued`-dedup as
    /// [`BgEnqueueOutcome::Unavailable`] only when the worker is gone;
    /// callers re-check [`JitShared::entry_queued`] before inlining so a
    /// live worker's in-flight block is never compiled twice.
    Unavailable,
}

/// Intermediate states observed while waiting on a background compile.
pub(super) enum BgWaitState {
    Ready(CompiledBlock),
    Never,
}

/// Process-wide JIT compilation cache and guest memory, shared across threads.
/// One instance per process, shared via Arc across all per-thread engines.
#[doc(hidden)]
pub struct JitShared {
    /// Cranelift JIT module (behind Mutex: compiled blocks are serialized anyway).
    #[doc(hidden)]
    pub engine: Mutex<Option<JitEngine>>,
    /// Guest memory: page tables, mmap backend. RwLock protects metadata;
    /// reads are the common path (generation, pins, decode), writes are rare (map/free).
    #[doc(hidden)]
    pub mem: RwLock<GuestMemory>,
    /// Guest entry VA → CacheEntry.
    #[doc(hidden)]
    pub cache: ConcurrentHashMap<u64, CacheEntry>,
    /// Ready-block FuncIds for chaining.
    #[doc(hidden)]
    pub chain_ids: ConcurrentHashMap<u64, cranelift_module::FuncId>,
    /// Guest page keys covered by Ready blocks (SMC tracking).
    #[doc(hidden)]
    pub code_pages: Mutex<HashMap<u64, u32>>,
    /// VAs installed as `Ready` since a consumer last drained the list, in
    /// install order. Guests consume this delta on chain-table resync
    /// instead of walking the whole cache per epoch advance (measured:
    /// 5,130 resyncs × 593-entry width ≈ 3M inserts per run before this).
    /// Consumers validate each VA is still `Ready` — an install that was
    /// later invalidated may linger here and is skipped, never linked.
    pub recent_installs: Mutex<Vec<u64>>,
    /// Guest page keys written by JIT stores (SMC invalidation). Separate lock
    /// so drain_pending_code_writes avoids GuestMemory write lock.
    pub pending_code_writes: Mutex<Vec<u64>>,
    /// Set when too many distinct pages were written; drain does a full code flush.
    pub pending_code_overflow: AtomicBool,
    /// Cached GuestMemory generation, updated on map/protect/free (avoids mem lock).
    pub mem_gen: AtomicU64,
    /// Whether the Cranelift engine was successfully initialized.
    pub engine_ready: AtomicBool,
    /// Background compiler queue sender. The worker thread owns the receiver
    /// and dies when this sender drops (i.e. when the last `Arc<JitShared>` goes
    /// away). `None` when the worker was never spawned or failed to spawn.
    /// Jobs carry `(rip, decoded kind, invalidate_gen snapshot)`: the
    /// generation was read on the guest thread BEFORE the bytes were decoded,
    /// so blocks compiled from pre-invalidation bytes never bake a newer
    /// generation over them (a high/stale guard miss must be impossible).
    pub bg_tx: Mutex<Option<SyncSender<(u64, BlockKind, u64)>>>,
    /// Set once the worker thread has been spawned (spawn-once latch).
    pub bg_spawned: AtomicBool,
    /// Whether the background compiler is currently alive (set true on spawn,
    /// cleared when the worker exits). Guest threads fall back to inline
    /// compilation when this is false.
    pub bg_alive: Arc<AtomicBool>,
    /// Bumped on every background cache install. Per-thread engines re-sync
    /// their late-bound chain tables when they observe a new epoch.
    pub cache_epoch: AtomicU64,
    /// Bumped whenever `Ready` entries LEAVE the shared cache (range
    /// invalidation or a full clear). Unlike [`Self::cache_epoch`], which
    /// tracks installs only, this signals drops: another thread's chain table
    /// may still map the dropped VAs to now-stale fn pointers, and only the
    /// invalidating thread repairs its own table. Engines watch this counter
    /// and force a full chain-table rebuild whenever it moves.
    ///
    /// Release on bump / Acquire on load: observing the bump must also make
    /// the preceding cache drops visible, so the rebuilding thread's `pin()`
    /// cannot re-link an entry from a pre-drop snapshot.
    pub invalidate_gen: AtomicU64,
    /// Times [`Self::cache_epoch`] advanced (installs / invalidations).
    /// Folded into `JitStats::chain_epoch_bumps`: a high rate means every
    /// guest thread pays an O(cache) chain-table resync between blocks.
    pub chain_epoch_bumps: AtomicU64,
    /// Total background compiles installed (shared across threads; surfaced in
    /// per-thread [`JitStats`] snapshots).
    pub bg_compiles: AtomicU64,
    /// UCRT fast-API pairs mirrored from each engine's `configure_fast_path` so
    /// the worker lowers calls exactly like the inline path would. Shared as an
    /// `Arc<[_]>` so each background job pays a refcount bump, not a Vec clone.
    pub bg_fast_api: Mutex<Arc<[(u64, FastApiKind)]>>,
    /// Lock-free background-compile timing (worker writes, per-thread
    /// snapshots read via [`super::JitCpu::stats`]).
    pub bg_compile: BgCompileProfile,
    /// Approximate number of items currently queued or in flight on the
    /// background worker (incremented on enqueue, decremented per processed
    /// item). Backpressure signal: a deep queue raises the local promotion
    /// threshold instead of feeding a backlog guests will time out on.
    pub bg_queue_depth: AtomicU64,
    /// Test-only latch forcing the background path on for this instance
    /// (env-independent, and per-`JitShared` so parallel unit tests cannot
    /// interfere with each other).
    #[cfg(test)]
    pub(super) bg_force: AtomicBool,
}

impl JitShared {
    /// Copy of installs at index `from..len` plus the new watermark (`len`).
    ///
    /// Append-only by design: each consumer thread keeps its own watermark,
    /// so entries are re-readable and never stolen from another thread.
    pub(crate) fn installs_since(&self, from: usize) -> (Vec<u64>, usize) {
        let g = self.recent_installs.lock().unwrap();
        let new_wm = g.len();
        (g[from.min(g.len())..].to_vec(), new_wm)
    }

    /// Current install-list length (watermark anchor after a full rebuild).
    pub(crate) fn recent_installs_len(&self) -> usize {
        self.recent_installs.lock().unwrap().len()
    }
    pub(super) fn new() -> Self {
        let has_engine = match JitEngine::new() {
            Ok(e) => {
                tracing::debug!("cranelift JIT module ready");
                Some(e)
            }
            Err(e) => {
                tracing::warn!(error = %e, "cranelift JIT unavailable; iced-only");
                None
            }
        };
        let engine_ready = has_engine.is_some();
        Self {
            engine: Mutex::new(has_engine),
            mem: RwLock::new(GuestMemory::new()),
            cache: ConcurrentHashMap::new(),
            chain_ids: ConcurrentHashMap::new(),
            code_pages: Mutex::new(HashMap::new()),
            recent_installs: Mutex::new(Vec::new()),
            pending_code_writes: Mutex::new(Vec::new()),
            pending_code_overflow: AtomicBool::new(false),
            mem_gen: AtomicU64::new(0),
            engine_ready: AtomicBool::new(engine_ready),
            bg_tx: Mutex::new(None),
            bg_spawned: AtomicBool::new(false),
            bg_alive: Arc::new(AtomicBool::new(false)),
            cache_epoch: AtomicU64::new(0),
            invalidate_gen: AtomicU64::new(0),
            chain_epoch_bumps: AtomicU64::new(0),
            bg_compiles: AtomicU64::new(0),
            bg_fast_api: Mutex::new(Arc::from(Vec::new())),
            bg_compile: BgCompileProfile::default(),
            bg_queue_depth: AtomicU64::new(0),
            #[cfg(test)]
            bg_force: AtomicBool::new(false),
        }
    }

    /// Whether the background path is enabled for this instance: env default
    /// (on for real runs, off under `cfg(test)`) or the test latch.
    pub(super) fn bg_enabled_here(&self) -> bool {
        JitConfig::get().bg_enabled() || self.bg_force_test()
    }

    /// Whether `rip` is currently `Queued` in the cache — a background
    /// compile for this exact block is already in flight, so a caller that
    /// just got [`BgEnqueueOutcome::Unavailable`] must not inline-compile it
    /// while the worker lives (that would build the block twice).
    pub(super) fn entry_queued(&self, rip: u64) -> bool {
        matches!(self.cache.pin().get(&rip), Some(CacheEntry::Queued(_)))
    }

    #[cfg(test)]
    fn bg_force_test(&self) -> bool {
        self.bg_force.load(Ordering::Relaxed)
    }

    #[cfg(not(test))]
    fn bg_force_test(&self) -> bool {
        let _ = self;
        false
    }

    /// Install a compiled block into the shared cache (inline-path entry point).
    ///
    /// Shared-only bookkeeping: drops any replaced Ready entry from `chain_ids`
    /// and `code_pages`, registers the new block for direct chaining + SMC
    /// tracking, and inserts the Ready entry. No per-thread side effects.
    pub(crate) fn insert_ready(&self, rip: u64, compiled: CompiledBlock) {
        let removed = {
            let cache = self.cache.pin();
            let old = cache.remove(&rip).cloned();
            if let Some(CacheEntry::Ready(old)) = old {
                self.chain_ids.pin().remove(&rip);
                Some((old.guest_start, old.guest_end))
            } else {
                None
            }
        };
        if let Some((gs, ge)) = removed {
            self.code_pages_remove_range(gs, ge);
        }
        if JitConfig::get().chain_enabled() {
            let fid = compiled.func_id;
            if let Some(fid) = fid {
                self.chain_ids.pin().insert(rip, fid);
            }
            // Inline installs are visible to other threads' delta-resyncs too.
            self.recent_installs.lock().unwrap().push(rip);
        }
        self.code_pages_add_range(compiled.guest_start, compiled.guest_end);
        self.cache.pin().insert(rip, CacheEntry::Ready(compiled));
    }

    /// Spawn the background compiler thread exactly once.
    ///
    /// The thread is detached (no stored join handle): it holds a `Weak` to
    /// this shared state and dies when the last `Arc` drops (the sender in
    /// `bg_tx` disconnects, so `recv` errors). This avoids a self-join if the
    /// worker happens to hold the final strong reference while a job runs.
    pub(crate) fn ensure_bg_worker(self: &Arc<Self>) {
        if self.bg_spawned.swap(true, Ordering::AcqRel) {
            return;
        }
        let (tx, rx) = mpsc::sync_channel::<(u64, BlockKind, u64)>(BG_QUEUE_CAP);
        *self.bg_tx.lock().unwrap() = Some(tx);
        let weak = Arc::downgrade(self);
        let alive = Arc::clone(&self.bg_alive);
        let spawned = std::thread::Builder::new()
            .name("wie-jit-bg-compiler".into())
            .spawn(move || {
                Self::bg_worker_main(&weak, &rx);
                alive.store(false, Ordering::Release);
            });
        if spawned.is_ok() {
            // The worker is up (it can only exit after this sender drops,
            // which requires the shared state to be gone — impossible while
            // we hold `self`). Guests may enqueue immediately.
            self.bg_alive.store(true, Ordering::Release);
        } else {
            // Spawn failed (resource limits): revert so callers fall back
            // to inline compilation and a later call can retry.
            self.bg_spawned.store(false, Ordering::Release);
            self.bg_tx.lock().unwrap().take();
        }
    }

    /// Worker main loop: compile queued blocks, install Ready entries, fire
    /// completion tokens. Exits when the channel disconnects (shared state
    /// dropped).
    ///
    /// Batch-drain: after each blocking `recv`, the loop pulls every item
    /// already queued via `try_recv` before sleeping again, so a promotion
    /// burst costs one wakeup instead of one wakeup per block.
    fn bg_worker_main(shared: &Weak<Self>, rx: &Receiver<(u64, BlockKind, u64)>) {
        while let Ok(first) = rx.recv() {
            let Some(shared) = shared.upgrade() else {
                break;
            };
            shared.bg_process_one(first);
            // Batch-drain: pull every currently-queued item before blocking
            // again so a promotion burst costs one wakeup, not one per block.
            // We hold a strong `shared` for the whole batch, so teardown
            // cannot land mid-drain.
            while let Ok(item) = rx.try_recv() {
                shared.bg_process_one(item);
            }
        }
    }

    /// Compile + install one queued job (shared by the blocking and
    /// batch-drain paths). Decrements [`Self::bg_queue_depth`] exactly once.
    /// `inv_gen` is the generation snapshot taken before the job's bytes were
    /// decoded (see [`Self::bg_tx`]).
    fn bg_process_one(&self, (rip, kind, inv_gen): (u64, BlockKind, u64)) {
        self.bg_queue_depth.fetch_sub(1, Ordering::Relaxed);
        let mem_gen_before = self.mem_gen.load(Ordering::Acquire);
        // Arc clone: refcount bump only (table is built once per engine).
        let fast_api = self.bg_fast_api.lock().unwrap().clone();
        let start = Instant::now();
        let compiled = self.compile_from_kind_shared(fast_api.as_ref(), rip, kind, inv_gen);
        let us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let insns = compiled.as_ref().map_or(0, |c| u64::from(c.insn_count));
        self.bg_compile.record(insns, us);
        // If guest memory was remapped (map/protect/free) or a code page
        // write is pending while we compiled, the cached bytes may be
        // stale — drop the result and let the guest re-request.
        let stale = self.mem_gen.load(Ordering::Acquire) != mem_gen_before
            || compiled.as_ref().is_some_and(|c| {
                let len = usize::try_from(c.guest_end.saturating_sub(c.guest_start)).unwrap_or(0);
                self.pending_code_write_overlaps(c.guest_start, len)
            });
        if stale {
            self.bg_install_drop(rip);
        } else {
            match compiled {
                Some(c) => self.bg_install_ready(rip, c),
                None => self.bg_install_never(rip),
            }
        }
    }

    /// Compile a decoded block without any per-thread state.
    ///
    /// Identical to the inline path's lowering (same `compile_block`, same
    /// `chain_ids` snapshot for direct chaining, same `call_fast` resolution)
    /// so background output is byte-for-byte the same code — only the *when*
    /// differs. `inv_gen` must be the [`Self::invalidate_gen`] snapshot taken
    /// before the caller decoded the guest bytes: baking an older-or-equal
    /// generation guarantees a guard mismatch whenever the baked bytes went
    /// stale (never bakes a newer gen over pre-invalidation bytes).
    pub(super) fn compile_from_kind_shared(
        &self,
        fast_api: &[(u64, FastApiKind)],
        rip: u64,
        result: BlockKind,
        inv_gen: u64,
    ) -> Option<CompiledBlock> {
        match result {
            BlockKind::Pure {
                insns,
                end_rip,
                bytes_len,
                term,
            } => {
                // 1–3 insn guest stubs: hand-written host trampoline (no Cranelift).
                if let Some(micro) = match_micro_stub(&insns, term) {
                    let guest_end = rip.saturating_add(u64::from(bytes_len));
                    return Some(CompiledBlock {
                        func: micro.func(),
                        func_id: None,
                        insn_count: micro.insn_count(),
                        guest_start: rip,
                        guest_end,
                        inv_gen,
                    });
                }

                // Resolve import thunks before mutably borrowing the JIT engine.
                let call_fast = match term {
                    Some(block::BlockTerm::Call { target, .. }) => {
                        let mem = self.mem.read().unwrap();
                        let final_va = resolve_thunk_va(&mem, target);
                        drop(mem);
                        fast_api
                            .iter()
                            .find_map(|&(k, kind)| (k == final_va).then_some(kind))
                    }
                    _ => None,
                };
                let chain_on = JitConfig::get().chain_enabled();
                // Snapshot the chain-id table for this compile (pin guard must
                // not outlive the snapshot — the engine lock and `compile_block`
                // run after it drops).
                let chain_map: HashMap<u64, cranelift_module::FuncId> = if chain_on {
                    let guard = self.chain_ids.pin();
                    guard.iter().map(|(&va, &fid)| (va, fid)).collect()
                } else {
                    HashMap::new()
                };
                let mut eng_guard = self.engine.lock().unwrap();
                let eng = eng_guard.as_mut()?;
                match compile_block(
                    eng, rip, &insns, end_rip, term, call_fast, &chain_map, bytes_len, inv_gen,
                ) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        // Verifier/codegen rejection: the block's IR failed a
                        // lowering or verification pass. Surface it loudly,
                        // then fall back to interpretation for this block.
                        // Rate-limited: first REJECT_WARN_MAX rejections at
                        // WARN (unique enough to triage), the tail at DEBUG.
                        let n = REJECTION_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let msg_args = (format_args!("{rip:#x}"), bytes_len, n + 1, e.to_string());
                        if n < REJECT_WARN_MAX {
                            tracing::warn!(
                                rip = format_args!("{}", msg_args.0),
                                bytes_len = msg_args.1,
                                rejection_seq = msg_args.2,
                                error = %msg_args.3,
                                "jit compile rejected by verifier/codegen — \
                                 block falls back to the interpreter"
                            );
                        } else {
                            tracing::debug!(
                                rip = format_args!("{}", msg_args.0),
                                bytes_len = msg_args.1,
                                rejection_seq = msg_args.2,
                                error = %msg_args.3,
                                "jit compile rejected by verifier/codegen — \
                                 block falls back to the interpreter"
                            );
                        }
                        use cranelift_module::Module;
                        eng.module.clear_context(&mut eng.ctx);
                        None
                    }
                }
            }
            BlockKind::NotPure => None,
        }
    }

    /// Worker-side install of a successfully compiled block: cache + chaining +
    /// SMC bookkeeping + epoch bump + waiter notification.
    fn bg_install_ready(&self, rip: u64, compiled: CompiledBlock) {
        let notify = {
            let cache = self.cache.pin();
            let old = cache.remove(&rip).cloned();
            let notify = match &old {
                Some(CacheEntry::Queued(n)) => Some(Arc::clone(n)),
                _ => None, // resolved elsewhere (guest inline fallback): skip
            };
            if let Some(CacheEntry::Ready(old)) = old {
                self.chain_ids.pin().remove(&rip);
                self.code_pages_remove_range(old.guest_start, old.guest_end);
            }
            if JitConfig::get().chain_enabled()
                && let Some(fid) = compiled.func_id
            {
                self.chain_ids.pin().insert(rip, fid);
            }
            self.code_pages_add_range(compiled.guest_start, compiled.guest_end);
            cache.insert(rip, CacheEntry::Ready(compiled));
            notify
        };
        // Record the install for delta-resync BEFORE bumping the epoch: a
        // thread that observes the new epoch is guaranteed to find the VA in
        // its drain (install-order lock discipline).
        self.recent_installs.lock().unwrap().push(rip);
        self.cache_epoch.fetch_add(1, Ordering::Relaxed);
        self.chain_epoch_bumps.fetch_add(1, Ordering::Relaxed);
        self.bg_compiles.fetch_add(1, Ordering::Relaxed);
        if let Some(n) = notify {
            n.notify_all();
        }
    }

    /// Worker-side resolution when the block failed to compile: mark Never and
    /// wake waiters (they fall through to iced, no inline re-attempt needed).
    fn bg_install_never(&self, rip: u64) {
        let notify = {
            let cache = self.cache.pin();
            match cache.remove(&rip).cloned() {
                Some(CacheEntry::Queued(n)) => {
                    cache.insert(rip, CacheEntry::Never);
                    Some(n)
                }
                _ => None,
            }
        };
        if let Some(n) = notify {
            n.notify_all();
        }
    }

    /// Worker-side resolution when the result was stale (guest memory changed
    /// while compiling): drop the Queued entry entirely so the guest re-decodes
    /// fresh, and wake waiters so they fall back immediately instead of burning
    /// their full wait budget.
    fn bg_install_drop(&self, rip: u64) {
        let notify = {
            let cache = self.cache.pin();
            match cache.remove(&rip).cloned() {
                Some(CacheEntry::Queued(n)) => Some(n),
                _ => None,
            }
        };
        if let Some(n) = notify {
            n.notify_all();
        }
    }

    /// Whether any guest code page write is currently pending over `[addr, addr+len)`.
    fn pending_code_write_overlaps(&self, addr: u64, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let end = addr.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        let pending = self.pending_code_writes.lock().unwrap();
        let mut page = addr >> 12;
        let last = end.saturating_sub(1) >> 12;
        while page <= last {
            if pending.contains(&page) {
                return true;
            }
            page = page.saturating_add(1);
        }
        false
    }

    pub(super) fn code_pages_overlap(&self, addr: u64, len: usize) -> bool {
        let code_pages = self.code_pages.lock().unwrap();
        if len == 0 || code_pages.is_empty() {
            return false;
        }
        let end = addr.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        if end <= addr {
            return !code_pages.is_empty();
        }
        let mut page = addr >> 12;
        let last = end.saturating_sub(1) >> 12;
        while page <= last {
            if code_pages.contains_key(&page) {
                return true;
            }
            page = page.saturating_add(1);
        }
        false
    }

    fn code_pages_add_range(&self, guest_start: u64, guest_end: u64) {
        if guest_end <= guest_start {
            return;
        }
        let mut code_pages = self.code_pages.lock().unwrap();
        let mut page = guest_start >> 12;
        let last = guest_end.saturating_sub(1) >> 12;
        while page <= last {
            code_pages
                .entry(page)
                .and_modify(|c| *c = c.saturating_add(1))
                .or_insert(1);
            page = page.saturating_add(1);
        }
    }

    pub(super) fn code_pages_remove_range(&self, guest_start: u64, guest_end: u64) {
        if guest_end <= guest_start {
            return;
        }
        let mut code_pages = self.code_pages.lock().unwrap();
        let mut page = guest_start >> 12;
        let last = guest_end.saturating_sub(1) >> 12;
        while page <= last {
            match code_pages.get_mut(&page) {
                Some(c) if *c > 1 => *c = c.saturating_sub(1),
                Some(_) => {
                    code_pages.remove(&page);
                }
                None => {}
            }
            page = page.saturating_add(1);
        }
    }
}

// SAFETY: GuestMemory is behind `RwLock`; raw pointers inside GuestMemory are
// non-owning views of mmap arenas that live for the process lifetime.
// The lock provides synchronization for metadata updates; the JIT hot path uses
// per-thread TLB with host pointers directly.
#[expect(unsafe_code)]
unsafe impl Send for JitShared {}
#[expect(unsafe_code)]
unsafe impl Sync for JitShared {}

/// Per-thread JIT execution state: registers, TLB, chain table, shadow stack.
/// One instance per guest thread. Not shared.
#[doc(hidden)]
pub struct PerThreadJitState {
    /// x86-64 register file.
    #[doc(hidden)]
    pub regs: RegFile,
    /// Runtime hook window (stop-bitmap for fake-API range).
    #[doc(hidden)]
    pub hooks: Option<HookWindow>,
    /// Recent RIP history for diagnostics (ring buffer).
    pub rip_trace: [u64; 32],
    pub rip_trace_i: usize,
    pub rip_trace_n: usize,
    /// Instructions retired via the iced interpreter (diagnostic counter).
    pub iced_steps: u64,
    /// Persistent set-associative page TLB across chained blocks.
    pub tlb: GenTlb<u64, TlbValue, TLB_SETS, TLB_WAYS_PER_SET>,
    /// Sticky last-hit page for inline IR mem path.
    pub tlb_hot_page: u64,
    pub tlb_hot_ptr: *mut u8,
    pub tlb_hot_prot: u64,
    pub tlb_hot_gen: u64,
    /// Multi sticky ways.
    pub sticky_page: [u64; STICKY_WAYS],
    pub sticky_ptr: [*mut u8; STICKY_WAYS],
    pub sticky_prot: [u64; STICKY_WAYS],
    pub sticky_gen: [u64; STICKY_WAYS],
    pub sticky_rr: u64,
    /// Region-direct pins.
    pub pins: [MemPin; PIN_SLOTS],
    /// Generation at which pins were last rebuilt.
    pub pins_gen: u64,
    /// Open-addressing guest VA → host block fn (late-bound block chaining).
    pub chain_slots: Box<[lower::ChainSlot; CHAIN_SLOTS]>,
    /// Monomorphic edge IC.
    pub edge_ic_va: [u64; lower::EDGE_IC_SLOTS],
    pub edge_ic_fn: [u64; lower::EDGE_IC_SLOTS],
    pub edge_ic_rr: u64,
    /// Shadow return-stack depth.
    pub shadow_sp: u64,
    pub shadow_ret: [u64; lower::SHADOW_DEPTH],
    /// Sampling counter for the iced-residue opcode histogram (increments
    /// every iced step; the decode+bucket happens only every Nth step and
    /// only when `WIE_JIT_OPCODE_HISTO` is on).
    pub opcode_sample_i: u32,
    /// Visit threshold selected by the most recent miss-path promotion
    /// decision. Consumed by `enqueue_bg` when it builds the wait cell (the
    /// cell carries the threshold so a later timeout can double it for
    /// cooldown hysteresis). Owned by the thread; no synchronization needed.
    pub pending_promote_thr: u32,
}

// SAFETY: TLB/pin raw pointers are non-owning views of guest mmap arenas.
// Each PerThreadJitState is owned by one host thread; never moved between threads.
#[expect(unsafe_code)]
unsafe impl Send for PerThreadJitState {}

impl PerThreadJitState {
    pub(super) fn new() -> Self {
        Self {
            regs: RegFile::new(),
            hooks: None,
            rip_trace: [0; 32],
            rip_trace_i: 0,
            rip_trace_n: 0,
            iced_steps: 0,
            tlb: GenTlb::new(),
            tlb_hot_page: TLB_EMPTY,
            tlb_hot_ptr: std::ptr::null_mut(),
            tlb_hot_prot: 0,
            tlb_hot_gen: 0,
            sticky_page: [TLB_EMPTY; STICKY_WAYS],
            sticky_ptr: [std::ptr::null_mut(); STICKY_WAYS],
            sticky_prot: [0; STICKY_WAYS],
            sticky_gen: [0; STICKY_WAYS],
            sticky_rr: 0,
            pins: [MemPin::EMPTY; PIN_SLOTS],
            pins_gen: u64::MAX,
            chain_slots: Box::new([lower::ChainSlot::empty(); CHAIN_SLOTS]),
            edge_ic_va: [0; lower::EDGE_IC_SLOTS],
            edge_ic_fn: [0; lower::EDGE_IC_SLOTS],
            edge_ic_rr: 0,
            shadow_sp: 0,
            shadow_ret: [0; lower::SHADOW_DEPTH],
            opcode_sample_i: 0,
            pending_promote_thr: 0,
        }
    }
}
