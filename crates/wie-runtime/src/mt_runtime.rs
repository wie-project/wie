//! Process execution state: per-thread engine spawn/join and shared resources.
//!
//! Each guest thread runs on its own `CpuEngine` instance. JIT workers share
//! `Arc<JitShared>` (compilation cache); Iced workers share
//! `Arc<RwLock<GuestMemory>>`. WinAPI is always behind `Arc<Mutex<>>`.

use crate::hooks::{SoftApiTable, resolve_fake_api_at};
use crate::memory::RuntimeMemoryLayout;
use crate::session::GuestTid;
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use wie_cpu::{CpuEngine, GuestMemory, IcedCpu, JitCpu};
use wie_winapi::kernel32::{resolve_cs_queue, resolve_wait_target};
use wie_winapi::{HandlerContext, HostParkReason, PendingSpawn, WinApiControlSignal, WinApiState};

// ── Lock helpers ───────────────────────────────────────────────────────

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
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
    /// Guest message queue behind its own mutex — host input posts through
    /// this without ever locking `shared_winapi`.
    pub shared_message_queue: Arc<Mutex<wie_winapi::present::MessageQueue>>,
    /// Join handles for spawned guest worker threads.
    pub worker_joins: Vec<JoinHandle<()>>,
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
            win: lock(&self.shared_winapi),
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
        f(&lock(&self.shared_winapi))
    }

    pub(crate) fn layout(&self) -> &RuntimeMemoryLayout {
        &self.config.layout
    }
    pub(crate) fn environment(&self) -> &wie_winapi::WinApiEnvironment {
        &self.config.environment
    }
    pub(crate) fn soft_apis(&self) -> &SoftApiTable {
        &self.config.soft_apis
    }
    pub(crate) fn primary_tid(&self) -> u32 {
        self.config.primary_tid.0
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
        join_workers_impl(&self.shared_winapi, &mut self.worker_joins);
    }

    /// Spawn a host thread for each pending `CreateThread` spawn.
    pub(crate) fn drain_spawns(&mut self) -> Result<()> {
        let spawns: Vec<PendingSpawn> =
            self.with_mut(|_, st| std::mem::take(&mut st.kernel.sync.pending_spawns));
        if spawns.is_empty() {
            return Ok(());
        }
        if mt_debug() {
            eprintln!(
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

        for spawn in spawns {
            let engine: Box<dyn CpuEngine> = if let Some(ref jit) = shared_jit {
                Box::new(JitCpu::new_shared(Arc::clone(jit)))
            } else if let Some(ref mem) = guest_mem {
                let temp = IcedCpu::new_standalone_with_mem(Arc::clone(mem));
                Box::new(IcedCpu::new_shared(&temp))
            } else {
                anyhow::bail!("no shared memory for worker spawn")
            };
            let winapi = Arc::clone(&shared_winapi);
            let cfg = Arc::clone(&config);
            const STACK: usize = 8 * 1024 * 1024;
            let handle = std::thread::Builder::new()
                .name(format!("wie-guest-{}", spawn.tid))
                .stack_size(STACK)
                .spawn(move || worker_main(engine, winapi, cfg, spawn.tid))
                .context("failed to spawn guest worker")?;
            tracing::debug!(target: "wiegui", tid = spawn.tid, "guest worker thread started");
            self.worker_joins.push(handle);
        }
        Ok(())
    }
}

// ── Join workers ───────────────────────────────────────────────────────

fn join_workers_impl(winapi: &Arc<Mutex<WinApiState>>, joins: &mut Vec<JoinHandle<()>>) {
    {
        let mut st = lock(winapi);
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
            }
        }
    }
    for j in joins.drain(..) {
        let _ = j.join();
    }
}

// ── Single worker main (JIT and Iced) ──────────────────────────────────

fn worker_main(
    mut engine: Box<dyn CpuEngine>,
    shared_winapi: Arc<Mutex<WinApiState>>,
    config: Arc<ProcessConfig>,
    tid: u32,
) {
    if mt_debug() {
        eprintln!("[mt] worker_main start tid={tid:#x}");
    }
    let layout = &config.layout;
    let budget = layout.instruction_budget;
    let fake_api_end = layout
        .fake_api_base
        .saturating_add(u64::try_from(layout.fake_api_size).unwrap_or(0))
        .saturating_sub(1);

    if let Err(e) = engine.install_runtime_hooks(
        layout.fake_api_base,
        fake_api_end,
        config.stop_bitmap.clone(),
    ) {
        tracing::error!(tid, error = %e, "failed to install runtime hooks for worker");
        if mt_debug() {
            eprintln!("[mt] worker_main hooks failed tid={tid:#x}: {e}");
        }
        // Always mark finished so joiners do not hang forever.
        let st = lock(&shared_winapi);
        finish_tid(&st, tid, 1);
        return;
    }

    // Load initial thread context set by CreateThread.
    {
        let st = lock(&shared_winapi);
        if let Some(ctx) = st.kernel.sync.thread_cpu.get(&tid).cloned() {
            drop(st);
            engine.restore_thread_context(&ctx);
            engine.on_thread_switch();
        }
    }

    loop {
        // Activate + liveness under WinAPI lock only — do **not** hold the lock
        // across pure guest execution (per-thread engines need concurrent quanta).
        {
            let mut st = lock(&shared_winapi);
            st.kernel.threads.activate(tid);
            if st.kernel.sync.process_dying {
                finish_tid(&st, tid, 1);
                return;
            }
        }

        let begin = engine.read_rip().unwrap_or(0);
        if begin == 0 {
            let code = u32::try_from(engine.read_rax().unwrap_or(0) & 0xffff_ffff).unwrap_or(0);
            let st = lock(&shared_winapi);
            finish_tid(&st, tid, code);
            return;
        }

        // Guest compute / iced / JIT: no shared WinAPI mutex.
        let run =
            match engine.run_until_stop(begin, 0, 0, budget, layout.fake_api_base, fake_api_end) {
                Ok(r) => r,
                Err(e) => {
                    if mt_debug() {
                        eprintln!("[mt] worker_main run error tid={tid:#x}: {e}");
                    }
                    let st = lock(&shared_winapi);
                    finish_tid(&st, tid, 1);
                    return;
                }
            };

        // Pthread return trampoline: the start routine returned. RAX holds
        // the `void *` result. Mark the pthread finished, wake joiners, exit.
        // Check BEFORE the generic invalid-memory handler so the trampoline
        // fault is recognised as a normal thread completion.
        if run.invalid_memory.hit
            && run.invalid_memory.address == wie_winapi::pthread_return_trampoline_va()
        {
            let return_value = engine.read_rax().unwrap_or(0);
            if mt_debug() {
                eprintln!("[mt] worker tid={tid:#x} pthread return value={return_value:#x}");
            }
            let mut st = lock(&shared_winapi);
            st.kernel.threads.activate(tid);
            if let Some(pt) = st.pthread().by_tid.get(&tid).copied()
                && let Some(thread) = st.pthread().threads.get_mut(&pt)
            {
                thread.exit_value = return_value;
                thread.finished = true;
                thread.queue.wake();
            }
            finish_tid(&st, tid, 0);
            return;
        }

        // ThreadProc that `ret`s to the planted 0 return address: RIP becomes 0,
        // or the next fetch faults at VA 0. Both mean normal exit (code in RAX).
        let rip_now = engine.read_rip().unwrap_or(0);
        if rip_now == 0 || (run.invalid_memory.hit && run.invalid_memory.address == 0) {
            let code = u32::try_from(engine.read_rax().unwrap_or(0) & 0xffff_ffff).unwrap_or(0);
            if mt_debug() {
                eprintln!("[mt] worker_main exit tid={tid:#x} code={code} (ret-to-0)");
            }
            let st = lock(&shared_winapi);
            finish_tid(&st, tid, code);
            return;
        }

        // Invalid guest access must not soft-yield forever (same RIP retried).
        // Primary session path treats this as a hard stop; workers must too.
        if run.invalid_memory.hit {
            let _rsp = engine.read_rsp().unwrap_or(0);
            let _inv = run.invalid_memory;
            let st = lock(&shared_winapi);
            finish_tid(&st, tid, 1);
            return;
        }

        if !run.code.hit {
            // Pure-compute quantum exhausted — yield so peers can run.
            std::thread::yield_now();
            continue;
        }
        let hook = run.code;

        if hook.address == layout.callback_return_trampoline_va {
            continue;
        }

        // SEH / C++ EH continuation trampoline (primary path is session.rs; workers
        // can hit it if a throw originated on that thread).
        if hook.address == wie_winapi::seh_continue_trampoline_va() {
            let mut st = lock(&shared_winapi);
            // Always re-activate: peer threads may have stolen `active` while we
            // ran pure guest code without the WinAPI lock.
            st.kernel.threads.activate(tid);
            if let Err(e) = wie_winapi::seh::continue_pending(&mut *engine, &mut st) {
                tracing::warn!(tid, error = %e, "worker SEH continue failed");
                finish_tid(&st, tid, 1);
                return;
            }
            continue;
        }

        let Some(resolved) = resolve_fake_api_at(hook.address, &config.soft_apis) else {
            let st = lock(&shared_winapi);
            finish_tid(&st, tid, 1);
            return;
        };

        let park_reason: Option<HostParkReason>;
        {
            let mut st = lock(&shared_winapi);
            // Re-activate after pure guest run (primary/peers may have activated).
            st.kernel.threads.activate(tid);
            if st.kernel.sync.process_dying {
                finish_tid(&st, tid, 1);
                return;
            }

            if resolved.traits.exit_process() {
                st.kernel.sync.process_dying = true;
                let code = u32::try_from(engine.read_rcx().unwrap_or(0) & 0xffff_ffff).unwrap_or(0);
                finish_tid(&st, tid, code);
                return;
            }

            let mut ctx = HandlerContext::new(&mut *engine, config.environment, &mut st);
            let dispatch = if let Some(id) = resolved.winapi_id {
                wie_winapi::dispatch_winapi_id(&mut ctx, id)
            } else {
                wie_winapi::dispatch_winapi(&mut ctx, &resolved.library, &resolved.name)
            };

            match dispatch {
                Ok(_) => park_reason = None,
                Err(e) => {
                    if let Some(WinApiControlSignal::ExitThread { code }) = e.downcast_ref() {
                        finish_tid(&st, tid, *code);
                        return;
                    }
                    if let Some(WinApiControlSignal::HostPark { reason }) = e.downcast_ref() {
                        park_reason = Some(*reason);
                    } else {
                        finish_tid(&st, tid, 1);
                        return;
                    }
                }
            }
            // Flush coalesced publishes only when the worker is about to
            // block (park) — a repaint cycle's BitBlt + control paints across
            // dispatches publish once at the park boundary instead of once per
            // dispatch (which emitted child-less intermediate frames). Workers
            // have no guest callbacks. While the worker keeps dispatching, the
            // pending publishes accumulate in the shared set; the primary's
            // idle drain publishes them whenever it reaches an empty queue.
            if park_reason.is_some() {
                st.present().drain_pending_publishes();
            }
        } // drop WinAPI lock before host park

        if let Some(reason) = park_reason {
            handle_park(&mut engine, &shared_winapi, tid, reason);
        }
    }
}

fn handle_park(
    engine: &mut Box<dyn CpuEngine>,
    shared_winapi: &Arc<Mutex<WinApiState>>,
    tid: u32,
    reason: HostParkReason,
) {
    match reason {
        HostParkReason::CriticalSection { cs } => {
            let q = {
                let mut st = shared_winapi.lock().unwrap_or_else(|p| p.into_inner());
                resolve_cs_queue(&mut st, cs)
            };
            q.park_brief();
        }
        HostParkReason::WaitObject { handle, timeout_ms } => {
            let target = {
                let st = shared_winapi.lock().unwrap_or_else(|p| p.into_inner());
                resolve_wait_target(&st, handle)
            };
            let result = wait_on_target(target, timeout_ms, shared_winapi, tid);
            let st = lock(shared_winapi);
            if st.kernel.sync.process_dying {
                finish_tid(&st, tid, 1);
                return;
            }
            let _ = engine
                .return_from_win64_api(u64::from(result))
                .map_err(|e| tracing::error!("guest stack corrupted on wait park: {e}"));
        }
        HostParkReason::PthreadWait => {
            // Pthread parking is handled through WakeQueue inside the handler.
            // Yield briefly so the next handler re-entry can check the condition.
            // Spawns are drained by the caller before entering handle_park.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        HostParkReason::WaitMultiple => {
            let req = {
                let mut st = lock(shared_winapi);
                st.kernel.sync.multi_wait.remove(&tid)
            };
            let result = wait_multiple_result(req, shared_winapi, tid);
            let st = lock(shared_winapi);
            if st.kernel.sync.process_dying {
                finish_tid(&st, tid, 1);
                return;
            }
            let _ = engine
                .return_from_win64_api(u64::from(result))
                .map_err(|e| tracing::error!("guest stack corrupted on wait multiple park: {e}"));
        }
    }
}

// ── Common helpers ────────────────────────────────────────────────────

fn finish_tid(st: &WinApiState, tid: u32, code: u32) {
    let thread = st
        .kernel
        .sync
        .objects
        .values()
        .find(|obj| matches!(obj, wie_winapi::KernelObject::Thread(t) if t.tid == tid));
    if let Some(wie_winapi::KernelObject::Thread(t)) = thread {
        t.finish(code);
    }
}

fn wait_on_target(
    target: Option<wie_winapi::WaitTarget>,
    timeout_ms: u32,
    shared_winapi: &Arc<Mutex<WinApiState>>,
    tid: u32,
) -> u32 {
    match target {
        Some(t) => {
            if timeout_ms == wie_winapi::INFINITE {
                loop {
                    let r = t.wait(50);
                    if r == wie_winapi::WAIT_OBJECT_0 {
                        return r;
                    }
                    let st = shared_winapi.lock().unwrap_or_else(|p| p.into_inner());
                    if st.kernel.sync.process_dying {
                        finish_tid(&st, tid, 1);
                        return wie_winapi::WAIT_FAILED;
                    }
                }
            } else {
                t.wait(timeout_ms)
            }
        }
        None => wie_winapi::WAIT_FAILED,
    }
}

fn wait_multiple_result(
    req: Option<wie_winapi::MultiWaitRequest>,
    shared_winapi: &Arc<Mutex<WinApiState>>,
    tid: u32,
) -> u32 {
    let Some(req) = req else {
        return wie_winapi::WAIT_FAILED;
    };
    let targets = {
        let st = shared_winapi.lock().unwrap_or_else(|p| p.into_inner());
        st.kernel.sync.wait_targets(&req.handles)
    };
    let Some(ts) = targets else {
        return wie_winapi::WAIT_FAILED;
    };

    if req.timeout_ms == wie_winapi::INFINITE {
        loop {
            let r = wie_winapi::wait_multiple(&ts, req.wait_all, 50);
            if r != wie_winapi::WAIT_TIMEOUT {
                return r;
            }
            let st = shared_winapi.lock().unwrap_or_else(|p| p.into_inner());
            if st.kernel.sync.process_dying {
                finish_tid(&st, tid, 1);
                return wie_winapi::WAIT_FAILED;
            }
        }
    } else {
        wie_winapi::wait_multiple(&ts, req.wait_all, req.timeout_ms)
    }
}
