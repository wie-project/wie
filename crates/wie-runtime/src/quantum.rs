//! Shared per-thread quantum executor: activate → run → resolve → dispatch → park → finish.
//!
//! The primary session pump (`session/pump.rs`) and guest workers
//! (`mt_runtime.rs`) both drive the same per-quantum state machine. The
//! machine owns the WinAPI lock ordering (locked only around activation /
//! last-error TEB sync / dispatch, never across pure guest execution or
//! host parks), the active-TID rules, the instruction budget, and the
//! thread-finish transitions. Primary-only behaviors — the static DllMain
//! phase, guest-callback completion, session journaling, the bridge and
//! child-spawn control signals — are injected through the [`QuantumHooks`]
//! delegate; the defaults are the worker behavior.

use crate::hooks::{ResolvedFakeApi, resolve_fake_api_at};
use crate::memory::RuntimeMemoryLayout;
use crate::mt_runtime::{LockWaitStats, ProcessConfig, lock_wait};
use crate::trace::EntryTraceTermination;
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use wie_cpu::{CodeHookOutcome, CpuEngine, CpuError, InvalidMemoryAccess, RunUntilHook};
use wie_winapi::{
    HandlerContext, HostParkReason, WinApiControlSignal, WinApiState,
    kernel32::{resolve_cs_queue, resolve_wait_target},
};

/// Next action for the caller after one shared quantum.
#[derive(Debug, Clone)]
pub(crate) enum Step {
    /// Run another quantum immediately.
    Next,
    /// The quantum ran pure guest code with no fake-API hook. The worker
    /// yields before retrying; the primary never produces this (its
    /// no-hook accounting returns `Next` or `Stop` directly).
    PureCompute,
    /// Park on a host wait outside the WinAPI lock, then re-enter the same
    /// guest API. Spawn draining and the post-wait dying policy are the
    /// caller's.
    Park(HostParkReason),
    /// The thread exited. Workers mark the `Thread` kernel object finished;
    /// the primary treats it as `ExitProcess`.
    ExitThread(u32),
    /// Hard stop with a session-level termination reason.
    Stop(EntryTraceTermination),
}

/// Primary-specific behaviors injected into the shared quantum loop.
///
/// The default implementations are the worker behavior: no session tracing,
/// no callbacks, no DllMain phase. The primary session pump overrides the
/// methods that journal, charge API stops, bridge to the host, and complete
/// guest callbacks.
pub(crate) trait QuantumHooks {
    /// Called once per quantum, before activation, with no WinAPI lock held.
    /// Returns the API index for this quantum (the primary allocates and
    /// journals it; workers return 0).
    fn prepare_quantum(&mut self, _core: &mut QuantumCore) -> Result<usize> {
        Ok(0)
    }

    /// Called once before the first quantum. The primary prepares the first
    /// static `DllMain(PROCESS_ATTACH)` call when static dependencies exist.
    fn prepare_first_quantum(&mut self, _core: &mut QuantumCore) -> Result<()> {
        Ok(())
    }

    /// Called right after activation, under the WinAPI lock, before the
    /// guest quantum. The worker observes `process_dying` and finishes its
    /// thread here; the primary returns `None`.
    fn on_activated(
        &mut self,
        _core: &mut QuantumCore,
        _st: &mut WinApiState,
    ) -> Result<Option<Step>> {
        Ok(None)
    }

    /// RIP was 0 at quantum start. `Some(va)` resumes from `va` (the primary
    /// dispatches the PE entry point); `None` means the thread returned and
    /// exits with the low u32 of RAX (worker semantics).
    fn zero_rip_begin(&mut self, _core: &mut QuantumCore) -> Result<Option<u64>> {
        Ok(None)
    }

    /// Run-outcome pre-check (run error / invalid memory), called before the
    /// shared SEH handling. The worker recognizes its pthread-return
    /// trampoline, ret-to-0 exit, and hard-fault exits here; the primary
    /// returns `None` so the shared SEH / diagnostic handling applies.
    fn on_run_fault(
        &mut self,
        _core: &mut QuantumCore,
        _run: &RunUntilHook,
        _error: Option<&CpuError>,
    ) -> Result<Option<Step>> {
        Ok(None)
    }

    /// A non-divide-by-zero run error stopped the engine. Reached only when
    /// [`Self::on_run_fault`] declined the error (the primary). The default
    /// builds the generic emulation-error diagnostic; the primary adds the
    /// last journaled API for context.
    fn on_run_error(
        &mut self,
        core: &mut QuantumCore,
        error: &CpuError,
        api_index: usize,
    ) -> Result<Step> {
        let rip = core
            .engine()
            .read_rip()
            .context("failed to read RIP after runtime emulation error")?;
        let rsp = core
            .engine()
            .read_rsp()
            .context("failed to read RSP after runtime emulation error")?;
        let mut slot = [0_u8; 8];
        let slot_va = rsp.wrapping_add(0x160);
        let slot_val = core
            .engine()
            .mem_read(slot_va, &mut slot)
            .ok()
            .map(|()| u64::from_le_bytes(slot));
        Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
            "emulation error (api_index={api_index}, last_api=-): {error}; \
             rip={rip:#018x}; rsp={rsp:#018x}; [rsp+0x160]={slot_val:?}"
        ))))
    }

    /// No fake-API hook this quantum. The worker yields and retries; the
    /// primary accounts no-hook slices and may stop the session.
    fn on_no_hook(&mut self, _core: &mut QuantumCore, _begin: u64) -> Result<Step> {
        Ok(Step::PureCompute)
    }

    /// A fake-API hook the delegate owns (the primary's static DllMain return
    /// trampoline), handled under the WinAPI lock. `Some(step)` claims the
    /// hook; `None` falls through to the shared callback / SEH / resolve /
    /// dispatch path.
    fn claim_hook_locked(
        &mut self,
        _core: &mut QuantumCore,
        _st: &mut WinApiState,
        _address: u64,
    ) -> Result<Option<Step>> {
        Ok(None)
    }

    /// Guest-callback return trampoline. The delegate owns the WinAPI lock
    /// guard so it can drop it while completing the callback frame (the
    /// completion re-locks through the core). Default: ignore (workers never
    /// run bridged callbacks).
    fn on_callback_return(
        &mut self,
        _core: &mut QuantumCore,
        _guard: MutexGuard<'_, WinApiState>,
        _address: u64,
        _api_index: usize,
    ) -> Result<Step> {
        Ok(Step::Next)
    }

    /// A shared SEH continuation ran successfully. Observation point for the
    /// primary's journaling + API charging.
    fn on_seh_continue(
        &mut self,
        _core: &mut QuantumCore,
        _api_index: usize,
        _address: u64,
        _return_value: u64,
        _return_address: u64,
    ) {
    }

    /// Resolved fake-API stop — the full dispatch policy. The delegate owns
    /// the WinAPI lock guard (bridges / callbacks / child spawns drop it and
    /// re-lock through the core). The default is the worker policy.
    fn dispatch(
        &mut self,
        core: &mut QuantumCore,
        mut guard: MutexGuard<'_, WinApiState>,
        resolved: &ResolvedFakeApi,
        _hook_address: u64,
        _api_index: usize,
    ) -> Result<Step> {
        let st = &mut *guard;
        if st.kernel.sync.process_dying {
            return Ok(Step::ExitThread(1));
        }
        if resolved.traits.exit_process() {
            st.kernel.sync.process_dying = true;
            // Flush buffered CRT console output (printf/puts buffer in the
            // guest stream; fwrite bypasses it). Windows flushes stdout at
            // process exit — without this, trailing printf output is lost.
            st.flush_console();
            let code = u32::try_from(core.engine().read_rcx().unwrap_or(0) & u64::from(u32::MAX))
                .unwrap_or(0);
            return Ok(Step::ExitThread(code));
        }
        match core.run_handler(st, resolved) {
            Ok(_) => {
                // Publish handler writes (`process.last_error`) back to this
                // thread's per-thread slot and its engine's GS-relative TEB
                // slot before the next quantum.
                st.publish_last_error_to_guest(core.engine());
                Ok(Step::Next)
            }
            Err(e) => {
                if let Some(WinApiControlSignal::ExitThread { code }) = e.downcast_ref() {
                    return Ok(Step::ExitThread(*code));
                }
                if let Some(WinApiControlSignal::HostPark { reason }) = e.downcast_ref() {
                    st.publish_last_error_to_guest(core.engine());
                    // Flush coalesced publishes only when the worker is about
                    // to block (park) — a repaint cycle's BitBlt + control
                    // paints across dispatches publish once at the park
                    // boundary instead of once per dispatch. Workers have no
                    // guest callbacks.
                    st.present().drain_pending_publishes();
                    return Ok(Step::Park(*reason));
                }
                Ok(Step::ExitThread(1))
            }
        }
    }

    /// Whether the delegate wants per-quantum timing observations (the
    /// primary's `WIE_RUNTIME_PROFILE`).
    fn wants_timing(&self) -> bool {
        false
    }

    /// One `run_until_stop` quantum elapsed (`emu_ns`).
    fn on_emu_time(&mut self, _core: &mut QuantumCore, _ns: u128) {}

    /// A fake-API hook resolved (`resolve_ns`).
    fn on_resolved(&mut self, _core: &mut QuantumCore, _ns: u128) {}
}

/// The shared executor for one guest thread's quantum loop.
///
/// Holds the three resources every quantum needs — the per-thread engine,
/// the process-wide config, and the shared WinAPI state mutex — plus the
/// thread identity (`tid`) the loop must keep active.
pub(crate) struct QuantumCore<'a> {
    /// Guest CPU engine for the thread this executor drives.
    engine: &'a mut dyn CpuEngine,
    /// Process-wide config (layout, environment, soft-API table).
    config: &'a ProcessConfig,
    /// Shared WinAPI state mutex.
    winapi: &'a Arc<Mutex<WinApiState>>,
    /// Lock-free `shared_winapi` wait accumulators (guest-side timing).
    lock_wait_stats: &'a LockWaitStats,
    /// Guest TID this executor drives.
    tid: u32,
    /// Inclusive end of the fake-API stop window (precomputed).
    fake_api_end: u64,
}

impl<'a> QuantumCore<'a> {
    pub(crate) fn new(
        engine: &'a mut dyn CpuEngine,
        config: &'a ProcessConfig,
        winapi: &'a Arc<Mutex<WinApiState>>,
        tid: u32,
        lock_wait_stats: &'a LockWaitStats,
    ) -> Result<Self> {
        let fake_api_size_u64 =
            u64::try_from(config.layout.fake_api.size).context("fake API size does not fit u64")?;
        let fake_api_end = config
            .layout
            .fake_api
            .base
            .checked_add(fake_api_size_u64)
            .context("fake API end overflow")?
            .checked_sub(1)
            .context("fake API end underflow")?;
        Ok(Self {
            engine,
            config,
            winapi,
            lock_wait_stats,
            tid,
            fake_api_end,
        })
    }

    /// Guest CPU engine (unlocked; used for register/memory access).
    pub(crate) fn engine(&mut self) -> &mut dyn CpuEngine {
        self.engine
    }

    /// Process-wide layout (fixed guest VA geometry + tuning scalars).
    pub(crate) fn layout(&self) -> &RuntimeMemoryLayout {
        &self.config.layout
    }

    /// Soft fake-API table the hook range resolves through.
    pub(crate) fn soft_apis(&self) -> &crate::hooks::SoftApiTable {
        &self.config.soft_apis
    }

    /// Run `f` with the engine and the WinAPI lock held. The lock is released
    /// when `f` returns — callers that must block (bridges, waits) do so
    /// outside `with_locked`.
    pub(crate) fn with_locked<R>(
        &mut self,
        f: impl FnOnce(&mut dyn CpuEngine, &mut WinApiState) -> R,
    ) -> R {
        let mut st = lock_wait(self.winapi, self.lock_wait_stats);
        f(self.engine, &mut st)
    }

    /// Shared dispatch mechanics: build a `HandlerContext` and invoke the
    /// dense or string dispatcher. Returns the raw handler outcome.
    pub(crate) fn run_handler(
        &mut self,
        st: &mut WinApiState,
        resolved: &ResolvedFakeApi,
    ) -> Result<wie_winapi::WinApiHandlerResult> {
        let mut ctx = HandlerContext::new(self.engine, self.config.environment, st);
        if let Some(id) = resolved.winapi_id {
            wie_winapi::dispatch_winapi_id(&mut ctx, id)
        } else {
            wie_winapi::dispatch_winapi(&mut ctx, &resolved.library, &resolved.name)
        }
    }

    /// One iteration of the shared quantum state machine.
    ///
    /// Lock discipline mirrors the session pump exactly: the WinAPI lock is
    /// taken only around activation / last-error mirror sync / dispatch /
    /// resolve, never across pure guest execution or host parks.
    pub(crate) fn step<H: QuantumHooks>(&mut self, hooks: &mut H) -> Result<Step> {
        let api_index = hooks.prepare_quantum(self)?;

        // Activate this thread under the WinAPI lock — pure guest run must
        // not hold `shared_winapi` so peer quanta can overlap (per-thread
        // engines). Publish the ACTIVE thread's last-error into the ACTIVE
        // engine's GS-relative TEB slot so in-guest GetLastError /
        // SetLastError stubs observe this thread's value during the upcoming
        // quantum — the engine's GS base binds the slot to this thread's own
        // TEB page (the primary's fixed `GS_BASE`; a worker's `PerThreadTeb`).
        {
            let mut st = lock_wait(self.winapi, self.lock_wait_stats);
            if st.kernel.threads.active.tid != self.tid {
                st.kernel.threads.activate(self.tid);
            }
            if let Some(step) = hooks.on_activated(self, &mut st)? {
                return Ok(step);
            }
            st.process.last_error = st.kernel.threads.active.last_error;
            st.publish_last_error_to_guest(self.engine);
        }

        // Determine the run VA. RIP 0 means entry-not-yet-reached (primary)
        // or ThreadProc returned (worker).
        let begin = match self
            .engine
            .read_rip()
            .context("failed to read RIP before runtime step")?
        {
            0 => match hooks.zero_rip_begin(self)? {
                Some(va) => va,
                None => {
                    // Worker: the start routine ret'd to the planted 0 return
                    // address; RAX carries the exit code.
                    let code =
                        u32::try_from(self.engine.read_rax().unwrap_or(0) & u64::from(u32::MAX))
                            .unwrap_or(0);
                    return Ok(Step::ExitThread(code));
                }
            },
            current_rip => current_rip,
        };

        // One guest quantum (JIT or iced) — no WinAPI lock.
        let wants_timing = hooks.wants_timing();
        let emu_t0 = wants_timing.then(Instant::now);
        let run = self.engine.run_until_stop(
            begin,
            0,
            0,
            self.config.layout.instruction_budget,
            self.config.layout.fake_api.base,
            self.fake_api_end,
        );
        if let Some(t0) = emu_t0 {
            hooks.on_emu_time(self, t0.elapsed().as_nanos());
        }

        // Worker fault pre-checks run BEFORE the WinAPI lock is taken: the
        // pthread-return path re-locks through the core (`with_locked`), so a
        // held guard would self-deadlock the worker. The primary never claims
        // here and falls through to the locked SEH / diagnostic handling.
        match &run {
            Ok(result) => {
                let outcome = RunUntilHook {
                    code: result.code,
                    invalid_memory: result.invalid_memory,
                };
                if let Some(step) = hooks.on_run_fault(self, &outcome, None)? {
                    return Ok(step);
                }
            }
            Err(error) => {
                let empty = RunUntilHook {
                    code: CodeHookOutcome::default(),
                    invalid_memory: InvalidMemoryAccess::default(),
                };
                if let Some(step) = hooks.on_run_fault(self, &empty, Some(error))? {
                    return Ok(step);
                }
            }
        }

        // Handle the outcome under the lock. Workers may have activated
        // themselves while we ran pure guest code without the WinAPI lock —
        // reclaim this thread's identity before any dispatch that uses
        // `current_tid()` (CS owner, TLS, waits).
        let mut guard = lock_wait(self.winapi, self.lock_wait_stats);
        if guard.kernel.threads.active.tid != self.tid {
            guard.kernel.threads.activate(self.tid);
        }

        match run {
            Ok(result) => {
                let outcome = RunUntilHook {
                    code: result.code,
                    invalid_memory: result.invalid_memory,
                };
                if outcome.invalid_memory.hit {
                    crate::session::fault_capture(
                        self.engine,
                        &guard,
                        guard.kernel.threads.active.tid,
                        &outcome.invalid_memory,
                    );
                    // Route through guest SEH before terminating.
                    match wie_winapi::seh::dispatch_hardware_fault(
                        self.engine,
                        &mut guard,
                        outcome.invalid_memory.exception_code,
                        outcome.invalid_memory.address,
                    ) {
                        Ok(result) => {
                            // Handler found — guest continues at catch block.
                            tracing::trace!(
                                exc = outcome.invalid_memory.exception_code,
                                addr = outcome.invalid_memory.address,
                                resume_rip = result.return_value,
                                "hardware fault handled by guest SEH"
                            );
                            return Ok(Step::Next);
                        }
                        Err(_unhandled) => {
                            tracing::debug!(
                                exc = outcome.invalid_memory.exception_code,
                                "unhandled hardware fault"
                            );
                            return Ok(Step::Stop(crate::session::invalid_memory_diagnostic(
                                self.engine,
                                &outcome.invalid_memory,
                            )?));
                        }
                    }
                }
                if !outcome.code.hit {
                    return hooks.on_no_hook(self, begin);
                }
                self.handle_hook(hooks, guard, outcome.code.address, api_index)
            }
            Err(CpuError::DivideByZero(div_rip)) => {
                match wie_winapi::seh::dispatch_hardware_fault(
                    self.engine,
                    &mut guard,
                    wie_cpu::exception_code::INT_DIVIDE_BY_ZERO,
                    div_rip,
                ) {
                    Ok(result) => {
                        tracing::trace!(
                            rip = div_rip,
                            resume_rip = result.return_value,
                            "divide-by-zero handled by SEH"
                        );
                        Ok(Step::Next)
                    }
                    Err(_) => {
                        tracing::debug!(rip = div_rip, "unhandled divide-by-zero");
                        Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
                            "integer divide by zero at rip={div_rip:#x}"
                        ))))
                    }
                }
            }
            Err(error) => hooks.on_run_error(self, &error, api_index),
        }
    }

    /// Hook-hit processing: delegate-owned hooks (DllMain), the guest-callback
    /// trampoline, the shared SEH continuation, resolution, and dispatch.
    fn handle_hook<H: QuantumHooks>(
        &mut self,
        hooks: &mut H,
        mut guard: MutexGuard<'_, WinApiState>,
        address: u64,
        api_index: usize,
    ) -> Result<Step> {
        // Delegate-owned hooks (primary: static DllMain return), under lock.
        {
            let st = &mut *guard;
            if let Some(step) = hooks.claim_hook_locked(self, st, address)? {
                return Ok(step);
            }
        }
        // Guest-callback return trampoline — the delegate owns the guard so
        // it can drop the WinAPI lock while completing the callback frame.
        if address == self.config.layout.callback_return_trampoline_va {
            return hooks.on_callback_return(self, guard, address, api_index);
        }
        let st = &mut *guard;
        // SEH / C++ EH continuation (UnwindMap actions, MSVC catch funclets).
        if address == wie_winapi::seh_continue_trampoline_va() {
            // Uniform last-error discipline for every host stop: absorb the
            // ACTIVE engine's GS-relative TEB slot into the active thread
            // before host handling, publish back afterwards.
            st.absorb_guest_last_error(self.engine);
            match wie_winapi::seh::continue_pending(self.engine, st) {
                Ok(result) => {
                    hooks.on_seh_continue(
                        self,
                        api_index,
                        address,
                        result.return_value,
                        result.return_address,
                    );
                    st.publish_last_error_to_guest(self.engine);
                    return Ok(Step::Next);
                }
                Err(error) => {
                    return Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
                        "failed to continue SEH sequence: {error}"
                    ))));
                }
            }
        }
        // Resolve the stop VA into dispatch metadata.
        let wants_timing = hooks.wants_timing();
        let resolve_t0 = wants_timing.then(Instant::now);
        let resolved_opt = resolve_fake_api_at(address, self.soft_apis());
        let Some(resolved) = resolved_opt else {
            return Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
                "unresolved fake API at {address:#018x}"
            ))));
        };
        if let Some(t0) = resolve_t0 {
            hooks.on_resolved(self, t0.elapsed().as_nanos());
        }
        // Absorb the ACTIVE engine's GS-relative TEB slot into the ACTIVE
        // thread's per-thread slot before host dispatch — the in-guest
        // SetLastError stub may have advanced the engine's TEB slot since this
        // thread last published.
        st.absorb_guest_last_error(self.engine);
        // Dispatch — the delegate owns the guard (bridges / callbacks / child
        // spawns release and re-acquire the lock through the core).
        hooks.dispatch(self, guard, &resolved, address, api_index)
    }

    /// Block on a host wait reason outside the WinAPI lock, observing
    /// `process_dying` so teardown never deadlocks.
    ///
    /// `drain` runs before infinite-wait slices (the primary drains pending
    /// `CreateThread` spawns so nested workers keep starting; workers pass a
    /// no-op — the primary's drain covers the process). `on_dying` runs when
    /// `process_dying` is observed mid-wait (workers finish their tid; the
    /// primary just returns `WAIT_FAILED`).
    ///
    /// Returns `Some(wait_result)` for the wait-carrying reasons
    /// (`WaitObject` / `WaitMultiple`) — the caller writes it back into RAX
    /// via `return_from_win64_api` after its own post-wait policy — and
    /// `None` for the retry/sleep reasons (`CriticalSection` / `PthreadWait`).
    pub(crate) fn park(
        &mut self,
        reason: HostParkReason,
        drain: &mut dyn FnMut(),
        on_dying: &mut dyn FnMut(),
    ) -> Option<u32> {
        match reason {
            HostParkReason::CriticalSection { cs } => {
                // Clone queue under lock, park **without** process locks so the
                // CS owner can Leave and wake us.
                let q = {
                    let mut st = lock_wait(self.winapi, self.lock_wait_stats);
                    resolve_cs_queue(&mut st, cs)
                };
                q.park_brief();
                None
            }
            HostParkReason::WaitObject { handle, timeout_ms } => {
                drain();
                let target = {
                    let st = lock_wait(self.winapi, self.lock_wait_stats);
                    resolve_wait_target(&st, handle)
                };
                let result = Self::wait_with_dying(
                    target,
                    timeout_ms,
                    self.winapi,
                    self.lock_wait_stats,
                    drain,
                    on_dying,
                );
                Some(result)
            }
            HostParkReason::PthreadWait => {
                // Pthread parking is handled through WakeQueue inside the
                // handler. Yield briefly so the next handler re-entry can
                // check the condition.
                drain();
                std::thread::sleep(std::time::Duration::from_millis(1));
                None
            }
            HostParkReason::WaitMultiple => {
                drain();
                let req = {
                    let mut st = lock_wait(self.winapi, self.lock_wait_stats);
                    st.kernel.sync.multi_wait.remove(&self.tid)
                };
                let result = Self::wait_multiple_with_dying(
                    req,
                    self.winapi,
                    self.lock_wait_stats,
                    drain,
                    on_dying,
                );
                Some(result)
            }
        }
    }

    /// Slice an infinite `WaitForSingleObject` so the caller can drain spawns
    /// and observe `process_dying`; finite timeouts wait once.
    fn wait_with_dying(
        target: Option<wie_winapi::WaitTarget>,
        timeout_ms: u32,
        winapi: &Arc<Mutex<WinApiState>>,
        stats: &LockWaitStats,
        drain: &mut dyn FnMut(),
        on_dying: &mut dyn FnMut(),
    ) -> u32 {
        let Some(target) = target else {
            return wie_winapi::WAIT_FAILED;
        };
        if timeout_ms == wie_winapi::INFINITE {
            loop {
                let r = target.wait(50);
                if r == wie_winapi::WAIT_OBJECT_0 {
                    return r;
                }
                drain();
                // Scope the guard: `on_dying` re-acquires the same mutex and
                // would self-deadlock if invoked while `st` is still held.
                let dying = {
                    let st = lock_wait(winapi, stats);
                    st.kernel.sync.process_dying
                };
                if dying {
                    on_dying();
                    return wie_winapi::WAIT_FAILED;
                }
            }
        } else {
            target.wait(timeout_ms)
        }
    }

    /// Slice an infinite `WaitForMultipleObjects` like [`Self::wait_with_dying`].
    fn wait_multiple_with_dying(
        req: Option<wie_winapi::MultiWaitRequest>,
        winapi: &Arc<Mutex<WinApiState>>,
        stats: &LockWaitStats,
        drain: &mut dyn FnMut(),
        on_dying: &mut dyn FnMut(),
    ) -> u32 {
        let Some(req) = req else {
            return wie_winapi::WAIT_FAILED;
        };
        let targets = {
            let st = lock_wait(winapi, stats);
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
                drain();
                // Scope the guard: `on_dying` re-acquires the same mutex and
                // would self-deadlock if invoked while `st` is still held.
                let dying = {
                    let st = lock_wait(winapi, stats);
                    st.kernel.sync.process_dying
                };
                if dying {
                    on_dying();
                    return wie_winapi::WAIT_FAILED;
                }
            }
        } else {
            wie_winapi::wait_multiple(&ts, req.wait_all, req.timeout_ms)
        }
    }
}

// ── Shared finish transition ───────────────────────────────────────────

/// Mark the guest `Thread` kernel object for `tid` finished with `code`,
/// waking joiners (`GetExitCodeProcess` / `WaitForSingleObject` / pthread
/// joins). Idempotent — the worker and the teardown path may both call it.
pub(crate) fn finish_tid(st: &WinApiState, tid: u32, code: u32) {
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::as_conversions)]
mod tests {
    use super::*;
    use crate::hooks::SoftApiTable;
    use crate::memory::DEFAULT_LAYOUT;
    use crate::mt_runtime::lock;
    use crate::session::GuestTid;
    use ahash::HashMapExt;
    use std::collections::VecDeque;
    use wie_cpu::{CodeHookOutcome, InvalidMemoryAccess};
    use wie_pe::ProcessIdentity;

    // ── Mock engine ─────────────────────────────────────────────────────

    /// Scripted `CpuEngine` for deterministic quantum state-machine tests.
    ///
    /// `run_until_stop` pops the next entry from `script`; the register reads
    /// return the stored values; the engine's GS-relative TEB last-error slot
    /// is a byte store with a recorded write log so tests can assert the
    /// publish discipline. `gs_base` tracks the engine's TEB binding
    /// (`set_gs_base` calls) so tests can assert the per-activation
    /// re-affirmation.
    struct MockEngine {
        rip: u64,
        rax: u64,
        rcx: u64,
        rsp: u64,
        /// `(begin, instruction_budget)` of every `run_until_stop` call.
        run_calls: Vec<(u64, usize)>,
        /// Scripted outcomes, consumed in order.
        script: VecDeque<Result<RunUntilHook, CpuError>>,
        /// Byte store for guest memory.
        mem: ahash::HashMap<u64, u8>,
        /// Every `mem_write` to the guest TEB last-error mirror.
        last_error_writes: Vec<u32>,
        /// Engine's current GS segment base (TEB page binding).
        gs_base: u64,
        /// Every `set_gs_base` call, in order.
        set_gs_calls: Vec<u64>,
    }

    fn mock_engine() -> MockEngine {
        MockEngine {
            rip: 0x1400_1000,
            rax: 0,
            rcx: 0,
            rsp: 0x1000,
            run_calls: Vec::new(),
            script: VecDeque::new(),
            mem: ahash::HashMap::new(),
            last_error_writes: Vec::new(),
            gs_base: wie_cpu::GS_BASE,
            set_gs_calls: Vec::new(),
        }
    }

    /// A scripted stop on a fake-API hook at `va`.
    fn hook_run(va: u64) -> Result<RunUntilHook, CpuError> {
        Ok(RunUntilHook {
            code: CodeHookOutcome {
                hit: true,
                address: va,
                size: 1,
            },
            invalid_memory: InvalidMemoryAccess::default(),
        })
    }

    /// A scripted pure-compute slice (no hook, no fault).
    fn compute_run() -> Result<RunUntilHook, CpuError> {
        Ok(RunUntilHook {
            code: CodeHookOutcome::default(),
            invalid_memory: InvalidMemoryAccess::default(),
        })
    }

    impl CpuEngine for MockEngine {
        fn mem_map(
            &mut self,
            _address: u64,
            _size: usize,
            _perms: wie_cpu::RwxPerms,
        ) -> Result<(), CpuError> {
            Ok(())
        }
        fn mem_write(&mut self, address: u64, bytes: &[u8]) -> Result<(), CpuError> {
            for (i, b) in bytes.iter().enumerate() {
                self.mem.insert(address.wrapping_add(i as u64), *b);
            }
            // Record last-error writes at the engine's GS-relative TEB slot —
            // the same address the runtime helpers publish to.
            if address == self.gs_base + wie_cpu::guest_layout::TEB_LAST_ERROR_OFFSET
                && bytes.len() >= 4
            {
                let mut raw = [0_u8; 4];
                raw.copy_from_slice(&bytes[..4]);
                self.last_error_writes.push(u32::from_le_bytes(raw));
            }
            Ok(())
        }
        fn mem_read(&mut self, address: u64, bytes: &mut [u8]) -> Result<(), CpuError> {
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = self
                    .mem
                    .get(&address.wrapping_add(i as u64))
                    .copied()
                    .unwrap_or(0);
            }
            Ok(())
        }
        fn install_runtime_hooks(
            &mut self,
            _hook_begin: u64,
            _hook_end: u64,
            _stop_bitmap: std::sync::Arc<[u8]>,
        ) -> Result<(), CpuError> {
            Ok(())
        }
        fn run_until_stop(
            &mut self,
            begin: u64,
            _until: u64,
            _timeout: u64,
            count: usize,
            _hook_begin: u64,
            _hook_end: u64,
        ) -> Result<RunUntilHook, CpuError> {
            self.run_calls.push((begin, count));
            match self.script.pop_front() {
                Some(outcome) => outcome,
                None => Err(CpuError::Message("mock script exhausted".into())),
            }
        }
        fn return_from_win64_api(&mut self, rax: u64) -> Result<u64, CpuError> {
            self.rax = rax;
            Ok(0x1400_1001)
        }
        fn read_rip(&mut self) -> Result<u64, CpuError> {
            Ok(self.rip)
        }
        fn write_rip(&mut self, value: u64) -> Result<(), CpuError> {
            self.rip = value;
            Ok(())
        }
        fn read_rsp(&mut self) -> Result<u64, CpuError> {
            Ok(self.rsp)
        }
        fn write_rsp(&mut self, value: u64) -> Result<(), CpuError> {
            self.rsp = value;
            Ok(())
        }
        fn read_rax(&mut self) -> Result<u64, CpuError> {
            Ok(self.rax)
        }
        fn write_rax(&mut self, value: u64) -> Result<(), CpuError> {
            self.rax = value;
            Ok(())
        }
        fn read_rcx(&mut self) -> Result<u64, CpuError> {
            Ok(self.rcx)
        }
        fn write_rcx(&mut self, value: u64) -> Result<(), CpuError> {
            self.rcx = value;
            Ok(())
        }
        fn read_rdx(&mut self) -> Result<u64, CpuError> {
            Ok(0)
        }
        fn write_rdx(&mut self, _value: u64) -> Result<(), CpuError> {
            Ok(())
        }
        fn read_r8(&mut self) -> Result<u64, CpuError> {
            Ok(0)
        }
        fn write_r8(&mut self, _value: u64) -> Result<(), CpuError> {
            Ok(())
        }
        fn read_r9(&mut self) -> Result<u64, CpuError> {
            Ok(0)
        }
        fn write_r9(&mut self, _value: u64) -> Result<(), CpuError> {
            Ok(())
        }
        fn read_rbx(&mut self) -> Result<u64, CpuError> {
            Ok(0)
        }
        fn read_r12(&mut self) -> Result<u64, CpuError> {
            Ok(0)
        }
        fn set_gs_base(&mut self, base: u64) {
            self.gs_base = base;
            self.set_gs_calls.push(base);
        }
        fn gs_base(&self) -> u64 {
            self.gs_base
        }
    }

    // ── Test fixtures ───────────────────────────────────────────────────

    /// A config + shared WinAPI state for the primary thread of a fake guest.
    fn test_env() -> (
        ProcessConfig,
        Arc<Mutex<WinApiState>>,
        crate::mt_runtime::LockWaitStats,
    ) {
        let process = ProcessIdentity {
            module_file_name: "quantum-test.exe".to_owned(),
            module_path: r"C:\quantum-test.exe".to_owned(),
            current_directory: r"C:\".to_owned(),
            command_line: "quantum-test.exe".to_owned(),
        };
        let winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        let config = ProcessConfig {
            soft_apis: SoftApiTable::default(),
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
            stop_bitmap: Arc::<[u8]>::from(Vec::<u8>::new()),
            primary_tid: GuestTid(wie_winapi::PRIMARY_THREAD_ID),
            static_dll_mains: Vec::new(),
        };
        (
            config,
            Arc::new(Mutex::new(winapi_state)),
            crate::mt_runtime::LockWaitStats::new(),
        )
    }

    fn core_for<'a>(
        engine: &'a mut MockEngine,
        config: &'a ProcessConfig,
        winapi: &'a Arc<Mutex<WinApiState>>,
        stats: &'a crate::mt_runtime::LockWaitStats,
    ) -> QuantumCore<'a> {
        QuantumCore::new(engine, config, winapi, config.primary_tid.0, stats).expect("core")
    }

    /// Hooks that use the default (worker) behavior everywhere — for tests of
    /// the shared SEH / resolve / dispatch mechanics.
    struct DefaultHooks;

    impl QuantumHooks for DefaultHooks {}

    /// Recording hooks with scripted responses for the primary-side branches.
    #[derive(Default)]
    struct TestHooks {
        /// `zero_rip_begin` response (`None` = worker exit).
        zero_rip: Option<u64>,
        /// `prepare_quantum` response (the per-quantum API index).
        api_index: usize,
        /// `dispatch` response.
        dispatch_step: Option<Step>,
        /// `claim_hook_locked` response.
        claim_step: Option<Step>,
        /// `on_run_fault` response.
        run_fault_step: Option<Step>,
        /// Records: (library, name, api_index) of each dispatch call.
        dispatch_calls: Vec<(String, String, usize)>,
        /// Number of `on_callback_return` calls.
        callback_calls: usize,
        /// Claimed hook addresses.
        claim_calls: Vec<u64>,
        /// Number of `on_seh_continue` observations.
        seh_continues: usize,
        /// Number of `prepare_first_quantum` calls.
        prepare_first_calls: usize,
    }

    impl QuantumHooks for TestHooks {
        fn prepare_quantum(&mut self, _core: &mut QuantumCore) -> Result<usize> {
            Ok(self.api_index)
        }
        fn prepare_first_quantum(&mut self, _core: &mut QuantumCore) -> Result<()> {
            self.prepare_first_calls = self.prepare_first_calls.saturating_add(1);
            Ok(())
        }
        fn zero_rip_begin(&mut self, _core: &mut QuantumCore) -> Result<Option<u64>> {
            Ok(self.zero_rip)
        }
        fn on_run_fault(
            &mut self,
            _core: &mut QuantumCore,
            _run: &RunUntilHook,
            _error: Option<&CpuError>,
        ) -> Result<Option<Step>> {
            Ok(self.run_fault_step.clone())
        }
        fn claim_hook_locked(
            &mut self,
            _core: &mut QuantumCore,
            _st: &mut WinApiState,
            address: u64,
        ) -> Result<Option<Step>> {
            self.claim_calls.push(address);
            Ok(self.claim_step.clone())
        }
        fn on_callback_return(
            &mut self,
            _core: &mut QuantumCore,
            _guard: MutexGuard<'_, WinApiState>,
            _address: u64,
            _api_index: usize,
        ) -> Result<Step> {
            self.callback_calls = self.callback_calls.saturating_add(1);
            Ok(Step::Next)
        }
        fn on_seh_continue(
            &mut self,
            _core: &mut QuantumCore,
            _api_index: usize,
            _address: u64,
            _return_value: u64,
            _return_address: u64,
        ) {
            self.seh_continues = self.seh_continues.saturating_add(1);
        }
        fn dispatch(
            &mut self,
            _core: &mut QuantumCore,
            _guard: MutexGuard<'_, WinApiState>,
            resolved: &ResolvedFakeApi,
            _hook_address: u64,
            api_index: usize,
        ) -> Result<Step> {
            self.dispatch_calls.push((
                resolved.library.to_string(),
                resolved.name.to_string(),
                api_index,
            ));
            Ok(self.dispatch_step.clone().unwrap_or(Step::Next))
        }
    }

    // ── State-machine tests ─────────────────────────────────────────────

    /// One quantum with a resolved soft-API stop: the thread is activated,
    /// its per-thread last-error is published into the engine's GS-relative
    /// TEB slot before the run, and the dispatch hook receives the resolved
    /// metadata.
    #[test]
    fn step_activates_and_publishes_last_error_before_dispatch() {
        let (mut config, winapi, stats) = test_env();
        let (soft_va, _) = config
            .soft_apis
            .intern("KERNEL32.dll", "QuantSink", 0)
            .expect("intern");
        let mut engine = mock_engine();
        engine.script.push_back(hook_run(soft_va));
        let primary = config.primary_tid.0;
        {
            let mut st = lock(&winapi);
            st.kernel.threads.active.last_error = 0x2A;
        }
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks::default();
        let step = core.step(&mut hooks).expect("step");

        assert!(matches!(step, Step::Next));
        // Drop the core so the engine (borrowed by it) is inspectable again.
        let _ = core;
        assert_eq!(engine.run_calls.len(), 1);
        assert_eq!(
            engine.run_calls[0].0, 0x1400_1000,
            "runs from the stored RIP"
        );
        let st = lock(&winapi);
        assert_eq!(
            st.kernel.threads.active.tid, primary,
            "primary stays active"
        );
        drop(st);
        assert_eq!(
            engine.last_error_writes.last(),
            Some(&0x2A),
            "the active thread's last-error reached the engine's GS-relative TEB slot"
        );
        assert_eq!(hooks.dispatch_calls.len(), 1);
        assert_eq!(hooks.dispatch_calls[0].1, "QuantSink");
    }

    /// RIP 0 before entry: the hook's entry VA becomes the run begin.
    #[test]
    fn zero_rip_begin_resumes_from_entry_va() {
        let (mut config, winapi, stats) = test_env();
        let (soft_va, _) = config
            .soft_apis
            .intern("KERNEL32.dll", "QuantSink", 0)
            .expect("intern");
        let mut engine = mock_engine();
        engine.rip = 0;
        engine.script.push_back(hook_run(soft_va));
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks {
            zero_rip: Some(0x1400_1000),
            ..TestHooks::default()
        };

        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(step, Step::Next));
        assert_eq!(engine.run_calls[0].0, 0x1400_1000);
    }

    /// RIP 0 with no entry VA (worker): the thread exits with the low u32 of
    /// RAX and never runs a guest quantum.
    #[test]
    fn zero_rip_without_entry_exits_with_rax() {
        let (config, winapi, stats) = test_env();
        let mut engine = mock_engine();
        engine.rip = 0;
        engine.rax = 0x2A;
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks::default();

        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(step, Step::ExitThread(0x2A)));
        assert!(
            engine.run_calls.is_empty(),
            "no guest run for an exited thread"
        );
    }

    /// A pure-compute slice (no hook, no fault) yields the worker's
    /// `PureCompute` step and never reaches resolution.
    #[test]
    fn pure_compute_slice_returns_pure_compute() {
        let (config, winapi, stats) = test_env();
        let mut engine = mock_engine();
        engine.script.push_back(compute_run());
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks::default();

        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(step, Step::PureCompute));
        assert!(hooks.dispatch_calls.is_empty());
    }

    /// A hook whose `on_activated` re-affirms the engine's GS base — the
    /// exact semantics of the runtime's worker hooks (`mt_runtime.rs`), used
    /// here to pin the per-activation re-affirmation contract.
    struct TebReaffirmHooks {
        /// The TEB page VA this thread's engine must be bound to.
        teb_va: u64,
        /// Number of `on_activated` calls (one per quantum).
        activations: usize,
    }

    impl QuantumHooks for TebReaffirmHooks {
        fn on_activated(
            &mut self,
            core: &mut QuantumCore,
            _st: &mut WinApiState,
        ) -> Result<Option<Step>> {
            self.activations = self.activations.saturating_add(1);
            core.engine().set_gs_base(self.teb_va);
            Ok(None)
        }
    }

    /// Every quantum re-affirms the engine's GS base at activation: a worker's
    /// engine stays bound to ITS TEB page, never drifting to the fixed
    /// primary `GS_BASE`.
    #[test]
    fn activation_reaffirms_engine_gs_base_each_quantum() {
        let (config, winapi, stats) = test_env();
        let worker_teb_va = 0x0000_7000_0040_C000_u64;
        let mut engine = mock_engine();
        // Two pure-compute quanta: both activate (and re-affirm) without any
        // host dispatch, keeping the test focused on the binding.
        engine.script.push_back(compute_run());
        engine.script.push_back(compute_run());
        assert_eq!(
            engine.gs_base,
            wie_cpu::GS_BASE,
            "engine starts at the fixed GS_BASE"
        );
        let mut hooks = TebReaffirmHooks {
            teb_va: worker_teb_va,
            activations: 0,
        };
        {
            let mut core = core_for(&mut engine, &config, &winapi, &stats);
            assert!(matches!(
                core.step(&mut hooks).expect("first step"),
                Step::PureCompute
            ));
            assert!(matches!(
                core.step(&mut hooks).expect("second step"),
                Step::PureCompute
            ));
        }

        assert_eq!(hooks.activations, 2, "activation runs once per quantum");
        assert_eq!(
            engine.gs_base, worker_teb_va,
            "the engine stays bound to the worker's TEB page"
        );
        assert_eq!(
            engine.set_gs_calls,
            vec![worker_teb_va, worker_teb_va],
            "the binding is re-affirmed on every activation"
        );
    }

    /// The primary thread's engine is never rebound: with the default (no-op)
    /// activation hook it keeps the fixed `GS_BASE`.
    #[test]
    fn primary_engine_keeps_gs_base_through_quanta() {
        let (config, winapi, stats) = test_env();
        let mut engine = mock_engine();
        engine.script.push_back(compute_run());
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = DefaultHooks;

        assert!(matches!(
            core.step(&mut hooks).expect("step"),
            Step::PureCompute
        ));
        let _ = core;
        assert_eq!(
            engine.gs_base,
            wie_cpu::GS_BASE,
            "primary never leaves the fixed GS_BASE"
        );
        assert!(
            engine.set_gs_calls.is_empty(),
            "the default hooks never call set_gs_base"
        );
    }

    /// `on_run_fault` claims an invalid-memory stop before the shared SEH
    /// handling runs (worker semantics: hard thread exit, no guest SEH).
    #[test]
    fn run_fault_hook_claims_invalid_memory_as_thread_exit() {
        let (config, winapi, stats) = test_env();
        let mut engine = mock_engine();
        engine.script.push_back(Ok(RunUntilHook {
            code: CodeHookOutcome::default(),
            invalid_memory: InvalidMemoryAccess {
                hit: true,
                exception_code: wie_cpu::exception_code::ACCESS_VIOLATION,
                address: 0x5678,
                ..Default::default()
            },
        }));
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks {
            run_fault_step: Some(Step::ExitThread(1)),
            ..TestHooks::default()
        };

        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(step, Step::ExitThread(1)));
    }

    /// The SEH continuation trampoline runs the shared mechanics; with no
    /// pending SEH state it stops the session and never journals.
    #[test]
    fn seh_continue_trampoline_without_pending_stops() {
        let (config, winapi, stats) = test_env();
        let mut engine = mock_engine();
        engine
            .script
            .push_back(hook_run(wie_winapi::seh_continue_trampoline_va()));
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks::default();

        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(
            step,
            Step::Stop(EntryTraceTermination::RuntimeStop(_))
        ));
        assert_eq!(hooks.seh_continues, 0, "no journal observation on failure");
        assert!(hooks.dispatch_calls.is_empty());
    }

    /// The guest-callback return trampoline is delegated to the hooks with the
    /// lock guard (the primary drops it while completing the callback frame).
    #[test]
    fn callback_trampoline_delegates_to_hooks() {
        let (config, winapi, stats) = test_env();
        let mut engine = mock_engine();
        engine
            .script
            .push_back(hook_run(DEFAULT_LAYOUT.callback_return_trampoline_va));
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks::default();

        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(step, Step::Next));
        assert_eq!(hooks.callback_calls, 1, "trampoline reached the hooks");
        assert!(
            hooks.dispatch_calls.is_empty(),
            "callback completion never dispatches a fake API"
        );
    }

    /// A hook stop outside the fake-API range is an unresolved fake API: the
    /// session stops with a diagnostic instead of dispatching.
    #[test]
    fn unresolved_fake_api_stops_session() {
        let (config, winapi, stats) = test_env();
        let mut engine = mock_engine();
        engine.script.push_back(hook_run(0x1234));
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks::default();

        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(
            step,
            Step::Stop(EntryTraceTermination::RuntimeStop(_))
        ));
        assert!(hooks.dispatch_calls.is_empty());
    }

    /// The default (worker) dispatch policy handles ExitProcess: it marks the
    /// process dying, flushes console state, and exits the thread with RCX.
    #[test]
    fn default_dispatch_handles_exit_process() {
        let (mut config, winapi, stats) = test_env();
        let (exit_va, _) = config
            .soft_apis
            .intern("KERNEL32.dll", "ExitProcess", 0)
            .expect("intern");
        let mut engine = mock_engine();
        engine.rcx = 42;
        engine.script.push_back(hook_run(exit_va));
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = DefaultHooks;

        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(step, Step::ExitThread(42)));
        let _ = core;
        let st = lock(&winapi);
        assert!(st.kernel.sync.process_dying);
    }

    /// `claim_hook_locked` runs before the callback trampoline / SEH / resolve
    /// path and can claim the hook (primary: the static DllMain return).
    #[test]
    fn claim_hook_locked_claims_the_stop() {
        let (config, winapi, stats) = test_env();
        let mut engine = mock_engine();
        engine.script.push_back(hook_run(0x9999));
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks {
            claim_step: Some(Step::Next),
            ..TestHooks::default()
        };

        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(step, Step::Next));
        assert_eq!(hooks.claim_calls, vec![0x9999]);
        assert!(
            hooks.dispatch_calls.is_empty(),
            "claimed hooks skip dispatch"
        );
    }

    /// `prepare_first_quantum` runs once before the loop, and the per-quantum
    /// API index flows from `prepare_quantum` into `dispatch`.
    #[test]
    fn api_index_flows_from_prepare_into_dispatch() {
        let (mut config, winapi, stats) = test_env();
        let (soft_va, _) = config
            .soft_apis
            .intern("KERNEL32.dll", "QuantSink", 0)
            .expect("intern");
        let mut engine = mock_engine();
        engine.script.push_back(hook_run(soft_va));
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks {
            api_index: 7,
            ..TestHooks::default()
        };

        hooks
            .prepare_first_quantum(&mut core)
            .expect("prepare first");
        let step = core.step(&mut hooks).expect("step");
        assert!(matches!(step, Step::Next));
        assert_eq!(hooks.prepare_first_calls, 1);
        assert_eq!(hooks.dispatch_calls[0].2, 7, "api index reaches dispatch");
    }

    /// A deterministic multi-quantum stop sequence: pure compute, a resolved
    /// dispatch, then ExitProcess — the state machine transitions through
    /// each step with the same core and hooks.
    #[test]
    fn deterministic_stop_sequence_across_quanta() {
        let (mut config, winapi, stats) = test_env();
        let (soft_va, _) = config
            .soft_apis
            .intern("KERNEL32.dll", "QuantSink", 0)
            .expect("intern");
        let (exit_va, _) = config
            .soft_apis
            .intern("KERNEL32.dll", "ExitProcess", 0)
            .expect("intern");
        let mut engine = mock_engine();
        engine.rcx = 7;
        engine.script.push_back(compute_run());
        engine.script.push_back(hook_run(soft_va));
        engine.script.push_back(hook_run(exit_va));
        let mut core = core_for(&mut engine, &config, &winapi, &stats);
        let mut hooks = TestHooks::default();

        let s1 = core.step(&mut hooks).expect("quantum 1");
        assert!(matches!(s1, Step::PureCompute));
        let s2 = core.step(&mut hooks).expect("quantum 2");
        assert!(matches!(s2, Step::Next));
        // The third quantum runs under the default (worker) dispatch policy so
        // the ExitProcess stop produces the real thread-exit transition.
        let mut defaults = DefaultHooks;
        let s3 = core.step(&mut defaults).expect("quantum 3");
        assert!(matches!(s3, Step::ExitThread(7)));
        assert_eq!(hooks.dispatch_calls.len(), 1);
        assert_eq!(hooks.dispatch_calls[0].0, "KERNEL32.dll");
    }

    /// The guest-side wait instrumentation: `with_locked` blocks on a
    /// peer-held `shared_winapi` and the wait lands in the guest-side stats
    /// (total + max) while profiling is enabled.
    #[test]
    fn with_locked_times_guest_wait_when_enabled() {
        let (config, winapi, stats) = test_env();
        stats.set_enabled(true);
        let mut engine = mock_engine();
        let state_arc = Arc::clone(&winapi);
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = lock(&state_arc);
            let _ = tx.send(());
            std::thread::sleep(std::time::Duration::from_millis(10));
        });
        let _ = rx.recv();
        {
            let mut core = core_for(&mut engine, &config, &winapi, &stats);
            core.with_locked(|_engine, _st| {});
        }
        let _ = holder.join();
        let snap = stats.snapshot();
        assert!(
            snap.guest_total_ns >= 5_000_000,
            "the 10 ms held lock must be observed on the guest side (got {} ns)",
            snap.guest_total_ns
        );
        assert!(
            snap.guest_max_ns >= 5_000_000,
            "the guest max must capture the same wait (got {} ns)",
            snap.guest_max_ns
        );
        assert_eq!(snap.presenter_total_ns, 0);
        assert_eq!(snap.presenter_max_ns, 0);
    }
}
