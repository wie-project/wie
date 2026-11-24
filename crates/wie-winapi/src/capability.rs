//! Named capability traits off [`WinApiState`] (ADR-0005).
//!
//! # Why
//!
//! [`HandlerContext`](crate::state::HandlerContext) is a real port — four
//! fields, one of them `&mut dyn CpuEngine` — but its `state` field is
//! `&mut WinApiState`, so narrowness holds only at the top: any handler that
//! holds a context can reach all eight top-level sub-states and every DLL slot.
//! The measured consequence (ADR-0005 Context) is that two clusters dominate
//! the crate's cross-boundary reach, and both are boundary errors rather than
//! necessities:
//!
//! - `kernel32` reached `ConsoleState` 70 times, though `DllId::Console` is
//!   owned by [`crate::console`].
//! - `comdlg32` reached `WindowState` 146 times for dialog plumbing — a
//!   `user32`-owned struct with 31 public fields.
//!
//! This module names those two capabilities, plus the presenter's, and
//! [`HandlerContext`](crate::state::HandlerContext) hands them out. A cluster
//! converted to a capability can no longer reach anything else: the seam is
//! the *static type of the field it holds*, not a convention.
//!
//! Stage 3 added a fourth, [`LastErrorAccess`], after stage 2 measured which
//! single member was blocking the most handlers: 138 whole-state bindings wrote
//! the process last-error alias and 33 of them did nothing else. It is the
//! template for however this continues — one capability per *state cluster*,
//! sized by measurement, added only when a cluster's reach is otherwise one
//! scalar or one verb list.
//!
//! # What a capability is not
//!
//! A trait that hands back `&mut WindowState` would be the current `&mut` with
//! a new name — the failure mode this design exists to avoid. So
//! [`WindowDialogAccess`] exposes one accessor per dialog-plumbing value, never
//! the struct, and [`PresentAccess`] exposes the presenter's verbs, never
//! [`PresentState`](crate::present::PresentState).
//!
//! [`ConsoleAccess`] is the one place a whole struct is reachable, and only
//! because after stage 1 of ADR-0005 the console *is* [`crate::console`]:
//! `DllId::Console`'s owner and its callers finally agree, and
//! [`ConsoleState`] has no other owner to cross into.
//!
//! # What is deliberately not reachable
//!
//! No capability exposes `&mut WinApiState`, and none exposes `process`,
//! `kernel`, `heap_state`, `module_state`, or `dll_states`. A cluster that
//! needs one of those is reaching past its capability — which is the point:
//! the compiler says so instead of a reviewer noticing.
//!
//! The one exception is a *scalar* inside `process`: the last-error alias,
//! which [`LastErrorAccess`] reads and writes by name. It is not a hole in the
//! rule above — the trait hands out two verbs over one `u32`, never the struct
//! — and it exists because 138 handlers reached all eight sub-states to touch
//! exactly that `u32`, 33 of them to touch nothing else.

use wie_cpu::CpuEngine;

use crate::console::ConsoleState;
use crate::gdi32::IRect;
use crate::handles::Hwnd;
use crate::present::WindowSurface;
use crate::state::{
    FileDialogPolicy, FileDialogSession, FontDialogPolicy, FontDialogSession,
    PageSetupDialogPolicy, PendingNativeFileDialog, PendingNativePageSetup,
    PendingNativePrintDialog, PrintDialogPolicy, WinApiState,
};

// ── Last error ─────────────────────────────────────────────────────────────

/// The Win32 last-error slot a handler may read and write, and nothing else.
///
/// This is the crate's most-used state touch by a wide margin: 138 of the 476
/// whole-state bindings ADR-0005 stage 2 counted wrote the last-error alias, and
/// for 33 of them that write was their *entire* reach — an eight-sub-state
/// borrow to store one `u32`. Two verbs cover every one of those sites.
///
/// # What the two verbs are, and why there are only two
///
/// The last-error value exists in three places, and the split between them is
/// load-bearing rather than incidental:
///
/// | Place | Who writes it | How a handler reaches it |
/// | --- | --- | --- |
/// | `process.last_error` (the alias) | handlers, through [`Self::set_process_last_error`] | this trait |
/// | the active thread's own slot | the dispatch boundary (`absorb_guest_last_error`) | not reachable — no handler needs it |
/// | the guest TEB page | the dispatch boundary (`publish_last_error_to_guest`) | not reachable — no handler needs it |
///
/// So [`Self::last_error`] reads the alias and
/// [`Self::set_process_last_error`] writes it, and neither touches the thread
/// slot. That is correct *because of the fold-at-boundary rule*: after every
/// handler dispatch `publish_last_error_to_guest` copies `process.last_error`
/// into the active thread's slot and then into the engine's GS-relative TEB
/// word, so a handler that stored the value in the alias has published it to
/// the guest by the time the guest runs again. A handler that also wrote the
/// thread slot would be duplicating a write the boundary makes anyway.
///
/// The runtime's thread-activation seam is the opposite case — there the alias
/// deliberately still holds the PREVIOUS thread's value, so
/// `WinApiState::set_active_last_error` writes the slot alone. That seam holds
/// `&mut WinApiState` (it is `wie-runtime`'s dispatch code, not a handler), so
/// it needs no verb here and gets none: `active_last_error`,
/// `set_active_last_error`, `publish_last_error_to_guest` and
/// `absorb_guest_last_error` stay inherent methods on `WinApiState`.
///
/// # Relationship to [`ConsoleAccess`]
///
/// `ConsoleAccess` used to declare its own `set_process_last_error`, with the
/// fold-at-boundary reasoning written out on it. That verb *is* this verb — same
/// receiver, same effect, same justification — so [`ConsoleAccess`] is now a
/// subtrait of this one and console handlers keep calling it unchanged. One
/// spelling, one doc, one implementation.
pub trait LastErrorAccess {
    /// The process last-error alias — what `GetLastError`/`WSAGetLastError`
    /// report, and the value the dispatch boundary folds into the guest's TEB.
    fn last_error(&self) -> u32;

    /// Overwrite the process last-error alias.
    ///
    /// Deliberately not `WinApiState::set_last_error`, which also overwrites the
    /// active thread's slot: the handlers that clear or set an error write the
    /// alias, and `publish_last_error_to_guest` folds it into the thread slot at
    /// the dispatch boundary. Writing both here would be the same value stored
    /// twice, and it would make the activation seam's "slot only" write
    /// indistinguishable from a handler's write.
    fn set_process_last_error(&mut self, error: u32);
}

impl LastErrorAccess for WinApiState {
    fn last_error(&self) -> u32 {
        self.process.last_error
    }

    fn set_process_last_error(&mut self, error: u32) {
        self.process.last_error = error;
    }
}

// ── Console ────────────────────────────────────────────────────────────────

/// Everything a `kernel32.dll` console handler needs, and nothing else.
///
/// Before ADR-0005 stage 1 the console cluster reached 70 `State`-relative
/// sites from `kernel32`, every one of them through `&mut WinApiState`. What it
/// actually uses is the console's own state (its `DllId` slot), the console
/// stdin buffer, the process last-error alias, and the buffered-output flush.
///
/// The stdin verbs exist because the console stdin buffer lives in
/// [`FileIoState`](crate::state::FileIoState) — a file-I/O struct — while every
/// reader of it is a console call. Naming the operations the console performs on
/// it is narrower than handing out `&mut FileIoState`, which is what the
/// unmigrated code did. Stage 2 added the character-level ones
/// ([`Self::stdin_read_char`], [`Self::stdin_is_live_host`]) for `ucrt`'s
/// `getchar`/`fgets`, and gave [`Self::stdin_refill`] a `max_line` parameter so
/// each caller keeps its own buffering policy.
pub trait ConsoleAccess: LastErrorAccess {
    /// The console's own state. Lazily allocated on first call.
    ///
    /// This is the one whole struct a capability hands back, and it is the
    /// subject of the cluster rather than a neighbour's god object: see the
    /// module docs.
    fn console(&mut self) -> &mut ConsoleState;

    /// The console's state without allocating it.
    ///
    /// The handle→buffer resolver runs on read paths (`GetConsoleMode`,
    /// `WriteConsole`) that must not create the slot as a side effect of
    /// answering "not a console handle".
    fn console_if_loaded(&self) -> Option<&ConsoleState>;

    // `set_process_last_error` (every console failure path) is inherited from
    // the [`LastErrorAccess`] supertrait rather than re-declared: it is the same
    // verb over the same field, and one spelling of it is enough.

    /// Unconsumed bytes in the console stdin buffer.
    fn stdin_available(&self) -> usize;

    /// Refill the console stdin buffer from one host line.
    ///
    /// `Ok(false)` is host EOF, `Err(())` a read failure — the two are
    /// distinguished because `ReadConsole` reports them differently.
    ///
    /// `max_line` is the caller's buffering policy and is spelled at the call
    /// site because there are two of them: `ReadConsole`/`ReadFile` use
    /// [`MAX_HOST_STDIN_LINE`](crate::console::stdin::MAX_HOST_STDIN_LINE),
    /// the `ucrt` line reader 4 KiB. Folding one policy into the verb would
    /// have changed the other caller's behaviour.
    fn stdin_refill(&mut self, max_line: usize) -> Result<bool, ()>;

    /// One character with `getchar` semantics — the buffered byte, one line
    /// refilled from the host, or `None` for EOF.
    ///
    /// A verb rather than a pair of `stdin_bytes`/`stdin_cursor` accessors: the
    /// console reads the buffer a *line* at a time (`getchar` consumes the
    /// first byte and discards the rest up to `\n`), and that rule is the
    /// console's, not the caller's. `ucrt`'s `getchar` is the only caller and
    /// it asks for exactly one character, so one verb is the whole need.
    fn stdin_read_char(&mut self) -> Option<u8>;

    /// Whether the guest's stdin is served live from the host (`LiveHost`)
    /// rather than only from the injected buffer (`InjectOnly`).
    ///
    /// A predicate, not `file_io.stdin_mode`: the line reader asks this one
    /// question before deciding to block on the host at all, and it must not be
    /// able to set the mode.
    fn stdin_is_live_host(&self) -> bool;

    /// Take up to `capacity` bytes from the console stdin buffer, advancing it.
    fn stdin_take(&mut self, capacity: usize) -> Vec<u8>;

    /// Flush buffered Stream-mode console output to the host terminal.
    fn flush_console(&mut self);
}

impl ConsoleAccess for WinApiState {
    fn console(&mut self) -> &mut ConsoleState {
        WinApiState::console(self)
    }

    fn console_if_loaded(&self) -> Option<&ConsoleState> {
        self.try_console()
    }

    fn stdin_available(&self) -> usize {
        self.file_io
            .stdin_bytes
            .len()
            .saturating_sub(self.file_io.stdin_cursor)
    }

    fn stdin_refill(&mut self, max_line: usize) -> Result<bool, ()> {
        crate::console::stdin::refill_stdin_from_host(&mut self.file_io, max_line)
    }

    fn stdin_is_live_host(&self) -> bool {
        self.file_io.stdin_mode == crate::GuestStdinMode::LiveHost
    }

    fn stdin_take(&mut self, capacity: usize) -> Vec<u8> {
        let io = &mut self.file_io;
        let cursor = io.stdin_cursor;
        let available = io.stdin_bytes.get(cursor..).unwrap_or(&[]);
        let take = available.len().min(capacity);
        let chunk: Vec<u8> = available.get(..take).unwrap_or(&[]).to_vec();
        io.stdin_cursor = cursor.saturating_add(take);
        chunk
    }

    fn flush_console(&mut self) {
        WinApiState::flush_console(self);
    }

    fn stdin_read_char(&mut self) -> Option<u8> {
        crate::console::stdin::read_char(&mut self.file_io)
    }
}

/// A console handler's whole world: the CPU port and the console capability.
///
/// Two fields, both narrow — the same shape that makes
/// [`HandlerContext`](crate::state::HandlerContext) the crate's strongest asset,
/// minus its `&mut WinApiState`. Handlers in [`crate::console`] take this and
/// therefore cannot name window, present, heap, module, kernel, or any other
/// DLL slot, which is what ADR-0005 asks the compiler to enforce.
///
/// The fields are separate (rather than the struct implementing
/// [`ConsoleAccess`] and hiding the engine) so the existing borrow discipline
/// carries over verbatim: `cx.engine.read_rcx()?` and `cx.state.console()` are
/// disjoint borrows, so a handler reads an argument, pokes the console, and
/// writes a result in any order — exactly as it did through `ctx.engine` /
/// `ctx.state`.
pub struct ConsoleCall<'a> {
    /// CPU engine (mem_read / mem_write / register access).
    pub engine: &'a mut dyn CpuEngine,
    /// Everything this call may reach outside the CPU.
    pub state: &'a mut dyn ConsoleAccess,
}

// ── Window dialogs ─────────────────────────────────────────────────────────

/// The dialog plumbing `comdlg32` needs off `WindowState`, and nothing else.
///
/// `comdlg32` reached `WindowState` 146 times when ADR-0005 was written and 171
/// (non-comment) when stage 2 re-measured it. Almost none of that is window machinery — it is dialog
/// session state that happens to live in `WindowState`: which policy the host
/// set, the in-flight session, the guest VA of the planted modal loop, the
/// common-dialog extended error. Those become named accessors here.
///
/// What is deliberately absent, because it belongs to `user32` and not to a
/// dialog: `windows`, `focus_window_handle`, `capture_window_handle`,
/// `control_states`, `registered_messages`, `modal_frames`,
/// `window_mirror_rev`. A dialog that truly needs those is doing window work
/// and should say so.
///
/// The two host bridges follow the same rule at a finer grain:
/// `file_dialog_bridge` is reachable only as
/// [`WindowDialogAccess::has_native_file_dialog_bridge`] (the handler's single
/// question), and `print_dialog_bridge` is not reachable at all.
pub trait WindowDialogAccess {
    /// The common-dialog extended error last reported by a common dialog.
    fn comm_dlg_extended_error(&mut self) -> u32;

    /// Overwrite the common-dialog extended error.
    fn set_comm_dlg_extended_error(&mut self, error: u32);

    /// Host-side decision for `GetOpenFileName`/`GetSaveFileName`.
    fn file_dialog_policy(&mut self) -> FileDialogPolicy;

    /// Whether the host registered a native file-dialog bridge (the macOS panel
    /// behind `GuestHandle::set_file_dialog_bridge`).
    ///
    /// A yes/no verb rather than the `file_dialog_bridge` field: the handler
    /// asks whether to take the bridge flow and must not be able to reach the
    /// host closure behind it. `print_dialog_bridge` is deliberately NOT
    /// exposed this way — the print panel is driven through
    /// [`Self::take_pending_native_print_dialog`] and the modal-panel plumbing
    /// that stays on `WindowState`, so `PrintDlgW` is a wide handler.
    fn has_native_file_dialog_bridge(&mut self) -> bool;

    /// Record (or clear) the last selected file-dialog path.
    fn set_last_file_dialog_path(&mut self, path: Option<String>);

    /// The in-flight interactive file dialog, if one is open.
    fn file_dialog(&mut self) -> Option<&FileDialogSession>;

    /// Open or close the interactive file dialog.
    fn set_file_dialog(&mut self, session: Option<FileDialogSession>);

    /// Take the parked native file-dialog request, if the guest is parked.
    fn take_pending_native_file_dialog(&mut self) -> Option<PendingNativeFileDialog>;

    /// Park a native file-dialog request.
    fn set_pending_native_file_dialog(&mut self, pending: Option<PendingNativeFileDialog>);

    /// Guest VA of the planted file-dialog modal-loop body.
    fn file_dialog_loop_va(&mut self) -> u64;

    /// Guest VA of the planted file-dialog proc stub.
    fn file_dialog_proc_va(&mut self) -> u64;

    /// Host-side decision for `ChooseFontW`.
    fn font_dialog_policy(&mut self) -> FontDialogPolicy;

    /// The in-flight modal font dialog, if one is open.
    fn font_dialog(&mut self) -> Option<&FontDialogSession>;

    /// The in-flight modal font dialog, mutably, if one is open — the effects
    /// toggle flips the session's own checkbox state and keeps the dialog open.
    fn font_dialog_mut(&mut self) -> Option<&mut FontDialogSession>;

    /// Open or close the modal font dialog.
    fn set_font_dialog(&mut self, session: Option<FontDialogSession>);

    /// Host-side decision for `PrintDlgW`.
    fn print_dialog_policy(&mut self) -> PrintDialogPolicy;

    /// Take the parked native print-dialog request.
    fn take_pending_native_print_dialog(&mut self) -> Option<PendingNativePrintDialog>;

    /// Park a native print-dialog request.
    fn set_pending_native_print_dialog(&mut self, pending: Option<PendingNativePrintDialog>);

    /// Host-side decision for `PageSetupDlgW`.
    fn page_setup_dialog_policy(&mut self) -> PageSetupDialogPolicy;

    /// Take the parked native page-setup request.
    fn take_pending_native_page_setup(&mut self) -> Option<PendingNativePageSetup>;

    /// Park a native page-setup request.
    fn set_pending_native_page_setup(&mut self, pending: Option<PendingNativePageSetup>);

    /// Next host `NSPrintInfo` id-table key (`PrintDlgW` hands one out).
    fn next_print_info_id(&mut self) -> u32;

    /// Advance the host `NSPrintInfo` id-table key.
    fn set_next_print_info_id(&mut self, id: u32);
}

impl WindowDialogAccess for WinApiState {
    fn comm_dlg_extended_error(&mut self) -> u32 {
        self.window_state().comm_dlg_extended_error
    }

    fn set_comm_dlg_extended_error(&mut self, error: u32) {
        self.window_state().comm_dlg_extended_error = error;
    }

    fn file_dialog_policy(&mut self) -> FileDialogPolicy {
        self.window_state().file_dialog_policy.clone()
    }

    fn has_native_file_dialog_bridge(&mut self) -> bool {
        self.try_window_state()
            .is_some_and(|ws| ws.file_dialog_bridge.is_some())
    }

    fn set_last_file_dialog_path(&mut self, path: Option<String>) {
        self.window_state().last_file_dialog_path = path;
    }

    fn file_dialog(&mut self) -> Option<&FileDialogSession> {
        self.window_state().file_dialog.as_ref()
    }

    fn set_file_dialog(&mut self, session: Option<FileDialogSession>) {
        self.window_state().file_dialog = session;
    }

    fn take_pending_native_file_dialog(&mut self) -> Option<PendingNativeFileDialog> {
        self.window_state().pending_native_file_dialog.take()
    }

    fn set_pending_native_file_dialog(&mut self, pending: Option<PendingNativeFileDialog>) {
        self.window_state().pending_native_file_dialog = pending;
    }

    fn file_dialog_loop_va(&mut self) -> u64 {
        self.window_state().file_dialog_loop_va
    }

    fn file_dialog_proc_va(&mut self) -> u64 {
        self.window_state().file_dialog_proc_va
    }

    fn font_dialog_policy(&mut self) -> FontDialogPolicy {
        self.window_state().font_dialog_policy.clone()
    }

    fn font_dialog(&mut self) -> Option<&FontDialogSession> {
        self.window_state().font_dialog.as_ref()
    }

    fn font_dialog_mut(&mut self) -> Option<&mut FontDialogSession> {
        self.window_state().font_dialog.as_mut()
    }

    fn set_font_dialog(&mut self, session: Option<FontDialogSession>) {
        self.window_state().font_dialog = session;
    }

    fn print_dialog_policy(&mut self) -> PrintDialogPolicy {
        self.window_state().print_dialog_policy.clone()
    }

    fn take_pending_native_print_dialog(&mut self) -> Option<PendingNativePrintDialog> {
        self.window_state().pending_native_print_dialog.take()
    }

    fn set_pending_native_print_dialog(&mut self, pending: Option<PendingNativePrintDialog>) {
        self.window_state().pending_native_print_dialog = pending;
    }

    fn page_setup_dialog_policy(&mut self) -> PageSetupDialogPolicy {
        self.window_state().page_setup_dialog_policy.clone()
    }

    fn take_pending_native_page_setup(&mut self) -> Option<PendingNativePageSetup> {
        self.window_state().pending_native_page_setup.take()
    }

    fn set_pending_native_page_setup(&mut self, pending: Option<PendingNativePageSetup>) {
        self.window_state().pending_native_page_setup = pending;
    }

    fn next_print_info_id(&mut self) -> u32 {
        self.window_state().next_print_info_id
    }

    fn set_next_print_info_id(&mut self, id: u32) {
        self.window_state().next_print_info_id = id;
    }
}

// ── Presenter ──────────────────────────────────────────────────────────────

/// The presenter verbs a paint or frame handler needs, and nothing else.
///
/// Outside `state/` and `present/` the crate reached `PresentState` 55 times
/// (user32 20, gdi32 16, d3d9 10, opengl32_wgl 8, shell32 1). Every one is a
/// verb — publish this window, mark this region dirty, hand me the surface to
/// paint into — so the capability is a verb list rather than a struct.
///
/// What this does **not** yet do: `WinApiState::present()` remains `pub` and
/// still hands back `&mut PresentState`, so the fields are reachable from any
/// handler that asks for the whole state. The trait narrows the handlers that
/// take it as a `dyn` parameter; it is not a closure of the surface. There is
/// deliberately no `HandlerContext::present()` (ADR-0005's stage-1 note: every
/// present-only handler also needs the CPU port or another capability, so the
/// accessor would be dead code), which is why present narrowing happens at
/// helper signatures — see `user32::window::geom_ops::apply_z_order`.
///
/// The gap that leaves is measured rather than assumed: ADR-0005's stage-2 note
/// counts whole-state bindings per cluster, and
/// `scripts/check.sh --only state-bindings` fails if that count rises.
///
/// Every method takes `&mut self` even where it only reads: the presenter slot
/// is lazily allocated, so `PresentState::windows_rev()` through a `&self`
/// accessor would answer for a different (absent) slot than the one the
/// handlers see.
pub trait PresentAccess {
    /// Queue `window` for the coalesced publish the runtime drains at the idle
    /// boundary.
    fn publish_deferred(&mut self, window: Hwnd);

    /// Publish `window` immediately (one vsync-blocking present).
    fn publish(&mut self, window: Hwnd);

    /// Publish every queued window; returns how many were queued.
    fn drain_pending_publishes(&mut self) -> usize;

    /// Publish every window whose content revision moved.
    fn reconcile_and_publish(&mut self);

    /// Make sure `window` has a paint surface of at least this size.
    fn ensure_surface(&mut self, window: Hwnd, width: u32, height: u32);

    /// The paint surface for `window`, if one was created.
    fn surface(&mut self, window: Hwnd) -> Option<&WindowSurface>;

    /// The paint surface for `window`, mutably, if one was created.
    fn surface_mut(&mut self, window: Hwnd) -> Option<&mut WindowSurface>;

    /// Union `rect` into `window`'s dirty accumulator.
    fn mark_dirty(&mut self, window: Hwnd, rect: IRect);

    /// Composite a whole frame into `window`'s surface and publish it.
    fn blit_frame(&mut self, window: Hwnd, frame: &[u32], frame_width: u32, frame_height: u32);

    /// Record how long one BitBlt mask copy took, for the frame-timing report.
    fn record_blit_copy(&mut self, nanoseconds: u128);

    /// Record the erase background colour the window's class brush implies.
    fn set_background_color(&mut self, window: Hwnd, color: u32);

    /// Ask the host presenter to repaint now.
    fn request_host_sync(&mut self);

    /// Add `window` to the top-level window set (a parentless window was
    /// created), bumping the set revision the host presenter reconciles on.
    fn register_top_level(&mut self, window: Hwnd);

    /// Drop `window` from the top-level window set (it was destroyed).
    fn unregister_top_level(&mut self, window: Hwnd);

    /// Move `window` to the top of the top-level z-order.
    fn z_order_to_top(&mut self, window: Hwnd);

    /// Move `window` to the bottom of the top-level z-order.
    fn z_order_to_bottom(&mut self, window: Hwnd);

    /// Top-level handles in back-to-front z-order.
    fn top_level_z_order(&mut self) -> &[Hwnd];

    /// Invoke the host presenter's wake callback, if one is registered.
    fn wake_presenter(&mut self);
}

impl PresentAccess for WinApiState {
    fn publish_deferred(&mut self, window: Hwnd) {
        self.present().publish_deferred(window);
    }

    fn publish(&mut self, window: Hwnd) {
        self.present().publish(window);
    }

    fn drain_pending_publishes(&mut self) -> usize {
        self.present().drain_pending_publishes()
    }

    fn reconcile_and_publish(&mut self) {
        self.present().reconcile_and_publish();
    }

    fn ensure_surface(&mut self, window: Hwnd, width: u32, height: u32) {
        self.present().ensure_surface(window, width, height);
    }

    fn surface(&mut self, window: Hwnd) -> Option<&WindowSurface> {
        self.present().surfaces.get(&window)
    }

    fn surface_mut(&mut self, window: Hwnd) -> Option<&mut WindowSurface> {
        self.present().surfaces.get_mut(&window)
    }

    fn mark_dirty(&mut self, window: Hwnd, rect: IRect) {
        self.present().mark_dirty(window, rect);
    }

    fn blit_frame(&mut self, window: Hwnd, frame: &[u32], frame_width: u32, frame_height: u32) {
        self.present()
            .blit_frame(window, frame, frame_width, frame_height);
    }

    fn record_blit_copy(&mut self, nanoseconds: u128) {
        self.present().record_blit_copy(nanoseconds);
    }

    fn set_background_color(&mut self, window: Hwnd, color: u32) {
        self.present().set_background_color(window, color);
    }

    fn request_host_sync(&mut self) {
        self.present().request_host_sync();
    }

    fn register_top_level(&mut self, window: Hwnd) {
        self.present().register_top_level(window);
    }

    fn unregister_top_level(&mut self, window: Hwnd) {
        self.present().unregister_top_level(window);
    }

    fn z_order_to_top(&mut self, window: Hwnd) {
        let _ = self.present().z_order_to_top(window);
    }

    fn z_order_to_bottom(&mut self, window: Hwnd) {
        let _ = self.present().z_order_to_bottom(window);
    }

    fn top_level_z_order(&mut self) -> &[Hwnd] {
        self.present().z_order.as_slice()
    }

    fn wake_presenter(&mut self) {
        if let Some(wake) = self.present().wake.as_ref() {
            wake();
        }
    }
}
