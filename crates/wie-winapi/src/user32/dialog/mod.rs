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
use crate::user32::{WinApiControlSignal, WinApiState, deliver_focus_change, find_window_mut};
use anyhow::Result;

mod api;
mod modal;
mod paint;
mod proc;
mod template;

pub(crate) use paint::paint_dialog;

/// Activate a freshly created modal dialog: bump the queue's dialog depth (an
/// empty `GetMessage` must yield, not synthesize the regression-mode WM_QUIT),
/// hand activation to the dialog, move keyboard focus to its initial control,
/// and mark the given subtree invalidated so the first empty `GetMessage`
/// synthesizes the WM_PAINTs (dialog face + controls).
///
/// Shared by the three modal-dialog builders — `CreateDialogParamA/W`, the
/// comdlg32 open-file dialog, and the comdlg32 font dialog. The subtree is
/// explicit (not derived from `dialog_hwnd`) because the builders differ in
/// which children they seed with a first paint: the font dialog, for example,
/// paints its listboxes and effect buttons but not its labels or OK/Cancel.
///
/// `focus_hwnd == None` leaves the current focus untouched (a dialog without a
/// `WS_TABSTOP` child keeps the owner's focus). Returns the
/// `deliver_focus_change` bridge signal, which callers discard — the
/// dialog-specific tail (planted modal-loop stub vs `WM_INITDIALOG` guest
/// callback) runs after this returns.
pub(crate) fn activate_modal_dialog(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    dialog_hwnd: u64,
    focus_hwnd: Option<u64>,
    subtree: &[u64],
) -> Result<Option<WinApiControlSignal>> {
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

    // Mark the subtree invalidated so the first empty GetMessage paints the
    // dialog face + controls.
    for &hwnd in subtree {
        if let Some(window) = find_window_mut(state, hwnd) {
            window.invalidated = true;
        }
    }

    Ok(signal)
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
