//! Native-panel bridge protocol: the shared modal-frame "up" / "down" halves
//! for the host-panel (rfd / AppKit) bridges.
//!
//! Every native panel (`GetOpenFileName`/`GetSaveFileName`, `PrintDlgW`,
//! `PageSetupDlgW`, `MessageBoxA/W`, `ShellAboutW`) is a modal session in
//! the same sense a guest dialog is: while the guest is parked on the bridge
//! the message queue stays modal (an empty `GetMessage` must keep yielding),
//! and the panel covers the window that was active at launch. Each bridge
//! used to hand-roll the frame pair inline:
//!
//! ```text
//! let owner = state.window_state().active_window_handle.as_u64();
//! let (frame, _signal) = ModalFrame::activate(state, engine, owner, None, &[])?;
//! // ... the bridge runs without the shared lock ...
//! if let Some(frame) = frame { /* finish_modal(...) */ }
//! ```
//!
//! [`open_native_panel`] / [`finish_native_panel`] are that pair, shared by
//! every bridge. The per-kind RESULT MAPPING stays per-kind: each bridge maps
//! its own pick to a [`ModalResult`] and runs its own write-back + API return
//! (the `OPENFILENAME` round-trip, the DEVMODE/DEVNAMES write-back, the
//! dialog result) — those are genuinely per-kind strategies, and unifying
//! them into one mapper would be parameter soup.

use anyhow::Result;

use crate::user32::WinApiControlSignal;
use crate::user32::WinApiState;

use super::{ModalFrame, ModalResult, finish_modal};

/// Which native panel bridge is opening a modal frame.
///
/// Matches the current pending-record variants: file, print, page-setup, and
/// the MessageBox record shared by `MessageBoxA/W` and `ShellAboutW`. There
/// is no `Font` kind yet — `ChooseFontW` runs an in-app dialog, not a native
/// bridge; add the variant when a native font panel lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativePanelKind {
    /// `GetOpenFileNameA/W` / `GetSaveFileNameA/W` (rfd NSOpenPanel/NSSavePanel).
    File,
    /// `PrintDlgW` (rfd NSPrintPanel).
    Print,
    /// `PageSetupDlgW` (rfd NSPageLayout).
    PageSetup,
    /// `MessageBoxA/W` (rfd alert).
    MessageBox,
    /// `ShellAboutW` (the same rfd alert as MessageBox, MB_OK shape).
    ShellAbout,
}

/// Open the modal frame for a native panel launch: the shared "up" half.
///
/// The panel is a modal session keyed by the window that was active when it
/// opened — the native-bridge shape: `dialog_hwnd` = the active window, no
/// focus, empty subtree. The re-entry's [`finish_native_panel`] restores and
/// invalidates the owner. `kind` is logged so a session trace shows which
/// panel held the queue modal.
pub(crate) fn open_native_panel(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    kind: NativePanelKind,
) -> Result<ModalFrame> {
    let owner = state.window_state().active_window_handle.as_u64();
    let (frame, _signal) = ModalFrame::activate(state, engine, owner, None, &[])?;
    tracing::debug!(
        target: "wiegui",
        kind = ?kind,
        owner,
        "native panel frame opened"
    );
    Ok(frame)
}

/// Finish the native bridge's modal frame: the shared "down" half.
///
/// Symmetric to [`open_native_panel`]: depth down, activation restored, the
/// owner invalidated with the erase pattern — all via [`finish_modal`]. A
/// `None` frame (a bridge that opened no frame) finishes nothing and leaves
/// the depth untouched. Returns the `deliver_focus_change` bridge signal when
/// a guest-WndProc owner must run first; the caller returns it as `Err(..)`
/// so the outer API call completes with the panel result.
pub(crate) fn finish_native_panel(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    frame: Option<ModalFrame>,
    result: ModalResult,
) -> Result<Option<WinApiControlSignal>> {
    if let Some(frame) = frame {
        return finish_modal(state, engine, frame, result);
    }
    Ok(None)
}
