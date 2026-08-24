//! Process execution state: per-thread engine spawn/join and shared resources.
//!
//! Each guest thread runs on its own `CpuEngine` instance. JIT workers share
//! `Arc<JitShared>` (compilation cache); Iced workers share
//! `Arc<RwLock<GuestMemory>>`. WinAPI is always behind `Arc<Mutex<>>`. The
//! per-quantum state machine each worker drives lives in `crate::quantum`;
//! this module owns the worker loop, the shared `ProcessConfig` /
//! `ProcessResources`, and the spawn/join lifecycle.

use crate::hooks::SoftApiTable;
use crate::memory::RuntimeMemoryLayout;
use crate::quantum::{QuantumCore, QuantumHooks, Step, finish_tid};
use crate::session::GuestTid;
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use std::time::Instant;
use wie_cpu::{CpuEngine, GuestMemory, IcedCpu, JitCpu, RunUntilHook};
use wie_winapi::{PendingSpawn, WinApiState};

// ── Lock helpers ───────────────────────────────────────────────────────

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Lock-free `shared_winapi` wait accumulators (Task 7 instrumentation).
///
/// Guest threads (primary pump + workers) and the host presenter contend
/// for the same `WinApiState` mutex. When runtime profiling is enabled each
/// acquisition times its wait and records into the matching side's atomics;
/// when disabled the [`Self::enabled`] gate short-circuits before
/// `Instant::now()`, so the hot path pays one relaxed atomic load per lock
/// acquisition and nothing else.
#[derive(Debug)]
pub(crate) struct LockWaitStats {
    /// Profiling gate (set by the session when `WIE_RUNTIME_PROFILE` or
    /// `enable_frame_timing` activates).
    enabled: AtomicBool,
    /// Guest-side accumulated wait nanoseconds (primary + worker threads).
    guest_total_ns: AtomicU64,
    /// Largest single guest-side wait (ns).
    guest_max_ns: AtomicU64,
    /// Presenter-side accumulated wait nanoseconds (host `GuestHandle`).
    presenter_total_ns: AtomicU64,
    /// Largest single presenter-side wait (ns).
    presenter_max_ns: AtomicU64,
}

impl LockWaitStats {
    /// Fresh, disabled accumulator (no timing until the session enables it).
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            guest_total_ns: AtomicU64::new(0),
            guest_max_ns: AtomicU64::new(0),
            presenter_total_ns: AtomicU64::new(0),
            presenter_max_ns: AtomicU64::new(0),
        }
    }

    /// Arm or disarm the timing gate.
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Whether wait timing is active (the per-acquisition gate).
    #[must_use]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Record one guest-side wait of `ns` nanoseconds.
    pub(crate) fn record_guest(&self, ns: u64) {
        self.guest_total_ns.fetch_add(ns, Ordering::Relaxed);
        bump_max(&self.guest_max_ns, ns);
    }

    /// Record one presenter-side wait of `ns` nanoseconds.
    pub(crate) fn record_presenter(&self, ns: u64) {
        self.presenter_total_ns.fetch_add(ns, Ordering::Relaxed);
        bump_max(&self.presenter_max_ns, ns);
    }

    /// Relaxed snapshot of all four accumulators (no lock taken).
    #[must_use]
    pub(crate) fn snapshot(&self) -> LockWaitSnapshot {
        LockWaitSnapshot {
            guest_total_ns: self.guest_total_ns.load(Ordering::Relaxed),
            guest_max_ns: self.guest_max_ns.load(Ordering::Relaxed),
            presenter_total_ns: self.presenter_total_ns.load(Ordering::Relaxed),
            presenter_max_ns: self.presenter_max_ns.load(Ordering::Relaxed),
        }
    }
}

impl Default for LockWaitStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Point-in-time copy of the lock-wait accumulators (read for the report).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LockWaitSnapshot {
    /// Guest-side accumulated wait nanoseconds.
    pub guest_total_ns: u64,
    /// Largest single guest-side wait (ns).
    pub guest_max_ns: u64,
    /// Presenter-side accumulated wait nanoseconds.
    pub presenter_total_ns: u64,
    /// Largest single presenter-side wait (ns).
    pub presenter_max_ns: u64,
}

/// Track the largest observed wait in `slot` (relaxed CAS loop).
fn bump_max(slot: &AtomicU64, ns: u64) {
    let mut observed = slot.load(Ordering::Relaxed);
    while ns > observed {
        match slot.compare_exchange_weak(observed, ns, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(current) => observed = current,
        }
    }
}

/// Acquire `m`, timing the wait into `stats` when profiling is enabled.
///
/// Disabled path: one relaxed atomic load, then the plain [`lock`] — no
/// clock read, no accumulation. Poison is recovered exactly like [`lock`],
/// so lock ordering and poison behavior are unchanged.
pub(crate) fn lock_wait<'a, T>(m: &'a Mutex<T>, stats: &LockWaitStats) -> MutexGuard<'a, T> {
    if !stats.enabled() {
        return lock(m);
    }
    let t0 = Instant::now();
    let guard = lock(m);
    stats.record_guest(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
    guard
}

/// Cached `WIE_MT_DEBUG` flag. Was `env::var_os` on every spawn / park /
/// worker-exit path; now a single `getenv()` guarded by `OnceLock`.
pub(crate) fn mt_debug() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WIE_MT_DEBUG").is_some())
}

// ── Shared config ──────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct ProcessConfig {
    /// Hook table the fake-API stop range resolves through.
    pub soft_apis: SoftApiTable,
    /// Process-wide environment (paths, command line) served to WinAPI.
    pub environment: wie_winapi::WinApiEnvironment,
    /// Fixed guest memory layout (stack, heaps, fake-API range, stub pages).
    pub layout: RuntimeMemoryLayout,
    /// Bitmap marking which fake-API VAs stop the host vs pass through.
    pub stop_bitmap: Arc<[u8]>,
    /// Guest thread id of the primary (entry-point) thread.
    pub primary_tid: GuestTid,
    /// Statically-loaded guest DLLs awaiting `DllMain(PROCESS_ATTACH)`, in
    /// load order (dependencies before dependents). The session pump runs
    /// them before dispatching the exe entry point.
    pub static_dll_mains: Vec<wie_winapi::dll_loader::StaticDllMain>,
}

// ── Worker TEB pool ───────────────────────────────────────────────────

/// Shared allocator for distinct per-thread TEB pages.
///
/// Wraps the linear-bump [`wie_cpu::TebPool`] over the layout's
/// `worker_tebs` range plus a free list of pages returned by exited workers.
/// A page is only reusable after its worker's engine is dropped (the runtime
/// releases it as the last act of `worker_main`), so no live engine can
/// access a page that is handed to a new worker; [`wie_cpu::PerThreadTeb::init`]
/// zero-fills the page before reuse.
pub(crate) struct WorkerTebPool {
    /// Linear bump allocator over the layout's worker TEB range.
    pool: wie_cpu::TebPool,
    /// Pages returned by exited workers, ready for immediate reuse.
    free: Vec<wie_cpu::PerThreadTeb>,
}

impl WorkerTebPool {
    /// Build a pool over the layout's `worker_tebs` region.
    ///
    /// # Errors
    /// The layout range is not a valid pool geometry (misaligned, empty, or
    /// non-page-sized) — a layout bug caught at session start.
    pub(crate) fn new(layout: &RuntimeMemoryLayout) -> anyhow::Result<Self> {
        let pool = wie_cpu::TebPool::new(layout.worker_tebs.base, layout.worker_tebs.size)
            .context("worker TEB pool geometry is invalid (layout bug)")?;
        Ok(Self {
            pool,
            free: Vec::new(),
        })
    }

    /// Capacity of the underlying range (maximum concurrently live pages).
    #[must_use]
    pub(crate) fn capacity(&self) -> usize {
        self.pool.capacity()
    }

    /// Hand out the next distinct TEB page, preferring a freed one.
    ///
    /// `None` when every page is live (concurrent workers at capacity).
    pub(crate) fn allocate(&mut self) -> Option<wie_cpu::PerThreadTeb> {
        if let Some(teb) = self.free.pop() {
            return Some(teb);
        }
        self.pool.allocate()
    }

    /// Return a worker's TEB page to the pool for reuse.
    ///
    /// The caller guarantees the owning engine is dead (dropped) before this
    /// runs — the runtime releases in the last statement of `worker_main`,
    /// after the engine value is dropped.
    pub(crate) fn release(&mut self, teb: wie_cpu::PerThreadTeb) {
        self.free.push(teb);
    }
}

// ── ProcessResources: single struct for both JIT and Iced ──────────────

/// Shared process state: config, primary engine, backend caches, WinAPI.
pub(crate) struct ProcessResources {
    pub config: ProcessConfig,
    /// Primary (guest-entry) engine; workers get their own engine at spawn.
    pub engine: Box<dyn CpuEngine>,
    /// `Some` when the JIT backend is active; workers clone this to share
    /// the compilation cache. `None` for the Iced interpreter backend.
    pub shared_jit: Option<Arc<wie_cpu::JitShared>>,
    /// `Some` when the Iced backend is active; workers clone this to share
    /// guest memory (mmap arenas + page tables). `None` for JIT.
    pub guest_mem: Option<Arc<RwLock<GuestMemory>>>,
    pub shared_winapi: Arc<Mutex<WinApiState>>,
    /// Lock-free `shared_winapi` wait accumulators (Task 7). Shared with
    /// worker threads at spawn and with the presenter's `GuestHandle`.
    pub(crate) lock_wait_stats: Arc<LockWaitStats>,
    /// Guest message queue behind its own mutex — host input posts through
    /// this without ever locking `shared_winapi`.
    pub shared_message_queue: Arc<Mutex<wie_winapi::present::MessageQueue>>,
    /// Join handles for spawned guest worker threads.
    pub worker_joins: Vec<JoinHandle<()>>,
    /// Distinct per-thread TEB pages for guest workers, shared with the
    /// worker threads (they return their page on exit).
    pub(crate) worker_teb_pool: Arc<Mutex<WorkerTebPool>>,
}

/// Temporary exclusive access to engine + WinAPI.
pub(crate) struct ProcessGuard<'a> {
    pub eng: &'a mut dyn CpuEngine,
    pub win: MutexGuard<'a, WinApiState>,
}

impl ProcessGuard<'_> {
    pub(crate) fn both(&mut self) -> (&mut dyn CpuEngine, &mut WinApiState) {
        (&mut *self.eng, &mut *self.win)
    }
}

impl ProcessResources {
    pub(crate) fn lock_pair(&mut self) -> ProcessGuard<'_> {
        ProcessGuard {
            eng: &mut *self.engine,
            win: lock_wait(&self.shared_winapi, &self.lock_wait_stats),
        }
    }

    pub(crate) fn with_mut<R>(
        &mut self,
        f: impl FnOnce(&mut dyn CpuEngine, &mut WinApiState) -> R,
    ) -> R {
        let mut guard = self.lock_pair();
        let (e, w) = guard.both();
        f(e, w)
    }

    pub(crate) fn with_winapi_ref<R>(&self, f: impl FnOnce(&WinApiState) -> R) -> R {
        f(&lock_wait(&self.shared_winapi, &self.lock_wait_stats))
    }

    pub(crate) fn layout(&self) -> &RuntimeMemoryLayout {
        &self.config.layout
    }
    pub(crate) fn primary_tid(&self) -> u32 {
        self.config.primary_tid.0
    }

    pub(crate) fn static_dll_mains(&self) -> &[wie_winapi::dll_loader::StaticDllMain] {
        &self.config.static_dll_mains
    }

    pub(crate) fn winapi_arc(&self) -> Arc<Mutex<WinApiState>> {
        Arc::clone(&self.shared_winapi)
    }

    /// Clone of the guest message-queue Arc — host posts without locking the
    /// big WinApiState mutex.
    pub(crate) fn message_queue_arc(&self) -> Arc<Mutex<wie_winapi::present::MessageQueue>> {
        Arc::clone(&self.shared_message_queue)
    }

    pub(crate) fn join_workers(&mut self) {
        join_workers_impl(
            &self.shared_winapi,
            &mut self.worker_joins,
            &self.lock_wait_stats,
        );
    }

    /// Spawn a host thread for each pending `CreateThread` spawn.
    pub(crate) fn drain_spawns(&mut self) -> Result<()> {
        let spawns: Vec<PendingSpawn> =
            self.with_mut(|_, st| std::mem::take(&mut st.kernel.sync.pending_spawns));
        if spawns.is_empty() {
            return Ok(());
        }
        if mt_debug() {
            tracing::error!(
                "[mt] drain_spawns count={} tids={:?}",
                spawns.len(),
                spawns
                    .iter()
                    .map(|s| format!("{:#x}", s.tid))
                    .collect::<Vec<_>>()
            );
        }
        let shared_winapi = Arc::clone(&self.shared_winapi);
        let config = Arc::new(self.config.clone());
        let shared_jit = self.shared_jit.clone();
        let guest_mem = self.guest_mem.clone();
        let lock_wait_stats = Arc::clone(&self.lock_wait_stats);
        let worker_teb_pool = Arc::clone(&self.worker_teb_pool);
        // Every worker TEB points at the one process PEB, which lives in the
        // spare upper half of the primary TEB page (see session init).
        let peb_va = self.layout().teb_low.base.wrapping_add(0x800);

        for spawn in spawns {
            // A worker must never start without a distinct TEB page: allocate
            // BEFORE building the engine and fail just this spawn (mark its
            // thread finished so joiners never hang) when the pool is full.
            let teb = lock(&worker_teb_pool).allocate();
            let Some(teb) = teb else {
                tracing::error!(
                    tid = spawn.tid,
                    capacity = lock(&worker_teb_pool).capacity(),
                    "worker TEB pool exhausted — refusing to start worker without a distinct TEB"
                );
                let st = lock_wait(&shared_winapi, &lock_wait_stats);
                finish_tid(&st, spawn.tid, 1);
                continue;
            };
            let mut engine: Box<dyn CpuEngine> = if let Some(ref jit) = shared_jit {
                Box::new(JitCpu::new_shared(Arc::clone(jit)))
            } else if let Some(ref mem) = guest_mem {
                let temp = IcedCpu::new_standalone_with_mem(Arc::clone(mem));
                Box::new(IcedCpu::new_shared(&temp))
            } else {
                anyhow::bail!("no shared memory for worker spawn")
            };
            // Initialize THIS worker's TEB page through ITS engine: zero-filled
            // page + standard x64 TEB fields (Self, stack bounds, PEB, zero
            // last-error). Stack bounds are per-thread; the PEB is process-wide.
            let stack_top = spawn
                .stack_base
                .saturating_add(u64::try_from(spawn.stack_size).unwrap_or(0));
            let teb_init = wie_cpu::TebInit {
                stack_top,
                stack_limit: spawn.stack_base,
                peb_va,
            };
            if let Err(e) = teb.init(&mut *engine, &teb_init) {
                tracing::error!(
                    tid = spawn.tid,
                    teb = format_args!("{:#x}", teb.va()),
                    error = %e,
                    "failed to initialize worker TEB"
                );
                let st = lock_wait(&shared_winapi, &lock_wait_stats);
                finish_tid(&st, spawn.tid, 1);
                lock(&worker_teb_pool).release(teb);
                continue;
            }
            let winapi = Arc::clone(&shared_winapi);
            let cfg = Arc::clone(&config);
            let stats = Arc::clone(&lock_wait_stats);
            let pool = Arc::clone(&worker_teb_pool);
            const STACK: usize = 8 * 1024 * 1024;
            let handle = std::thread::Builder::new()
                .name(format!("wie-guest-{}", spawn.tid))
                .stack_size(STACK)
                .spawn(move || worker_main(engine, winapi, cfg, spawn.tid, stats, teb, pool));
            match handle {
                Ok(handle) => {
                    tracing::debug!(
                        target: "wiegui",
                        tid = spawn.tid,
                        "guest worker thread started"
                    );
                    self.worker_joins.push(handle);
                }
                Err(error) => {
                    // The host thread never ran: return the TEB page to the
                    // pool (nothing can access it) and mark the thread
                    // finished so a waiter never hangs, then surface the
                    // failure through the existing spawn error path.
                    lock(&worker_teb_pool).release(teb);
                    let st = lock_wait(&shared_winapi, &lock_wait_stats);
                    finish_tid(&st, spawn.tid, 1);
                    return Err(error).context("failed to spawn guest worker");
                }
            }
        }
        Ok(())
    }
}

// ── Join workers ───────────────────────────────────────────────────────

fn join_workers_impl(
    winapi: &Arc<Mutex<WinApiState>>,
    joins: &mut Vec<JoinHandle<()>>,
    stats: &LockWaitStats,
) {
    {
        let mut st = lock_wait(winapi, stats);
        st.kernel.sync.process_dying = true;
        for q in st.kernel.sync.cs_waiters.values() {
            q.notify_all();
        }
        for obj in st.kernel.sync.objects.values() {
            match obj {
                wie_winapi::KernelObject::Event(e) => e.set(),
                wie_winapi::KernelObject::Semaphore(s) => s.notify_all(),
                wie_winapi::KernelObject::Thread(t) => {
                    if !t.is_finished() {
                        t.finish(1);
                    }
                }
                // File mappings hold no waiters; nothing to wake at teardown.
                wie_winapi::KernelObject::FileMapping(_) => {}
                // Directory watches: waking the parked waiter unblocks the
                // join; the watcher itself stops when the object drops.
                wie_winapi::KernelObject::DirectoryWatch(d) => d.deactivate(),
                // Child-process objects: wake waiters so a parked
                // WaitForSingleObject never blocks the teardown join.
                wie_winapi::KernelObject::Process(p) => {
                    if !p.is_finished() {
                        p.finish(1);
                    }
                }
            }
        }
    }
    for j in joins.drain(..) {
        let _ = j.join();
    }
}

// ── Worker quantum hooks ───────────────────────────────────────────────

/// Worker-side `QuantumHooks`: the default impls already encode worker
/// behavior (no journaling, no callbacks, yield on pure compute); this
/// struct only adds the worker-specific run-fault exits (pthread return
/// trampoline, ret-to-0, hard faults) and the per-activation GS-base
/// re-affirmation.
struct WorkerHooks {
    tid: u32,
    /// Guest VA of THIS worker's TEB page (its GS base).
    teb_va: u64,
}

impl QuantumHooks for WorkerHooks {
    /// Runs under the WinAPI lock right after the thread activates. Re-affirm
    /// the engine's GS base every activation: the engine is 1:1 with this
    /// worker, so this is a re-assertion of the spawn-time binding, never a
    /// switch — it guards against any path that could have clobbered the
    /// engine's segment base since the last quantum.
    fn on_activated(
        &mut self,
        core: &mut QuantumCore,
        _st: &mut WinApiState,
    ) -> Result<Option<Step>> {
        core.engine().set_gs_base(self.teb_va);
        Ok(None)
    }

    fn on_run_fault(
        &mut self,
        core: &mut QuantumCore,
        run: &RunUntilHook,
        error: Option<&wie_cpu::CpuError>,
    ) -> Result<Option<Step>> {
        if error.is_some() {
            // Emulation backend failure — same hard-exit as the primary's
            // RuntimeStop, but for a worker thread it is a thread failure.
            return Ok(Some(Step::ExitThread(1)));
        }
        // Pthread return trampoline: the start routine returned. RAX holds
        // the `void *` result. Mark the pthread finished, wake joiners, exit.
        // Checked BEFORE the generic invalid-memory handler so the trampoline
        // fault is recognised as a normal thread completion.
        if run.invalid_memory.hit
            && run.invalid_memory.address == wie_winapi::pthread_return_trampoline_va()
        {
            let return_value = core.engine().read_rax().unwrap_or(0);
            if mt_debug() {
                tracing::error!(
                    "[mt] worker tid={:#x} pthread return value={return_value:#x}",
                    self.tid
                );
            }
            core.with_locked(|_, st| {
                st.kernel.threads.activate(self.tid);
                if let Some(pt) = st.pthread().by_tid.get(&self.tid).copied()
                    && let Some(thread) = st.pthread().threads.get_mut(&pt)
                {
                    thread.exit_value = return_value;
                    thread.finished = true;
                    thread.queue.wake();
                }
            });
            return Ok(Some(Step::ExitThread(0)));
        }
        // ThreadProc that `ret`s to the planted 0 return address: RIP becomes
        // 0, or the next fetch faults at VA 0. Both mean normal exit (code in
        // RAX).
        let rip_now = core.engine().read_rip().unwrap_or(0);
        if rip_now == 0 || (run.invalid_memory.hit && run.invalid_memory.address == 0) {
            let code = u32::try_from(core.engine().read_rax().unwrap_or(0) & u64::from(u32::MAX))
                .unwrap_or(0);
            if mt_debug() {
                tracing::error!(
                    "[mt] worker_main exit tid={:#x} code={code} (ret-to-0)",
                    self.tid
                );
            }
            return Ok(Some(Step::ExitThread(code)));
        }
        // Invalid guest access must not soft-yield forever (same RIP retried).
        // Primary session path treats this as a hard stop; workers must too.
        if run.invalid_memory.hit {
            return Ok(Some(Step::ExitThread(1)));
        }
        Ok(None)
    }
}

// ── Single worker main (JIT and Iced) ──────────────────────────────────

/// One worker's lifecycle: run its quantum loop, then release its TEB page
/// back to the pool.
///
/// The TEB release is deliberately the LAST act: the engine is dropped before
/// the page returns to the pool, so no live engine can access a page that a
/// future worker reuses.
fn worker_main(
    engine: Box<dyn CpuEngine>,
    shared_winapi: Arc<Mutex<WinApiState>>,
    config: Arc<ProcessConfig>,
    tid: u32,
    lock_wait_stats: Arc<LockWaitStats>,
    teb: wie_cpu::PerThreadTeb,
    worker_teb_pool: Arc<Mutex<WorkerTebPool>>,
) {
    let engine = run_worker(engine, shared_winapi, config, tid, lock_wait_stats, teb);
    // Engine is dead: its TEB page cannot be accessed anymore. Return the
    // page to the pool for the next worker (re-init zero-fills it).
    drop(engine);
    if mt_debug() {
        tracing::error!(
            "[mt] worker_main release teb tid={tid:#x} teb={:#x}",
            teb.va()
        );
    }
    lock(&worker_teb_pool).release(teb);
}

/// The worker's quantum loop; returns the engine so the caller can drop it
/// before the TEB page is released.
fn run_worker(
    mut engine: Box<dyn CpuEngine>,
    shared_winapi: Arc<Mutex<WinApiState>>,
    config: Arc<ProcessConfig>,
    tid: u32,
    lock_wait_stats: Arc<LockWaitStats>,
    teb: wie_cpu::PerThreadTeb,
) -> Box<dyn CpuEngine> {
    if mt_debug() {
        tracing::error!("[mt] worker_main start tid={tid:#x} teb={:#x}", teb.va());
    }
    let layout = &config.layout;
    let fake_api_end = layout.fake_api_end();

    if let Err(e) = engine.install_runtime_hooks(
        layout.fake_api.base,
        fake_api_end,
        config.stop_bitmap.clone(),
    ) {
        tracing::error!(tid, error = %e, "failed to install runtime hooks for worker");
        if mt_debug() {
            tracing::error!("[mt] worker_main hooks failed tid={tid:#x}: {e}");
        }
        // Always mark finished so joiners do not hang forever.
        let st = lock_wait(&shared_winapi, &lock_wait_stats);
        finish_tid(&st, tid, 1);
        return engine;
    }

    // Load initial thread context set by CreateThread.
    {
        let st = lock_wait(&shared_winapi, &lock_wait_stats);
        if let Some(ctx) = st.kernel.sync.thread_cpu.get(&tid).cloned() {
            drop(st);
            engine.restore_thread_context(&ctx);
            engine.on_thread_switch();
        }
    }

    // Bind THIS engine to THIS worker's TEB page. Must run AFTER
    // `restore_thread_context`: the stored context carries the creator's GS
    // base (the primary's GS_BASE) and would otherwise clobber the
    // per-thread binding.
    engine.set_gs_base(teb.va());

    let mut core =
        match QuantumCore::new(&mut *engine, &config, &shared_winapi, tid, &lock_wait_stats) {
            Ok(core) => core,
            Err(e) => {
                tracing::error!(tid, error = %e, "failed to build worker quantum executor");
                let st = lock_wait(&shared_winapi, &lock_wait_stats);
                finish_tid(&st, tid, 1);
                return engine;
            }
        };
    let mut hooks = WorkerHooks {
        tid,
        teb_va: teb.va(),
    };
    loop {
        let step = match core.step(&mut hooks) {
            Ok(step) => step,
            Err(e) => {
                if mt_debug() {
                    tracing::error!("[mt] worker_main step error tid={tid:#x}: {e}");
                }
                let st = lock_wait(&shared_winapi, &lock_wait_stats);
                finish_tid(&st, tid, 1);
                return engine;
            }
        };
        match step {
            Step::Next => {}
            Step::PureCompute => {
                // Pure-compute quantum exhausted — yield so peers can run.
                std::thread::yield_now();
            }
            Step::Park(reason) => {
                // Block outside the WinAPI lock. The primary's drain covers
                // CreateThread spawns for the whole process, so the worker
                // passes no-op drains; `process_dying` mid-wait finishes the
                // thread (joiners never hang).
                let api_result = core.park(reason, &mut || {}, &mut || {
                    let st = lock_wait(&shared_winapi, &lock_wait_stats);
                    finish_tid(&st, tid, 1);
                });
                if let Some(result) = api_result {
                    let st = lock_wait(&shared_winapi, &lock_wait_stats);
                    if st.kernel.sync.process_dying {
                        finish_tid(&st, tid, 1);
                        return engine;
                    }
                    let _ = core
                        .engine()
                        .return_from_win64_api(u64::from(result))
                        .map_err(|e| tracing::error!("guest stack corrupted on wait park: {e}"));
                }
            }
            Step::ExitThread(code) => {
                if mt_debug() {
                    tracing::error!("[mt] worker_main exit tid={tid:#x} code={code}");
                }
                let st = lock_wait(&shared_winapi, &lock_wait_stats);
                finish_tid(&st, tid, code);
                return engine;
            }
            Step::Stop(term) => {
                // Worker-side hard stop: unresolved fake API, SEH failure, or
                // emulation error. Same finish as a thread failure.
                tracing::warn!(tid, error = %format!("{term:?}"), "worker quantum stopped");
                let st = lock_wait(&shared_winapi, &lock_wait_stats);
                finish_tid(&st, tid, 1);
                return engine;
            }
        }
    }
}

// ── Common helpers ────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::as_conversions)]
mod tests {
    use super::{LockWaitStats, WorkerTebPool, lock, lock_wait};
    use crate::memory::DEFAULT_LAYOUT;
    use crate::mt_runtime::{ProcessConfig, ProcessResources};
    use crate::session::GuestTid;
    use std::sync::{Arc, Mutex, RwLock};
    use wie_cpu::guest_layout::TEB_PAGE_SIZE;
    use wie_cpu::{CpuEngine, GuestMemory, IcedCpu, RwxPerms, ThreadContext};
    use wie_pe::ProcessIdentity;
    use wie_winapi::PendingSpawn;

    /// A fresh pool over the default layout's worker TEB range.
    fn test_pool() -> WorkerTebPool {
        WorkerTebPool::new(&DEFAULT_LAYOUT).expect("default worker TEB pool")
    }

    /// The pool hands out distinct, page-aligned TEB pages and reuses a page
    /// after `release` (spawn/join cycles must not exhaust the range).
    #[test]
    fn worker_teb_pool_allocates_distinct_and_reuses_released() {
        let mut pool = test_pool();
        let capacity = pool.capacity();
        assert!(capacity >= 2, "layout must hold at least two worker TEBs");

        let first = pool.allocate().expect("first page");
        let second = pool.allocate().expect("second page");
        assert_ne!(
            first.va(),
            second.va(),
            "concurrent workers get distinct pages"
        );
        assert_eq!(first.va(), DEFAULT_LAYOUT.worker_tebs.base);
        assert_eq!(
            second.va(),
            DEFAULT_LAYOUT.worker_tebs.base + TEB_PAGE_SIZE as u64,
            "linear bump: second page is one page past the first"
        );

        // Release: the page returns for immediate reuse, distinct from the
        // still-live page.
        pool.release(first);
        let reused = pool.allocate().expect("reused page");
        assert_eq!(reused.va(), first.va(), "released page is reused");
        let _ = second;
    }

    /// Allocation reports exhaustion instead of reusing a live page, and a
    /// release unblocks the next allocation.
    #[test]
    fn worker_teb_pool_exhausts_then_unblocks_on_release() {
        let mut pool = test_pool();
        let capacity = pool.capacity();
        let mut live = Vec::new();
        for _ in 0..capacity {
            live.push(pool.allocate().expect("page within capacity"));
        }
        assert!(
            pool.allocate().is_none(),
            "no free pages while every page is live"
        );
        pool.release(live.remove(0));
        let unblocked = pool.allocate().expect("release unblocks allocation");
        assert_eq!(unblocked.va(), live[0].va() - TEB_PAGE_SIZE as u64);
    }

    /// End-to-end: `drain_spawns` allocates a distinct TEB per worker,
    /// initializes it with the worker's own stack bounds and the process PEB,
    /// and the exited worker's page returns to the pool for the next spawn.
    ///
    /// Uses the real iced backend + shared `GuestMemory`, so the TEB writes
    /// are plain soft-translated guest stores exactly as in a live session.
    #[test]
    fn drain_spawns_initializes_distinct_worker_tebs_and_reuses_after_exit() {
        let process = ProcessIdentity {
            module_file_name: "teb-test.exe".to_owned(),
            module_path: r"C:\teb-test.exe".to_owned(),
            current_directory: r"C:\".to_owned(),
            command_line: "teb-test.exe".to_owned(),
        };
        let winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        // The shared guest memory: primary engine + every worker share it.
        let primary_cpu = IcedCpu::open_x86_64();
        let mem = Arc::clone(primary_cpu.guest_mem_arc());
        let mut engine: Box<dyn wie_cpu::CpuEngine> = Box::new(primary_cpu);
        // Map the pages drain_spawns writes: the worker TEB pool (init target)
        // and the primary TEB page (last-error mirror publish target).
        engine
            .mem_map(
                DEFAULT_LAYOUT.worker_tebs.base,
                DEFAULT_LAYOUT.worker_tebs.size,
                RwxPerms::READ_WRITE,
            )
            .expect("map worker TEB pool");
        engine
            .mem_map(
                DEFAULT_LAYOUT.teb_low.base,
                DEFAULT_LAYOUT.teb_low.size,
                RwxPerms::READ_WRITE,
            )
            .expect("map primary TEB page");

        let config = ProcessConfig {
            soft_apis: crate::hooks::SoftApiTable::default(),
            environment: crate::memory::default_winapi_environment(
                &DEFAULT_LAYOUT,
                0x1400_0000,
                0,
                0,
                0,
                0,
                0,
            ),
            layout: DEFAULT_LAYOUT,
            // Fake-API window bitmap: worker hook install validates its size
            // against the 4 MiB window before the worker can run a quantum.
            stop_bitmap: Arc::<[u8]>::from(vec![0_u8; DEFAULT_LAYOUT.fake_api.size / 8]),
            primary_tid: GuestTid(wie_winapi::PRIMARY_THREAD_ID),
            static_dll_mains: Vec::new(),
        };
        let mut resources = ProcessResources {
            config,
            engine,
            shared_jit: None,
            guest_mem: Some(Arc::clone(&mem)),
            shared_winapi: Arc::new(Mutex::new(winapi_state)),
            lock_wait_stats: Arc::new(LockWaitStats::new()),
            shared_message_queue: Arc::new(
                Mutex::new(wie_winapi::present::MessageQueue::default()),
            ),
            worker_joins: Vec::new(),
            worker_teb_pool: Arc::new(Mutex::new(test_pool())),
        };

        // One pending spawn: fresh worker thread with its own stack range.
        let spawn = |tid: u32, handle: u64, stack_base: u64, stack_size: usize| PendingSpawn {
            tid,
            handle,
            start_address: 0x1400_1000,
            parameter: 0,
            stack_base,
            stack_size,
        };
        let stack_a = 0x0000_0000_2200_0000_u64;
        let stack_size = 0x10_0000_usize;
        let worker_a = {
            let tid = 0x5679_u32;
            resources.with_mut(|_, st| {
                st.kernel.sync.register_thread(tid, ThreadContext::new());
                st.kernel
                    .sync
                    .pending_spawns
                    .push(spawn(tid, 0x8000_0001, stack_a, stack_size));
            });
            tid
        };
        resources.drain_spawns().expect("spawn worker A");
        // The TEB is initialized synchronously inside drain_spawns (before the
        // host thread starts), so its fields are readable right after.
        let teb_a = read_worker_teb(&mem, worker_a, DEFAULT_LAYOUT.worker_tebs.base);
        assert_eq!(
            teb_a.self_ptr, DEFAULT_LAYOUT.worker_tebs.base,
            "TEB.Self points at this worker's own page"
        );
        assert_eq!(
            teb_a.stack_top,
            stack_a + stack_size as u64,
            "TEB.StackBase is this worker's stack top"
        );
        assert_eq!(
            teb_a.stack_limit, stack_a,
            "TEB.StackLimit is this worker's stack base"
        );
        assert_eq!(
            teb_a.peb,
            DEFAULT_LAYOUT.teb_low.base + 0x800,
            "every TEB points at the process PEB"
        );
        assert_eq!(teb_a.last_error, 0, "fresh TEB starts with zero last-error");

        // Join: the worker exits (RIP 0 → ExitThread), then releases its page.
        resources.join_workers();

        // Second spawn: the exited worker's page returns to the pool and is
        // handed to the next worker (same VA — release → reuse).
        let worker_b = {
            let tid = 0x567A_u32;
            resources.with_mut(|_, st| {
                st.kernel.sync.register_thread(tid, ThreadContext::new());
                st.kernel.sync.pending_spawns.push(spawn(
                    tid,
                    0x8000_0002,
                    stack_a + 0x20_0000,
                    stack_size,
                ));
            });
            tid
        };
        resources.drain_spawns().expect("spawn worker B");
        let teb_b = read_worker_teb(&mem, worker_b, DEFAULT_LAYOUT.worker_tebs.base);
        assert_eq!(
            teb_b.self_ptr, teb_a.self_ptr,
            "the exited worker's page is reused by the next worker"
        );
        assert_eq!(
            teb_b.stack_top,
            stack_a + 0x20_0000 + stack_size as u64,
            "reused page is re-initialized with the NEW worker's stack"
        );
        resources.join_workers();
    }

    /// Snapshot of the standard TEB fields at the FIRST page of the pool
    /// (the deterministic allocation order for the test above).
    struct TebSnapshot {
        self_ptr: u64,
        stack_top: u64,
        stack_limit: u64,
        peb: u64,
        last_error: u32,
    }

    fn read_worker_teb(mem: &Arc<RwLock<GuestMemory>>, _tid: u32, teb_va: u64) -> TebSnapshot {
        use wie_cpu::guest_layout::{
            TEB_LAST_ERROR_OFFSET, TEB_PEB_OFFSET, TEB_SELF_OFFSET, TEB_STACK_BASE_OFFSET,
            TEB_STACK_LIMIT_OFFSET,
        };
        // Read through a temporary engine over the SAME shared memory the
        // worker wrote (plain soft-translated guest memory access).
        let mut cpu = IcedCpu::new_standalone_with_mem(Arc::clone(mem));
        let read_u64 = |cpu: &mut IcedCpu, va: u64| -> u64 {
            let mut bytes = [0_u8; 8];
            cpu.mem_read(va, &mut bytes).expect("read TEB field");
            u64::from_le_bytes(bytes)
        };
        let mut err_bytes = [0_u8; 4];
        cpu.mem_read(teb_va + TEB_LAST_ERROR_OFFSET, &mut err_bytes)
            .expect("read TEB last-error");
        TebSnapshot {
            self_ptr: read_u64(&mut cpu, teb_va + TEB_SELF_OFFSET),
            stack_top: read_u64(&mut cpu, teb_va + TEB_STACK_BASE_OFFSET),
            stack_limit: read_u64(&mut cpu, teb_va + TEB_STACK_LIMIT_OFFSET),
            peb: read_u64(&mut cpu, teb_va + TEB_PEB_OFFSET),
            last_error: u32::from_le_bytes(err_bytes),
        }
    }

    /// The worker TEB pool region holds whole TEB pages and never overlaps
    /// the primary TEB at `GS_BASE` (a compile-time gate; asserted here for
    /// the default layout too so a future layout edit is caught in tests).
    #[test]
    fn worker_teb_layout_is_page_aligned_and_disjoint_from_primary() {
        let region = DEFAULT_LAYOUT.worker_tebs;
        assert!(region.size.is_multiple_of(TEB_PAGE_SIZE));
        assert!(!region.contains(wie_cpu::GS_BASE));
        // The primary page is exactly one TEB_PAGE_SIZE; the pool never
        // aliases it (PerThreadTeb::primary lives at GS_BASE).
        assert_ne!(region.base, wie_cpu::GS_BASE);
    }

    /// `lock_wait` records total + max into the guest side of the stats
    /// while profiling is enabled; a held lock makes the recorded wait
    /// non-trivial so the max path is exercised.
    #[test]
    fn lock_wait_accumulates_guest_total_and_max_when_enabled() {
        let stats = LockWaitStats::new();
        stats.set_enabled(true);
        let state = Arc::new(Mutex::new(42_u32));
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn({
            let state = Arc::clone(&state);
            move || {
                let _guard = lock(&state);
                let _ = tx.send(());
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
        let _ = rx.recv();
        let guard = lock_wait(&state, &stats);
        assert_eq!(*guard, 42);
        drop(guard);
        let _ = holder.join();

        let snap = stats.snapshot();
        assert!(
            snap.guest_total_ns >= 5_000_000,
            "the 10 ms held lock must be observed (got {} ns)",
            snap.guest_total_ns
        );
        assert!(
            snap.guest_max_ns >= 5_000_000,
            "the max counter must capture the same wait (got {} ns)",
            snap.guest_max_ns
        );
        assert_eq!(snap.presenter_total_ns, 0);
        assert_eq!(snap.presenter_max_ns, 0);
    }

    /// Disabled mode: `lock_wait` returns the guard without recording —
    /// the snapshot stays zero even across acquisitions, so the hot path
    /// pays only the atomic gate load.
    #[test]
    fn lock_wait_disabled_records_nothing() {
        let stats = LockWaitStats::new();
        assert!(!stats.enabled(), "stats start disabled");
        let state = Arc::new(Mutex::new(7_u32));
        {
            let guard = lock_wait(&state, &stats);
            assert_eq!(*guard, 7);
        }
        {
            let guard = lock_wait(&state, &stats);
            assert_eq!(*guard, 7);
        }
        let snap = stats.snapshot();
        assert_eq!(snap.guest_total_ns, 0);
        assert_eq!(snap.guest_max_ns, 0);
        assert_eq!(snap.presenter_total_ns, 0);
        assert_eq!(snap.presenter_max_ns, 0);
    }

    /// The `record_presenter` side accumulates independently of the guest
    /// side (the two never mix).
    #[test]
    fn record_presenter_is_independent_from_guest() {
        let stats = LockWaitStats::new();
        stats.set_enabled(true);
        stats.record_guest(1_000);
        stats.record_guest(3_000);
        stats.record_presenter(5_000);
        let snap = stats.snapshot();
        assert_eq!(snap.guest_total_ns, 4_000);
        assert_eq!(snap.guest_max_ns, 3_000);
        assert_eq!(snap.presenter_total_ns, 5_000);
        assert_eq!(snap.presenter_max_ns, 5_000);
    }
}
