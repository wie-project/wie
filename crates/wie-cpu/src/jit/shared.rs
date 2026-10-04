//! Process-wide compiled-code cache, guest memory, and the background
//! compiler worker pool.
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
use super::cache_persist::{LedgerProbe, PersistentJitCache, jit_cache_pe_hash};
use super::tier::{ChainTarget, OptTier, TierCounters, TierPlan};

// [verifier-rejection warn rate-limit] First REJECT_WARN_MAX rejections log at
// WARN; the tail logs at DEBUG so pathological guests don't spam the console.
const REJECT_WARN_MAX: usize = 50;
static REJECTION_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// First 5 M guest instructions run in boot mode: threshold = 1, no deferral
/// doubling, and bounded inline on deferred (2 per 10 ms token bucket).
pub(super) const BOOT_MODE_INSNS: u64 = 5_000_000;
use super::config::{BG_QUEUE_CAP, JitConfig};
use super::engine::{CodeCache, JitEngine, PersistedCode};
use super::fast_api::{FastApiKind, LargeFreeList};
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
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
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

/// One queued background-compile job.
///
/// `inv_gen` is the generation snapshot taken on the producing guest thread
/// BEFORE the job's bytes were decoded (see [`JitShared::bg_pool`]).
pub(super) struct BgJob {
    /// Guest entry VA of the block.
    pub(super) rip: u64,
    /// Decoded block classification, baked at enqueue time.
    kind: BlockKind,
    /// Bake-before-decode generation snapshot. Guards depend on it: baking
    /// an older-or-equal generation guarantees a guard mismatch whenever the
    /// baked bytes went stale.
    inv_gen: u64,
}

/// Mutex-protected work state for the background worker pool: two FIFO lanes
/// plus the pool shutdown flag.
///
/// **Ordering semantics (chosen):** [`Self::pop_job`] drains the urgent lane
/// FIFO first and only falls to the normal lane when no urgent job exists.
/// An URGENT job is therefore *started* before every NORMAL job that was
/// already queued at its enqueue time — and before every job enqueued after
/// it in either lane. This is a pop-order guarantee, not a global completion
/// order: with K workers, compiles run concurrently and completions
/// interleave across workers. It is also not fairness for the normal lane —
/// a sustained stream of urgents may starve normals; producers hitting the
/// [`BG_QUEUE_CAP`] depth bound fall back to inline compilation, which caps
/// how long any backlog can grow.
pub(super) struct BgQueue {
    /// Jobs whose producer blocks (or is about to block within one
    /// interpreted iteration) on this exact compile.
    urgent: VecDeque<BgJob>,
    /// Speculative prefetches nobody waits on directly.
    normal: VecDeque<BgJob>,
    /// Set by [`JitShared`]'s `Drop` under this mutex: parked workers wake,
    /// observe it, and exit. Producers can never race it — enqueueing
    /// requires a strong `Arc<JitShared>`, which defers the drop.
    shutdown: bool,
}

impl BgQueue {
    /// Pop discipline for workers: urgent lane first, then normal lane;
    /// each lane FIFO. See the type-level ordering contract.
    pub(super) fn pop_job(&mut self) -> Option<BgJob> {
        self.urgent.pop_front().or_else(|| self.normal.pop_front())
    }

    /// Append one job to its lane; `false` when the pool is shutting down or
    /// the combined depth already sits at [`BG_QUEUE_CAP`] (the caller then
    /// falls back to inline compilation).
    fn push(&mut self, job: BgJob, urgent: bool) -> bool {
        if self.shutdown || self.total_len() >= BG_QUEUE_CAP {
            return false;
        }
        if urgent {
            self.urgent.push_back(job);
        } else {
            self.normal.push_back(job);
        }
        true
    }

    /// Total queued jobs across both lanes (backpressure metric).
    fn total_len(&self) -> usize {
        self.urgent.len().saturating_add(self.normal.len())
    }
}

/// Shared worker-pool handle: the guarded queue plus its wakeup condvar.
///
/// Owned jointly by [`JitShared`] (which flags shutdown on drop) and each
/// worker thread (which captures an `Arc` clone). Deliberately OUTSIDE
/// [`JitShared`]: a worker must be able to park on the condvar while holding
/// only a `Weak<JitShared>` — parking would deadlock teardown if the condvar
/// lived inside `JitShared`, since keeping it alive would keep the strong
/// count nonzero forever.
pub(super) struct BgPool {
    /// Guarded queue state. Exposed `pub(super)` so JIT-module tests can lock
    /// it and exercise the real pop discipline directly.
    pub(super) q: Mutex<BgQueue>,
    work_available: Condvar,
}

impl BgPool {
    pub(super) fn new() -> Self {
        Self {
            q: Mutex::new(BgQueue {
                urgent: VecDeque::new(),
                normal: VecDeque::new(),
                shutdown: false,
            }),
            work_available: Condvar::new(),
        }
    }

    /// Enqueue one job and wake one worker; `false` when rejected (full /
    /// shutting down). The queue mutex is held only here — never during
    /// compilation — so producers never wait behind worker installs.
    pub(super) fn push(&self, rip: u64, kind: BlockKind, inv_gen: u64, urgent: bool) -> bool {
        let job = BgJob { rip, kind, inv_gen };
        let accepted = {
            let mut q = self.q.lock().unwrap();
            q.push(job, urgent)
        };
        if accepted {
            // One new job → wake exactly one parked worker. Workers batch-
            // drain, so a burst of pushes still costs few wakeups overall.
            self.work_available.notify_one();
        }
        accepted
    }
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
    /// Ready-block FuncIds for chaining, **tagged with the module (tier) that
    /// declared them** — a `FuncId` is only valid inside its own
    /// `JITModule`, so the tag is what keeps direct chaining same-tier-only.
    /// See [`Self::chain_map_for`].
    #[doc(hidden)]
    pub chain_ids: ConcurrentHashMap<u64, ChainTarget>,
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
    /// Background compiler worker pool: guarded two-lane queue + wakeup
    /// condvar, shared with every worker via `Arc`. Workers exit when this
    /// state drops (its `Drop` flags shutdown and wakes all parked workers),
    /// which requires the last strong `Arc<JitShared>` to go away — exactly
    /// like the former channel-disconnect teardown. `None` when the pool was
    /// never spawned or failed to spawn.
    ///
    /// Jobs carry `(rip, decoded kind, invalidate_gen snapshot)` inside
    /// [`BgJob`]: the generation was read on the guest thread BEFORE the
    /// bytes were decoded, so blocks compiled from pre-invalidation bytes
    /// never bake a newer generation over them (a high/stale guard miss must
    /// be impossible).
    pub bg_pool: Mutex<Option<Arc<BgPool>>>,
    /// Worker-pool size recorded at a successful spawn (`bg.workers` stat).
    /// Zero while no pool was ever started for this instance.
    pub bg_workers_spawned: AtomicUsize,
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
    /// Guest heap layout for the UCRT `malloc`/`free` fast path, as
    /// `[ctrl_va, base, end]` (see [`JitHeapLayout`](super::JitHeapLayout)).
    ///
    /// Written once by `JitCpu::configure_fast_path` at session init and read
    /// by every `run_compiled` on every engine, so it is `AtomicU64` here even
    /// though the per-frame copy on [`JitCtx`](super::lower::JitCtx) is a plain
    /// `u64`: the copy needs no atomicity (the frame owns the context
    /// exclusively), but the publish must not race a running frame. This is
    /// also the reason the layout is per-`JitShared` (per session) rather than
    /// a process global — see [`Self::bg_fast_api`] for the same pattern.
    pub heap: [AtomicU64; 3],
    /// Lock-free background-compile timing (worker writes, per-thread
    /// snapshots read via [`super::JitCpu::stats`]).
    pub bg_compile: BgCompileProfile,
    /// Approximate number of items currently queued or in flight on the
    /// background worker (incremented on enqueue, decremented per processed
    /// item). Backpressure signal: a deep queue raises the local promotion
    /// threshold instead of feeding a backlog guests will time out on.
    pub bg_queue_depth: AtomicU64,
    /// Per-VA opt-level decisions + the run's tier-up budget. One decision per
    /// VA, taken before the single compile it governs, so nothing is ever
    /// recompiled at a different level. See [`super::tier`].
    pub(super) tier_plan: Mutex<TierPlan>,
    /// Retired guest instructions (jit + iced) for boot-mode gating.
    pub(super) guest_insns: AtomicU64,
    /// Boot inline token bucket: `(window_start, used_in_window)` for the
    /// 2-per-10ms bounded inline pool during boot (thrash guard).
    pub(super) boot_inline_window: Mutex<(Instant, u32)>,
    /// Persistent disk code-cache ledger (`WIE_JIT_CACHE`). Always present;
    /// inert when disabled (`=0`, or default-off under `cfg(test)`). See
    /// [`super::cache_persist`] for why this stores metadata rather than
    /// machine-code bytes.
    pub(super) persist: Arc<PersistentJitCache>,
    /// Persisted machine code (`WIE_JIT_CODE_CACHE`), independent of
    /// [`Self::persist`] and **inert unless explicitly enabled**.
    pub(super) code_cache: Arc<CodeCache>,
    /// Test-only injection point for an explicitly enabled cache.
    /// `WIE_JIT_CODE_CACHE` is deliberately inert under `cfg(test)` (see
    /// [`JitConfig::code_cache_enabled`]), so the round-trip test installs a
    /// directory-scoped handle here instead of mutating the process environment.
    #[cfg(test)]
    code_cache_override: RwLock<Option<Arc<CodeCache>>>,
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
        // Tiering is armed only if a tier-up module was actually built (knob
        // on, budget non-zero, and the two opt levels differ). Disarmed, every
        // decision resolves to `Base` and the JIT behaves exactly as it did
        // before tiering existed.
        let tier_plan = TierPlan::new(
            has_engine
                .as_ref()
                .is_some_and(super::engine::JitEngine::has_tier_module),
            JitConfig::get().tier_budget(),
        );
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
            bg_pool: Mutex::new(None),
            bg_workers_spawned: AtomicUsize::new(0),
            bg_spawned: AtomicBool::new(false),
            bg_alive: Arc::new(AtomicBool::new(false)),
            cache_epoch: AtomicU64::new(0),
            invalidate_gen: AtomicU64::new(0),
            chain_epoch_bumps: AtomicU64::new(0),
            bg_compiles: AtomicU64::new(0),
            bg_fast_api: Mutex::new(Arc::from(Vec::new())),
            heap: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            bg_compile: BgCompileProfile::default(),
            bg_queue_depth: AtomicU64::new(0),
            tier_plan: Mutex::new(tier_plan),
            guest_insns: AtomicU64::new(0),
            boot_inline_window: Mutex::new((Instant::now(), 0)),
            persist: Arc::new(PersistentJitCache::new()),
            code_cache: Arc::new(CodeCache::new()),
            #[cfg(test)]
            code_cache_override: RwLock::new(None),
            #[cfg(test)]
            bg_force: AtomicBool::new(false),
        }
    }

    /// Whether the background path is enabled for this instance: env default
    /// (on for real runs, off under `cfg(test)`) or the test latch.
    pub(super) fn bg_enabled_here(&self) -> bool {
        JitConfig::get().bg_enabled() || self.bg_force_test()
    }

    // -- persistent disk-cache ledger (WIE_JIT_CACHE) -----------------------

    /// Attach (and bulk-load) the persistent JIT ledger for `pe_hash`,
    /// computed by the caller over the PE image file bytes via
    /// [`crate::jit_cache_pe_hash`]. Call once at session init, before guest
    /// execution. Seeds `Never` records straight into the live cache so warm
    /// boots skip re-decoding known-bad blocks immediately. No-op when the
    /// cache is disabled (`WIE_JIT_CACHE=0`).
    pub fn attach_pe_cache(&self, pe_hash: u64) {
        self.persist.attach(pe_hash);
        if self.code_cache().enabled() {
            let opt = JitConfig::get().opt_level();
            // The import list is a property of the live module, and it is part
            // of the code file's identity, so attach cannot happen without it.
            let mut guard = self.engine.lock().unwrap();
            if let Some(eng) = guard.as_mut() {
                self.code_cache().attach(pe_hash, opt, eng.base_mut());
            }
        }
        let cache = self.cache.pin();
        for va in self.persist.never_vas() {
            // Only from absence: a live Ready/Hot/Queued decision outranks it.
            if cache.get(&va).is_none() {
                cache.insert(va, CacheEntry::Never);
            }
        }
    }

    /// Hash of a PE image for [`Self::attach_pe_cache`] (re-exported at the
    /// crate root as [`crate::jit_cache_pe_hash`]).
    #[must_use]
    pub fn pe_cache_hash(pe_file_bytes: &[u8]) -> u64 {
        jit_cache_pe_hash(pe_file_bytes)
    }

    /// Warm-boot prewarm probe: get a usable block for `va`, and report its
    /// instruction count if one now exists.
    ///
    /// Consults the machine-code cache first and falls back to the metadata
    /// ledger, because a *restored* block is strictly better than a merely
    /// known-good one — the first is already-compiled code, the second only
    /// skips the hotness warm-up and still pays the compile.
    ///
    /// **This may install a block as a side effect** (that is the restore half),
    /// so the name says which cache it reaches for. The split out of
    /// [`Self::restore_from_code_cache`] was originally forced by write scope —
    /// the only call site lives in `pipeline.rs` — and completing the rename once
    /// that file was in scope is what this name is for.
    pub(super) fn restore_or_probe(&self, mem: &GuestMemory, va: u64) -> Option<LedgerProbe> {
        if let Some(restored) = self.restore_from_code_cache(mem, va) {
            return Some(LedgerProbe {
                insn_count: restored.insn_count,
            });
        }
        self.persist.probe(mem, va)
    }

    /// Replay persisted machine code for `va`, installing it as a Ready block on
    /// success.
    ///
    /// Installs the block it replays, so the caller's next dispatch finds a
    /// `Ready` entry instead of compiling. Returns the installed block.
    ///
    /// Lock order: guest memory → engine, which is the order the compile path
    /// already uses (`compile_from_kind_shared` releases its `mem.read()` before
    /// taking `engine`). The caller holds `mem.read()` for the duration of this
    /// call, so taking the engine lock here is consistent with — not inverted
    /// from — that order. Nothing takes the engine lock and then guest memory.
    fn restore_from_code_cache(&self, mem: &GuestMemory, va: u64) -> Option<CompiledBlock> {
        if !self.code_cache().enabled() {
            return None;
        }
        let live_gen = self.invalidate_gen.load(Ordering::Acquire);
        let cache = self.code_cache();
        let Some(code) = cache.probe(mem, va, live_gen) else {
            cache.note(cache.classify_miss(mem, va, live_gen));
            return None;
        };
        let tier = code.compiled_at_opt;
        let restored = {
            // `try_lock`, NOT `lock`: this runs on the guest thread inside the
            // dispatcher's miss path, and the same mutex is held for the whole
            // of every Cranelift compile. Blocking here puts a ~3 ms compile (or
            // much worse, a queue of them on a loaded host) in front of guest
            // execution — which is exactly backwards for a feature whose whole
            // purpose is cutting launch latency, and it starves the worker that
            // would otherwise release the lock.
            //
            // Losing the race costs one ordinary compile of this block on this
            // visit; the blob is still in the table, so the next dispatch
            // restores it. That is the right trade: a redundant compile is
            // bounded and rare, a stalled guest thread is neither.
            let Ok(mut guard) = self.engine.try_lock() else {
                self.code_cache().note_restore_deferred();
                return None;
            };
            let eng = guard.as_mut()?;
            let module = eng.module_for(tier)?;
            module.define_persisted(&code)
        };
        let (func_id, ptr) = match restored {
            Ok(v) => v,
            Err(outcome) => {
                self.code_cache().note(outcome);
                return None;
            }
        };
        // SAFETY: `define_persisted` returned a pointer from
        // `JITModule::get_finalized_function`, i.e. the entry point of a blob
        // cranelift-jit allocated in executable memory with the same
        // `(ctx_ptr) -> ()` signature `lower::compile_block` produces. The
        // module (and therefore the memory) outlives every `CompiledBlock`,
        // which is only dropped with the `JitShared` that owns the engine.
        #[expect(unsafe_code)]
        let func: unsafe extern "C" fn(*mut lower::JitCtx) = unsafe { std::mem::transmute(ptr) };
        self.code_cache().note_restored(code.code.len());
        let block = CompiledBlock {
            func,
            func_id: Some(func_id),
            tier,
            insn_count: code.insn_count,
            guest_start: code.va,
            guest_end: code.guest_end,
            inv_gen: live_gen,
        };
        // Install through the normal path so `chain_ids`, `code_pages` and the
        // Ready entry are all populated exactly as they would be for a compile.
        self.insert_ready(va, block);
        Some(block)
    }

    /// Persist one block's emitted code, when the capture for it was available.
    ///
    /// `try_lock` on purpose: this runs on the install path, which the compile
    /// path may still hold the engine for on another thread. Skipping the blob
    /// is a pure perf loss (the next run recompiles one block); blocking would
    /// put a disk-format concern on the critical path of every block install.
    fn record_code(&self, compiled: &CompiledBlock) {
        if !self.code_cache().enabled() {
            return;
        }
        let Some(func_id) = compiled.func_id else {
            return;
        };
        let Ok(mut guard) = self.engine.try_lock() else {
            return;
        };
        let captured = {
            let Some(eng) = guard.as_mut() else {
                return;
            };
            let Some(module) = eng.module_for(compiled.tier) else {
                return;
            };
            module.take_captured(func_id)
        };
        let Some(captured) = captured else {
            return;
        };
        let mem = self.mem.read().unwrap();
        let Some(bytes_hash) =
            super::cache_persist::hash_guest_range(&mem, compiled.guest_start, compiled.guest_end)
        else {
            return;
        };
        self.code_cache().record(PersistedCode {
            va: compiled.guest_start,
            guest_end: compiled.guest_end,
            bytes_hash,
            insn_count: compiled.insn_count,
            inv_gen: compiled.inv_gen,
            compiled_at_opt: compiled.tier,
            align: captured.align,
            code: captured.bytes,
            relocs: captured.relocs,
        });
    }

    /// Snapshot of the code-cache counters; see [`CodeCache`].
    ///
    /// The field-filling half of the profile dump. The printing half belongs to
    /// `JitCpu::stats` in `pipeline.rs`, which is outside this change's write
    /// scope: fold `code_cache_restored` / `code_cache_refused` /
    /// `code_cache_unpersistable` from [`JitProfile`](super::profile::JitProfile)
    /// here and print them in `diag.rs`.
    pub(super) fn code_cache_counters(&self) -> super::engine::CodeCacheCounts {
        self.code_cache().counters().snapshot()
    }

    /// One-line code-cache hit rate, or `None` when `WIE_JIT_CODE_CACHE` is off.
    #[cfg(test)]
    pub(super) fn code_cache_summary(&self) -> Option<String> {
        self.code_cache().summary()
    }

    /// The code cache this process should use.
    ///
    /// An `Arc` clone rather than a borrow so the test override below can hand
    /// back a handle that outlives the guard it came from. Cloning an `Arc` is
    /// one atomic increment, and this is called at most once per block install
    /// and once per dispatch-miss probe — never on a block execution.
    fn code_cache(&self) -> Arc<CodeCache> {
        #[cfg(test)]
        if let Some(injected) = self
            .code_cache_override
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return injected;
        }
        Arc::clone(&self.code_cache)
    }

    /// Test-only: name of host import declaration `index` in the live module.
    #[cfg(test)]
    pub(super) fn code_import_name(&self, index: u32) -> Option<String> {
        let guard = self.engine.lock().expect("engine lock");
        let eng = guard.as_ref()?;
        eng.base.import_name_at(index).map(str::to_owned)
    }

    /// Flush the JIT's persistent caches and report their counters, now.
    ///
    /// The deterministic end-of-session hook for `WIE_JIT_CODE_CACHE`. It is a
    /// **method, not a `Drop`**, because the last `Arc<JitShared>` does not
    /// reliably drop at the end of a guest session: measured over five runs
    /// each, `Arc<JitShared>` was still at strong-count 1 inside
    /// `RuntimeSession::drop` for `crt_hello` and `write_file` (0 for
    /// `gui_blit`), so a `Drop`-based flush silently never ran for those guests
    /// and their warm boots stayed permanently cold. Whatever still holds that
    /// reference outlives the session, and chasing it is a losing game — the
    /// owner of the lifetime should decide when the data is durable.
    ///
    /// Idempotent and cheap to call more than once: a flush with nothing
    /// recorded since the last write is a no-op, and the counters are reported
    /// exactly once. `JitShared`'s own `Drop` remains a backstop for paths that
    /// skip session teardown entirely.
    ///
    /// Safe to call on a shared backend with live guest threads only after they
    /// have been joined: a worker that records a blob after the flush will have
    /// it written by the next flush, never lost.
    pub fn finish_jit_caches(&self) {
        self.code_cache().finish();
    }

    /// Test-only: the code-cache handle itself, so a test can hold a second
    /// strong reference and prove the flush does not depend on the last one
    /// going away.
    #[cfg(test)]
    pub(super) fn code_cache_for_test(&self) -> Arc<CodeCache> {
        self.code_cache()
    }

    /// Test-only: install (or clear) the code cache this process should use.
    #[cfg(test)]
    pub(super) fn set_code_cache(&self, cache: Option<Arc<CodeCache>>) {
        *self
            .code_cache_override
            .write()
            .unwrap_or_else(|e| e.into_inner()) = cache;
    }

    /// Record one Ready install (`inline` and worker paths) into the ledger.
    pub(super) fn persist_record_ready(&self, compiled: &CompiledBlock) {
        self.record_code(compiled);
        let mem = self.mem.read().unwrap();
        self.persist.record_ready(
            &mem,
            compiled.guest_start,
            compiled.guest_end,
            compiled.insn_count,
            compiled.inv_gen,
        );
    }

    /// Record a `Never` verdict into the ledger (fixed-window hash).
    pub(super) fn persist_record_never(&self, va: u64) {
        if !self.persist_active() {
            return;
        }
        let mem = self.mem.read().unwrap();
        self.persist.record_never(&mem, va);
    }

    /// Ledger invalidation over `[addr, addr+len)` (SMC / X-loss / writes).
    pub(super) fn persist_invalidate_range(&self, addr: u64, end: u64) {
        let len = usize::try_from(end.saturating_sub(addr)).unwrap_or(usize::MAX);
        self.persist.invalidate_range(addr, len);
        // The code cache drops overlapping blobs eagerly rather than waiting
        // for the byte-hash guard to catch them: the guard is the correctness
        // net, but keeping a dead blob would make every later probe of that VA
        // pay a failed hash for the rest of the process.
        self.code_cache().invalidate_range(addr, end);
    }

    /// Ledger clear: `full = true` purges everything (guest-triggered full
    /// flushes); `full = false` keeps loaded/learned entries valid (session
    /// init clears before execution — probes stay hash-validated regardless).
    pub(super) fn persist_clear(&self, full: bool) {
        self.persist.clear_all(full);
    }

    /// Whether persistence is enabled AND attached to a PE.
    ///
    /// Either cache counts: the metadata ledger and the machine-code cache are
    /// independently switchable, and the dispatcher's warm-boot probe is the one
    /// seam that consults both. Gating on only the ledger would make
    /// `WIE_JIT_CODE_CACHE` silently inert whenever `WIE_JIT_CACHE=0`.
    pub(super) fn persist_active(&self) -> bool {
        self.persist.active_pe() != 0 || self.code_cache().attached()
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

    /// Whether the guest is still in boot mode (first 5 M insns).
    #[inline]
    pub(super) fn is_boot_mode(&self) -> bool {
        if cfg!(test) {
            return false;
        }
        self.guest_insns.load(Ordering::Relaxed) < BOOT_MODE_INSNS
    }

    /// Record retired guest instructions (jit + iced) for boot gating.
    #[inline]
    pub(super) fn record_guest_insns(&self, n: u64) {
        if n != 0 {
            self.guest_insns.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Bounded inline token bucket during boot: 2 inline compiles per 10 ms.
    ///
    /// Guards against thrash when every deferred promotion would otherwise
    /// inline-compile. Outside boot mode it always returns `false` (no extra
    /// inline budget).
    pub(super) fn try_acquire_boot_inline_token(&self) -> bool {
        if !self.is_boot_mode() {
            return false;
        }
        let now = Instant::now();
        let mut guard = self.boot_inline_window.lock().unwrap();
        if now.duration_since(guard.0) >= Duration::from_millis(10) {
            *guard = (now, 1);
            return true;
        }
        if guard.1 < 2 {
            guard.1 += 1;
            return true;
        }
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
                self.chain_ids.pin().insert(
                    rip,
                    ChainTarget {
                        func_id: fid,
                        tier: compiled.tier,
                    },
                );
            }
            // Inline installs are visible to other threads' delta-resyncs too.
            self.recent_installs.lock().unwrap().push(rip);
        }
        self.code_pages_add_range(compiled.guest_start, compiled.guest_end);
        self.cache.pin().insert(rip, CacheEntry::Ready(compiled));
        self.persist_record_ready(&compiled);
    }

    /// Clone of the live pool handle (`None` before spawn / after a failed
    /// spawn). Callers hold their own `Arc<JitShared>` while using it, so the
    /// pool cannot be torn down mid-push.
    pub(super) fn bg_pool_arc(&self) -> Option<Arc<BgPool>> {
        self.bg_pool.lock().unwrap().clone()
    }

    /// Spawn the background compiler worker pool exactly once.
    ///
    /// K workers ([`JitConfig::jit_workers`], `WIE_JIT_WORKERS`) share one
    /// [`BgPool`]. Each thread is detached (no stored join handle): it holds
    /// a `Weak` to this shared state and exits when this state drops — the
    /// `Drop` impl flags shutdown and wakes every parked worker. This avoids
    /// a self-join if a worker happens to hold the final strong reference
    /// while a job runs.
    ///
    /// `bg_alive` reflects "any worker alive": set once ≥ 1 worker spawned;
    /// the last exiting worker clears it. Partial spawn failure (resource
    /// limits) keeps the successfully spawned subset; total failure reverts
    /// the spawn latch so callers fall back to inline compilation and a later
    /// call can retry.
    pub(crate) fn ensure_bg_worker(self: &Arc<Self>) {
        if self.bg_spawned.swap(true, Ordering::AcqRel) {
            return;
        }
        let k = JitConfig::get().jit_workers();
        let pool = Arc::new(BgPool::new());
        *self.bg_pool.lock().unwrap() = Some(Arc::clone(&pool));
        // Live-worker countdown: the last exiting worker clears `bg_alive`
        // so guests stop waiting on a dead pool and inline-compile instead.
        let remaining = Arc::new(AtomicUsize::new(k));
        let alive = Arc::clone(&self.bg_alive);
        let mut spawned = 0_usize;
        for i in 0..k {
            let weak = Arc::downgrade(self);
            let pool = Arc::clone(&pool);
            let remaining = Arc::clone(&remaining);
            let alive = Arc::clone(&alive);
            let name = format!("wie-jit-bg-{i}");
            let spawned_ok = std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    Self::bg_worker_main(&weak, &pool);
                    if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                        alive.store(false, Ordering::Release);
                    }
                })
                .is_ok();
            if spawned_ok {
                if spawned == 0 {
                    // First live worker: open the gate NOW so concurrent
                    // producers queue instead of inline-compiling while the
                    // rest of the pool warms up.
                    self.bg_alive.store(true, Ordering::Release);
                }
                spawned = spawned.saturating_add(1);
            } else {
                break;
            }
        }
        if spawned > 0 {
            // The pool is up (workers can only exit after this state drops,
            // which requires the shared state gone — impossible while we
            // hold `self`). Guests may enqueue immediately.
            self.bg_workers_spawned.store(spawned, Ordering::Relaxed);
            // Compensate the live-worker countdown for workers that failed
            // to spawn so it reaches zero exactly when the real workers do
            // (otherwise a partially spawned pool could never clear
            // `bg_alive`).
            let never_spawned = k - spawned;
            if never_spawned > 0 {
                remaining.fetch_sub(never_spawned, Ordering::AcqRel);
            }
        } else {
            // Spawn failed entirely (resource limits): revert so callers fall
            // back to inline compilation and a later call can retry.
            self.bg_spawned.store(false, Ordering::Release);
            self.bg_pool.lock().unwrap().take();
        }
    }

    /// Worker main loop: pick jobs urgent-first, compile, install Ready
    /// entries, fire completion tokens. Exits when this shared state drops
    /// (its `Drop` sets the shutdown flag under the queue mutex and wakes
    /// every parked worker).
    ///
    /// Batch-drain: after each blocking wait, the loop pulls EVERY item
    /// already queued before sleeping again, so a promotion burst costs one
    /// wakeup instead of one wakeup per block (semantics preserved from the
    /// single-worker channel design).
    ///
    /// Lock discipline: the queue mutex is held ONLY to select jobs — never
    /// during compilation — so no path holds it across an install and
    /// workers cannot deadlock against producers or each other.
    fn bg_worker_main(shared: &Weak<Self>, pool: &BgPool) {
        let mut guard = pool.q.lock().unwrap();
        loop {
            // Block until a job arrives or the pool shuts down. The flag is
            // read under the same mutex `JitShared::drop` writes it under,
            // so there is no lost-wakeup window between check and park.
            let first = loop {
                if let Some(job) = guard.pop_job() {
                    break job;
                }
                if guard.shutdown {
                    return;
                }
                guard = pool.work_available.wait(guard).unwrap();
            };
            drop(guard);
            // One upgrade per batch: holding a strong ref for the whole
            // batch guarantees teardown (drop → shutdown flag) cannot land
            // mid-drain. An upgrade failure here means producers and waiters
            // are already gone (both need strong refs), so dropping the job
            // loses nothing observable.
            let Some(live) = shared.upgrade() else {
                return;
            };
            live.bg_process_one(first);
            // Batch-drain: pull every currently-queued item before blocking
            // again so a promotion burst costs one wakeup, not one per block.
            guard = loop {
                let mut g = pool.q.lock().unwrap();
                match g.pop_job() {
                    Some(job) => {
                        drop(g);
                        live.bg_process_one(job);
                    }
                    None => break g, // queue dry; keep the lock for phase 1
                }
            };
        }
    }

    /// Compile + install one queued job (shared by the blocking and
    /// batch-drain paths). Decrements [`Self::bg_queue_depth`] exactly once.
    /// `inv_gen` is the generation snapshot taken before the job's bytes were
    /// decoded (see [`Self::bg_pool`]).
    fn bg_process_one(&self, job: BgJob) {
        self.bg_queue_depth.fetch_sub(1, Ordering::Relaxed);
        let BgJob { rip, kind, inv_gen } = job;
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
    ///
    /// This is also where the block's opt-level tier is decided — once per
    /// VA, before the one compile it governs (see [`TierPlan`]). A tier
    /// compile the verifier rejects is retried once at the base level and the
    /// decision is downgraded permanently.
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
                        // Hand-written code belongs to no Cranelift module, so it
                        // carries no opt level: `Base` is the honest tag, and it
                        // keeps the ledger record on the base file.
                        tier: OptTier::Base,
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
                // Tier decision: pre-compile, from block shape, memoised per VA.
                let mut tier = self.decide_tier(rip, block::term_is_self_loop(term.as_ref(), rip));
                let mut eng_guard = self.engine.lock().unwrap();
                loop {
                    // Chain-id snapshot for THIS module only (pin guard must not
                    // outlive the snapshot — the engine lock and `compile_block`
                    // run after it drops).
                    let chain_map: HashMap<u64, cranelift_module::FuncId> = if chain_on {
                        self.chain_map_for(tier)
                    } else {
                        HashMap::new()
                    };
                    let outcome =
                        eng_guard
                            .as_mut()
                            .and_then(|eng| eng.module_for(tier))
                            .map(|module| {
                                compile_block(
                                    module, rip, &insns, end_rip, term, call_fast, &chain_map,
                                    bytes_len, inv_gen, tier,
                                )
                            });
                    let Some(outcome) = outcome else {
                        // No module for this tier (unreachable: a Speed decision
                        // implies a tier module). Never silently retarget.
                        tracing::warn!(
                            rip = format_args!("{rip:#x}"),
                            "jit tier module missing for a tier-up decision — \
                             block falls back to the interpreter"
                        );
                        return None;
                    };
                    match outcome {
                        Ok(c) => {
                            if tier == OptTier::Speed {
                                // File the ledger record under the tier key.
                                self.persist.mark_tiered(rip);
                            }
                            return Some(c);
                        }
                        Err(e) => {
                            // Verifier/codegen rejection: the block's IR failed a
                            // lowering or verification pass. Surface it loudly,
                            // then fall back to interpretation for this block.
                            // Rate-limited: first REJECT_WARN_MAX rejections at
                            // WARN (unique enough to triage), the tail at DEBUG.
                            let n =
                                REJECTION_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let msg_args =
                                (format_args!("{rip:#x}"), bytes_len, n + 1, e.to_string());
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
                            if let Some(eng) = eng_guard.as_mut()
                                && let Some(module) = eng.module_for(tier)
                            {
                                module.module.clear_context(&mut module.ctx);
                            }
                            if tier == OptTier::Speed {
                                // `speed` code the verifier rejects is not
                                // evidence the base level will accept, but the
                                // base level is what the guest ran before tiering
                                // existed: retry there once, and remember the
                                // downgrade so no later compile of this VA tries
                                // the tier again.
                                if self.tier_downgrade_after_reject(rip) {
                                    tier = OptTier::Base;
                                    continue;
                                }
                            }
                            return None;
                        }
                    }
                }
            }
            BlockKind::NotPure => None,
        }
    }

    /// The tier `rip` compiles at: one memoised decision per VA, taken before
    /// the single compile it governs (so no block is ever compiled twice at two
    /// levels) and charged against the run's tier-up budget at decision time.
    pub(super) fn decide_tier(&self, rip: u64, is_self_loop: bool) -> OptTier {
        let mut plan = self.tier_plan.lock().unwrap_or_else(|e| e.into_inner());
        plan.decide(rip, is_self_loop)
    }

    /// Record a rejected tier compile; see [`TierPlan::downgrade_after_reject`].
    fn tier_downgrade_after_reject(&self, rip: u64) -> bool {
        let mut plan = self.tier_plan.lock().unwrap_or_else(|e| e.into_inner());
        plan.downgrade_after_reject(rip)
    }

    /// Tier-up ledger for the profile report (see [`JitCpu::stats`](super::JitCpu::stats)).
    pub(super) fn tier_counters(&self) -> TierCounters {
        self.tier_plan
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counters()
    }

    /// Direct-chaining `FuncId` snapshot for a compile running in `tier`'s
    /// module — **SAME-TIER ONLY**, and the single place that decides it.
    ///
    /// `Module::declare_func_in_func` indexes the *declaring* module's own
    /// `compiled_functions`, so an id from the other tier's module would either
    /// panic or silently name a different function and emit a call to the wrong
    /// address. A cross-tier successor is therefore never offered to it: the
    /// edge falls back to the late-bound chain-table hop or the dispatcher,
    /// both of which were already the supported fallback paths and are covered
    /// by the chaining / edge-IC / dispatcher tests.
    pub(super) fn chain_map_for(&self, tier: OptTier) -> HashMap<u64, cranelift_module::FuncId> {
        let guard = self.chain_ids.pin();
        guard
            .iter()
            .filter(|(_, t)| t.tier == tier)
            .map(|(&va, t)| (va, t.func_id))
            .collect()
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
                self.chain_ids.pin().insert(
                    rip,
                    ChainTarget {
                        func_id: fid,
                        tier: compiled.tier,
                    },
                );
            }
            self.code_pages_add_range(compiled.guest_start, compiled.guest_end);
            cache.insert(rip, CacheEntry::Ready(compiled));
            notify
        };
        // Record the install for delta-resync BEFORE bumping the epoch: a
        // thread that observes the new epoch is guaranteed to find the VA in
        // its drain (install-order lock discipline).
        self.recent_installs.lock().unwrap().push(rip);
        self.persist_record_ready(&compiled);
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
                    // Negative ledger entry: warm boots probe as KnownNever.
                    self.persist_record_never(rip);
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

impl Drop for JitShared {
    /// Flag pool shutdown and wake every parked worker so they exit promptly.
    ///
    /// Runs only at strong-count zero, so no producer can race the flag
    /// (enqueueing requires a strong ref), and any worker mid-batch holds a
    /// strong ref that defers this drop until its batch completes. Workers
    /// clear `bg_alive` themselves as they exit.
    fn drop(&mut self) {
        let Some(pool) = self.bg_pool.lock().unwrap().take() else {
            return; // never spawned (or failed-spawn revert already ran)
        };
        {
            // Write the flag under the queue mutex: a worker either reads it
            // before parking (this order) or is woken by the notify below.
            // No lost-wakeup window exists between check and park.
            let mut q = pool.q.lock().unwrap();
            q.shutdown = true;
        }
        pool.work_available.notify_all();
    }
}

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
    /// Host-side large free list for the UCRT `malloc`/`free` fast path when
    /// size > [`LARGE_THRESHOLD`](crate::guest_layout::LARGE_THRESHOLD), as
    /// `(payload_va, size)` pairs.
    ///
    /// Per ENGINE, not per process. The guest control block only stores
    /// size-class heads, so large blocks need a host list (the same role as
    /// `GuestHeap::large_free` on the WinAPI path) — but a process-wide one is
    /// wrong for two reasons: its VAs are heap-region relative, so a second
    /// session's `configure_fast_path` would have to clear the first session's
    /// live blocks, and clearing them re-bump-allocates memory that is still
    /// handed out. Being per-engine also removes the `Mutex` from the
    /// large-alloc/free path, which a `static` required. No `Sync` needed: the
    /// list is reached only from this engine's own `run_compiled` frames, and
    /// the engine is owned by exactly one host thread.
    pub large_free: LargeFreeList,
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
            large_free: LargeFreeList::new(),
        }
    }
}
