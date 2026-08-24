//! Headless GUI driver — runs a guest session with message-loop support.
//!
//! Does not depend on winit or any windowing system.  The host presenter (CLI
//! with `--gui` or `--screenshot`) calls into this loop and reads frames from
//! [`wie_winapi::present::PresentState`] via the [`GuestHandle`] seam.

use std::sync::atomic::{AtomicBool, AtomicI32};

use anyhow::{Context, Result};

use crate::session::RuntimeSession;
use crate::trace::EntryTraceTermination;

/// Shared control flags for the GUI loop.
pub struct GuiControl {
    /// Set to `true` when the guest has exited.
    pub finished: AtomicBool,
    /// Exit code set by the guest.
    pub exit_code: AtomicI32,
    /// When `false`, [`run_windowed`] returns `WaitingForMessage` after the
    /// initial paint so a headless caller (e.g. `--screenshot`) can capture
    /// the frame.  When `true` (interactive GUI), it keeps waiting on the
    /// message signal until the guest exits.
    ///
    /// Atomic: the host presenter may flip it from another thread; the GUI
    /// loop only loads it (Relaxed — a control flag, ordering irrelevant).
    pub wait_for_input: AtomicBool,
}

impl GuiControl {
    #[must_use]
    pub fn new() -> Self {
        Self {
            finished: AtomicBool::new(false),
            exit_code: AtomicI32::new(0),
            wait_for_input: AtomicBool::new(true),
        }
    }
}

impl Default for GuiControl {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of a headless GUI session.
#[derive(Debug)]
pub enum GuiOutcome {
    /// Guest exited normally.
    Exited(i32),
    /// Guest hit the API stop limit.
    ApiBudgetExhausted,
    /// Guest is waiting for messages and no presenter is attached.
    WaitingForMessage,
    /// The host stopped the session: Ctrl+C arrived while
    /// `WIE_RUNTIME_PROFILE` was armed. Not a guest exit — the presenter owns
    /// reporting (profile dump + status 130) from here.
    HostInterrupt,
}

/// Run the guest in headless GUI mode.
///
/// Calls `run_until_stop` in a loop, posting `WM_PAINT` when the guest blocks
/// on `GetMessage` so the window procedure paints the initial frame.  Returns
/// when the guest calls `ExitProcess` (or `PostQuitMessage` → `GetMessage`
/// returns 0).
pub fn run_windowed(session: &mut RuntimeSession, control: &GuiControl) -> Result<GuiOutcome> {
    let mut paint_posted = false;
    loop {
        let summary = session
            .run_until_stop(crate::session::MAX_API_QUANTUM)
            .context("GUI loop run_until_stop failed")?;

        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                return Ok(GuiOutcome::Exited(code as i32));
            }
            EntryTraceTermination::WaitingForMessage => {
                if !paint_posted {
                    // Post WM_PAINT (0x000F) to kick the guest to paint its
                    // initial frame.  The guest's WndProc handles WM_PAINT by
                    // calling BitBlt, which publishes the frame and fires the
                    // wake callback.
                    let handle = session.guest_handle();
                    if let Some(hwnd) = handle.first_guest_window_handle() {
                        tracing::debug!(target: "wiegui", hwnd, "posted initial WM_PAINT");
                        handle.post_message(hwnd, 0x000F, 0, 0); // WM_PAINT
                        paint_posted = true;
                        continue;
                    }
                }
                if !control
                    .wait_for_input
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    // Headless capture mode: the guest has painted its
                    // initial frame and gone idle; return so the caller can
                    // take the frame.
                    return Ok(GuiOutcome::WaitingForMessage);
                }
                // After the initial paint, park until something wakes the
                // guest. Event sources (Painpoint 1):
                //   • a posted message — `GuestHandle::post_message` sets the
                //     trigger flag and notifies this condvar INSTANTLY;
                //   • the nearest armed guest timer — the wait deadline is
                //     aligned to `next_timer_deadline` so `WM_TIMER`s fire on
                //     time instead of being slept past (a fixed-tick wait
                //     could stall a timer-only guest forever);
                //   • the 50 ms ceiling — bounds control-flag (`stop`,
                //     headless capture) and Ctrl+C-under-profiling latency.
                //
                // A single bounded wait (no re-arm loop): a spurious wake
                // costs one cheap empty `run_until_stop` pass.
                let t0 = std::time::Instant::now();
                const PARK_CAP: std::time::Duration = std::time::Duration::from_millis(50);
                let wait = session
                    .next_timer_deadline()
                    .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
                    .map(|remaining| remaining.min(PARK_CAP))
                    .unwrap_or(PARK_CAP);
                let signal = session.guest_handle().message_signal();
                if let Some(signal) = signal {
                    let mut triggered = signal.triggered.lock().unwrap_or_else(|e| e.into_inner());
                    if !*triggered {
                        let (guard, _) = signal
                            .cvar
                            .wait_timeout(triggered, wait)
                            .unwrap_or_else(|e| e.into_inner());
                        triggered = guard;
                    }
                    // Consume the signal; the message is in the queue.
                    *triggered = false;
                } else {
                    // No present state installed — fall back to sleeping for
                    // the same bounded slice.
                    std::thread::sleep(wait);
                }
                if session.profile_enabled() {
                    session
                        .profile_mut()
                        .add_idle_residency_ns(t0.elapsed().as_nanos());
                }
                continue;
            }
            EntryTraceTermination::GuestCallbackRequested { .. } => {
                // The runtime handles guest callbacks internally; continue.
                continue;
            }
            EntryTraceTermination::ApiLimit => {
                // The API-stop budget bounds a single run_until_stop quantum;
                // it is NOT a session timeout.  An interactive GUI session
                // runs until the guest exits (ExitProcess / WM_QUIT) — never
                // terminate it on the stop count.  `charged_api` is a per-call
                // local, so continuing resets the quantum.  Tests keep their
                // budgets because they drive run_until_stop directly.
                continue;
            }
            EntryTraceTermination::HostInterrupt => {
                // Ctrl+C under `WIE_RUNTIME_PROFILE`: unwind cleanly so the
                // presenter can finalize and print the profile report before
                // the event loop ends (the session lives on THIS thread).
                return Ok(GuiOutcome::HostInterrupt);
            }
            _ => {
                return Ok(GuiOutcome::ApiBudgetExhausted);
            }
        }
    }
}
