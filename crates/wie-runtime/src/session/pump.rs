//! `run_until_stop` quantum loop and quiescent drain for the session.

use super::{invalid_memory_diagnostic, journal_api_return};
use crate::hooks::resolve_fake_api_at;
use crate::trace::{EntryTraceEvent, EntryTraceTermination, RuntimeRunSummary};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Instant;

impl super::RuntimeSession {
    /// Publish host `last_error` into guest TEB.LastErrorValue so in-guest
    /// `GetLastError` stubs stay coherent with host-side API failures.
    fn publish_last_error_to_guest(&mut self) {
        let err = self.process.with_mut(|_, st| st.process.last_error);
        if self.last_published_last_error == Some(err) {
            return;
        }
        let bytes = err.to_le_bytes();
        // Best-effort: TEB page is always mapped for this layout.
        let ok = self.process.with_mut(|eng, _| {
            eng.mem_write(crate::guest_stubs::TEB_LAST_ERROR_VA, &bytes)
                .is_ok()
        });
        if ok {
            self.last_published_last_error = Some(err);
        }
    }

    /// Publish the current clock snapshot into the guest clock table.
    ///
    /// Uses the primary engine directly (no WinAPI lock): the table lives in
    /// guest memory and the host is between quanta here, so no worker runs on
    /// this engine concurrently.
    fn refresh_clock_table(&mut self) {
        let clock_table_va = self.process.layout().clock_table.base;
        let _ = crate::guest_stubs::refresh_clock_table(&mut *self.process.engine, clock_table_va);
    }

    /// Runs the guest until it yields, terminates, reaches an unsupported API,
    /// or processes `max_api` additional API calls.
    pub fn run_until_stop(&mut self, max_api: usize) -> Result<RuntimeRunSummary> {
        let layout = *self.process.layout();
        let environment = *self.process.environment();
        let soft_apis = self.process.soft_apis().clone();
        let primary_tid = self.process.primary_tid();

        let fake_api_size_u64 =
            u64::try_from(layout.fake_api.size).context("fake API size does not fit u64")?;

        let fake_api_end = layout
            .fake_api
            .base
            .checked_add(fake_api_size_u64)
            .context("fake API end overflow")?
            .checked_sub(1)
            .context("fake API end underflow")?;

        let instruction_budget = layout.instruction_budget;
        let no_hook_limit = layout.no_hook_slice_limit;

        // Ceiling on API stops that did not charge toward `max_api` (noisy
        // fast-path returns): 50× the budget, at least a fixed 50k slack.
        const NOISY_API_FACTOR: usize = 50;
        const NOISY_API_SLACK: usize = 50_000;

        let mut events: Vec<crate::trace::EntryTraceEvent> = Vec::new();
        let mut termination = EntryTraceTermination::ApiLimit;

        let max_noisy_api = max_api
            .saturating_mul(NOISY_API_FACTOR)
            .max(max_api.saturating_add(NOISY_API_SLACK));
        let mut charged_api = 0_usize;
        let mut noisy_api = 0_usize;

        'outer: while charged_api < max_api {
            if noisy_api >= max_noisy_api {
                termination = EntryTraceTermination::ApiLimit;
                break;
            }

            // Start any CreateThread workers before the next quantum.
            self.process.drain_spawns()?;
            let index = self.next_api_index;
            self.next_api_index = self
                .next_api_index
                .checked_add(1)
                .context("runtime API index overflow")?;

            // Outcome of one locked quantum (locks dropped before host park).
            enum Quantum {
                Continue,
                Break,
                /// Park then retry same guest API (CS) or complete wait return.
                Park(wie_winapi::HostParkReason),
                /// Worker/primary ExitThread.
                ExitThread(u32),
            }

            let mut quantum = Quantum::Continue;
            let mut break_term: Option<EntryTraceTermination> = None;

            // Activate primary under WinAPI lock only — pure guest run must not
            // hold `shared_winapi` so worker quanta can overlap (per-thread engines).
            {
                let mut st = self
                    .process
                    .shared_winapi
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if st.kernel.threads.active.tid != primary_tid {
                    st.kernel.threads.activate(primary_tid);
                }
            }

            let begin = {
                let engine = &mut *self.process.engine;
                let current_rip = engine
                    .read_rip()
                    .context("failed to read RIP before runtime step")?;
                if current_rip == 0 {
                    if !self.entry_reached {
                        self.entry_reached = true;
                        tracing::info!(
                            target: "wiegui",
                            entry = self.entry_point_va.0,
                            "guest entry reached"
                        );
                    }
                    self.entry_point_va.0
                } else {
                    current_rip
                }
            };

            // Refresh the host-written guest clock table ahead of this
            // quantum so the in-guest clock stubs (GetTickCount / timeGetTime
            // / QPC / …) observe advancing values with no host stop. Frozen
            // under `WIE_FIXED_CLOCK=1` — the table was written once at
            // session init, so a guest busy-waiting on a constant never spins.
            if !wie_winapi::kernel32::clock::clock_is_fixed() {
                self.refresh_clock_table();
            }

            let emu_t0 = self.profile_enabled.then(Instant::now);
            let hook_result = self.process.engine.run_until_stop(
                begin,
                0,
                0,
                instruction_budget,
                layout.fake_api.base,
                fake_api_end,
            );
            if let Some(t0) = emu_t0 {
                self.profile.add_emu_ns(t0.elapsed().as_nanos());
            }

            {
                let mut pair = self.process.lock_pair();
                let (engine, winapi_state) = pair.both();
                // Workers may have activated themselves while we ran pure guest
                // code without the WinAPI lock. Reclaim primary identity before
                // any dispatch that uses current_tid() (CS owner, TLS, waits).
                if winapi_state.kernel.threads.active.tid != primary_tid {
                    winapi_state.kernel.threads.activate(primary_tid);
                }

                let (hook, invalid_memory) = match hook_result {
                    Ok(result) => (result.code, result.invalid_memory),
                    Err(wie_cpu::CpuError::DivideByZero(div_rip)) => {
                        match wie_winapi::seh::dispatch_hardware_fault(
                            engine,
                            winapi_state,
                            wie_cpu::exception_code::INT_DIVIDE_BY_ZERO,
                            div_rip,
                        ) {
                            Ok(result) => {
                                tracing::trace!(
                                    rip = div_rip,
                                    resume_rip = result.return_value,
                                    "divide-by-zero handled by SEH"
                                );
                                continue;
                            }
                            Err(_) => {
                                tracing::debug!(rip = div_rip, "unhandled divide-by-zero");
                                let reason = format!("integer divide by zero at rip={div_rip:#x}");
                                break_term = Some(EntryTraceTermination::RuntimeStop(reason));
                                quantum = Quantum::Break;
                            }
                        }
                        (
                            wie_cpu::CodeHookOutcome::default(),
                            wie_cpu::InvalidMemoryAccess::default(),
                        )
                    }
                    Err(error) => {
                        let rip = engine
                            .read_rip()
                            .context("failed to read RIP after runtime emulation error")?;
                        let rsp = engine
                            .read_rsp()
                            .context("failed to read RSP after runtime emulation error")?;
                        let mut slot = [0_u8; 8];
                        let slot_va = rsp.wrapping_add(0x160);
                        let slot_val = engine
                            .mem_read(slot_va, &mut slot)
                            .ok()
                            .map(|()| u64::from_le_bytes(slot));
                        let last_api = match events.last() {
                            Some(e) => format!("{}!{}", e.library.as_ref(), e.name.as_ref()),
                            None => "-".into(),
                        };
                        break_term = Some(EntryTraceTermination::RuntimeStop(format!(
                            "emulation error (api_index={index}, last_api={last_api}): {error}; \
                             rip={rip:#018x}; rsp={rsp:#018x}; [rsp+0x160]={slot_val:?}"
                        )));
                        quantum = Quantum::Break;
                        (
                            wie_cpu::CodeHookOutcome::default(),
                            wie_cpu::InvalidMemoryAccess::default(),
                        )
                    }
                };

                if matches!(quantum, Quantum::Break) {
                    // already set break_term
                } else if invalid_memory.hit {
                    // Route through guest SEH before terminating.
                    match wie_winapi::seh::dispatch_hardware_fault(
                        engine,
                        winapi_state,
                        invalid_memory.exception_code,
                        invalid_memory.address,
                    ) {
                        Ok(result) => {
                            // Handler found — guest continues at catch block.
                            tracing::trace!(
                                exc = invalid_memory.exception_code,
                                addr = invalid_memory.address,
                                resume_rip = result.return_value,
                                "hardware fault handled by guest SEH"
                            );
                            continue;
                        }
                        Err(_unhandled) => {
                            tracing::debug!(
                                exc = invalid_memory.exception_code,
                                "unhandled hardware fault"
                            );
                            break_term = Some(invalid_memory_diagnostic(engine, &invalid_memory)?);
                            quantum = Quantum::Break;
                        }
                    }
                } else if !hook.hit {
                    self.no_hook_slices = self
                        .no_hook_slices
                        .checked_add(1)
                        .context("no-hook slice count overflow")?;
                    if self.no_hook_slices == 1
                        || self.no_hook_slices.is_multiple_of(5)
                        || self.no_hook_slices == no_hook_limit
                    {
                        let rip = engine.read_rip().context("rip after no-hook")?;
                        let rsp = engine.read_rsp().context("rsp after no-hook")?;
                        let rax = engine.read_rax().context("rax after no-hook")?;
                        let rcx = engine.read_rcx().context("rcx after no-hook")?;
                        let rdx = engine.read_rdx().context("rdx after no-hook")?;
                        tracing::debug!(
                            slice = self.no_hook_slices,
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
                    if self.no_hook_slices >= no_hook_limit {
                        let rip = engine.read_rip().context("rip after no-hook")?;
                        let rsp = engine.read_rsp().context("rsp after no-hook")?;
                        let rax = engine.read_rax().context("rax after no-hook")?;
                        let rcx = engine.read_rcx().context("rcx after no-hook")?;
                        let rdx = engine.read_rdx().context("rdx after no-hook")?;
                        break_term = Some(EntryTraceTermination::RuntimeStop(format!(
                            "emulation stopped without hitting fake API hook after {} slices: \
                             begin={begin:#018x}; rip={rip:#018x}; rsp={rsp:#018x}; \
                             rax={rax:#018x}; rcx={rcx:#018x}; rdx={rdx:#018x}; \
                             budget={instruction_budget}",
                            self.no_hook_slices,
                        )));
                        quantum = Quantum::Break;
                    } else {
                        quantum = Quantum::Continue;
                    }
                } else {
                    self.no_hook_slices = 0;

                    if hook.address == layout.callback_return_trampoline_va {
                        // complete_guest_callback needs &mut self — handle outside.
                        // Save marker via special path: use pending flag.
                        drop(pair);
                        match self.complete_guest_callback() {
                            Ok(completion) => {
                                charged_api = charged_api.saturating_add(1);
                                events.push(EntryTraceEvent {
                                    index,
                                    library: completion.outer_library,
                                    name: completion.outer_name,
                                    fake_target_va: completion.outer_fake_va,
                                    handled: true,
                                    return_value: Some(completion.return_value),
                                    return_address: Some(completion.return_address),
                                });
                                self.publish_last_error_to_guest();
                                // Do NOT publish here. A guest WndProc
                                // is one message of a repaint cycle (parent
                                // BitBlt → child control paints across several
                                // messages); publishing mid-cycle would emit a
                                // frame with the children still missing. The
                                // drain happens once at the empty-queue idle
                                // boundary (WaitingForMessage) — one frame per
                                // full cycle.
                                continue 'outer;
                            }
                            Err(error) => {
                                termination = EntryTraceTermination::RuntimeStop(format!(
                                    "failed to complete guest callback: {error}"
                                ));
                                break 'outer;
                            }
                        }
                    }

                    // SEH / C++ EH continuation (UnwindMap actions, MSVC catch funclets).
                    let seh_handled = if hook.address == wie_winapi::seh_continue_trampoline_va() {
                        match wie_winapi::seh::continue_pending(engine, winapi_state) {
                            Ok(result) => {
                                charged_api = charged_api.saturating_add(1);
                                events.push(EntryTraceEvent {
                                    index,
                                    library: "ntdll.dll".into(),
                                    name: "SehContinue".into(),
                                    fake_target_va: hook.address,
                                    handled: true,
                                    return_value: Some(result.return_value),
                                    return_address: Some(result.return_address),
                                });
                                let err = winapi_state.process.last_error;
                                if self.last_published_last_error != Some(err) {
                                    let bytes = err.to_le_bytes();
                                    if engine
                                        .mem_write(crate::guest_stubs::TEB_LAST_ERROR_VA, &bytes)
                                        .is_ok()
                                    {
                                        self.last_published_last_error = Some(err);
                                    }
                                }
                                quantum = Quantum::Continue;
                                true
                            }
                            Err(error) => {
                                break_term = Some(EntryTraceTermination::RuntimeStop(format!(
                                    "failed to continue SEH sequence: {error}"
                                )));
                                quantum = Quantum::Break;
                                true
                            }
                        }
                    } else {
                        false
                    };

                    let resolve_t0 = self.profile_enabled.then(Instant::now);
                    let resolved_opt = if seh_handled {
                        None
                    } else {
                        resolve_fake_api_at(hook.address, &soft_apis)
                    };
                    if !seh_handled && resolved_opt.is_none() {
                        break_term = Some(EntryTraceTermination::RuntimeStop(format!(
                            "unresolved fake API at {:#018x}",
                            hook.address,
                        )));
                        quantum = Quantum::Break;
                    }

                    if let Some(resolved) = resolved_opt {
                        tracing::trace!(
                            api_index = index,
                            api = %format!(
                                "{}!{}",
                                resolved.library.as_ref(),
                                resolved.name.as_ref()
                            ),
                            "host API stop"
                        );
                        if let Some(t0) = resolve_t0 {
                            self.profile.add_resolve_ns(t0.elapsed().as_nanos());
                        }

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

                        {
                            let mut teb_err = [0_u8; 4];
                            if engine
                                .mem_read(crate::guest_stubs::TEB_LAST_ERROR_VA, &mut teb_err)
                                .is_ok()
                            {
                                winapi_state.process.last_error = u32::from_le_bytes(teb_err);
                            }
                        }

                        if resolved.traits.exit_process() {
                            let handler_t0 = self.profile_enabled.then(Instant::now);
                            let exit_code_raw = engine
                                .read_rcx()
                                .context("failed to read RCX for ExitProcess")?;
                            let exit_code = u32::try_from(exit_code_raw & u64::from(u32::MAX))
                                .context("ExitProcess code does not fit u32")?;
                            events.push(EntryTraceEvent {
                                index,
                                library: resolved.library.clone().into(),
                                name: resolved.name.clone().into(),
                                fake_target_va: hook.address,
                                handled: true,
                                return_value: None,
                                return_address: None,
                            });
                            if let Some(t0) = handler_t0 {
                                self.profile.record_handler(
                                    t0.elapsed().as_nanos(),
                                    false,
                                    export_key.as_deref(),
                                );
                            }
                            winapi_state.kernel.sync.process_dying = true;
                            // Flush buffered CRT console output (printf/puts
                            // buffer in the guest stream; fwrite bypasses it).
                            // Windows flushes stdout at process exit — without
                            // this, trailing printf output is silently lost.
                            winapi_state.flush_console();
                            // A non-zero exit code is a failure signal — log it
                            // so a live run shows WHY the guest stopped, not
                            // just the bare code (micro self-tests exit with
                            // the failing stage's code and trace the reason
                            // through OutputDebugStringA before exiting).
                            if exit_code != 0 {
                                tracing::error!(
                                    exit_code,
                                    "guest exited with a non-zero code (see the guest's OutputDebugStringA trace for the failing stage)"
                                );
                            }
                            break_term =
                                Some(EntryTraceTermination::ExitProcess { code: exit_code });
                            quantum = Quantum::Break;
                        } else if resolved.traits.fast_void_sync() {
                            let handler_t0 = self.profile_enabled.then(Instant::now);
                            engine
                                .return_from_win64_api(0)
                                .context("failed to return from fast synchronization API")?;
                            if let Some(t0) = handler_t0 {
                                self.profile.record_handler(
                                    t0.elapsed().as_nanos(),
                                    true,
                                    export_key.as_deref(),
                                );
                            }
                            noisy_api = noisy_api.saturating_add(1);
                            // publish last error
                            let err = winapi_state.process.last_error;
                            if self.last_published_last_error != Some(err) {
                                let bytes = err.to_le_bytes();
                                if engine
                                    .mem_write(crate::guest_stubs::TEB_LAST_ERROR_VA, &bytes)
                                    .is_ok()
                                {
                                    self.last_published_last_error = Some(err);
                                }
                            }
                            quantum = Quantum::Continue;
                        } else if matches!(
                            resolved.winapi_id,
                            Some(wie_winapi::WinApiId::Kernel32Heapalloc)
                        ) {
                            let handler_t0 = self.profile_enabled.then(Instant::now);
                            {
                                let mut ctx = wie_winapi::HandlerContext::new(
                                    engine,
                                    environment,
                                    winapi_state,
                                );
                                wie_winapi::kernel32::handle_heap_alloc(&mut ctx)?;
                            }
                            if let Some(t0) = handler_t0 {
                                self.profile.record_handler(
                                    t0.elapsed().as_nanos(),
                                    true,
                                    export_key.as_deref(),
                                );
                            }
                            noisy_api = noisy_api.saturating_add(1);
                            let err = winapi_state.process.last_error;
                            if self.last_published_last_error != Some(err) {
                                let bytes = err.to_le_bytes();
                                if engine
                                    .mem_write(crate::guest_stubs::TEB_LAST_ERROR_VA, &bytes)
                                    .is_ok()
                                {
                                    self.last_published_last_error = Some(err);
                                }
                            }
                            quantum = Quantum::Continue;
                        } else if matches!(
                            resolved.winapi_id,
                            Some(wie_winapi::WinApiId::Kernel32Heapfree)
                        ) {
                            let handler_t0 = self.profile_enabled.then(Instant::now);
                            {
                                let mut ctx = wie_winapi::HandlerContext::new(
                                    engine,
                                    environment,
                                    winapi_state,
                                );
                                wie_winapi::kernel32::handle_heap_free(&mut ctx)?;
                            }
                            if let Some(t0) = handler_t0 {
                                self.profile.record_handler(
                                    t0.elapsed().as_nanos(),
                                    true,
                                    export_key.as_deref(),
                                );
                            }
                            noisy_api = noisy_api.saturating_add(1);
                            let err = winapi_state.process.last_error;
                            if self.last_published_last_error != Some(err) {
                                let bytes = err.to_le_bytes();
                                if engine
                                    .mem_write(crate::guest_stubs::TEB_LAST_ERROR_VA, &bytes)
                                    .is_ok()
                                {
                                    self.last_published_last_error = Some(err);
                                }
                            }
                            quantum = Quantum::Continue;
                        } else if matches!(
                            resolved.winapi_id,
                            Some(wie_winapi::WinApiId::Kernel32Multibytetowidechar)
                        ) {
                            let handler_t0 = self.profile_enabled.then(Instant::now);
                            {
                                let mut ctx = wie_winapi::HandlerContext::new(
                                    engine,
                                    environment,
                                    winapi_state,
                                );
                                wie_winapi::kernel32::handle_multi_byte_to_wide_char(&mut ctx)?;
                            }
                            if let Some(t0) = handler_t0 {
                                self.profile.record_handler(
                                    t0.elapsed().as_nanos(),
                                    true,
                                    export_key.as_deref(),
                                );
                            }
                            noisy_api = noisy_api.saturating_add(1);
                            quantum = Quantum::Continue;
                        } else {
                            let handler_t0 = self.profile_enabled.then(Instant::now);
                            let mut ctx =
                                wie_winapi::HandlerContext::new(engine, environment, winapi_state);
                            let dispatch_result = if let Some(id) = resolved.winapi_id {
                                wie_winapi::dispatch_winapi_id(&mut ctx, id)
                            } else {
                                wie_winapi::dispatch_winapi(
                                    &mut ctx,
                                    &resolved.library,
                                    &resolved.name,
                                )
                            };
                            let handler_ns =
                                handler_t0.map(|t0| t0.elapsed().as_nanos()).unwrap_or(0);

                            match dispatch_result {
                                Ok(handler_result) => {
                                    if resolved.traits.noisy() {
                                        if self.profile_enabled {
                                            self.profile.record_handler(
                                                handler_ns,
                                                true,
                                                export_key.as_deref(),
                                            );
                                        }
                                        noisy_api = noisy_api.saturating_add(1);
                                    } else {
                                        if self.profile_enabled {
                                            self.profile.record_handler(
                                                handler_ns,
                                                false,
                                                export_key.as_deref(),
                                            );
                                        }
                                        charged_api = charged_api.saturating_add(1);
                                        events.push(EntryTraceEvent {
                                            index,
                                            library: resolved.library.clone().into(),
                                            name: resolved.name.clone().into(),
                                            fake_target_va: hook.address,
                                            handled: true,
                                            return_value: Some(handler_result.return_value),
                                            return_address: Some(handler_result.return_address),
                                        });
                                    }
                                    let err = winapi_state.process.last_error;
                                    if self.last_published_last_error != Some(err) {
                                        let bytes = err.to_le_bytes();
                                        if engine
                                            .mem_write(
                                                crate::guest_stubs::TEB_LAST_ERROR_VA,
                                                &bytes,
                                            )
                                            .is_ok()
                                        {
                                            self.last_published_last_error = Some(err);
                                        }
                                    }
                                    journal_api_return(
                                        index,
                                        resolved.library.as_ref(),
                                        resolved.name.as_ref(),
                                        engine,
                                        handler_result.return_value,
                                        handler_result.return_address,
                                    );
                                    quantum = Quantum::Continue;
                                }
                                Err(error) => {
                                    if self.profile_enabled {
                                        self.profile.record_handler(
                                            handler_ns,
                                            false,
                                            export_key.as_deref(),
                                        );
                                    }
                                    // Owned downcast: the print-job arm MOVES its
                                    // `PrintJobRequest` (page canvases ~34 MB each)
                                    // into the bridge — a reference would force a
                                    // clone. The consumed error is restored for the
                                    // unsupported-API diagnostic below.
                                    match error.downcast::<wie_winapi::WinApiControlSignal>() {
                                    Ok(wie_winapi::WinApiControlSignal::WaitingForMessage) => {
                                        self.next_api_index = self
                                            .next_api_index
                                            .checked_sub(1)
                                            .context(
                                                "runtime API index underflow after message yield",
                                            )?;
                                        // The message queue is empty and no
                                        // idle messages (timers / paints) remain to
                                        // synthesize — every WM_PAINT of this repaint
                                        // cycle has been dispatched, so the coalesced
                                        // publishes are complete. Emit one frame per
                                        // full cycle (parent + children) instead of
                                        // one per dispatch, which published
                                        // child-less intermediate frames during a
                                        // resize.
                                        //
                                        // Drain unconditionally — also while a guest
                                        // callback is in flight. A modal dialog
                                        // opened from a bridged WM_COMMAND (button
                                        // click → guest WndProc → DialogBoxParam)
                                        // runs its in-guest modal GetMessage loop
                                        // INSIDE that callback; skipping the drain
                                        // here leaves the dialog's painted frame (and
                                        // the button's unpressed repaint) unpublished
                                        // until the callback pops — the dialog never
                                        // appears. The empty-queue quiescence IS the
                                        // cycle-complete point regardless of callback
                                        // nesting, and full-frame publishes make the
                                        // emitted snapshot always coherent.
                                        winapi_state.present().drain_pending_publishes();
                                        // The pull half of the repaint latch:
                                        // republish every top-level whose
                                        // content revision advanced since its
                                        // last publish. A mutation that painted
                                        // this cycle was already published by
                                        // the drain (its surface buffer is now
                                        // empty, so the publish no-ops); a
                                        // mutation that produced no deferred
                                        // publish still reaches the host here.
                                        winapi_state.present().reconcile_and_publish();
                                        break_term =
                                            Some(EntryTraceTermination::WaitingForMessage);
                                        quantum = Quantum::Break;
                                    }
                                    Ok(
                                        wie_winapi::WinApiControlSignal::GuestCallbackRequested {
                                            request,
                                        },
                                    ) => {
                                        charged_api = charged_api.saturating_add(1);
                                        // begin_guest_callback needs full self — mark and handle after drop
                                        drop(pair);
                                        // Intern once per unique outer API name; every
                                        // subsequent bridged message clones the cached
                                        // Arc (refcount bump) instead of allocating a
                                        // fresh Arc box + string copy per message.
                                        let outer_library =
                                            self.intern_outer_api_name(resolved.library);
                                        let outer_name = self.intern_outer_api_name(resolved.name);
                                        events.push(EntryTraceEvent {
                                            index,
                                            library: Arc::clone(&outer_library),
                                            name: Arc::clone(&outer_name),
                                            fake_target_va: hook.address,
                                            handled: true,
                                            return_value: None,
                                            return_address: None,
                                        });
                                        if let Err(error) = self.begin_guest_callback(
                                            request,
                                            outer_library,
                                            outer_name,
                                            hook.address,
                                        ) {
                                            termination = EntryTraceTermination::RuntimeStop(
                                                format!("failed to begin guest callback: {error}"),
                                            );
                                            break 'outer;
                                        }
                                        continue 'outer;
                                    }
                                    Ok(
                                        wie_winapi::WinApiControlSignal::EnumerationCallbackRequested {
                                            request,
                                            enumeration_id,
                                        },
                                    ) => {
                                        charged_api = charged_api.saturating_add(1);
                                        // begin_guest_enum_callback needs full self — mark and handle after drop
                                        drop(pair);
                                        let outer_library =
                                            self.intern_outer_api_name(resolved.library);
                                        let outer_name = self.intern_outer_api_name(resolved.name);
                                        events.push(EntryTraceEvent {
                                            index,
                                            library: Arc::clone(&outer_library),
                                            name: Arc::clone(&outer_name),
                                            fake_target_va: hook.address,
                                            handled: true,
                                            return_value: None,
                                            return_address: None,
                                        });
                                        // The first item's frame is installed
                                        // relative to the current RSP.
                                        let dispatch_rsp = match self
                                            .process
                                            .with_mut(|engine, _| engine.read_rsp())
                                        {
                                            Ok(rsp) => rsp,
                                            Err(error) => {
                                                termination =
                                                    EntryTraceTermination::RuntimeStop(format!(
                                                        "failed to read RSP for enum callback: {error}"
                                                    ));
                                                break 'outer;
                                            }
                                        };
                                        if let Err(error) = self.begin_guest_enum_callback(
                                            request,
                                            enumeration_id,
                                            outer_library,
                                            outer_name,
                                            hook.address,
                                            dispatch_rsp,
                                        ) {
                                            termination = EntryTraceTermination::RuntimeStop(
                                                format!(
                                                    "failed to begin enum callback: {error}"
                                                ),
                                            );
                                            break 'outer;
                                        }
                                        continue 'outer;
                                    }
                                    Ok(
                                        wie_winapi::WinApiControlSignal::FileDialogBridgeRequested {
                                            request,
                                        },
                                    ) => {
                                        // The native panel (rfd) blocks the MAIN thread for the
                                        // whole session, and the winit event loop needs the SAME
                                        // shared state lock to service frame/user events while the
                                        // panel is up (take_frame, reconcile, hit-testing). Holding
                                        // the lock across the bridge deadlocks into the beachball,
                                        // so drop it for the whole panel session — the
                                        // GuestCallbackRequested pattern. Take the bridge out first
                                        // (it lives behind the lock) and restore it on return.
                                        let bridge = winapi_state
                                            .window_state()
                                            .file_dialog_bridge
                                            .take();
                                        drop(pair);
                                        let picked =
                                            bridge.as_ref().and_then(|bridge| bridge(&request));
                                        self.process.with_mut(|_, winapi_state| {
                                            if winapi_state.kernel.threads.active.tid != primary_tid
                                            {
                                                winapi_state.kernel.threads.activate(primary_tid);
                                            }
                                            let window_state = winapi_state.window_state();
                                            window_state.file_dialog_bridge = bridge;
                                            if let Some(pending) =
                                                window_state.pending_native_file_dialog.as_mut()
                                            {
                                                pending.pick = picked;
                                            }
                                        });
                                        // Continue: the engine re-executes the fake API stop, the
                                        // handler re-enters and writes the pick back.
                                        quantum = Quantum::Continue;
                                    }
                                    Ok(
                                        wie_winapi::WinApiControlSignal::MessageBoxBridgeRequested {
                                            request,
                                        },
                                    ) => {
                                        // The native alert (rfd) blocks the MAIN thread for the
                                        // whole session, and the winit event loop needs the SAME
                                        // shared state lock to service frame/user events while the
                                        // alert is up (take_frame, reconcile, hit-testing). Holding
                                        // the lock across the bridge deadlocks into the beachball
                                        // (the confirm-dialog hang), so drop it for the whole alert
                                        // session — the GuestCallbackRequested pattern, mirroring
                                        // the file-dialog arm above. Take the bridge out first (it
                                        // lives behind the lock) and restore it on return.
                                        let bridge = winapi_state
                                            .present()
                                            .message_box_bridge
                                            .take();
                                        drop(pair);
                                        let picked = bridge.as_ref().map(|bridge| {
                                            bridge(
                                                &request.caption,
                                                &request.text,
                                                request.message_box_type,
                                            )
                                        });
                                        self.process.with_mut(|_, winapi_state| {
                                            if winapi_state.kernel.threads.active.tid != primary_tid
                                            {
                                                winapi_state.kernel.threads.activate(primary_tid);
                                            }
                                            winapi_state.present().message_box_bridge = bridge;
                                            if let Some(pending) =
                                                winapi_state
                                                    .window_state()
                                                    .pending_native_message_box
                                                    .as_mut()
                                            {
                                                pending.pick = picked;
                                            }
                                        });
                                        // Continue: the engine re-executes the fake API stop, the
                                        // handler re-enters and returns the chosen id.
                                        quantum = Quantum::Continue;
                                    }
                                    Ok(
                                        wie_winapi::WinApiControlSignal::PrintDialogBridgeRequested {
                                            request,
                                        },
                                    ) => {
                                        // The native print panel (NSPrintPanel)
                                        // blocks the MAIN thread for the whole
                                        // session, and the winit event loop needs
                                        // the SAME shared state lock to service
                                        // frame/user events while the panel is up.
                                        // Holding the lock across the bridge
                                        // deadlocks into the beachball, so drop it
                                        // for the whole panel session — the
                                        // GuestCallbackRequested pattern, mirroring
                                        // the file-dialog arm above. Take the
                                        // bridge out first (it lives behind the
                                        // lock) and restore it on return.
                                        let bridge = winapi_state
                                            .window_state()
                                            .print_dialog_bridge
                                            .take();
                                        drop(pair);
                                        let picked =
                                            bridge.as_ref().and_then(|bridge| bridge(&request));
                                        self.process.with_mut(|_, winapi_state| {
                                            if winapi_state.kernel.threads.active.tid != primary_tid
                                            {
                                                winapi_state.kernel.threads.activate(primary_tid);
                                            }
                                            let window_state = winapi_state.window_state();
                                            window_state.print_dialog_bridge = bridge;
                                            if let Some(pending) =
                                                window_state.pending_native_print_dialog.as_mut()
                                            {
                                                pending.pick = picked;
                                            }
                                        });
                                        // Continue: the engine re-executes the fake
                                        // API stop, the handler re-enters and
                                        // writes the pick back.
                                        quantum = Quantum::Continue;
                                    }
                                    Ok(
                                        wie_winapi::WinApiControlSignal::PageSetupBridgeRequested {
                                            request,
                                        },
                                    ) => {
                                        // The native page-layout panel
                                        // (NSPageLayout) blocks the MAIN thread
                                        // for the whole session, and the winit
                                        // event loop needs the SAME shared
                                        // state lock to service frame/user
                                        // events while the panel is up.
                                        // Holding the lock across the bridge
                                        // deadlocks into the beachball, so drop
                                        // it for the whole panel session — the
                                        // print-dialog arm above. Take the
                                        // bridge out first (it lives behind
                                        // the lock) and restore it on return.
                                        let bridge = winapi_state
                                            .window_state()
                                            .page_setup_dialog_bridge
                                            .take();
                                        drop(pair);
                                        let picked =
                                            bridge.as_ref().and_then(|bridge| bridge(&request));
                                        self.process.with_mut(|_, winapi_state| {
                                            if winapi_state.kernel.threads.active.tid != primary_tid
                                            {
                                                winapi_state.kernel.threads.activate(primary_tid);
                                            }
                                            let window_state = winapi_state.window_state();
                                            window_state.page_setup_dialog_bridge = bridge;
                                            if let Some(pending) =
                                                window_state.pending_native_page_setup.as_mut()
                                            {
                                                pending.pick = picked;
                                            }
                                        });
                                        // Continue: the engine re-executes the fake
                                        // API stop, the handler re-enters and
                                        // writes the pick back.
                                        quantum = Quantum::Continue;
                                    }
                                    Ok(
                                        wie_winapi::WinApiControlSignal::PrintJobBridgeRequested {
                                            request,
                                        },
                                    ) => {
                                        // The native NSPrintOperation blocks the
                                        // MAIN thread for the whole print session,
                                        // and the winit event loop needs the SAME
                                        // shared state lock to service frame/user
                                        // events while the operation runs. Holding
                                        // the lock across the bridge deadlocks into
                                        // the beachball, so drop it for the whole
                                        // operation — the GuestCallbackRequested
                                        // pattern, mirroring the print-dialog arm
                                        // above. Take the bridge out first (it lives
                                        // behind the lock) and restore it on return.
                                        let bridge = winapi_state
                                            .window_state()
                                            .print_job_bridge
                                            .take();
                                        drop(pair);
                                        // The request is moved in BY VALUE (the
                                        // ~34 MB page canvases travel straight into
                                        // the native pipeline — never cloned).
                                        let succeeded =
                                            bridge.as_ref().map(|bridge| bridge(request));
                                        self.process.with_mut(|_, winapi_state| {
                                            if winapi_state.kernel.threads.active.tid != primary_tid
                                            {
                                                winapi_state.kernel.threads.activate(primary_tid);
                                            }
                                            let window_state = winapi_state.window_state();
                                            window_state.print_job_bridge = bridge;
                                            if let Some(pending) =
                                                window_state.pending_native_print_job.as_mut()
                                            {
                                                pending.success = succeeded;
                                            }
                                        });
                                        // Continue: the engine re-executes the fake
                                        // API stop, the handler re-enters and returns
                                        // the success flag as the EndDoc result.
                                        quantum = Quantum::Continue;
                                    }
                                    Ok(wie_winapi::WinApiControlSignal::ChildProcessSpawnRequested {
                                        host_path,
                                        guest_args,
                                        // Informational today: the child always
                                        // builds the standard WIE guest env.
                                        inherit_environment: _,
                                    }) => {
                                        // Spawn a child guest process: the child gets
                                        // its OWN RuntimeSession (engine + WinApiState —
                                        // never shared with the parent) registered as a
                                        // Process kernel object in THIS parent's handle
                                        // table, so the parent's GetExitCodeProcess /
                                        // WaitForSingleObject / OpenProcess resolve it.
                                        // The wrapper thread is the single caller of
                                        // ProcessObject::finish (after run_until_stop
                                        // returns — ANY termination, not just
                                        // ExitProcess), so a crashing child cannot hang a
                                        // waiting parent.
                                        //
                                        // Snapshot the parent's volume roots while the
                                        // state lock is held so the child sees the same
                                        // C:\ / D:\ mapping.
                                        let (bottle_root, drive_d_root) = {
                                            let st = winapi_state;
                                            (
                                                st.file_io.volumes.bottle_root.clone(),
                                                st.file_io.volumes.drive_d_root.clone(),
                                            )
                                        };
                                        drop(pair);

                                        // Build the child session OUTSIDE the parent's
                                        // state lock: it is fully self-contained (own
                                        // engine, WinApiState, guest memory). The session
                                        // API does not accept a shared JitShared, so each
                                        // child builds its own compilation cache
                                        // (correctness-neutral; a per-process cache).
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
                                        let spawn_outcome: Option<(u64, u64, u32, u32)> =
                                            match build {
                                                Ok(mut child) => {
                                                    let pid = self.process.with_mut(|_, st| {
                                                        st.kernel.sync.alloc_child_pid()
                                                    });
                                                    let (h_process, proc_obj) =
                                                        self.process.with_mut(|_, st| {
                                                            st.kernel.sync.register_process(pid)
                                                        });
                                                    let (h_thread, thread_obj) =
                                                        self.process.with_mut(|_, st| {
                                                            st.kernel
                                                                .sync
                                                                .register_detached_thread(pid)
                                                        });
                                                    let spawned = std::thread::Builder::new()
                                                        .name(format!("wie-child-{pid}"))
                                                        .stack_size(8 * 1024 * 1024)
                                                        .spawn(move || {
                                                            let summary = child.run_until_stop(
                                                                super::MAX_API_QUANTUM,
                                                            );
                                                            let code = match &summary {
                                                                Ok(s) => match &s.termination {
                                                                    EntryTraceTermination::ExitProcess {
                                                                        code,
                                                                    } => *code,
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
                                                            // JoinHandle dropped: the child
                                                            // host thread is detached and
                                                            // notifies the Process object when
                                                            // it finishes.
                                                            Some((
                                                                h_process,
                                                                h_thread,
                                                                pid,
                                                                primary_tid,
                                                            ))
                                                        }
                                                        Err(error) => {
                                                            tracing::error!(
                                                                pid,
                                                                error = %error,
                                                                "failed to spawn child host thread"
                                                            );
                                                            self.process.with_mut(|_, st| {
                                                                st.kernel
                                                                    .sync
                                                                    .objects
                                                                    .remove(
                                                                        &wie_winapi::KernelHandle::from(
                                                                            h_process,
                                                                        ),
                                                                    );
                                                                st.kernel
                                                                    .sync
                                                                    .objects
                                                                    .remove(
                                                                        &wie_winapi::KernelHandle::from(
                                                                            h_thread,
                                                                        ),
                                                                    );
                                                                st.kernel
                                                                    .sync
                                                                    .process_by_pid
                                                                    .remove(&pid);
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

                                        if let Some((h_process, h_thread, pid, tid)) =
                                            spawn_outcome
                                        {
                                            let recorded = self.process.with_mut(
                                                |_, winapi_state| {
                                                    if winapi_state.kernel.threads.active.tid
                                                        != primary_tid
                                                    {
                                                        winapi_state
                                                            .kernel
                                                            .threads
                                                            .activate(primary_tid);
                                                    }
                                                    winapi_state
                                                        .window_state()
                                                        .set_child_spawn_result(
                                                            h_process,
                                                            h_thread,
                                                            pid,
                                                            tid,
                                                        )
                                                },
                                            );
                                            if !recorded {
                                                // The write-back slot vanished (the
                                                // handler's re-entry raced a teardown):
                                                // the guest will fail closed.
                                                tracing::warn!(
                                                    pid,
                                                    "child spawned but PROCESS_INFORMATION write-back slot missing"
                                                );
                                            }
                                        }
                                        // Continue: the engine re-executes the fake API
                                        // stop, the handler re-enters, takes the pending
                                        // record, and writes PROCESS_INFORMATION (a `None`
                                        // result reads as FALSE + ERROR_INVALID_PARAMETER).
                                        quantum = Quantum::Continue;
                                    }
                                    Ok(wie_winapi::WinApiControlSignal::HostPark { reason }) => {
                                        // Per-thread engine: primary regs are already in `engine`;
                                        // only persist thread bookkeeping for TLS tracking.
                                        winapi_state.kernel.threads.save_active();
                                        // The guest is about to block on a
                                        // wait — flush any coalesced publishes so
                                        // the frame reaches the host before the
                                        // park. Skipped while a guest callback is
                                        // in flight (the callback owns the paint
                                        // cycle).
                                        if self.pending_callbacks.is_empty() {
                                            winapi_state.present().drain_pending_publishes();
                                        }
                                        quantum = Quantum::Park(reason);
                                    }
                                    Ok(wie_winapi::WinApiControlSignal::ExitThread { code }) => {
                                        // Flush pending publishes before the
                                        // thread exits so the last painted frame is
                                        // not lost.
                                        if self.pending_callbacks.is_empty() {
                                            winapi_state.present().drain_pending_publishes();
                                        }
                                        quantum = Quantum::ExitThread(code);
                                    }
                                    Err(error) => {
                                        let api = format!(
                                            "{}!{}: {error}",
                                            resolved.library.as_ref(),
                                            resolved.name.as_ref(),
                                        );
                                        // Surface the failure through tracing so a
                                        // live run (console or GUI) shows WHY the
                                        // session stopped — the guest-fault family
                                        // (e.g. an unsupported UCRT export) is
                                        // otherwise only visible in the entry-trace
                                        // summary and headless reproductions.
                                        tracing::error!(
                                            api = %api,
                                            rip = format_args!("{:#x}", hook.address),
                                            "unsupported API (session will stop)"
                                        );
                                        events.push(EntryTraceEvent {
                                            index,
                                            library: resolved.library.clone().into(),
                                            name: resolved.name.clone().into(),
                                            fake_target_va: hook.address,
                                            handled: false,
                                            return_value: None,
                                            return_address: None,
                                        });
                                        break_term =
                                            Some(EntryTraceTermination::UnsupportedApi(api));
                                        quantum = Quantum::Break;
                                    }
                                }
                                }
                            }
                        }
                    } // break_term.is_none resolved block
                }
            } // drop pair (process locks)

            match quantum {
                Quantum::Continue => {}
                Quantum::Break => {
                    if let Some(t) = break_term {
                        termination = t;
                    }
                    break 'outer;
                }
                Quantum::ExitThread(code) => {
                    // Primary ExitThread ≈ ExitProcess for session.
                    termination = EntryTraceTermination::ExitProcess { code };
                    break 'outer;
                }
                Quantum::Park(reason) => {
                    match reason {
                        wie_winapi::HostParkReason::CriticalSection { cs } => {
                            // Clone queue under lock, park **without** process locks
                            // so the CS owner can Leave and wake us.
                            let q = self
                                .process
                                .with_mut(|_, st| wie_winapi::kernel32::resolve_cs_queue(st, cs));
                            q.park_brief();
                            // Retry Enter: per-thread engine keeps primary regs; only restore TLS.
                            self.process.with_mut(|_eng, st| {
                                st.kernel.threads.activate(primary_tid);
                            });
                            // Do not charge API index again — undo increment.
                            self.next_api_index = self.next_api_index.saturating_sub(1);
                        }
                        wie_winapi::HostParkReason::WaitObject { handle, timeout_ms } => {
                            // Detach waitable object, wait **outside** process locks
                            // so workers can ExitThread / SetEvent / CreateThread.
                            let _ = self.process.drain_spawns();
                            if crate::mt_runtime::mt_debug() {
                                tracing::error!(
                                    "[mt] primary park WaitObject handle={handle:#x} timeout={timeout_ms:#x}"
                                );
                            }
                            let target = self.process.with_mut(|_, st| {
                                wie_winapi::kernel32::resolve_wait_target(st, handle)
                            });
                            let result = match target {
                                Some(t) => {
                                    // Slice infinite waits: drain nested CreateThread
                                    // from workers and observe process_dying.
                                    if timeout_ms == wie_winapi::INFINITE {
                                        loop {
                                            let r = t.wait(50);
                                            if r == wie_winapi::WAIT_OBJECT_0 {
                                                break r;
                                            }
                                            let _ = self.process.drain_spawns();
                                            let dying = self
                                                .process
                                                .with_winapi_ref(|st| st.kernel.sync.process_dying);
                                            if dying {
                                                break wie_winapi::WAIT_FAILED;
                                            }
                                        }
                                    } else {
                                        t.wait(timeout_ms)
                                    }
                                }
                                None => wie_winapi::WAIT_FAILED,
                            };
                            self.process.with_mut(|eng, st| {
                                st.kernel.threads.activate(primary_tid);
                                let _ = eng.return_from_win64_api(u64::from(result)).map_err(|e| {
                                    tracing::error!("guest stack corrupted on wait park: {e}")
                                });
                            });
                            charged_api = charged_api.saturating_add(1);
                        }
                        wie_winapi::HostParkReason::PthreadWait => {
                            // Drain any pending CreateThread/pthread_create spawns
                            // so the worker can start executing guest code.
                            let _ = self.process.drain_spawns();
                            if crate::mt_runtime::mt_debug() {
                                tracing::error!("[mt] primary park PthreadWait");
                            }
                            // Yield briefly so the handler can re-check its
                            // condition (WakeQueue park) on re-entry.
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        wie_winapi::HostParkReason::WaitMultiple => {
                            let _ = self.process.drain_spawns();
                            if crate::mt_runtime::mt_debug() {
                                tracing::error!("[mt] primary park WaitMultiple");
                            }
                            let req = self
                                .process
                                .with_mut(|_, st| st.kernel.sync.multi_wait.remove(&primary_tid));
                            let result = match req {
                                Some(req) => {
                                    let targets = self.process.with_mut(|_, st| {
                                        st.kernel.sync.wait_targets(&req.handles)
                                    });
                                    match targets {
                                        Some(ts) => {
                                            if req.timeout_ms == wie_winapi::INFINITE {
                                                loop {
                                                    let r = wie_winapi::wait_multiple(
                                                        &ts,
                                                        req.wait_all,
                                                        50,
                                                    );
                                                    if r != wie_winapi::WAIT_TIMEOUT {
                                                        break r;
                                                    }
                                                    let _ = self.process.drain_spawns();
                                                    let dying =
                                                        self.process.with_winapi_ref(|st| {
                                                            st.kernel.sync.process_dying
                                                        });
                                                    if dying {
                                                        break wie_winapi::WAIT_FAILED;
                                                    }
                                                }
                                            } else {
                                                wie_winapi::wait_multiple(
                                                    &ts,
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
                            self.process.with_mut(|eng, st| {
                                st.kernel.threads.activate(primary_tid);
                                let _ = eng.return_from_win64_api(u64::from(result)).map_err(|e| {
                                    tracing::error!("guest stack corrupted on wait park: {e}")
                                });
                            });
                            charged_api = charged_api.saturating_add(1);
                        }
                    }
                }
            }
        }

        // Per-frame timing sample — sync present accumulators into the
        // profile and log host-stop / iced-vs-jit deltas on publish. Locks are
        // dropped; only active when `WIE_RUNTIME_PROFILE` (or
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
