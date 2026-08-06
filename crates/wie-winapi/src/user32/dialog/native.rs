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
//! if let Some(frame) = frame { /* frame.finish(state, engine, result) */ }
//! ```
//!
//! [`NativePanelCtx`] is that pair, shared by every bridge: the ctx is
//! created per bridge entry ([`NativePanelCtx::new`]), the first entry opens
//! the frame ([`NativePanelCtx::open`]) and the re-entry finishes it
//! ([`NativePanelCtx::finish`]). The per-kind RESULT MAPPING stays per-kind:
//! each bridge maps its own pick to a [`ModalResult`] and runs its own
//! write-back + API return (the `OPENFILENAME` round-trip, the
//! DEVMODE/DEVNAMES write-back, the dialog result) — those are genuinely
//! per-kind strategies, and unifying them into one mapper would be parameter
//! soup.

use anyhow::Result;

use crate::user32::WinApiControlSignal;
use crate::user32::WinApiState;

use super::{ModalFrame, ModalResult};

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

/// The transient per-entry bundle for a native panel bridge: the shared "up"
/// and "down" halves of the panel's modal frame.
///
/// The ctx is created fresh per bridge entry (the first entry's
/// [`NativePanelCtx::open`] and the re-entry's [`NativePanelCtx::finish`]
/// live in different handler invocations — the frame rides inside the pending
/// record across the lock-free two-entry contract, unchanged). It exists so
/// the bridge's state/engine pair is named once and the kind travels with the
/// frame lifecycle for the session trace.
pub(crate) struct NativePanelCtx<'a> {
    state: &'a mut WinApiState,
    engine: &'a mut dyn wie_cpu::CpuEngine,
    kind: NativePanelKind,
}

impl<'a> NativePanelCtx<'a> {
    /// Bundle `state` + `engine` with the panel `kind` for one bridge entry.
    pub(crate) fn new(
        state: &'a mut WinApiState,
        engine: &'a mut dyn wie_cpu::CpuEngine,
        kind: NativePanelKind,
    ) -> Self {
        Self {
            state,
            engine,
            kind,
        }
    }

    /// Open the modal frame for a native panel launch: the shared "up" half.
    ///
    /// The panel is a modal session keyed by the window that was active when
    /// it opened — the native-bridge shape: `dialog_hwnd` = the active window,
    /// no focus, empty subtree. The re-entry's [`NativePanelCtx::finish`]
    /// restores and invalidates the owner. `kind` is logged so a session trace
    /// shows which panel held the queue modal.
    pub(crate) fn open(&mut self) -> Result<ModalFrame> {
        let owner = self.state.window_state().active_window_handle.as_u64();
        let (frame, _signal) = ModalFrame::activate(self.state, self.engine, owner, None, &[])?;
        tracing::debug!(
            target: "wiegui",
            kind = ?self.kind,
            owner,
            "native panel frame opened"
        );
        Ok(frame)
    }

    /// Finish the native bridge's modal frame: the shared "down" half.
    ///
    /// Symmetric to [`NativePanelCtx::open`]: depth down, activation restored,
    /// the owner invalidated with the erase pattern — all via
    /// [`ModalFrame::finish`]. A `None` frame (a bridge that opened no frame)
    /// finishes nothing and leaves the depth untouched. Returns the
    /// `deliver_focus_change` bridge signal when a guest-WndProc owner must
    /// run first; the caller returns it as `Err(..)` so the outer API call
    /// completes with the panel result.
    pub(crate) fn finish(
        &mut self,
        frame: Option<ModalFrame>,
        result: ModalResult,
    ) -> Result<Option<WinApiControlSignal>> {
        if let Some(frame) = frame {
            return frame.finish(self.state, self.engine, result);
        }
        Ok(None)
    }
}
