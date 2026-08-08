//! Interactive font-dialog integration (host side).
//!
//! `ChooseFontW` (comdlg32) is implemented in `wie-winapi`: under
//! [`FontDialogPolicy::Interactive`] the handler builds the font-dialog window
//! (family LISTBOX from the host fontdb families, size LISTBOX with point
//! sizes 8..72, Strikeout/Underline effects buttons, OK/Cancel) and runs the
//! same in-guest modal loop the file dialog uses (`wie-runtime/src/guest_stubs`
//! — the loop + proc stubs are shared), so the dialog renders through the
//! normal paint pipeline on the wgpu surface and the guest stays responsive
//! while it is open. The dialog's OK button writes the selection back into the
//! guest `LOGFONTW` (`lpLogFont`) and the `CHOOSEFONTW` fields; RNotepad then
//! calls `CreateFontIndirectW` on the returned LOGFONT and `WM_SETFONT`s its
//! edit control.
//!
//! This module is the GUI integration point: it registers the interactive
//! policy on the runtime session so [`super::app::run_gui_windowed`] enables
//! the dialog exactly when a real window is on screen. Headless runs and
//! `trace` keep the default [`FontDialogPolicy::Cancel`] policy and never open
//! a dialog.

use wie_runtime::RuntimeSession;
use wie_winapi::FontDialogPolicy;

/// Enable interactive font dialogs on a GUI session.
///
/// `ChooseFontW` then builds the host font dialog instead of returning a
/// scripted policy answer. Must run before the guest executes — call it right
/// after the session is created, before `run_windowed`.
pub fn enable_interactive_font_dialogs(session: &mut RuntimeSession) {
    session.set_font_dialog_policy(FontDialogPolicy::Interactive);
}
