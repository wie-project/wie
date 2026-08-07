//! Modal dialog machinery: `CreateDialogParamA/W`, `EndDialog`,
//! `IsDialogMessageA/W`, `GetDlgItem*`, `SetDlgItemText*`, `DefDlgProc*`.
//!
//! The modal loop itself runs in-guest as a `GuestStubKind::DialogBoxParam`
//! stub (see `wie-runtime/src/guest_stubs.rs`); every piece of dialog logic
//! lives here on the host, split by concern:
//!
//! * [`template`] — `hInstance`+id template resolution (with the synthesized
//!   fallback when the id is missing).
//! * [`modal`] — the modal lifecycle: dialog window/child construction and
//!   `EndDialog` teardown.
//! * [`proc`] — the dialog-proc surface: `IsDialogMessage` keyboard routing
//!   and `DefDlgProc`.
//! * [`api`] — the `GetDlgItem*` / `SetDlgItemText*` / `SendDlgItemMessageW`
//!   accessors.
//! * [`paint`] — face + border painting into the owner surface.
//!
//! Deferred (documented in the structural-tier design): mnemonics, arrow-key
//! navigation, `WM_GETDLGCODE`-driven navigation, `DLGTEMPLATEEX`.

use crate::OuterReturn;
use crate::handles::Hwnd;
use crate::state::WindowFlags;
use crate::user32::{
    WinApiControlSignal, WinApiState, deliver_focus_change, find_window, find_window_mut,
};
use anyhow::Result;

mod api;
mod modal;
mod native;
mod paint;
mod proc;
mod template;

pub(crate) use native::{NativePanelCtx, NativePanelKind};
pub(crate) use paint::paint_dialog;

/// The unified result contract for a finished modal session.
///
/// Both modal mechanisms funnel into this one return contract: the in-guest
/// modal loop's `EndDialog` result (written to the guest result slot) and the
/// native bridge's host-side pick. [`ModalFrame::finish`] consumes it for the
/// shared bookkeeping; the per-kind tails (subtree removal, session close,
/// panel cleanup) stay per-site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalResult {
    /// The modal completed with a result value (`IDOK`, `IDYES`, …).
    Ok(u64),
    /// The modal was cancelled (`IDCANCEL`, a `None` native pick).
    Cancel,
}

/// One in-flight modal session's lifecycle bookkeeping.
///
/// Created by [`ModalFrame::activate`] when the modal opens (the "up" half)
/// and consumed by [`ModalFrame::finish`] when it closes (the "down" half).
/// The frame carries everything the shared teardown needs that is NOT
/// derivable once the modal window is gone: the previous active window (the
/// owner, restored and invalidated on finish) and whether the modal took
/// keyboard focus at activation.
#[derive(Debug, Clone)]
pub(crate) struct ModalFrame {
    /// The modal window: the dialog hwnd (template / file / font dialogs) or,
    /// for a native bridge with no guest window, the window that was active
    /// when the panel launched.
    pub(crate) active: Hwnd,
    /// The window the user returns to when the modal closes: the modal's
    /// owner (the dialog's parent, or the window that was active when a
    /// native panel launched). Restored and invalidated by
    /// [`ModalFrame::finish`].
    pub(crate) previous_active: Hwnd,
    /// The focus the modal took at activation (`None` = it left focus alone,
    /// so [`ModalFrame::finish`] restores nothing).
    pub(crate) focus: Option<Hwnd>,
    /// The windows invalidated at activation (the first-paint subtree).
    pub(crate) subtree: Vec<Hwnd>,
}

impl ModalFrame {
    /// Activate a freshly created modal session: bump the queue's dialog depth
    /// (an empty `GetMessage` must yield, not synthesize the regression-mode
    /// WM_QUIT), hand activation to the modal, move keyboard focus to its
    /// initial control, and mark the given subtree invalidated so the first
    /// empty `GetMessage` synthesizes the WM_PAINTs (dialog face + controls).
    ///
    /// Shared by the three modal-dialog builders — `CreateDialogParamA/W`, the
    /// comdlg32 open-file dialog, and the comdlg32 font dialog — and by the
    /// native bridge launches (print, MessageBox), which pass the window that
    /// was active when the panel opened as `dialog_hwnd`, no focus, and an
    /// empty subtree. The subtree is explicit (not derived from `dialog_hwnd`)
    /// because the builders differ in which children they seed with a first
    /// paint: the font dialog, for example, paints its listboxes and effect
    /// buttons but not its labels or OK/Cancel.
    ///
    /// `focus_hwnd == None` leaves the current focus untouched (a dialog
    /// without a `WS_TABSTOP` child keeps the owner's focus). Returns the
    /// `deliver_focus_change` bridge signal, which callers discard — the
    /// dialog-specific tail (planted modal-loop stub vs `WM_INITDIALOG` guest
    /// callback) runs after this returns.
    pub(crate) fn activate(
        state: &mut WinApiState,
        engine: &mut dyn wie_cpu::CpuEngine,
        dialog_hwnd: u64,
        focus_hwnd: Option<u64>,
        subtree: &[u64],
    ) -> Result<(ModalFrame, Option<WinApiControlSignal>)> {
        // Capture the window the user returns to BEFORE the modal takes over:
        // `ModalFrame::finish` restores it, and at finish time the modal window
        // may already be gone (the per-site subtree-removal tail runs first).
        // This is the modal's OWNER — the dialog's parent when it has one (the
        // active-window slot is not always set in synthetic/headless sessions,
        // while the dialog's parent always is), else the window that was
        // active at activation (the native-bridge shape, where the panel
        // covers the active window).
        let previous_active = state.window_state().active_window_handle;
        let owner = find_window(state, dialog_hwnd)
            .filter(|window| window.parent_handle != Hwnd::NULL)
            .map_or(previous_active, |window| window.parent_handle);

        {
            let mut queue = state.lock_message_queue();
            queue.dialog_depth = queue.dialog_depth.saturating_add(1);
            tracing::debug!(
                target: "wiegui",
                depth = queue.dialog_depth,
                "dialog depth up"
            );
        }
        // A modal dialog takes activation (real Windows): GetActiveWindow must
        // return the dialog while it is open — guests post Enter/keys to it.
        state.window_state().active_window_handle = Hwnd::from(dialog_hwnd);

        // Initial keyboard focus: the focused control receives WM_SETFOCUS
        // (host-side — controls have no guest WndProc, so no bridge signal).
        let mut signal = None;
        if let Some(focus_hwnd) = focus_hwnd {
            state.window_state().focus_window_handle = Hwnd::from(focus_hwnd);
            signal =
                deliver_focus_change(state, engine, 0, focus_hwnd, OuterReturn::Fixed(focus_hwnd))?;
        }

        // Mark the subtree invalidated so the first empty GetMessage paints
        // the dialog face + controls.
        for &hwnd in subtree {
            if let Some(window) = find_window_mut(state, hwnd) {
                window.invalidated = true;
            }
        }

        let frame = ModalFrame {
            active: Hwnd::from(dialog_hwnd),
            previous_active: owner,
            focus: focus_hwnd.map(Hwnd::from),
            subtree: subtree.iter().map(|&hwnd| Hwnd::from(hwnd)).collect(),
        };
        Ok((frame, signal))
    }

    /// Finish this modal session: the shared "down" half symmetric to
    /// [`ModalFrame::activate`].
    ///
    /// Decrements the queue's dialog depth (the single decrement point — the
    /// `WM_QUIT` the `EndDialog` tail posts is just a message, consumed by the
    /// loop with no bookkeeping), restores activation to the previous active
    /// window (the owner, captured in `activate`), hands keyboard focus back to
    /// the owner when the modal took it, and invalidates the owner with the
    /// erase pattern `EndDialog` uses so its next repaint erases the modal's
    /// region.
    ///
    /// The per-kind TAILS stay per-site: template-dialog window-subtree removal
    /// (`EndDialog`), file/font session close, native-bridge panel cleanup. The
    /// native bridge is not forced into the loop's shape — its frame is created
    /// and finished around the two-entry bridge flow instead. A dialog that
    /// closed with NO stored frame (a hand-built dialog or a double close) has
    /// nothing to restore; only the depth balances — see
    /// [`finish_modal_without_frame`].
    ///
    /// Returns the `deliver_focus_change` bridge signal when a guest-WndProc
    /// owner must run first (the caller returns it as `Err(..)`; the outer API
    /// call then completes with the modal result's value).
    pub(crate) fn finish(
        self,
        state: &mut WinApiState,
        engine: &mut dyn wie_cpu::CpuEngine,
        result: ModalResult,
    ) -> Result<Option<WinApiControlSignal>> {
        // Depth down, symmetric with activate's saturating_add.
        {
            let mut queue = state.lock_message_queue();
            queue.dialog_depth = queue.dialog_depth.saturating_sub(1);
            tracing::debug!(
                target: "wiegui",
                depth = queue.dialog_depth,
                "dialog depth down"
            );
        }

        // Restore activation to the window that owned the modal.
        state.window_state().active_window_handle = self.previous_active;

        // Restore keyboard focus to the owner when the modal took it at
        // activation. `deliver_focus_change` dispatches host-side controls in
        // place and bridges guest WndProcs / dialog procs; the outer API call the
        // bridge completes carries the modal result's value.
        let mut signal = None;
        if self.focus.is_some() {
            let owner = self.previous_active;
            if owner != Hwnd::NULL {
                let current_focus = state.window_state().focus_window_handle;
                if current_focus != owner {
                    state.window_state().focus_window_handle = owner;
                    signal = deliver_focus_change(
                        state,
                        engine,
                        current_focus.as_u64(),
                        owner.as_u64(),
                        OuterReturn::Fixed(modal_result_value(result)),
                    )?;
                }
            }
        }

        // Invalidate the owner with the erase pattern EndDialog uses: the dialog
        // composited into its surface, so the next cycle repaints the class-brush
        // background over the dialog region and every remaining control over the
        // face. The owner is the modal window's parent when it still exists (the
        // per-site subtree-removal tail runs before this in the EndDialog path,
        // so the captured `previous_active` carries it), else the window that was
        // active at activation.
        let owner = find_window(state, self.active.as_u64())
            .filter(|window| window.parent_handle != Hwnd::NULL)
            .map_or(self.previous_active, |window| window.parent_handle);
        if owner != Hwnd::NULL
            && let Some(window) = find_window_mut(state, owner.as_u64())
        {
            window.invalidated = true;
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
        invalidate_subtree(state, owner);

        // A top-level created WITHOUT `WS_VISIBLE` (RNotepad's main window —
        // it shows its children, not itself) never repaints through the paint
        // synthesizer, so the owner's own erase can never cover the modal's
        // vacated face. The window that actually sits BENEATH the dialog in
        // the owner surface — the main EDIT — must repaint over it, and an
        // EDIT repaints only its pending row band (a caret blink leaves a
        // 1-row band). Reset every EDIT in the owner subtree to a FULL
        // repaint so the next synthesized WM_PAINT covers the whole client —
        // the "Cancel takes two clicks" fix (mirrors comdlg32/find.rs): the
        // first click closed the dialog but its face stayed in the frame
        // until an unrelated repaint hid it.
        crate::user32::controls::reset_edit_bands_in_subtree(state, owner.as_u64());

        // Defensive, symmetric with activate's first-paint invalidation: any
        // modal window that survived its family's teardown tail (a future closer
        // that forgets to remove the subtree) still ends invalidated instead of
        // lingering painted on the owner surface.
        for &hwnd in &self.subtree {
            if let Some(window) = find_window_mut(state, hwnd.as_u64()) {
                window.invalidated = true;
            }
        }

        Ok(signal)
    }
}

/// The frame-less modal teardown: `EndDialog`'s hand-built / double-close path.
///
/// When a dialog closes with no stored [`ModalFrame`] (the builder never ran
/// `activate`, or the close already consumed the frame), there is nothing to
/// restore — activation and focus were never taken — but the queue depth still
/// balances so a stale depth cannot swallow the next command.
pub(crate) fn finish_modal_without_frame(state: &mut WinApiState) {
    let mut queue = state.lock_message_queue();
    queue.dialog_depth = queue.dialog_depth.saturating_sub(1);
}

/// The numeric value a modal result contributes to the outer API return of a
/// bridged focus message (the WndProc's messages complete the modal-closing
/// API call).
#[must_use]
fn modal_result_value(result: ModalResult) -> u64 {
    match result {
        ModalResult::Ok(value) => value,
        ModalResult::Cancel => 0,
    }
}

/// Mark `hwnd` and every descendant invalidated so the next paint cycle
/// repaints the whole subtree in one pass — the full-window rerender after a
/// modal dialog closes (the dialog composited into the owner surface, so the
/// owner background and all remaining controls must repaint over its region).
fn invalidate_subtree(state: &mut WinApiState, hwnd: Hwnd) {
    if let Some(window) = find_window_mut(state, hwnd.as_u64()) {
        window.invalidated = true;
    }
    let mut frontier: Vec<Hwnd> = vec![hwnd];
    while !frontier.is_empty() {
        let mut next: Vec<Hwnd> = Vec::new();
        for window in &mut state.window_state().windows {
            if frontier.contains(&window.parent_handle) {
                window.invalidated = true;
                next.push(window.handle);
            }
        }
        frontier = next;
    }
}

pub use api::{
    handle_get_dlg_item_a, handle_get_dlg_item_int, handle_get_dlg_item_text_a,
    handle_get_dlg_item_text_w, handle_get_dlg_item_w, handle_send_dlg_item_message_w,
    handle_set_dlg_item_int, handle_set_dlg_item_text_a, handle_set_dlg_item_text_w,
};
pub use modal::{handle_create_dialog_param_a, handle_create_dialog_param_w, handle_end_dialog};
pub use proc::{
    handle_def_dlg_proc_a, handle_def_dlg_proc_w, handle_is_dialog_message_a,
    handle_is_dialog_message_w,
};
