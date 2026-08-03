//! FindTextW / ReplaceTextW integration (host side).
//!
//! `FindTextW` / `ReplaceTextW` (comdlg32) are implemented entirely inside
//! `wie-winapi`'s comdlg32 module: the handler builds the modeless find
//! dialog from the existing EDIT / STATIC / BUTTON controls (the dialog is a
//! guest overlay composited into the owner surface, exactly like the file
//! dialog — no presenter-side rendering), and the dialog's buttons write the
//! user's choices back into the guest `FINDREPLACE` struct and post the
//! registered `FINDMSGSTRING` message to the owner.
//!
//! Unlike the modal file dialog, the find dialog needs no policy gate and no
//! runtime wiring: it is modeless, so the guest's main loop keeps pumping and
//! a headless/trace run cannot hang on it. The window records and the message
//! queue post are harmless when no real window is on screen. This module is
//! the explicit GUI integration point for symmetry with the file dialog; it
//! exists so future presenter-side find-dialog work has a home, and it is
//! called (as a documented no-op) by `super::app::run_gui_windowed` so GUI
//! sessions state their intent to show find dialogs.

use wie_runtime::RuntimeSession;

/// Enable interactive Find/Replace dialogs on a GUI session.
///
/// Currently a no-op: `FindTextW` / `ReplaceTextW` always build their modeless
/// host dialog (see the module docs for why no gate is needed). Called from
/// `run_gui_windowed` next to
/// [`super::file_dialog::enable_interactive_file_dialogs`] so the two common
/// dialogs are wired through the same GUI entry point.
pub fn enable_interactive_find_dialogs(_session: &mut RuntimeSession) {}
