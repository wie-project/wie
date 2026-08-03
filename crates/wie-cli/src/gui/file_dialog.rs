//! Interactive file-dialog integration (host side).
//!
//! `GetOpenFileNameW` / `GetSaveFileNameW` (comdlg32) are implemented in
//! `wie-winapi`: under [`FileDialogPolicy::Interactive`] the handler builds the
//! file-dialog window from the existing EDIT / LISTBOX / BUTTON controls and
//! runs its modal message loop in-guest (the same mechanism `DialogBoxParam`
//! uses — see `wie-runtime/src/guest_stubs`), so the dialog renders through
//! the normal paint pipeline on the wgpu surface and the guest stays
//! responsive while it is open. The "host dialog" is therefore a guest overlay
//! composited into the owner surface; no presenter-side rendering is needed.
//!
//! This module is the GUI integration point: it registers the interactive
//! policy on the runtime session so [`super::app::run_gui_windowed`] enables
//! the dialog exactly when a real window is on screen. Headless runs and
//! `trace` keep the default [`FileDialogPolicy::Cancel`] policy and never open
//! a dialog.

use wie_runtime::RuntimeSession;
use wie_winapi::FileDialogPolicy;

/// Enable interactive file dialogs on a GUI session.
///
/// `GetOpenFileName`/`GetSaveFileName` then build the host file dialog (path
/// EDIT + directory LISTBOX + OK/Cancel) instead of returning a scripted
/// policy answer. Must run before the guest executes — call it right after the
/// session is created, before `run_windowed`.
pub fn enable_interactive_file_dialogs(session: &mut RuntimeSession) {
    session.set_file_dialog_policy(FileDialogPolicy::Interactive);
}
