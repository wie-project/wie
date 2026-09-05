//! `run_until_stop` quantum loop and quiescent drain for the session.
//!
//! The per-quantum state machine (activate → run → resolve → dispatch →
//! park → finish) lives in `crate::quantum`; this module drives it for the
//! PRIMARY thread and supplies the primary-only behaviors through
//! [`SessionPumpHooks`]: the static DllMain phase, guest-callback completion,
//! session journaling, the host bridges, and child-process spawns.

use super::RuntimeProfile;
use super::callback::{GuestCallbackCompletion, PendingGuestCallback};
use super::journal_api_return;
use crate::hooks::ResolvedFakeApi;
use crate::quantum::{QuantumCore, QuantumHooks, Step};
use crate::trace::{EntryTraceEvent, EntryTraceTermination, RuntimeRunSummary};
use ahash::HashMap;
use anyhow::{Context, Result};
use std::borrow::Cow;
use std::sync::{Arc, MutexGuard};
use std::time::Instant;
use wie_cpu::CpuError;
use wie_winapi::{
    GuestCallbackRequest, HostParkReason, KernelHandle, WinApiControlSignal, WinApiState,
    dll_loader,
};

/// Primary-thread hook state for one `run_until_stop` segment.
///
/// Holds `&mut` references to the session fields the quantum loop touches and
/// the per-segment counters the journaling arms mutate. Process access goes
/// through the [`QuantumCore`] the hooks receive, never through the session.
struct SessionPumpHooks<'a> {
    /// PE entry-point VA to dispatch when RIP is 0 (before entry is reached).
    entry_point_va: u64,
    /// Whether the guest entry point has been reached.
    entry_reached: &'a mut bool,
    /// Next API-stop index (journal ordering; undone on message yield / CS park).
    next_api_index: &'a mut usize,
    /// Consecutive no-hook slice counter (session stop diagnostic).
    no_hook_slices: &'a mut usize,
    /// In-flight bridged guest callbacks.
    pending_callbacks: &'a mut Vec<PendingGuestCallback>,
    /// Cache of `Arc<str>` copies of callback-outer API names, so every
    /// bridged window message clones a refcounted string instead of
    /// allocating a fresh `Arc` box + copy per message.
    outer_api_names: &'a mut HashMap<String, Arc<str>>,
    /// Whether `WIE_RUNTIME_PROFILE` is active for this session.
    profile_enabled: bool,
    /// Accumulated host-side profile.
    profile: &'a mut RuntimeProfile,
    /// Journaled API events for the run summary.
    events: &'a mut Vec<EntryTraceEvent>,
    /// Statically-loaded guest DLLs awaiting `DllMain(PROCESS_ATTACH)`, in
    /// load order (dependencies before dependents).
    static_dll_mains: Vec<dll_loader::StaticDllMain>,
    /// Index of the next static DllMain to run.
    dll_main_index: usize,
    /// Trampoline each static DllMain returns through.
    dll_main_return_va: u64,
    /// Guest TID of the primary (entry-point) thread.
    primary_tid: u32,
    /// APIs charged toward `max_api` this run segment.
    charged_api: usize,
    /// Fast-path (noisy) API stops not charged toward `max_api`.
    noisy_api: usize,
}

impl<'a> SessionPumpHooks<'a> {
    /// Shared native-panel bridge dispatch (file dialog, message box, print
    /// dialog, page setup, print job). Take the bridge out first (it lives
    /// behind the shared state lock), then drop the guard: the native panel
    /// blocks the MAIN thread for its whole session, and the winit event loop
    /// needs that SAME lock to service frame/user events while the panel is up
    /// (take_frame, reconcile, hit-testing) — holding it across the bridge
    /// deadlocks into the beachball. This is the GuestCallbackRequested
    /// pattern. On return, under the lock again: reactivate the primary
    /// thread, restore the bridge, write the outcome into the pending slot.
    ///
    /// Continue: the engine re-executes the fake API stop, the handler
    /// re-enters and consumes the pending write-back.
    fn dispatch_native_bridge<B, P>(
        &self,
        core: &mut QuantumCore,
        mut guard: MutexGuard<'_, WinApiState>,
        take_bridge: impl FnOnce(&mut WinApiState) -> Option<B>,
        invoke: impl FnOnce(Option<&B>) -> P,
        finish: impl FnOnce(&mut WinApiState, Option<B>, P),
    ) -> Step {
        let bridge = take_bridge(&mut guard);
        drop(guard);
        let outcome = invoke(bridge.as_ref());
        core.with_locked(|_, winapi_state| {
            if winapi_state.kernel.threads.active.tid != self.primary_tid {
                winapi_state.kernel.threads.activate(self.primary_tid);
            }
            finish(winapi_state, bridge, outcome);
        });
        Step::Next
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        entry_point_va: u64,
        entry_reached: &'a mut bool,
        next_api_index: &'a mut usize,
        no_hook_slices: &'a mut usize,
        pending_callbacks: &'a mut Vec<PendingGuestCallback>,
        outer_api_names: &'a mut HashMap<String, Arc<str>>,
        profile_enabled: bool,
        profile: &'a mut RuntimeProfile,
        events: &'a mut Vec<EntryTraceEvent>,
        static_dll_mains: Vec<dll_loader::StaticDllMain>,
        dll_main_return_va: u64,
        primary_tid: u32,
    ) -> Self {
        Self {
            entry_point_va,
            entry_reached,
            next_api_index,
            no_hook_slices,
            pending_callbacks,
            outer_api_names,
            profile_enabled,
            profile,
            events,
            static_dll_mains,
            dll_main_index: 0,
            dll_main_return_va,
            primary_tid,
            charged_api: 0,
            noisy_api: 0,
        }
    }

    /// Publish the ACTIVE (primary) thread's last-error into the primary
    /// engine's GS-relative TEB slot so in-guest `GetLastError` stubs stay
    /// coherent with host-side API failures.
    ///
    /// Runs after a bridged guest callback completes (guest WndProc code may
    /// have `SetLastError`'d through the in-guest stub since the last
    /// dispatch): absorb the engine's TEB slot into the primary's per-thread
    /// slot first, then publish it back. Re-activates the primary like the
    /// bridge arms — a worker may have claimed `active` while the callback ran
    /// on the primary engine without the WinAPI lock.
    fn publish_last_error_to_guest(&mut self, core: &mut QuantumCore) {
        let primary_tid = self.primary_tid;
        core.with_locked(|engine, st| {
            if st.kernel.threads.active.tid != primary_tid {
                st.kernel.threads.activate(primary_tid);
            }
            st.absorb_guest_last_error(engine);
            st.publish_last_error_to_guest(engine);
        });
    }

    /// Returns a shareable `Arc<str>` for a callback-outer API name, caching
    /// the first conversion so every subsequent bridged window message clones
    /// a refcounted string (refcount bump) instead of allocating a fresh
    /// `Arc` box + string copy per message.
    fn intern_outer_api_name(&mut self, name: Cow<'static, str>) -> Arc<str> {
        if let Some(arc) = self.outer_api_names.get(name.as_ref()) {
            return Arc::clone(arc);
        }
        let arc: Arc<str> = Arc::from(name);
        self.outer_api_names
            .insert(arc.to_string(), Arc::clone(&arc));
        arc
    }

    /// Sets up Win64 calling convention and transfers control to a guest WndProc.
    ///
    /// Stack layout below the original `DispatchMessageA` frame:
    /// ```text
    /// [dispatch_rsp]      return address of DispatchMessageA caller
    /// [dispatch_rsp-8]    alignment padding
    /// [dispatch_rsp-0x28] 32-byte shadow space
    /// [dispatch_rsp-0x30] trampoline return address  ← new RSP / WndProc entry
    /// ```
    fn begin_guest_callback(
        &mut self,
        core: &mut QuantumCore,
        request: GuestCallbackRequest,
        outer_library: Arc<str>,
        outer_name: Arc<str>,
        outer_fake_va: u64,
    ) -> Result<()> {
        let trampoline = core.layout().callback_return_trampoline_va;
        let dispatch_rsp = core.with_locked(|engine, _| {
            crate::guest_callback::install_guest_callback_frame(engine, &request, trampoline)
        })?;

        self.pending_callbacks.push(PendingGuestCallback {
            dispatch_rsp,
            request,
            outer_library,
            outer_name,
            outer_fake_va,
            outer_return: request.outer_return,
            enumeration_id: None,
        });

        Ok(())
    }

    /// Begins a guest `FONTENUMPROC` callback for one item of a font
    /// enumeration.
    ///
    /// `dispatch_rsp` is the RSP the frame is installed relative to: the
    /// current RSP for the first item, or the ORIGINAL outer API RSP for a
    /// continuation (so the final completion restores the true outer frame
    /// even though each re-entry grows the stack down by 0x30).
    #[allow(clippy::too_many_arguments)] // one arg per Win64 callback slot
    fn begin_guest_enum_callback(
        &mut self,
        core: &mut QuantumCore,
        request: GuestCallbackRequest,
        enumeration_id: u64,
        outer_library: Arc<str>,
        outer_name: Arc<str>,
        outer_fake_va: u64,
        dispatch_rsp: u64,
    ) -> Result<()> {
        let trampoline = core.layout().callback_return_trampoline_va;
        core.with_locked(|engine, _| {
            crate::guest_callback::install_guest_enum_callback_frame(
                engine,
                &request,
                trampoline,
                dispatch_rsp,
            )
        })?;

        self.pending_callbacks.push(PendingGuestCallback {
            dispatch_rsp,
            request,
            outer_library,
            outer_name,
            outer_fake_va,
            outer_return: request.outer_return,
            enumeration_id: Some(enumeration_id),
        });

        Ok(())
    }

    /// Completes the most recent guest WndProc and returns from the outer host API.
    fn complete_guest_callback(
        &mut self,
        core: &mut QuantumCore,
    ) -> Result<GuestCallbackCompletion> {
        let pending = self
            .pending_callbacks
            .pop()
            .context("callback trampoline hit without a pending guest callback")?;

        // Read the callback's return value (RAX) without restoring the frame
        // yet — a font-enumeration continuation needs it to decide whether to
        // re-enter with the next item.
        let lresult = core
            .with_locked(|engine, _| engine.read_rax())
            .context("failed to read guest callback return value")?;

        // Full-iteration continuation: while the callback returns non-zero and
        // more items remain, re-enter with the next item instead of completing
        // the outer API. One-shot callers (EnumWindows, WndProc dispatch) have
        // `enumeration_id == None` and always fall through to completion.
        if let Some(enumeration_id) = pending.enumeration_id
            && lresult != 0
        {
            let next = core.with_locked(|engine, winapi_state| {
                wie_winapi::gdi32::enumerate::advance_enumeration(
                    engine,
                    winapi_state,
                    enumeration_id,
                )
            })?;
            if let Some(next_request) = next {
                // Re-enter with the next item, preserving the ORIGINAL outer
                // frame RSP so the final completion restores it correctly.
                self.begin_guest_enum_callback(
                    core,
                    next_request,
                    enumeration_id,
                    Arc::clone(&pending.outer_library),
                    Arc::clone(&pending.outer_name),
                    pending.outer_fake_va,
                    pending.dispatch_rsp,
                )?;
                return Ok(GuestCallbackCompletion {
                    outer_library: pending.outer_library,
                    outer_name: pending.outer_name,
                    outer_fake_va: pending.outer_fake_va,
                    return_value: lresult,
                    // Not used for a continuation: the pump re-enters the loop
                    // and the next callback runs on the following iteration.
                    return_address: 0,
                });
            }
        }

        let (return_value, return_address) = core.with_locked(|engine, _| {
            crate::guest_callback::finish_guest_callback(
                engine,
                pending.dispatch_rsp,
                pending.outer_return,
            )
        })?;

        tracing::debug!(
            outer_library = %pending.outer_library.as_ref(),
            outer_name = %pending.outer_name.as_ref(),
            callback = pending.request.callback_address,
            hwnd = pending.request.window_handle,
            message = pending.request.message,
            return_value,
            resume = return_address,
            "completed guest window callback"
        );

        Ok(GuestCallbackCompletion {
            outer_library: pending.outer_library,
            outer_name: pending.outer_name,
            outer_fake_va: pending.outer_fake_va,
            return_value,
            return_address,
        })
    }
}

impl QuantumHooks for SessionPumpHooks<'_> {
    fn prepare_quantum(&mut self, core: &mut QuantumCore) -> Result<usize> {
        let index = *self.next_api_index;
        *self.next_api_index = self
            .next_api_index
            .checked_add(1)
            .context("runtime API index overflow")?;

        // Refresh the host-written guest clock table ahead of this quantum so
        // the in-guest clock stubs (GetTickCount / timeGetTime / QPC / …)
        // observe advancing values with no host stop. Frozen under
        // `WIE_FIXED_CLOCK=1` — the table was written once at session init,
        // so a guest busy-waiting on a constant never spins.
        if !wie_winapi::kernel32::clock::clock_is_fixed() {
            let clock_table_va = core.layout().clock_table.base;
            let _ = crate::guest_stubs::refresh_clock_table(core.engine(), clock_table_va);
        }
        Ok(index)
    }

    fn prepare_first_quantum(&mut self, core: &mut QuantumCore) -> Result<()> {
        // Static-dependency DllMain(PROCESS_ATTACH) init phase: Windows calls
        // each static dep's DllMain in load order (dependencies first) BEFORE
        // the exe entry point. Runs once, on the first quantum, guarded by
        // `entry_reached`; completing the phase resets RIP to 0 so the next
        // iteration dispatches the exe entry. Skipped entirely when the
        // budget is zero (preparing would push a return address nothing ever
        // pops) — the caller gates on `max_api > 0`.
        if let Some(first) = self.static_dll_mains.first() {
            dll_loader::prepare_dll_main_call(
                core.engine(),
                first.image_base,
                first.entry_rva,
                dll_loader::DLL_PROCESS_ATTACH,
                1, // lpvReserved: non-zero marks a static (loader) call.
                self.dll_main_return_va,
            )
            .context("failed to prepare first static DllMain call")?;
        }
        Ok(())
    }

    fn zero_rip_begin(&mut self, _core: &mut QuantumCore) -> Result<Option<u64>> {
        if !*self.entry_reached {
            *self.entry_reached = true;
            tracing::info!(
                target: "wiegui",
                entry = self.entry_point_va,
                "guest entry reached"
            );
        }
        Ok(Some(self.entry_point_va))
    }

    fn claim_hook_locked(
        &mut self,
        core: &mut QuantumCore,
        _st: &mut WinApiState,
        address: u64,
    ) -> Result<Option<Step>> {
        // A statically-loaded dependency's DllMain returned. RAX carries the
        // BOOL result; FALSE aborts process init (Windows
        // STATUS_DLL_INIT_FAILED semantics) — the exe entry never runs.
        if !*self.entry_reached && address == self.dll_main_return_va {
            let dll_ok = core
                .engine()
                .read_rax()
                .context("failed to read RAX after static DllMain")?;
            let name = self
                .static_dll_mains
                .get(self.dll_main_index)
                .map(|m| m.name.as_str())
                .unwrap_or("?");
            if dll_ok == 0 {
                return Ok(Some(Step::Stop(EntryTraceTermination::RuntimeStop(
                    format!(
                        "{name}!DllMain returned FALSE (DLL_PROCESS_ATTACH) \
                     — process init aborted"
                    ),
                ))));
            }
            self.dll_main_index = self
                .dll_main_index
                .checked_add(1)
                .context("static DllMain index overflow")?;
            if let Some(next) = self.static_dll_mains.get(self.dll_main_index) {
                dll_loader::prepare_dll_main_call(
                    core.engine(),
                    next.image_base,
                    next.entry_rva,
                    dll_loader::DLL_PROCESS_ATTACH,
                    1,
                    self.dll_main_return_va,
                )
                .context("failed to prepare next static DllMain call")?;
            } else {
                // Init phase complete: reset RIP so the next iteration
                // dispatches the exe entry point.
                core.engine()
                    .write_rip(0)
                    .context("failed to reset RIP after static DllMain phase")?;
            }
            return Ok(Some(Step::Next));
        }
        Ok(None)
    }

    fn on_callback_return(
        &mut self,
        core: &mut QuantumCore,
        guard: MutexGuard<'_, WinApiState>,
        _address: u64,
        api_index: usize,
    ) -> Result<Step> {
        // complete_guest_callback needs the lock dropped (it re-locks through
        // the core) — release the guard before running it.
        drop(guard);
        match self.complete_guest_callback(core) {
            Ok(completion) => {
                self.charged_api = self.charged_api.saturating_add(1);
                self.events.push(EntryTraceEvent {
                    index: api_index,
                    library: completion.outer_library,
                    name: completion.outer_name,
                    fake_target_va: completion.outer_fake_va,
                    handled: true,
                    return_value: Some(completion.return_value),
                    return_address: Some(completion.return_address),
                });
                self.publish_last_error_to_guest(core);
                // Do NOT publish a frame here. A guest WndProc is one message
                // of a repaint cycle (parent BitBlt → child control paints
                // across several messages); publishing mid-cycle would emit a
                // frame with the children still missing. The drain happens
                // once at the empty-queue idle boundary
                // (WaitingForMessage) — one frame per full cycle.
                Ok(Step::Next)
            }
            Err(error) => Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
                "failed to complete guest callback: {error}"
            )))),
        }
    }

    fn on_seh_continue(
        &mut self,
        _core: &mut QuantumCore,
        api_index: usize,
        address: u64,
        return_value: u64,
        return_address: u64,
    ) {
        self.charged_api = self.charged_api.saturating_add(1);
        self.events.push(EntryTraceEvent {
            index: api_index,
            library: "ntdll.dll".into(),
            name: "SehContinue".into(),
            fake_target_va: address,
            handled: true,
            return_value: Some(return_value),
            return_address: Some(return_address),
        });
    }

    fn on_no_hook(&mut self, core: &mut QuantumCore, begin: u64) -> Result<Step> {
        let no_hook_limit = core.layout().no_hook_slice_limit;
        *self.no_hook_slices = self
            .no_hook_slices
            .checked_add(1)
            .context("no-hook slice count overflow")?;
        if *self.no_hook_slices == 1
            || self.no_hook_slices.is_multiple_of(5)
            || *self.no_hook_slices == no_hook_limit
        {
            let rip = core.engine().read_rip().context("rip after no-hook")?;
            let rsp = core.engine().read_rsp().context("rsp after no-hook")?;
            let rax = core.engine().read_rax().context("rax after no-hook")?;
            let rcx = core.engine().read_rcx().context("rcx after no-hook")?;
            let rdx = core.engine().read_rdx().context("rdx after no-hook")?;
            tracing::debug!(
                slice = *self.no_hook_slices,
                limit = no_hook_limit,
                begin,
                rip,
                rsp,
                rax,
                rcx,
                rdx,
                "runtime no-hook slice"
            );
        }
        if *self.no_hook_slices >= no_hook_limit {
            let rip = core.engine().read_rip().context("rip after no-hook")?;
            let rsp = core.engine().read_rsp().context("rsp after no-hook")?;
            let rax = core.engine().read_rax().context("rax after no-hook")?;
            let rcx = core.engine().read_rcx().context("rcx after no-hook")?;
            let rdx = core.engine().read_rdx().context("rdx after no-hook")?;
            return Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
                "emulation stopped without hitting fake API hook after {} slices: \
                 begin={begin:#018x}; rip={rip:#018x}; rsp={rsp:#018x}; \
                 rax={rax:#018x}; rcx={rcx:#018x}; rdx={rdx:#018x}; \
                 budget={}",
                *self.no_hook_slices,
                core.layout().instruction_budget,
            ))));
        }
        Ok(Step::Next)
    }

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
        let last_api = match self.events.last() {
            Some(e) => format!("{}!{}", e.library.as_ref(), e.name.as_ref()),
            None => "-".into(),
        };
        Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
            "emulation error (api_index={api_index}, last_api={last_api}): {error}; \
             rip={rip:#018x}; rsp={rsp:#018x}; [rsp+0x160]={slot_val:?}"
        ))))
    }

    fn wants_timing(&self) -> bool {
        self.profile_enabled
    }

    fn on_emu_time(&mut self, _core: &mut QuantumCore, ns: u128) {
        self.profile.add_emu_ns(ns);
    }

    fn on_resolved(&mut self, _core: &mut QuantumCore, ns: u128) {
        self.profile.add_resolve_ns(ns);
        self.profile.inc_host_stops();
    }

    fn dispatch(
        &mut self,
        core: &mut QuantumCore,
        mut guard: MutexGuard<'_, WinApiState>,
        resolved: &ResolvedFakeApi,
        hook_address: u64,
        api_index: usize,
    ) -> Result<Step> {
        tracing::trace!(
            api_index = api_index,
            api = %format!(
                "{}!{}",
                resolved.library.as_ref(),
                resolved.name.as_ref()
            ),
            "host API stop"
        );
        if self.profile_enabled {
            self.profile.inc_host_stops();
        }
        let export_key = if self.profile_enabled {
            Some(format!(
                "{}!{}",
                resolved.library.as_ref(),
                resolved.name.as_ref()
            ))
        } else {
            None
        };

        // WINMM `timeSetEvent` dispatch: if a timer is due, run its
        // `LPTIMECALLBACK` (`rcx=handle, rdx=0, r8=user_data, r9=0,
        // [rsp+0x28]=0`) before any other handler. At most one per
        // quantum, and never while a guest callback is already in flight
        // (reentrancy guard — matches the `pending_callbacks.is_empty()`
        // check at `HostPark`).
        //
        // The callback that itself calls `timeSetEvent` naturally enqueues a
        // future record (new `due_tick_ms`); remaining due timers wait for
        // the next boundary because this arm pops at most one entry per
        // quantum.
        if self.pending_callbacks.is_empty() {
            let now = {
                let raw = wie_winapi::kernel32::clock::tick_count_32();
                u32::try_from(raw & u64::from(u32::MAX)).unwrap_or(0)
            };
            let due = guard.pop_next_due_timer(now);
            if let Some(due) = due {
                let request = match due.kind {
                    wie_winapi::winmm::DueTimerKind::TimeEvent => {
                        wie_winapi::GuestCallbackRequest::timer(
                            due.handle,
                            due.callback_va,
                            due.user_data,
                        )
                    }
                    wie_winapi::winmm::DueTimerKind::WaveOutDone => {
                        wie_winapi::GuestCallbackRequest::wave_out_done(
                            due.handle,
                            due.callback_va,
                            due.user_data,
                            wie_winapi::winmm::WOM_DONE,
                        )
                    }
                };
                self.charged_api = self.charged_api.saturating_add(1);
                let outer_library = self.intern_outer_api_name(resolved.library.clone());
                let outer_name = self.intern_outer_api_name(resolved.name.clone());
                self.events.push(EntryTraceEvent {
                    index: api_index,
                    library: Arc::clone(&outer_library),
                    name: Arc::clone(&outer_name),
                    fake_target_va: hook_address,
                    handled: true,
                    return_value: None,
                    return_address: None,
                });
                drop(guard);
                if let Err(error) = self.begin_guest_callback(
                    core,
                    request,
                    outer_library,
                    outer_name,
                    hook_address,
                ) {
                    return Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
                        "failed to begin timer callback: {error}"
                    ))));
                }
                return Ok(Step::Next);
            }
        }

        if resolved.traits.exit_process() {
            let handler_t0 = self.profile_enabled.then(Instant::now);
            let exit_code_raw = core
                .engine()
                .read_rcx()
                .context("failed to read RCX for ExitProcess")?;
            let exit_code = u32::try_from(exit_code_raw & u64::from(u32::MAX))
                .context("ExitProcess code does not fit u32")?;
            // Caller RIP ([rsp] at the fake-API stop) attributes the exit to
            // the guest module that requested it (exe vs SDL2 vs ucrt).
            let caller_rip = core
                .engine()
                .read_rsp()
                .ok()
                .and_then(|rsp| {
                    let mut slot = [0_u8; 8];
                    core.engine()
                        .mem_read(rsp, &mut slot)
                        .ok()
                        .map(|()| u64::from_le_bytes(slot))
                })
                .unwrap_or(0);
            tracing::warn!(target: "wie_exit", code = exit_code, caller_rip = format_args!("{caller_rip:#x}"), "ExitProcess");
            self.events.push(EntryTraceEvent {
                index: api_index,
                library: resolved.library.clone().into(),
                name: resolved.name.clone().into(),
                fake_target_va: hook_address,
                handled: true,
                return_value: None,
                return_address: None,
            });
            if let Some(t0) = handler_t0 {
                self.profile
                    .record_handler(t0.elapsed().as_nanos(), false, export_key.as_deref());
            }
            guard.kernel.sync.process_dying = true;
            // Wake every inbox-parked thread (Painpoint 1): teardown is
            // explicit — no park loop may outlive the process decision.
            guard
                .kernel
                .sync
                .wake_hub
                .broadcast(wie_winapi::Wake::Shutdown);
            // Flush buffered CRT console output (printf/puts buffer in the
            // guest stream; fwrite bypasses it). Windows flushes stdout at
            // process exit — without this, trailing printf output is silently
            // lost.
            guard.flush_console();
            // A non-zero exit code is a failure signal — log it so a live run
            // shows WHY the guest stopped, not just the bare code (micro
            // self-tests exit with the failing stage's code and trace the
            // reason through OutputDebugStringA before exiting).
            if exit_code != 0 {
                tracing::error!(
                    exit_code,
                    "guest exited with a non-zero code (see the guest's OutputDebugStringA trace for the failing stage)"
                );
            }
            return Ok(Step::Stop(EntryTraceTermination::ExitProcess {
                code: exit_code,
            }));
        }

        if resolved.traits.fast_void_sync() {
            let handler_t0 = self.profile_enabled.then(Instant::now);
            core.engine()
                .return_from_win64_api(0)
                .context("failed to return from fast synchronization API")?;
            if let Some(t0) = handler_t0 {
                self.profile
                    .record_handler(t0.elapsed().as_nanos(), true, export_key.as_deref());
            }
            self.noisy_api = self.noisy_api.saturating_add(1);
            // Publish handler writes back to the engine's GS-relative TEB slot.
            guard.publish_last_error_to_guest(core.engine());
            return Ok(Step::Next);
        }

        if resolved.winapi_id.is_some() && resolved.traits.fast_sync() {
            // Fast host sync (HeapAlloc / HeapFree / MultiByteToWideChar): one
            // dispatch tail. Dense-id dispatch (the trait is only assigned to
            // WinApiId exports), noisy unbilled accounting, no event
            // journaling, and last-error publication — the trailing publish is
            // what keeps a failed MultiByteToWideChar visible to a guest
            // GetLastError stub (its engine's GS-relative TEB read, no host
            // stop).
            let handler_t0 = self.profile_enabled.then(Instant::now);
            {
                let st = &mut *guard;
                core.run_handler(st, resolved)?;
            }
            if let Some(t0) = handler_t0 {
                self.profile
                    .record_handler(t0.elapsed().as_nanos(), true, export_key.as_deref());
            }
            self.noisy_api = self.noisy_api.saturating_add(1);
            guard.publish_last_error_to_guest(core.engine());
            return Ok(Step::Next);
        }

        let handler_t0 = self.profile_enabled.then(Instant::now);
        let dispatch_result = core.run_handler(&mut guard, resolved);
        let handler_ns = handler_t0.map(|t0| t0.elapsed().as_nanos()).unwrap_or(0);

        match dispatch_result {
            Ok(handler_result) => {
                if resolved.traits.noisy() {
                    if self.profile_enabled {
                        self.profile
                            .record_handler(handler_ns, true, export_key.as_deref());
                    }
                    self.noisy_api = self.noisy_api.saturating_add(1);
                } else {
                    if self.profile_enabled {
                        self.profile
                            .record_handler(handler_ns, false, export_key.as_deref());
                    }
                    self.charged_api = self.charged_api.saturating_add(1);
                    self.events.push(EntryTraceEvent {
                        index: api_index,
                        library: resolved.library.clone().into(),
                        name: resolved.name.clone().into(),
                        fake_target_va: hook_address,
                        handled: true,
                        return_value: Some(handler_result.return_value),
                        return_address: Some(handler_result.return_address),
                    });
                }
                guard.publish_last_error_to_guest(core.engine());
                journal_api_return(
                    api_index,
                    resolved.library.as_ref(),
                    resolved.name.as_ref(),
                    core.engine(),
                    handler_result.return_value,
                    handler_result.return_address,
                );
                Ok(Step::Next)
            }
            Err(error) => {
                if self.profile_enabled {
                    self.profile
                        .record_handler(handler_ns, false, export_key.as_deref());
                }
                // Owned downcast: the print-job arm MOVES its `PrintJobRequest`
                // (page canvases ~34 MB each) into the bridge — a reference
                // would force a clone. The consumed error is restored for the
                // unsupported-API diagnostic below.
                match error.downcast::<WinApiControlSignal>() {
                    Ok(WinApiControlSignal::WaitingForMessage) => {
                        // WINMM timer poll at the idle boundary: if a
                        // `timeSetEvent` timer is due, dispatch its callback
                        // instead of parking. Reentrancy guard (pending
                        // callbacks non-empty → wait) matches the per-quantum
                        // poll above; at most one timer per boundary.
                        if self.pending_callbacks.is_empty() {
                            let now = {
                                let raw = wie_winapi::kernel32::clock::tick_count_32();
                                u32::try_from(raw & u64::from(u32::MAX)).unwrap_or(0)
                            };
                            let due = guard.pop_next_due_timer(now);
                            if let Some(due) = due {
                                let request = wie_winapi::GuestCallbackRequest::timer(
                                    due.handle,
                                    due.callback_va,
                                    due.user_data,
                                );
                                self.charged_api = self.charged_api.saturating_add(1);
                                let outer_library =
                                    self.intern_outer_api_name(resolved.library.clone());
                                let outer_name = self.intern_outer_api_name(resolved.name.clone());
                                self.events.push(EntryTraceEvent {
                                    index: api_index,
                                    library: Arc::clone(&outer_library),
                                    name: Arc::clone(&outer_name),
                                    fake_target_va: hook_address,
                                    handled: true,
                                    return_value: None,
                                    return_address: None,
                                });
                                drop(guard);
                                if let Err(error) = self.begin_guest_callback(
                                    core,
                                    request,
                                    outer_library,
                                    outer_name,
                                    hook_address,
                                ) {
                                    return Ok(Step::Stop(EntryTraceTermination::RuntimeStop(
                                        format!("failed to begin timer callback: {error}"),
                                    )));
                                }
                                return Ok(Step::Next);
                            }
                        }
                        *self.next_api_index = self
                            .next_api_index
                            .checked_sub(1)
                            .context("runtime API index underflow after message yield")?;
                        // The message queue is empty and no idle messages
                        // (timers / paints) remain to synthesize — every
                        // WM_PAINT of this repaint cycle has been dispatched,
                        // so the coalesced publishes are complete. Emit one
                        // frame per full cycle (parent + children) instead of
                        // one per dispatch, which published child-less
                        // intermediate frames during a resize.
                        //
                        // Drain unconditionally — also while a guest callback
                        // is in flight. A modal dialog opened from a bridged
                        // WM_COMMAND (button click → guest WndProc →
                        // DialogBoxParam) runs its in-guest modal GetMessage
                        // loop INSIDE that callback; skipping the drain here
                        // leaves the dialog's painted frame (and the button's
                        // unpressed repaint) unpublished until the callback
                        // pops — the dialog never appears. The empty-queue
                        // quiescence IS the cycle-complete point regardless of
                        // callback nesting, and full-frame publishes make the
                        // emitted snapshot always coherent.
                        guard.present().drain_pending_publishes();
                        // The pull half of the repaint latch: republish every
                        // top-level whose content revision advanced since its
                        // last publish. A mutation that painted this cycle was
                        // already published by the drain (its surface buffer
                        // is now empty, so the publish no-ops); a mutation
                        // that produced no deferred publish still reaches the
                        // host here.
                        guard.present().reconcile_and_publish();
                        Ok(Step::Stop(EntryTraceTermination::WaitingForMessage))
                    }
                    Ok(WinApiControlSignal::GuestCallbackRequested { request }) => {
                        self.charged_api = self.charged_api.saturating_add(1);
                        drop(guard);
                        // Intern once per unique outer API name; every
                        // subsequent bridged message clones the cached Arc
                        // (refcount bump) instead of allocating a fresh Arc
                        // box + string copy per message.
                        let outer_library = self.intern_outer_api_name(resolved.library.clone());
                        let outer_name = self.intern_outer_api_name(resolved.name.clone());
                        self.events.push(EntryTraceEvent {
                            index: api_index,
                            library: Arc::clone(&outer_library),
                            name: Arc::clone(&outer_name),
                            fake_target_va: hook_address,
                            handled: true,
                            return_value: None,
                            return_address: None,
                        });
                        if let Err(error) = self.begin_guest_callback(
                            core,
                            request,
                            outer_library,
                            outer_name,
                            hook_address,
                        ) {
                            return Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
                                "failed to begin guest callback: {error}"
                            ))));
                        }
                        Ok(Step::Next)
                    }
                    Ok(WinApiControlSignal::EnumerationCallbackRequested {
                        request,
                        enumeration_id,
                    }) => {
                        self.charged_api = self.charged_api.saturating_add(1);
                        drop(guard);
                        let outer_library = self.intern_outer_api_name(resolved.library.clone());
                        let outer_name = self.intern_outer_api_name(resolved.name.clone());
                        self.events.push(EntryTraceEvent {
                            index: api_index,
                            library: Arc::clone(&outer_library),
                            name: Arc::clone(&outer_name),
                            fake_target_va: hook_address,
                            handled: true,
                            return_value: None,
                            return_address: None,
                        });
                        // The first item's frame is installed relative to the
                        // current RSP.
                        let dispatch_rsp = match core.with_locked(|engine, _| engine.read_rsp()) {
                            Ok(rsp) => rsp,
                            Err(error) => {
                                return Ok(Step::Stop(EntryTraceTermination::RuntimeStop(
                                    format!("failed to read RSP for enum callback: {error}"),
                                )));
                            }
                        };
                        if let Err(error) = self.begin_guest_enum_callback(
                            core,
                            request,
                            enumeration_id,
                            outer_library,
                            outer_name,
                            hook_address,
                            dispatch_rsp,
                        ) {
                            return Ok(Step::Stop(EntryTraceTermination::RuntimeStop(format!(
                                "failed to begin enum callback: {error}"
                            ))));
                        }
                        Ok(Step::Next)
                    }
                    Ok(WinApiControlSignal::FileDialogBridgeRequested { request }) => Ok(self
                        .dispatch_native_bridge(
                            core,
                            guard,
                            |st| st.window_state().file_dialog_bridge.take(),
                            |bridge| bridge.and_then(|hook| hook(&request)),
                            |st, bridge, picked| {
                                let window_state = st.window_state();
                                window_state.file_dialog_bridge = bridge;
                                if let Some(pending) =
                                    window_state.pending_native_file_dialog.as_mut()
                                {
                                    pending.pick = picked;
                                }
                            },
                        )),
                    Ok(WinApiControlSignal::MessageBoxBridgeRequested { request }) => Ok(self
                        .dispatch_native_bridge(
                            core,
                            guard,
                            |st| st.present().message_box_bridge.take(),
                            |bridge| {
                                bridge.map(|hook| {
                                    hook(&request.caption, &request.text, request.message_box_type)
                                })
                            },
                            |st, bridge, picked| {
                                st.present().message_box_bridge = bridge;
                                if let Some(pending) =
                                    st.window_state().pending_native_message_box.as_mut()
                                {
                                    pending.pick = picked;
                                }
                            },
                        )),
                    Ok(WinApiControlSignal::PrintDialogBridgeRequested { request }) => Ok(self
                        .dispatch_native_bridge(
                            core,
                            guard,
                            |st| st.window_state().print_dialog_bridge.take(),
                            |bridge| bridge.and_then(|hook| hook(&request)),
                            |st, bridge, picked| {
                                let window_state = st.window_state();
                                window_state.print_dialog_bridge = bridge;
                                if let Some(pending) =
                                    window_state.pending_native_print_dialog.as_mut()
                                {
                                    pending.pick = picked;
                                }
                            },
                        )),
                    Ok(WinApiControlSignal::PageSetupBridgeRequested { request }) => Ok(self
                        .dispatch_native_bridge(
                            core,
                            guard,
                            |st| st.window_state().page_setup_dialog_bridge.take(),
                            |bridge| bridge.and_then(|hook| hook(&request)),
                            |st, bridge, picked| {
                                let window_state = st.window_state();
                                window_state.page_setup_dialog_bridge = bridge;
                                if let Some(pending) =
                                    window_state.pending_native_page_setup.as_mut()
                                {
                                    pending.pick = picked;
                                }
                            },
                        )),
                    Ok(WinApiControlSignal::PrintJobBridgeRequested { request }) => {
                        // The request is moved in BY VALUE (the ~34 MB page
                        // canvases travel straight into the native pipeline —
                        // never cloned).
                        Ok(self.dispatch_native_bridge(
                            core,
                            guard,
                            |st| st.window_state().print_job_bridge.take(),
                            |bridge| bridge.map(|hook| hook(request)),
                            |st, bridge, succeeded| {
                                let window_state = st.window_state();
                                window_state.print_job_bridge = bridge;
                                if let Some(pending) =
                                    window_state.pending_native_print_job.as_mut()
                                {
                                    pending.success = succeeded;
                                }
                            },
                        ))
                    }
                    Ok(WinApiControlSignal::ChildProcessSpawnRequested {
                        host_path,
                        guest_args,
                        // Informational today: the child always builds the
                        // standard WIE guest env.
                        inherit_environment: _,
                    }) => {
                        // Spawn a child guest process: the child gets its OWN
                        // RuntimeSession (engine + WinApiState — never shared
                        // with the parent) registered as a Process kernel
                        // object in THIS parent's handle table, so the
                        // parent's GetExitCodeProcess / WaitForSingleObject /
                        // OpenProcess resolve it. The wrapper thread is the
                        // single caller of ProcessObject::finish (after
                        // run_until_stop returns — ANY termination, not just
                        // ExitProcess), so a crashing child cannot hang a
                        // waiting parent.
                        //
                        // Snapshot the parent's volume roots while the state
                        // lock is held so the child sees the same C:\ / D:\
                        // mapping.
                        let (bottle_root, drive_d_root) = {
                            let st = &mut *guard;
                            (
                                st.file_io.volumes.bottle_root.clone(),
                                st.file_io.volumes.drive_d_root.clone(),
                            )
                        };
                        drop(guard);

                        // Build the child session OUTSIDE the parent's state
                        // lock: it is fully self-contained (own engine,
                        // WinApiState, guest memory). The session API does not
                        // accept a shared JitShared, so each child builds its
                        // own compilation cache (correctness-neutral; a
                        // per-process cache).
                        let build = crate::RuntimeSession::new_with_options(
                            &host_path,
                            wie_winapi::MessageQueueIdlePolicy::ExitOnIdle,
                            crate::DEFAULT_LAYOUT,
                            crate::SessionOptions {
                                bottle_root,
                                drive_d_root,
                                guest_args,
                                ..crate::SessionOptions::default()
                            },
                        );

                        // (hProcess, hThread, dwProcessId, dwThreadId).
                        let spawn_outcome: Option<(u64, u64, u32, u32)> = match build {
                            Ok(mut child) => {
                                let pid =
                                    core.with_locked(|_, st| st.kernel.sync.alloc_child_pid());
                                let (h_process, proc_obj) =
                                    core.with_locked(|_, st| st.kernel.sync.register_process(pid));
                                let (h_thread, thread_obj) = core.with_locked(|_, st| {
                                    st.kernel.sync.register_detached_thread(pid)
                                });
                                let spawned = std::thread::Builder::new()
                                    .name(format!("wie-child-{pid}"))
                                    .stack_size(8 * 1024 * 1024)
                                    .spawn(move || {
                                        let summary = child.run_until_stop(super::MAX_API_QUANTUM);
                                        let code = match &summary {
                                            Ok(s) => match &s.termination {
                                                EntryTraceTermination::ExitProcess { code } => {
                                                    *code
                                                }
                                                _ => 1,
                                            },
                                            Err(error) => {
                                                tracing::error!(
                                                    pid,
                                                    error = %error,
                                                    "child session stopped with an error"
                                                );
                                                1
                                            }
                                        };
                                        tracing::info!(
                                            target: "wiegui",
                                            pid,
                                            code,
                                            "child guest process exited"
                                        );
                                        proc_obj.finish(code);
                                        thread_obj.finish(code);
                                    });
                                match spawned {
                                    Ok(_) => {
                                        // JoinHandle dropped: the child host
                                        // thread is detached and notifies the
                                        // Process object when it finishes.
                                        Some((h_process, h_thread, pid, self.primary_tid))
                                    }
                                    Err(error) => {
                                        tracing::error!(
                                            pid,
                                            error = %error,
                                            "failed to spawn child host thread"
                                        );
                                        core.with_locked(|_, st| {
                                            st.kernel
                                                .sync
                                                .objects
                                                .remove(&KernelHandle::from(h_process));
                                            st.kernel
                                                .sync
                                                .objects
                                                .remove(&KernelHandle::from(h_thread));
                                            st.kernel.sync.process_by_pid.remove(&pid);
                                        });
                                        None
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::error!(
                                    path = %host_path.display(),
                                    error = %error,
                                    "failed to build child session"
                                );
                                None
                            }
                        };

                        if let Some((h_process, h_thread, pid, tid)) = spawn_outcome {
                            let recorded = core.with_locked(|_, winapi_state| {
                                if winapi_state.kernel.threads.active.tid != self.primary_tid {
                                    winapi_state.kernel.threads.activate(self.primary_tid);
                                }
                                winapi_state
                                    .window_state()
                                    .set_child_spawn_result(h_process, h_thread, pid, tid)
                            });
                            if !recorded {
                                // The write-back slot vanished (the handler's
                                // re-entry raced a teardown): the guest will
                                // fail closed.
                                tracing::warn!(
                                    pid,
                                    "child spawned but PROCESS_INFORMATION write-back slot missing"
                                );
                            }
                        }
                        // Continue: the engine re-executes the fake API stop,
                        // the handler re-enters, takes the pending record, and
                        // writes PROCESS_INFORMATION (a `None` result reads as
                        // FALSE + ERROR_INVALID_PARAMETER).
                        Ok(Step::Next)
                    }
                    Ok(WinApiControlSignal::HostPark { reason }) => {
                        // Per-thread engine: primary regs are already in
                        // `engine`; only persist thread bookkeeping for TLS
                        // tracking.
                        guard.kernel.threads.save_active();
                        // The guest is about to block on a wait — flush any
                        // coalesced publishes so the frame reaches the host
                        // before the park. Skipped while a guest callback is
                        // in flight (the callback owns the paint cycle).
                        if self.pending_callbacks.is_empty() {
                            guard.present().drain_pending_publishes();
                        }
                        Ok(Step::Park(reason))
                    }
                    Ok(WinApiControlSignal::ExitThread { code }) => {
                        // Flush pending publishes before the thread exits so
                        // the last painted frame is not lost.
                        if self.pending_callbacks.is_empty() {
                            guard.present().drain_pending_publishes();
                        }
                        Ok(Step::ExitThread(code))
                    }
                    Err(error) => {
                        let api = format!(
                            "{}!{}: {error}",
                            resolved.library.as_ref(),
                            resolved.name.as_ref(),
                        );
                        // Surface the failure through tracing so a live run
                        // (console or GUI) shows WHY the session stopped — the
                        // guest-fault family (e.g. an unsupported UCRT export)
                        // is otherwise only visible in the entry-trace summary
                        // and headless reproductions.
                        tracing::error!(
                            api = %api,
                            rip = format_args!("{:#x}", hook_address),
                            "unsupported API (session will stop)"
                        );
                        self.events.push(EntryTraceEvent {
                            index: api_index,
                            library: resolved.library.clone().into(),
                            name: resolved.name.clone().into(),
                            fake_target_va: hook_address,
                            handled: false,
                            return_value: None,
                            return_address: None,
                        });
                        Ok(Step::Stop(EntryTraceTermination::UnsupportedApi(api)))
                    }
                }
            }
        }
    }
}

impl super::RuntimeSession {
    /// Runs the guest until it yields, terminates, reaches an unsupported API,
    /// or processes `max_api` additional API calls.
    pub fn run_until_stop(&mut self, max_api: usize) -> Result<RuntimeRunSummary> {
        let primary_tid = self.process.primary_tid();
        let dll_main_return_va = wie_winapi::dll_main_return_trampoline_va();
        let static_dll_mains: Vec<dll_loader::StaticDllMain> = if self.entry_reached {
            Vec::new()
        } else {
            self.process.static_dll_mains().to_vec()
        };

        // Ceiling on API stops that did not charge toward `max_api` (noisy
        // fast-path returns): 50× the budget, at least a fixed 50k slack.
        const NOISY_API_FACTOR: usize = 50;
        const NOISY_API_SLACK: usize = 50_000;
        let max_noisy_api = max_api
            .saturating_mul(NOISY_API_FACTOR)
            .max(max_api.saturating_add(NOISY_API_SLACK));

        let mut events: Vec<EntryTraceEvent> = Vec::new();
        let mut termination = EntryTraceTermination::ApiLimit;

        // Painpoint 1 park state: the primary thread's wake inbox and the
        // accumulated idle-residency for this run segment (parked wall time).
        let inbox = self.primary_inbox();
        let mut park_residency_ns: u128 = 0;

        let mut hooks = SessionPumpHooks::new(
            self.entry_point_va.0,
            &mut self.entry_reached,
            &mut self.next_api_index,
            &mut self.no_hook_slices,
            &mut self.pending_callbacks,
            &mut self.outer_api_names,
            self.profile_enabled,
            &mut self.profile,
            &mut events,
            static_dll_mains,
            dll_main_return_va,
            primary_tid,
        );

        // Static-dependency DllMain(PROCESS_ATTACH) init phase: Windows calls
        // each static dep's DllMain in load order (dependencies first) BEFORE
        // the exe entry point. Skipped when the budget is zero (preparing
        // would push a return address nothing ever pops).
        if max_api > 0 {
            let mut core = QuantumCore::new(
                &mut *self.process.engine,
                &self.process.config,
                &self.process.shared_winapi,
                &self.process.shared_heap,
                primary_tid,
                &self.process.lock_wait_stats,
            )?;
            hooks.prepare_first_quantum(&mut core)?;
        }

        'outer: while hooks.charged_api < max_api {
            if hooks.noisy_api >= max_noisy_api {
                termination = EntryTraceTermination::ApiLimit;
                break;
            }

            // Profiling Ctrl+C stop: with `WIE_RUNTIME_PROFILE` armed, a
            // SIGINT ends the whole session here instead of reaching the
            // guest as a key event. Checked at the top of every iteration so
            // responsiveness is bounded by one quantum/API-stop boundary;
            // gate-off cost is a single cached-bool load (no env access in
            // this hot loop).
            if wie_winapi::console::take_ctrlc_for_profile_stop() {
                termination = EntryTraceTermination::HostInterrupt;
                break 'outer;
            }

            // Start any CreateThread workers before the next quantum.
            self.process.drain_spawns()?;

            // One shared quantum — activate → run → resolve → dispatch →
            // park/finish decision. The primary-only hooks (DllMain phase,
            // callbacks, bridges, journaling) live in `hooks`; the core owns
            // the lock ordering, active-TID rules, and instruction budget.
            // The core is scoped so its engine borrow drops before the park
            // arm touches `self.process` again.
            let step = {
                let mut core = QuantumCore::new(
                    &mut *self.process.engine,
                    &self.process.config,
                    &self.process.shared_winapi,
                    &self.process.shared_heap,
                    primary_tid,
                    &self.process.lock_wait_stats,
                )?;
                core.step(&mut hooks)?
            };

            match step {
                Step::Next | Step::PureCompute => {}
                Step::Stop(term) => {
                    termination = term;
                    break 'outer;
                }
                Step::ExitThread(code) => {
                    // Primary ExitThread ≈ ExitProcess for the session.
                    termination = EntryTraceTermination::ExitProcess { code };
                    break 'outer;
                }
                Step::Park(reason) => {
                    if crate::mt_runtime::mt_debug() {
                        match reason {
                            HostParkReason::WaitObject { handle, timeout_ms } => {
                                tracing::error!(
                                    "[mt] primary park WaitObject handle={handle:#x} timeout={timeout_ms:#x}"
                                );
                            }
                            HostParkReason::PthreadWait => {
                                tracing::error!("[mt] primary park PthreadWait");
                            }
                            HostParkReason::WaitMultiple => {
                                tracing::error!("[mt] primary park WaitMultiple");
                            }
                            HostParkReason::CriticalSection { .. } => {}
                        }
                    }
                    match reason {
                        HostParkReason::CriticalSection { cs } => {
                            // Clone queue under lock, park **without** process
                            // locks so the CS owner can Leave and wake us.
                            let t0 = Instant::now();
                            let q = self
                                .process
                                .with_mut(|_, st| wie_winapi::kernel32::resolve_cs_queue(st, cs));
                            q.park_brief();
                            park_residency_ns =
                                park_residency_ns.saturating_add(t0.elapsed().as_nanos());
                            // Retry Enter: per-thread engine keeps primary
                            // regs; only restore TLS.
                            self.process.with_mut(|_eng, st| {
                                st.kernel.threads.activate(primary_tid);
                            });
                            // Do not charge API index again — undo increment.
                            *hooks.next_api_index = hooks.next_api_index.saturating_sub(1);
                        }
                        HostParkReason::WaitObject { handle, timeout_ms } => {
                            // Detach waitable object, wait **outside** process
                            // locks so workers can ExitThread / SetEvent /
                            // CreateThread.
                            let _ = self.process.drain_spawns();
                            let target = self.process.with_mut(|_, st| {
                                wie_winapi::kernel32::resolve_wait_target(st, handle)
                            });
                            // Event-driven infinite wait (Painpoint 1):
                            // register on the object's waiter registry FIRST,
                            // then block on the inbox — a signal from any
                            // peer thread delivers a token instead of the old
                            // 50 ms poll slices. Tokens are hints; every wake
                            // re-checks the object state. The 50 ms cap only
                            // bounds `process_dying` / spawn-drain latency.
                            let t0 = Instant::now();
                            // Set when the park was cut short by a due WINMM
                            // timer: the wait result must NOT be written (the
                            // fake API stop re-executes — the same idempotent
                            // re-entry the CS park uses) and the quantum
                            // boundary dispatches the timer.
                            let mut timer_wake = false;
                            let result = match target {
                                Some(target) => {
                                    if timeout_ms == wie_winapi::INFINITE {
                                        target.enter_wait(&inbox);
                                        let mut result = wie_winapi::WAIT_FAILED;
                                        loop {
                                            if target.try_wait() {
                                                result = wie_winapi::WAIT_OBJECT_0;
                                                break;
                                            }
                                            let _ = self.process.drain_spawns();
                                            let dying = self
                                                .process
                                                .with_winapi_ref(|st| st.kernel.sync.process_dying);
                                            if dying {
                                                break;
                                            }
                                            // Timer-resolution lane: bound the
                                            // wait by the next due WINMM
                                            // timer (clamped by any
                                            // timeBeginPeriod request, max
                                            // 50 ms liveness). When one is
                                            // due, leave the park WITHOUT
                                            // writing a wait result — the API
                                            // stop re-executes, the quantum
                                            // boundary dispatches the timer,
                                            // and the handler re-parks.
                                            let now = u32::try_from(
                                                wie_winapi::kernel32::clock::tick_count_32()
                                                    & u64::from(u32::MAX),
                                            )
                                            .unwrap_or(0);
                                            let bound = {
                                                let next = self.process.with_winapi_ref(|st| {
                                                    st.winmm_ref()
                                                        .and_then(|w| w.next_due_in_ms(now))
                                                });
                                                let period = self.process.with_winapi_ref(|st| {
                                                    st.winmm_ref()
                                                        .map_or(0, |w| w.timer_period_ms())
                                                });
                                                let base = next.unwrap_or(50);
                                                base.min(if period > 0 { period } else { 50 })
                                                    .max(1)
                                            };
                                            let _ = inbox.wait_bounded(
                                                None,
                                                std::time::Duration::from_millis(u64::from(bound)),
                                            );
                                            let due = self.process.with_winapi_ref(|st| {
                                                let now2 = u32::try_from(
                                                    wie_winapi::kernel32::clock::tick_count_32()
                                                        & u64::from(u32::MAX),
                                                )
                                                .unwrap_or(0);
                                                st.winmm_ref().is_some_and(|w| w.peek_due(now2))
                                            });
                                            if due {
                                                timer_wake = true;
                                                break;
                                            }
                                        }
                                        target.exit_wait(&inbox);
                                        result
                                    } else {
                                        target.wait(timeout_ms)
                                    }
                                }
                                None => wie_winapi::WAIT_FAILED,
                            };
                            park_residency_ns =
                                park_residency_ns.saturating_add(t0.elapsed().as_nanos());
                            if !timer_wake {
                                self.process.with_mut(|eng, st| {
                                    st.kernel.threads.activate(primary_tid);
                                    let _ =
                                        eng.return_from_win64_api(u64::from(result)).map_err(|e| {
                                            tracing::error!(
                                                "guest stack corrupted on wait park: {e}"
                                            )
                                        });
                                });
                                hooks.charged_api = hooks.charged_api.saturating_add(1);
                            }
                        }
                        HostParkReason::PthreadWait => {
                            // Drain any pending CreateThread/pthread_create
                            // spawns so the worker can start executing guest
                            // code.
                            let _ = self.process.drain_spawns();
                            // Event-driven pthread park (Painpoint 1): the
                            // handler queued a PtPark (queue + observed wake
                            // sequence + bounded slice). Block on the inbox
                            // until the queue's wake sequence moves (its
                            // registry delivers tokens), the slice expires,
                            // or the process starts dying — then re-enter the
                            // idempotent handler.
                            let t0 = Instant::now();
                            let park = self
                                .process
                                .with_mut(|_, st| wie_winapi::pthread::take_park(st, primary_tid));
                            match park {
                                Some(park) => {
                                    let deadline = Instant::now() + park.slice;
                                    park.queue.enter_wait(&inbox);
                                    loop {
                                        if park.queue.observe() != park.observed {
                                            break;
                                        }
                                        if Instant::now() >= deadline {
                                            break;
                                        }
                                        let dying = self
                                            .process
                                            .with_winapi_ref(|st| st.kernel.sync.process_dying);
                                        if dying {
                                            break;
                                        }
                                        let _ = self.process.drain_spawns();
                                        inbox.wait_bounded(
                                            Some(deadline),
                                            std::time::Duration::from_millis(50),
                                        );
                                    }
                                    park.queue.exit_wait(&inbox);
                                }
                                None => {
                                    // No queued park (spurious re-entry):
                                    // brief yield so the handler can
                                    // re-check its condition.
                                    std::thread::sleep(std::time::Duration::from_millis(1));
                                }
                            }
                            park_residency_ns =
                                park_residency_ns.saturating_add(t0.elapsed().as_nanos());
                        }
                        HostParkReason::WaitMultiple => {
                            let _ = self.process.drain_spawns();
                            let req = self
                                .process
                                .with_mut(|_, st| st.kernel.sync.multi_wait.remove(&primary_tid));
                            let t0 = Instant::now();
                            let result = match req {
                                Some(req) => {
                                    let targets = self.process.with_mut(|_, st| {
                                        st.kernel.sync.wait_targets(&req.handles)
                                    });
                                    match targets {
                                        Some(targets) => {
                                            if req.timeout_ms == wie_winapi::INFINITE {
                                                // Event-driven multi-wait:
                                                // register on EVERY target's
                                                // waiter registry, then
                                                // re-check on each token.
                                                for target in &targets {
                                                    target.enter_wait(&inbox);
                                                }
                                                let mut result = wie_winapi::WAIT_FAILED;
                                                loop {
                                                    if let Some(code) =
                                                        wie_winapi::wait_multiple_step(
                                                            &targets,
                                                            req.wait_all,
                                                        )
                                                    {
                                                        result = code;
                                                        break;
                                                    }
                                                    let _ = self.process.drain_spawns();
                                                    let dying =
                                                        self.process.with_winapi_ref(|st| {
                                                            st.kernel.sync.process_dying
                                                        });
                                                    if dying {
                                                        break;
                                                    }
                                                    // INFINITE wait: no deadline,
                                                    // tokens + 50 ms liveness cap.
                                                    inbox.wait_bounded(
                                                        None,
                                                        std::time::Duration::from_millis(50),
                                                    );
                                                }
                                                for target in &targets {
                                                    target.exit_wait(&inbox);
                                                }
                                                result
                                            } else {
                                                wie_winapi::wait_multiple(
                                                    &targets,
                                                    req.wait_all,
                                                    req.timeout_ms,
                                                )
                                            }
                                        }
                                        None => wie_winapi::WAIT_FAILED,
                                    }
                                }
                                None => wie_winapi::WAIT_FAILED,
                            };
                            park_residency_ns =
                                park_residency_ns.saturating_add(t0.elapsed().as_nanos());
                            self.process.with_mut(|eng, st| {
                                st.kernel.threads.activate(primary_tid);
                                let _ = eng.return_from_win64_api(u64::from(result)).map_err(|e| {
                                    tracing::error!("guest stack corrupted on wait park: {e}")
                                });
                            });
                            hooks.charged_api = hooks.charged_api.saturating_add(1);
                        }
                    }
                }
            }
        }

        let charged_api = hooks.charged_api;
        drop(hooks);

        // Fold this segment's parked wall time into the idle-residency
        // counter (Painpoint 1). The hooks borrow owns `profile` during the
        // loop; only after dropping it can we write.
        if self.profile_enabled {
            self.profile.add_idle_residency_ns(park_residency_ns);
        }

        // Terminal session stops (guest exit / host interrupt / diagnostics)
        // wake every inbox-parked thread: teardown is explicit.
        if matches!(
            termination,
            EntryTraceTermination::ExitProcess { .. }
                | EntryTraceTermination::RuntimeStop(_)
                | EntryTraceTermination::UnsupportedApi(_)
                | EntryTraceTermination::HostInterrupt
        ) {
            self.wake_hub.broadcast(wie_winapi::Wake::Shutdown);
        }

        // Per-frame timing sample — sync present accumulators into the
        // profile and log host-stop / iced-vs-jit deltas on publish. Locks
        // are dropped; only active when `WIE_RUNTIME_PROFILE` (or
        // `enable_frame_timing`) is set.
        if self.profile_enabled {
            self.sample_frame_timing();
        }

        // Join workers if process exited.
        if matches!(termination, EntryTraceTermination::ExitProcess { .. }) {
            self.process.join_workers();
        }

        match &termination {
            EntryTraceTermination::ExitProcess { code } => {
                tracing::info!(target: "wiegui", code = *code, "guest exited");
            }
            EntryTraceTermination::ApiLimit => {
                tracing::warn!(target: "wiegui", "API stop limit hit");
            }
            EntryTraceTermination::WaitingForMessage => {
                // Log only meaningful idle transitions: the first idle, or a
                // re-idle after the guest ran (charged API stops) — not the
                // 50 ms poll-loop re-entry with an empty queue.
                if !self.was_waiting_for_message || charged_api > 0 {
                    tracing::debug!(target: "wiegui", "guest went idle waiting for messages");
                }
                self.was_waiting_for_message = true;
            }
            _ => {
                self.was_waiting_for_message = false;
            }
        }

        let (final_rip, final_rsp) = self.process.with_mut(|eng, _| {
            Ok::<_, anyhow::Error>((
                eng.read_rip().context("failed to read final runtime RIP")?,
                eng.read_rsp().context("failed to read final runtime RSP")?,
            ))
        })?;

        Ok(RuntimeRunSummary {
            events,
            termination,
            final_rip,
            final_rsp,
        })
    }
}
