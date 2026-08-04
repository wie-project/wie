//! Interactive file-dialog integration (host side).
//!
//! `GetOpenFileNameW` / `GetSaveFileNameW` (comdlg32) are implemented in
//! `wie-winapi`: under [`FileDialogPolicy::Interactive`] the handler either
//! calls the native file-dialog bridge registered here (a real macOS
//! NSOpenPanel/NSSavePanel via rfd — the OS-equivalent of Windows' common
//! dialogs) or, when no bridge is registered, builds the in-app "FileDialog"
//! window from the existing EDIT / LISTBOX / BUTTON controls and runs its
//! modal message loop in-guest (the same mechanism `DialogBoxParam` uses).
//! The in-app fallback renders through the normal paint pipeline on the wgpu
//! surface; the bridge path shows the native panel instead.
//!
//! This module is the GUI integration point: it registers the interactive
//! policy on the runtime session so [`super::app::run_gui_windowed`] enables
//! the dialog exactly when a real window is on screen, and — on macOS, where
//! rfd lives — registers the native bridge. Headless runs and `trace` keep
//! the default [`FileDialogPolicy::Cancel`] policy and never open a dialog.

use wie_runtime::RuntimeSession;
use wie_winapi::FileDialogPolicy;

/// Enable interactive file dialogs on a GUI session.
///
/// `GetOpenFileName`/`GetSaveFileName` then either show the native panel (when
/// the bridge is registered) or build the host file dialog (path EDIT +
/// directory LISTBOX + OK/Cancel) instead of returning a scripted policy
/// answer. Must run before the guest executes — call it right after the
/// session is created, before `run_windowed`.
pub fn enable_interactive_file_dialogs(session: &mut RuntimeSession) {
    session.set_file_dialog_policy(FileDialogPolicy::Interactive);
}

/// Shared per-top-level parent slots: the native-panel bridge (registered on
/// the guest thread BEFORE any window exists) reads the FOCUSED (or primary)
/// window's slot to parent its rfd panel; `WieApp` fills each slot when its
/// window is created.
#[cfg(target_os = "macos")]
type WindowSlot = super::app::ParentWindowSlots;

/// Register the native macOS file-panel bridge (rfd) on a GUI session.
///
/// With a bridge registered, `GetOpenFileName`/`GetSaveFileName` under
/// [`FileDialogPolicy::Interactive`] show a real NSOpenPanel/NSSavePanel: the
/// guest thread blocks inside the bridge until the user picks (dialog
/// semantics, the same seam as the MessageBox bridge), then the handler
/// writes the pick back into the `OPENFILENAME` buffer — confining it to a
/// guest volume at accept, so a pick outside the bottle cancels. The panel
/// starts in the guest directory mapped into the bottle and is parented to
/// the focused/primary winit window once it exists (`window_slots`). Sessions
/// that never register a bridge keep the in-app emulated dialog (headless
/// runs, `trace`).
#[cfg(target_os = "macos")]
pub fn register_native_file_dialog_bridge(
    handle: &wie_runtime::GuestHandle,
    window_slots: WindowSlot,
) {
    handle.set_file_dialog_bridge(Box::new({
        let handle = handle.clone();
        move |request| show_native_file_dialog(request, &handle, &window_slots)
    }));
}

/// Show one native file panel from the bridge callback.
///
/// Runs on the guest thread; rfd dispatches the panel to the main thread and
/// blocks until the user dismisses it (the same dispatch the MessageBox
/// bridge uses), so the guest parks with correct dialog semantics.
#[cfg(target_os = "macos")]
fn show_native_file_dialog(
    request: &wie_winapi::FileDialogRequest,
    handle: &wie_runtime::GuestHandle,
    window_slots: &WindowSlot,
) -> Option<wie_winapi::FileDialogPick> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(parent) = super::app::resolve_dialog_parent(handle, window_slots) {
        dialog = dialog.set_parent(parent.as_ref());
    }
    if let Some(directory) = &request.initial_host_dir {
        dialog = dialog.set_directory(directory);
    }
    if let Some(file_name) = &request.default_file_name {
        dialog = dialog.set_file_name(file_name.clone());
    }
    // The comdlg32 parse already validated simple `*.ext` globs; rfd wants
    // bare extensions ("txt") — what macOS's allowed-file-types needs. A
    // complex guest filter never reaches this point (the parse drops it).
    for filter in &request.filters {
        let extensions: Vec<String> = filter
            .patterns
            .iter()
            .filter_map(|pattern| pattern.strip_prefix("*.").map(str::to_owned))
            .collect();
        if !extensions.is_empty() {
            dialog = dialog.add_filter(filter.name.clone(), &extensions);
        }
    }
    let picked = if request.is_save {
        // NSSavePanel confirms overwriting an existing file natively.
        dialog.save_file()
    } else {
        dialog.pick_file()
    };
    picked.map(|host_path| wie_winapi::FileDialogPick { host_path })
}
