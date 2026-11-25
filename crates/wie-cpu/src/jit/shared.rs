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
use super::engine::{
    CodeCache, CodeRestoreOutcome, JitEngine, PersistedCode, classify_capture_relocs,
};
use super::fast_api::{FastApiKind, LargeFreeList};
use super::lower::err::LowerError;
use super::lower::{self, CHAIN_SLOTS, CompiledBlock, JitCtx, compile_block};
use super::pipeline::{NO_STALL_SKIP_CAP, resolve_thunk_va};
use super::profile::BgCompileProfile;
use super::profile::{PROFILE_BUCKETS, TimeBuckets};
use super::trampolines::match_micro_stub;
use crate::exec::HookWindow;
use crate::lock::{lock_unpoisoned, read_unpoisoned};
use crate::mem::GuestMemory;
use crate::regs::RegFile;
use crate::{ConcurrentHashMap, new_concurrent_map};
use ahash::HashMap;
use ahash::HashMapExt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};
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
    /// Visits this thread has spent interpreting this block while its compile
    /// was in flight (see the dispatch path's **no-stall rule**,
    /// [`JitCpu::step_one`](super::pipeline)).
    ///
    /// This is the liveness bound, and it is deliberately *work*, not elapsed
    /// time. An earlier version bounded the skip by the job's age and it was
    /// wrong for the exact case the rule exists for: at session init the
    /// runtime queues one speculative compile per planted guest stub (124 of
    /// them on `crt_hello`), so by the time the guest reaches stub #100 its
    /// job is 40 ms old — an age bound reads that as "the job is lost" when it
    /// is only *queued behind 99 peers*. Counting interpreted visits measures
    /// the thing we actually want to cap: how much work we are willing to do
    /// before insisting the pool answer.
    ///
    /// Every resolution of a queued job replaces or removes the entry
    /// (`bg_install_ready` → `Ready`, `bg_install_never` → `Never`,
    /// `bg_install_drop` → *absent*), so a healthy pool resolves long before
    /// the cap. The cap only fires for a job the pool lost — a worker that
    /// panicked mid-compile while its peers kept `bg_alive` true — and then
    /// the wait path's timeout inline-compiles the block, resolving the entry.
    skips: AtomicU32,
}

impl BgWaitCell {
    pub(super) fn new(thr: u32) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            rx: Mutex::new(Some(rx)),
            thr: AtomicU32::new(thr),
            skips: AtomicU32::new(0),
        })
    }

    /// Record one visit interpreted instead of waiting; `true` when the skip
    /// budget is spent and the caller should take the bounded wait instead.
    pub(super) fn note_skip(&self) -> bool {
        self.skips.fetch_add(1, Ordering::Relaxed) >= NO_STALL_SKIP_CAP
    }

    /// Fire the one-shot completion token (worker install/fail/drop paths).
    ///
    /// Idempotent: a second call finds the sender taken and does nothing.
    /// `pub(super)` so JIT-module tests can fire tokens directly.
    pub(super) fn notify_all(&self) {
        if let Some(tx) = lock_unpoisoned(&self.tx).take() {
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
        let Some(rx) = lock_unpoisoned(&self.rx).take() else {
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
            let mut q = lock_unpoisoned(&self.q);
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
    /// Stable per-instance id, assigned at construction (`0` for the first
    /// engine in the process). Diagnostic only: the engine-lock split keys its
    /// tally by it so a multi-engine run stays attributable instead of being
    /// summed into one number that no `compile_us` can be compared against.
    pub(super) split_id: u64,
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
    /// See [`Self::chain_func_for`].
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
    /// Bumped on every mutation of `pending_code_writes`.
    ///
    /// `drain_pending_code_writes` runs once per dispatch entry (~1.5 M/s on a
    /// real guest) and only cares when the set is non-empty or the overflow flag
    /// moved — both of which are rare. Without this counter the common case had
    /// to take the set's mutex to find out it was empty; with it the entry path
    /// is one relaxed load, and the lock is taken only by a thread that has
    /// actually seen a new generation.
    pub pending_code_gen: AtomicU64,
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
    /// Test-only latch forcing **loop-region** compilation on for this instance.
    ///
    /// Same contract and the same reason as [`Self::bg_force`]: `JitConfig` is a
    /// process-wide `OnceLock` read once per process, so a unit test cannot flip
    /// `WIE_JIT_REGION` at run time — and must not try, because the whole
    /// `wie-cpu` suite shares one process and a global flip would silently change
    /// the shape of other tests' blocks. Per-`JitShared` keeps it local to the
    /// engine the test opened.
    #[cfg(test)]
    pub(super) region_force: AtomicBool,
    /// Test-only latch for **one-block self-loop** regions, separate from
    /// [`Self::region_force`] because the two are separately gated in shipping
    /// builds: `WIE_JIT_REGION` and `WIE_JIT_REGION_SELF_LOOP`. A test that wants
    /// the self-loop emitter must ask for it by name, which is also what makes
    /// the gate itself testable (turn it off with `region_force` on and the same
    /// RIP must come out a block).
    #[cfg(test)]
    pub(super) region_self_loop_force: AtomicBool,
}

// ---------------------------------------------------------------------------
// Engine-lock wait vs hold, keyed per engine (`WIE_RUNTIME_PROFILE`).
//
// `compile_us` and every `jit_compile_cost` bucket are timed *around* the
// compile call, which acquires `self.engine` inside it — so a worker that
// queues behind others bills its queueing as compile work. Measured on DOOM
// Retro over a fixed 40 s window (compile counts within 8 %): the per-compile
// mean is 849 us (median of three) at `WIE_JIT_WORKERS=1` and 2 952 us at 4, a
// 3.48x inflation of the metric for the same work, and the uncontended figure
// agrees with the isolated single-threaded floor for a five-instruction block
// (447 us, `pipeline::tests::compile_cost_floor_split`).
//
// `wait_us` is time waiting to acquire the engine mutex; `hold_us` is time
// holding it. Each is recorded against the owning engine's `split_id`, because a
// process can hold more than one `JitShared` (the `open_x86_64` fixtures do, and
// any second VM does) while the profile's `compile_us` is primary-engine only —
// summing engines would produce a total nothing can be compared against. The
// report pairs every engine's `wait + hold` with that same engine's own
// `bg_compile.compile_us`, so "these add up to more than the compile time they
// are supposed to be part of" is visible per row instead of being hidden by the
// sum.
// ---------------------------------------------------------------------------

/// Which `JitShared` instance was created first in this process.
static NEXT_SPLIT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Default, Clone)]
struct EngineSplit {
    acquisitions: u64,
    wait_us: u64,
    hold_us: u64,
    /// This engine's own `bg_compile.compile_us`, sampled at the last record.
    compile_us: u64,
    bucket_wait_us: [u64; PROFILE_BUCKETS],
    bucket_hold_us: [u64; PROFILE_BUCKETS],
    bucket_count: [u64; PROFILE_BUCKETS],
}

static LOCK_SPLIT: OnceLock<Mutex<HashMap<u64, EngineSplit>>> = OnceLock::new();

fn lock_split_on() -> bool {
    super::diag::profile_gate_on()
}

fn lock_split_tally() -> &'static Mutex<HashMap<u64, EngineSplit>> {
    LOCK_SPLIT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one acquisition of `engine`'s compile lock: `wait` before it was
/// granted, `compile_us` as that engine's running bg-compile total.
fn lock_split_note_wait(engine: u64, compile_us: u64, insns: u64, wait: Duration) {
    if !lock_split_on() {
        return;
    }
    let us = u64::try_from(wait.as_micros()).unwrap_or(u64::MAX);
    let i = TimeBuckets::bucket_for(insns);
    if let Ok(mut g) = lock_split_tally().lock() {
        let e = g.entry(engine).or_default();
        e.acquisitions = e.acquisitions.saturating_add(1);
        e.wait_us = e.wait_us.saturating_add(us);
        e.compile_us = compile_us;
        if let Some(b) = e.bucket_wait_us.get_mut(i) {
            *b = b.saturating_add(us);
        }
        if let Some(c) = e.bucket_count.get_mut(i) {
            *c = c.saturating_add(1);
        }
    }
}

/// Record one release of `engine`'s compile lock.
fn lock_split_note_hold(engine: u64, compile_us: u64, insns: u64, hold: Duration) {
    if !lock_split_on() {
        return;
    }
    let us = u64::try_from(hold.as_micros()).unwrap_or(u64::MAX);
    let i = TimeBuckets::bucket_for(insns);
    if let Ok(mut g) = lock_split_tally().lock() {
        let e = g.entry(engine).or_default();
        e.hold_us = e.hold_us.saturating_add(us);
        e.compile_us = compile_us;
        if let Some(b) = e.bucket_hold_us.get_mut(i) {
            *b = b.saturating_add(us);
        }
    }
}

/// RAII: charges the engine-lock hold from construction to drop, so every exit
/// path from the compile — including the early `return None` — is accounted.
struct LockHold<'a> {
    shared: &'a JitShared,
    insns: u64,
    start: Instant,
}

impl<'a> LockHold<'a> {
    fn new(shared: &'a JitShared, insns: u64) -> Self {
        Self {
            shared,
            insns,
            start: Instant::now(),
        }
    }
}

impl Drop for LockHold<'_> {
    fn drop(&mut self) {
        let compile_us = self.shared.bg_compile.compile_us.load(Ordering::Relaxed);
        lock_split_note_hold(
            self.shared.split_id,
            compile_us,
            self.insns,
            self.start.elapsed(),
        );
    }
}

/// One engine's row, plus the aggregate buckets, for rendering.
#[derive(Default)]
struct LockSplitSnapshot {
    engines: Vec<(u64, u64, u64, u64, u64)>,
    buckets: Vec<(u64, u64, u64, u64)>,
}

/// Render: one row per engine, then the aggregate per-bucket means.
fn lock_split_lines(snap: &LockSplitSnapshot) -> Vec<String> {
    if snap.engines.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for &(id, acq, wait, hold, compile_us) in &snap.engines {
        let share = if wait.saturating_add(hold) == 0 {
            0.0
        } else {
            #[expect(clippy::cast_precision_loss, reason = "report percentage only")]
            {
                100.0 * (wait as f64) / (wait.saturating_add(hold) as f64)
            }
        };
        // The self-check: `wait + hold` is a subset of this engine's own
        // `compile_us` (which also contains the pre-lock work), so anything
        // else means the two are measuring different sets and the row must
        // say so rather than be quietly compared.
        let span = wait.saturating_add(hold);
        let consistent = if compile_us == 0 {
            "n/a"
        } else if span <= compile_us {
            "yes"
        } else {
            "NO"
        };
        out.push(format!(
            "[wie] jit_compile_lock: engine {id}: acquisitions={acq} wait_us={wait} \
             hold_us={hold} (per acquisition wait={} us hold={} us, wait_share={share:.1}%) \
             engine_compile_us={compile_us} subset_of_compile_us={consistent}",
            wait.checked_div(acq).unwrap_or(0),
            hold.checked_div(acq).unwrap_or(0)
        ));
    }
    let mut rows: Vec<String> = snap
        .buckets
        .iter()
        .filter(|(_, _, _, count)| *count > 0)
        .map(|(lo, bw, bh, count)| {
            format!(
                "[wie] jit_compile_lock:   insns>={lo}: wait={bw} us hold={bh} us n={count} \
                 (mean wait={} hold={})",
                bw.checked_div(*count).unwrap_or(0),
                bh.checked_div(*count).unwrap_or(0)
            )
        })
        .collect();
    out.append(&mut rows);
    out
}

/// The engine-lock split as report lines; empty when nothing was recorded.
pub(super) fn lock_split_report_lines() -> Vec<String> {
    if !lock_split_on() {
        return Vec::new();
    }
    let Some(tally) = LOCK_SPLIT.get() else {
        return Vec::new();
    };
    let Ok(g) = tally.lock() else {
        return Vec::new();
    };
    let mut engines: Vec<(u64, u64, u64, u64, u64)> = g
        .iter()
        .map(|(id, e)| (*id, e.acquisitions, e.wait_us, e.hold_us, e.compile_us))
        .collect();
    engines.sort_unstable_by_key(|r| r.0);
    let mut buckets = Vec::new();
    for i in 0..PROFILE_BUCKETS {
        let mut wait = 0_u64;
        let mut hold = 0_u64;
        let mut count = 0_u64;
        for e in g.values() {
            wait = wait.saturating_add(e.bucket_wait_us.get(i).copied().unwrap_or(0));
            hold = hold.saturating_add(e.bucket_hold_us.get(i).copied().unwrap_or(0));
            count = count.saturating_add(e.bucket_count.get(i).copied().unwrap_or(0));
        }
        buckets.push((1_u64 << i, wait, hold, count));
    }
    lock_split_lines(&LockSplitSnapshot { engines, buckets })
}

impl JitShared {
    /// Copy of installs at index `from..len` plus the new watermark (`len`).
    ///
    /// Append-only by design: each consumer thread keeps its own watermark,
    /// so entries are re-readable and never stolen from another thread.
    pub(crate) fn installs_since(&self, from: usize) -> (Vec<u64>, usize) {
        let g = lock_unpoisoned(&self.recent_installs);
        let new_wm = g.len();
        (g[from.min(g.len())..].to_vec(), new_wm)
    }

    /// Current install-list length (watermark anchor after a full rebuild).
    pub(crate) fn recent_installs_len(&self) -> usize {
        lock_unpoisoned(&self.recent_installs).len()
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
            split_id: NEXT_SPLIT_ID.fetch_add(1, Ordering::Relaxed),
            engine: Mutex::new(has_engine),
            mem: RwLock::new(GuestMemory::new()),
            cache: new_concurrent_map(),
            chain_ids: new_concurrent_map(),
            code_pages: Mutex::new(HashMap::new()),
            recent_installs: Mutex::new(Vec::new()),
            pending_code_writes: Mutex::new(Vec::new()),
            pending_code_overflow: AtomicBool::new(false),
            pending_code_gen: AtomicU64::new(0),
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
            #[cfg(test)]
            region_force: AtomicBool::new(false),
            #[cfg(test)]
            region_self_loop_force: AtomicBool::new(false),
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
            let mut guard = lock_unpoisoned(&self.engine);
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
                // Restored code is already installed under `va`, so the caller
                // needs no compile at all — and `restored: true` is what stops
                // it from issuing one (see `LedgerProbe::wants_eager`). Before
                // that flag existed, every restore was followed by a *duplicate
                // full compile of the block it had just installed*.
                entries: 0,
                restored: true,
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
        // The live generation is stamped onto the restored block (not compared
        // against the blob's): the emitted guard reads it from the frame's
        // `JitCtx`, so a blob recorded under any generation is equally
        // replayable. See the `code_cache` module docs.
        let live_gen = self.invalidate_gen.load(Ordering::Acquire);
        let cache = self.code_cache();
        let Some(code) = cache.probe(mem, va) else {
            cache.note(cache.classify_miss(mem, va));
            return None;
        };
        let tier = code.compiled_at_opt;
        // Direct chain edges are re-resolved **here**, against this run's chain
        // table, and a successor this run cannot offer refuses the whole blob:
        // the recorded target is a guest VA, so honouring it without a live
        // entry would be exactly the stale re-resolution the format exists to
        // make impossible (see the `code_cache` module docs). Done before the
        // engine lock — it only reads `chain_ids`.
        let Some(relocs) = code.resolved_relocs(|target_va| self.chain_func_for(tier, target_va))
        else {
            cache.note(CodeRestoreOutcome::UnrelocatableTarget);
            return None;
        };
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
            module.define_persisted(&code, &relocs)
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
        let (captured, import_count) = {
            let Some(eng) = guard.as_mut() else {
                return;
            };
            let Some(module) = eng.module_for(compiled.tier) else {
                return;
            };
            let import_count = module.import_count();
            let Some(captured) = module.take_captured(func_id) else {
                return;
            };
            (captured, import_count)
        };
        // Split the capture into the format's two replayable classes, or give the
        // block up: an import index is re-checked by name on restore, a direct
        // chain edge is re-resolved from its successor's guest VA, and anything
        // else has no re-resolution at all (see `code_cache`'s module docs).
        let tier = compiled.tier;
        let Some((relocs, chain_relocs)) =
            classify_capture_relocs(captured.relocs, import_count, |target_id| {
                self.chain_va_of(tier, target_id)
            })
        else {
            self.code_cache().note_unpersistable();
            return;
        };
        let mem = read_unpoisoned(&self.mem);
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
            relocs,
            chain_relocs,
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
        // Was `.expect("engine lock")`: that message named the mutex, not a
        // state invariant, so it survives here rather than in the helper.
        let guard = lock_unpoisoned(&self.engine);
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
        // And the *ledger*, which used to be flushed only by `Drop for
        // PersistentJitCache` — a hook that, as the paragraph above documents,
        // does not run for console guests. A short guest therefore persisted
        // nothing: `crt_hello` writes one record, and both lazy-flush thresholds
        // (`APPEND_FLUSH_CAP` records, `FLUSH_MIN_INTERVAL`) are out of reach, so
        // its warm boot was permanently cold while the RUNBOOK claimed
        // otherwise.
        self.persist.finish();
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
        let mem = read_unpoisoned(&self.mem);
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
        let mem = read_unpoisoned(&self.mem);
        self.persist.record_never(&mem, va);
    }

    /// Ledger invalidation over `[addr, addr+len)` (SMC / X-loss / writes).
    pub(super) fn persist_invalidate_range(&self, addr: u64, end: u64) {
        let len = usize::try_from(end.saturating_sub(addr)).unwrap_or(usize::MAX);
        self.persist.invalidate_range(addr, len);
        // The code cache drops overlapping blobs eagerly rather than waiting
        // for the byte-hash guard to catch them: the guard is the correctness
        // net, but keeping a dead blob would make every later probe of that VA
        // pay a failed hash for the rest of the process. The flush this triggers
        // writes the file from the table and discards queued copies of the
        // dropped blobs, so the on-disk half drops with the in-memory one —
        // except when the drop empties the table, where no rewrite happens at all
        // (`CodeCache::invalidate_range` documents that limit).
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

    /// Whether loop-region compilation is enabled for this instance: the env
    /// default (`WIE_JIT_REGION=1`, off) or the test latch.
    ///
    /// The single place the decision is made, so `pipeline::try_compile` cannot
    /// disagree with the tests about it.
    pub(super) fn region_enabled_here(&self) -> bool {
        JitConfig::get().region_enabled() || self.region_force_test()
    }

    #[cfg(test)]
    fn region_force_test(&self) -> bool {
        self.region_force.load(Ordering::Relaxed)
    }

    #[cfg(not(test))]
    fn region_force_test(&self) -> bool {
        let _ = self;
        false
    }

    /// Whether a **one-block self-loop** may be folded into a region, for this
    /// instance: the env opt-in (`WIE_JIT_REGION_SELF_LOOP=1`, off) or the test
    /// latch.
    ///
    /// Independent of [`Self::region_enabled_here`] in both directions, on
    /// purpose: the measured loss is specific to the one-block shape (DOOM's blit
    /// at 171 host instructions per iteration against the block path's 156;
    /// `long_loop` neutral within noise; `chain_hops` 1.45x *faster*), so "regions
    /// are on" must not imply "self-loops are on" — that is the trap this split
    /// removes.
    pub(super) fn region_self_loop_here(&self) -> bool {
        JitConfig::get().region_self_loop_enabled() || self.region_self_loop_force_test()
    }

    #[cfg(test)]
    fn region_self_loop_force_test(&self) -> bool {
        self.region_self_loop_force.load(Ordering::Relaxed)
    }

    #[cfg(not(test))]
    fn region_self_loop_force_test(&self) -> bool {
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
        let mut guard = lock_unpoisoned(&self.boot_inline_window);
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
            lock_unpoisoned(&self.recent_installs).push(rip);
        }
        self.code_pages_add_range(compiled.guest_start, compiled.guest_end);
        self.cache.pin().insert(rip, CacheEntry::Ready(compiled));
        self.persist_record_ready(&compiled);
    }

    /// Clone of the live pool handle (`None` before spawn / after a failed
    /// spawn). Callers hold their own `Arc<JitShared>` while using it, so the
    /// pool cannot be torn down mid-push.
    pub(super) fn bg_pool_arc(&self) -> Option<Arc<BgPool>> {
        lock_unpoisoned(&self.bg_pool).clone()
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
        *lock_unpoisoned(&self.bg_pool) = Some(Arc::clone(&pool));
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
            lock_unpoisoned(&self.bg_pool).take();
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
        let mut guard = lock_unpoisoned(&pool.q);
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
                let mut g = lock_unpoisoned(&pool.q);
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
        let fast_api = lock_unpoisoned(&self.bg_fast_api).clone();
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
                        let mem = read_unpoisoned(&self.mem);
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
                // Engine-lock split (`WIE_RUNTIME_PROFILE`). Off by default and
                // free when off: an `Option` that stays `None` costs a branch,
                // and `_lock_hold` is never constructed. When on, the wait is
                // measured on this side of the acquisition and the hold by an
                // RAII timer, so the early `return None` below is charged too.
                let split_insns = u64::try_from(insns.len()).unwrap_or(u64::MAX);
                let lock_wait_start = lock_split_on().then(Instant::now);
                let mut eng_guard = lock_unpoisoned(&self.engine);
                let _lock_hold = lock_wait_start.map(|t| {
                    lock_split_note_wait(
                        self.split_id,
                        self.bg_compile.compile_us.load(Ordering::Relaxed),
                        split_insns,
                        t.elapsed(),
                    );
                    LockHold::new(self, split_insns)
                });
                loop {
                    // Chain targets are resolved one VA at a time, from inside
                    // the lowering, against the live `chain_ids` table filtered
                    // to THIS module's tier. This used to build a whole-table
                    // snapshot here — a walk of every chain entry plus one
                    // `HashMap` allocation, per compiled block, while holding
                    // `engine`, the one mutex every compiling thread serialises
                    // on (measured on 7-Zip `a -mx1`: ~8k chain entries, ~11k
                    // foreground compiles). `tier` is captured by value so the
                    // downgrade retry below can still change it.
                    let chain = |va: u64| {
                        if chain_on {
                            self.chain_func_for(tier, va)
                        } else {
                            None
                        }
                    };
                    let outcome =
                        eng_guard
                            .as_mut()
                            .and_then(|eng| eng.module_for(tier))
                            .map(|module| {
                                compile_block(
                                    module, rip, &insns, end_rip, term, call_fast, &chain,
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
                            // A lowering refusal, not necessarily a codegen
                            // bug: `LowerError::Unsupported` is routine (an x87
                            // guest) and `Form` means the JIT does the
                            // instruction but not this operand shape. The old
                            // message claimed all three were "verifier/codegen",
                            // which made a design limit look like a defect.
                            // Rate-limited by count: the first REJECT_WARN_MAX
                            // at WARN, the tail at DEBUG — except a genuine
                            // verifier failure, which stays loud because it is
                            // the one class that means emitted IR was wrong.
                            let n =
                                REJECTION_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let loud = n < REJECT_WARN_MAX || matches!(e, LowerError::Verifier(_));
                            if loud {
                                tracing::warn!(
                                    rip = format_args!("{rip:#x}"),
                                    bytes_len,
                                    rejection_seq = n + 1,
                                    kind = e.kind(),
                                    error = %e,
                                    "jit block lowering refused — block falls back to the \
                                     interpreter"
                                );
                            } else {
                                tracing::debug!(
                                    rip = format_args!("{rip:#x}"),
                                    bytes_len,
                                    rejection_seq = n + 1,
                                    kind = e.kind(),
                                    error = %e,
                                    "jit block lowering refused — block falls back to the \
                                     interpreter"
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
    /// Live direct-chain target for `va` in `tier`'s module.
    ///
    /// One point lookup against the tier-tagged table: the only shape the two
    /// consumers need. Lowering asks it for at most four VAs per block (a
    /// [headless note](`super::lower::ChainLookup`)), and code-cache restore
    /// asks it for each persisted chain edge. It deliberately does **not**
    /// materialise a snapshot: a `HashMap` of the whole table costs O(chain
    /// entries) plus one allocation per compiled block, and the compile path
    /// runs it while holding the `engine` mutex — the single lock every
    /// compiling thread serialises on. `None` means this run has no chainable
    /// block there (never compiled it, or invalidated it since), which refuses
    /// the blob rather than chaining into a dead target.
    ///
    /// The ids it returns are the load-bearing precondition of
    /// [`crate::jit::engine::IsaEngine::define_persisted`]: `chain_ids` is only
    /// written after a block is installed, i.e. after `finalize_definitions`, so
    /// the id always names code `cranelift-jit` can relocate against.
    pub(super) fn chain_func_for(
        &self,
        tier: OptTier,
        va: u64,
    ) -> Option<cranelift_module::FuncId> {
        let guard = self.chain_ids.pin();
        guard.get(&va).filter(|t| t.tier == tier).map(|t| t.func_id)
    }

    /// Guest VA this run compiled `func_id` for, in `tier`'s module.
    ///
    /// The reverse of [`Self::chain_func_for`], used only when *persisting* a
    /// captured block: its direct chain relocation names a `FuncId`, and the
    /// guest VA is the only form of that reference that survives a process
    /// boundary.
    ///
    /// Reads the authoritative forward table rather than a parallel index, so it
    /// cannot disagree with what the emitter was offered. Returns `None` when the
    /// entry is gone (invalidated since the compile) **and** when two entries
    /// claim the same id: recording refuses in both cases, which is what makes
    /// this independent of `FuncId`s never being reused — an implementation
    /// detail of cranelift's declaration table, not a property of ours.
    fn chain_va_of(&self, tier: OptTier, func_id: cranelift_module::FuncId) -> Option<u64> {
        let guard = self.chain_ids.pin();
        let mut found: Option<u64> = None;
        for (&va, target) in guard.iter() {
            if target.tier != tier || target.func_id != func_id {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(va);
        }
        found
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
        lock_unpoisoned(&self.recent_installs).push(rip);
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
        let pending = lock_unpoisoned(&self.pending_code_writes);
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
        let code_pages = lock_unpoisoned(&self.code_pages);
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
        let mut code_pages = lock_unpoisoned(&self.code_pages);
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
        let mut code_pages = lock_unpoisoned(&self.code_pages);
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
        let Some(pool) = lock_unpoisoned(&self.bg_pool).take() else {
            return; // never spawned (or failed-spawn revert already ran)
        };
        {
            // Write the flag under the queue mutex: a worker either reads it
            // before parking (this order) or is woken by the notify below.
            // No lost-wakeup window exists between check and park.
            let mut q = lock_unpoisoned(&pool.q);
            q.shutdown = true;
        }
        pool.work_available.notify_all();
    }
}

/// Per-thread filter that decides *when* a block entry is worth publishing to
/// the persistent ledger (see [`LedgerRec::entries`](super::cache_persist)).
///
/// The ledger needs one bit per block — "was this entered more than once?" — and
/// the dispatcher can afford roughly nothing to produce it: DOOM dispatches
/// ~149 M blocks in 100 s, so a shared-map lookup per entry would cost seconds
/// (measured elsewhere in this tree at 15–25 ns each). A direct-mapped array of
/// slots keyed on the RIP keeps the per-entry cost at a load, a compare and
/// usually nothing else, and publishes a block only when its count crosses a
/// level [`PersistentJitCache::count_entry`] should hear about.
///
/// **Deliberately lossy, in the safe direction.** Two blocks that hash to the
/// same slot evict each other, so a block can fail to reach a publish level and
/// simply never publish — which means it is compiled through the ordinary visit
/// threshold next boot instead of eagerly. False negatives cost the pre-ledger
/// behaviour; there are no false positives, because a slot only ever reports the
/// RIP it holds. 64 slots covers the hot working set of every guest measured
/// here (DOOM's hot loop touches ~8 blocks, its whole warm-up 557 of 3580), and
/// saturation means the hottest blocks stop paying anything at all.
pub(super) struct EntryTally {
    slots: [(u64, u32); ENTRY_TALLY_SLOTS],
}

/// Slots in [`EntryTally`] (1 KiB per guest thread: 64 × `(u64, u32)` padded).
const ENTRY_TALLY_SLOTS: usize = 64;

/// Highest published level. Past this the tally reports nothing more: the only
/// consumer compares against `cache_persist::EAGER_MIN_ENTRIES`, and a block
/// entered 64 times
/// is indistinguishable from one entered 10 000 times for that purpose.
const ENTRY_TALLY_CAP: u32 = 64;

impl EntryTally {
    const EMPTY: (u64, u32) = (0, 0);

    pub(super) const fn new() -> Self {
        Self {
            slots: [Self::EMPTY; ENTRY_TALLY_SLOTS],
        }
    }

    /// Count one entry of `rip`; return the level to publish, or `0` for none.
    ///
    /// Publishes at the **first** entry and then at each doubling, so a record
    /// says exactly `1` for "entered once" and a power-of-two lower bound above
    /// that. That first publish is what lets the persisted file separate the two
    /// shapes the eager rule must reject: `0` = compiled and never entered, `1` =
    /// entered exactly once. A publish is rare by construction: at most seven
    /// `count_entry` calls per block per thread, for the single hottest block,
    /// instead of millions.
    #[inline]
    pub(super) fn bump(&mut self, rip: u64) -> u32 {
        // Multiplication by an odd constant: RIPs of interest are 16-byte-ish
        // apart inside one image, so the low bits are near-constant and the high
        // bits carry the entropy.
        let idx = usize::try_from((rip.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) & 63)
            .unwrap_or(0)
            % ENTRY_TALLY_SLOTS;
        let slot = &mut self.slots[idx];
        // `rip` can never be 0 (no guest block starts at the null page), so 0 is
        // a safe "empty" tag and needs no separate valid flag.
        if slot.0 != rip || slot.1 == 0 {
            *slot = (rip, 1);
            return 1;
        }
        if slot.1 >= ENTRY_TALLY_CAP {
            return 0; // saturated: nothing left to learn about this block
        }
        slot.1 += 1;
        let n = slot.1;
        if n.is_power_of_two() { n } else { 0 }
    }
}

/// Per-thread JIT execution state: registers, the persistent compiled-code
/// frame, chain table, shadow stack. One instance per guest thread. Not shared.
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
    /// The one [`JitCtx`] this engine ever hands to compiled code.
    ///
    /// This is the frame, allocated once at engine construction and passed by
    /// pointer on every dispatcher entry instead of being built and torn down
    /// by value (`tlb` alone is 2.3 KB; the copy cost ~8.9% of the guest thread
    /// on `7za a -mx9`). It therefore also *holds* the state that used to be
    /// duplicated here and copied back after each run: the page TLB, the region
    /// pins, the sticky/hot page ways, the edge inline-cache and the shadow
    /// return stack all live in this field now, and nowhere else, so there is
    /// no second copy that a chain edge and a dispatcher entry could disagree
    /// about. See the ownership contract on [`JitCtx`]: the dispatcher re-stamps
    /// every raw pointer in it per entry and resets the per-run counters, which
    /// is what keeps the frame valid across a move of the owning engine.
    ///
    /// Reached only through `&mut` from this engine's own `run_compiled`
    /// (and, during that call, by the host helpers the compiled block calls
    /// with the same pointer).
    #[doc(hidden)]
    pub ctx: JitCtx,
    /// Generation at which the frame's pins were last rebuilt.
    pub pins_gen: u64,
    /// `JitShared::pending_code_gen` as of this engine's last drain.
    ///
    /// When it still matches, nothing has been queued since the last drain and
    /// the (mutex-guarded) set does not have to be visited at all.
    pub drained_pending_gen: u64,
    /// The session's one [`GuestMemory`], captured **once** at engine
    /// construction and stamped into [`JitCtx::mem`] on every entry.
    ///
    /// It exists because the pointer used to be taken per dispatcher entry:
    /// `run_compiled` acquired a read guard on [`JitShared::mem`] purely to
    /// derive `&raw const *guard`, dropped it before the call, and handed the
    /// pointer to compiled code that ran *without* the guard anyway (guest
    /// loads/stores translate through the TLB/pins, and the helpers take
    /// `mem` from the frame). A `std::sync::RwLock::read` costs 11.4 ns in
    /// isolation on this host but **3.47 ns** in situ (it overlaps with the
    /// rest of the entry), measured on an entry-dominated guest — see
    /// `docs/architecture/dispatcher-entry-attribution.md`. At ~200 M entries
    /// per `7za a -mx9` run that is ~0.6% of the run, for an address that
    /// cannot change.
    ///
    /// It cannot change because `GuestMemory` is a *field* of the
    /// `RwLock<GuestMemory>` inside `JitShared`, which this engine holds by
    /// `Arc` for its whole life: nothing replaces the payload (every writer
    /// takes the guard and calls a method on it — `map_image`,
    /// `virtual_alloc`, `write`, … — none assigns a new `GuestMemory`), and an
    /// `Arc` allocation never moves. Capturing it under one read guard at
    /// construction is therefore a *one-time* acquisition rather than a
    /// weakening of the lock: metadata edits still require the write guard,
    /// and every field read (`generation`, `jit_region_pins`) still takes the
    /// read guard at its own site.
    ///
    /// Non-null from construction on; `JitCtx::new_persistent` starts null and
    /// `PerThreadJitState::new` stamps both this and the frame.
    pub guest_mem: *mut GuestMemory,
    /// Open-addressing guest VA → host block fn (late-bound block chaining).
    pub chain_slots: Box<[lower::ChainSlot; CHAIN_SLOTS]>,
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
    /// Entry-evidence filter for the persistent ledger; see [`EntryTally`].
    ///
    /// Per thread by construction, and never read by compiled code: it is
    /// bumped from the dispatcher's per-entry hook and *publishes* into
    /// `JitShared::persist` only when a block crosses a level, so the ledger's
    /// `entries` column costs one array index in the hot path rather than a map
    /// lookup per dispatch.
    pub entry_tally: EntryTally,
}

// SAFETY: the raw pointers in `ctx` (guest `mem`, `chain_slots`, `large_free`,
// `inv_gen_ptr`, and the TLB/pin/sticky page bases) are non-owning views of
// mmap arenas and caches that outlive the state; `Send` moves the *owner*, not
// the pointees, and every one of them is owned by the `Arc<JitShared>` /
// mmap that the moving engine also owns a share of. Each `PerThreadJitState`
// is owned by one host thread and never moved between threads while a frame is
// live; `Sync` is deliberately NOT implemented, so the frame can never be
// reached from two threads at once — that is the property the dispatcher's
// `&mut` relies on.
#[expect(unsafe_code)]
unsafe impl Send for PerThreadJitState {}

impl PerThreadJitState {
    /// Bind a fresh engine to the session whose shared state it will use.
    ///
    /// This is the **only** place the guest-memory pointer is acquired: one
    /// read guard, once per engine, at construction (see
    /// [`Self::guest_mem`] for why the address cannot change afterwards).
    /// Stamping it here rather than in `run_compiled` is what removes a
    /// lock acquisition (measured 3.47 ns in situ) from every dispatcher entry.
    pub(super) fn new(shared: &JitShared) -> Self {
        let guest_mem = std::ptr::from_ref(&*read_unpoisoned(&shared.mem)).cast_mut();
        let mut ctx = JitCtx::new_persistent();
        ctx.mem = guest_mem;
        Self {
            regs: RegFile::new(),
            hooks: None,
            rip_trace: [0; 32],
            rip_trace_i: 0,
            rip_trace_n: 0,
            iced_steps: 0,
            ctx,
            pins_gen: u64::MAX,
            drained_pending_gen: 0,
            guest_mem,
            chain_slots: Box::new([lower::ChainSlot::empty(); CHAIN_SLOTS]),
            opcode_sample_i: 0,
            pending_promote_thr: 0,
            large_free: LargeFreeList::new(),
            entry_tally: EntryTally::new(),
        }
    }

    /// Count one dispatcher entry into the block at `rip`, and publish it to the
    /// persistent ledger when it earns entry evidence.
    ///
    /// Called from `JitCpu::finish_compiled` — the single funnel every run of a
    /// compiled block passes through, whether it was just compiled or found
    /// `Ready` — so this is the ledger's definition of "the block ran". Chained
    /// hops inside a block deliberately do not count: they are iterations of a
    /// block, not re-entries of it, and the eager rule is about blocks a warm
    /// boot will need *early*.
    ///
    /// The `persist_active` guard is the whole cost when the ledger is off
    /// (the default): one relaxed load of a flag that never changes after
    /// attach, on a path that is already doing far more work.
    #[inline]
    pub(super) fn note_block_entry(&mut self, rip: u64, shared: &JitShared) {
        if !shared.persist_active() {
            return;
        }
        let level = self.entry_tally.bump(rip);
        if level != 0 {
            shared.persist.count_entry(rip, level);
        }
    }
}

#[cfg(test)]
mod lock_split_tests {
    use super::{LockSplitSnapshot, lock_split_lines};
    use crate::CpuEngine;
    use std::sync::atomic::Ordering;

    /// The pairing the guest report flags (`subset_of_compile_us=NO`): for a
    /// single compile, `wait + hold` must be a subset of that engine's own
    /// `compile_us`, because the timer in `bg_process_one` wraps the very call
    /// that takes the lock. Single-threaded and one call, so neither cross-thread
    /// overlap nor a second engine can move the answer.
    ///
    /// Goes through `bg_process_one` on purpose: `compile_us` is recorded there,
    /// not in `compile_from_kind_shared`, so a direct call would pair my spans
    /// against a zero.
    #[test]
    fn one_compile_hold_is_inside_compile_us() {
        // SAFETY: this test owns its process (nextest runs one per test), so no
        // other thread observes the environment and the gate is read once.
        #[expect(unsafe_code, reason = "setting an env var is unsafe in edition 2024")]
        unsafe {
            std::env::set_var("WIE_RUNTIME_PROFILE", "1");
        }
        let mut cpu = super::super::JitCpu::open_x86_64();
        const BASE: u64 = 0x10d0_0000;
        cpu.virtual_alloc(
            BASE,
            0x1000,
            crate::mem::MEM_RESERVE | crate::mem::MEM_COMMIT,
            crate::mem::protect::PAGE_EXECUTE_READWRITE,
        )
        .expect("alloc floor page");
        // mov rax,rbx ; mov rax,rcx ; add rax,rcx ; inc rax ; ret
        cpu.mem_write(
            BASE,
            &[
                0x48, 0x89, 0xd8, 0x48, 0x89, 0xc8, 0x48, 0x01, 0xc8, 0x48, 0xff, 0xc0, 0xc3,
            ],
        )
        .expect("write code");
        let kind = {
            let mem = super::read_unpoisoned(&cpu.shared.mem);
            super::block::decode_pure_gpr_block(&mem, None, BASE)
        };
        let inv_gen = cpu.shared.invalidate_gen.load(Ordering::Acquire);
        cpu.shared.bg_queue_depth.fetch_add(1, Ordering::Relaxed);
        cpu.shared.bg_process_one(super::BgJob {
            rip: BASE,
            kind,
            inv_gen,
        });

        let compile_us = cpu.shared.bg_compile.compile_us.load(Ordering::Relaxed);
        let (acq, wait, hold) = {
            let g = super::lock_split_tally().lock().expect("tally");
            let e = g.get(&cpu.shared.split_id).expect("engine row");
            (e.acquisitions, e.wait_us, e.hold_us)
        };
        assert_eq!(acq, 1, "exactly one lock acquisition for one compile");
        assert!(
            compile_us > 0,
            "the compile must land in `compile_us` at all"
        );
        // Each span is truncated to whole microseconds, so two truncated spans
        // may exceed one by up to 2 us. Anything larger is a real inconsistency.
        let span = wait.saturating_add(hold);
        eprintln!(
            "one compile: wait={wait} us hold={hold} us (span={span} us) \
             compile_us={compile_us} us subset={}",
            if span <= compile_us { "yes" } else { "NO" }
        );
        assert!(
            span <= compile_us + 2,
            "wait+hold={span} us exceeds compile_us={compile_us} us for one compile \
             (wait={wait} hold={hold})"
        );
    }

    /// The per-engine rows and the self-check that makes the numbers usable.
    /// Shape from the measured DOOM pair: 4 workers spend most of the lock span
    /// queueing, 1 worker spends most of it holding.
    #[test]
    fn wait_and_hold_are_reported_per_engine() {
        let snap = LockSplitSnapshot {
            engines: vec![
                (0, 4_750, 7_077_843, 2_406_750, 9_600_000),
                (1, 10, 1_000, 900, 500),
            ],
            buckets: vec![
                (2, 5_000_000, 900_000, 1_800),
                (4, 3_000_000, 600_000, 1_200),
            ],
        };
        let lines = lock_split_lines(&snap);
        assert_eq!(
            lines.len(),
            4,
            "two engine rows plus two buckets: {lines:?}"
        );
        // Engine 0: queueing dominates, and it is a subset of its compile_us.
        assert!(lines[0].contains("engine 0"), "{}", lines[0]);
        assert!(lines[0].contains("wait_share=74.6%"), "{}", lines[0]);
        assert!(
            lines[0].contains("per acquisition wait=1490 us hold=506 us"),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains("engine_compile_us=9600000"),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains("subset_of_compile_us=yes"),
            "{}",
            lines[0]
        );
        // Engine 1 contradicts its own compile_us, and the row must say NO
        // rather than let a reader compare numbers from different sets.
        assert!(lines[1].contains("engine 1"), "{}", lines[1]);
        assert!(lines[1].contains("subset_of_compile_us=NO"), "{}", lines[1]);
        // Per-bucket means, so a bucket can be read as queueing or as work.
        assert!(
            lines[2].contains("mean wait=2777 hold=500"),
            "{:?}",
            lines[2]
        );
        assert!(
            lines[3].contains("mean wait=2500 hold=500"),
            "{:?}",
            lines[3]
        );
        // Nothing recorded prints nothing, not a zero line.
        assert!(lock_split_lines(&LockSplitSnapshot::default()).is_empty());
    }
}
