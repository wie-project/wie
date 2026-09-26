//! The presenter-side window mirror (Wave 2, Step 2): a snapshot of the
//! window-tree state the host reads per input event / per frame, living on
//! the [`PresentChannel`](super::PresentChannel) behind its own mutex —
//! the same pattern as the z-order/window-rev mirrors.
//!
//! Guest-side mutation sites call [`crate::WinApiState::sync_window_mirror`]
//! (a cheap projection rebuild) right after mutating `WindowState`; host-side
//! accessors (`GuestHandle::window_at`, `mouse_tracking`, `set_key_state`,
//! …) read ONLY this mirror, so a guest handler holding the big
//! `WinApiState` lock can never stall an input event or a frame.
//!
//! Two pieces travel outside the plain projection:
//!
//! - **Keyboard writes flow host → guest** through the same mirror: the
//!   host's `set_key_state` appends a `(vk, pressed)` event here, and the
//!   guest's keyboard-state readers (`GetKeyState`, `GetAsyncKeyState`,
//!   `GetKeyboardState`, `IsDialogMessage`, EDIT Shift+Tab) drain the events
//!   into `WindowState::keyboard_state` before reading — under the big lock
//!   they already hold, so no new lock ordering is introduced.
//! - **The mouse travels host → guest the same way**, in two pieces with two
//!   different trigger semantics: `mouse_buttons` holds the host's currently
//!   pressed `MK_*` mask as *level* state (a copy, never drained — a held
//!   button must read down on every `GetDeviceState`), and `wheel_notches`
//   accumulates *relative* movement that the DirectInput mouse report drains
//!   on read. Both are written host-side through channel-only accessors, so an
//!   input event still never takes the big lock.
//! - **The menu-bar cache gate** (`menu_dirty`) mirrors the
//!   `WindowState` flag so the per-Frame `window_menu_items` cache hit does
//!   not take the big lock; a rebuild (dirty or focus move) still takes it.

use crate::handles::Hwnd;

/// One mirrored window record: the projection of `WindowRecord` the host
/// reads for hit-testing, capture, focus walks, the menu bar, and tracking.
#[derive(Debug, Clone)]
pub struct MirrorWindow {
    /// Runtime-owned fake HWND.
    pub handle: Hwnd,
    /// Parent or owner window (NULL = top-level).
    pub parent: Hwnd,
    /// Position in the parent's client area.
    pub x: i32,
    pub y: i32,
    /// Size.
    pub width: i32,
    pub height: i32,
    /// Visibility (`ShowWindow` state).
    pub visible: bool,
    /// Window title (`SetWindowText` / WM_SETTEXT).
    pub title: String,
    /// Menu handle (0 = none / a child id).
    pub menu_handle: u64,
    /// Whether `TrackMouseEvent` armed hover/leave tracking.
    pub mouse_tracking: bool,
}

/// The mirrored window state + the pending host→guest keyboard writes.
#[derive(Debug, Default)]
pub(crate) struct WindowMirror {
    /// All live window records in creation order (the hit-test's z-order).
    windows: Vec<MirrorWindow>,
    /// The window with keyboard focus (what `GetFocus` returns in-guest).
    focus: Hwnd,
    /// The window holding the mouse capture (`SetCapture`), or NULL.
    capture: Hwnd,
    /// Mirror of `WindowState::menu_dirty` (the menu-bar cache gate).
    menu_dirty: bool,
    /// Pending host keyboard writes, drained by the guest keyboard readers.
    key_writes: Vec<(u16, bool)>,
    /// Latest host-reported cursor position in guest-logical screen pixels.
    /// Read-current (copy), never drained: `GetCursorPos` is
    /// level-triggered and must report the same position on repeated calls.
    cursor_pos: Option<(i32, i32)>,
    /// The host's currently pressed mouse buttons as Win32 `MK_*` bits
    /// (`MK_LBUTTON` 0x0001, `MK_RBUTTON` 0x0002, `MK_MBUTTON` 0x0010,
    /// `MK_XBUTTON1` 0x0020, `MK_XBUTTON2` 0x0040; the modifier bits
    /// `MK_SHIFT` / `MK_CONTROL` are never pushed here — a button mask is
    /// buttons).
    ///
    /// Level state, read-current (copy) and never drained, like
    /// `cursor_pos`: `GetDeviceState` must report the same held button on
    /// every read until the host pushes a newer mask. `0` before the first
    /// push is the truthful "nothing pressed".
    mouse_buttons: u16,
    /// Pending wheel movement as whole notches, `(horizontal, vertical)`,
    /// accumulated host → guest and drained by the DirectInput mouse report.
    ///
    /// Notches, not raw winit deltas: a `LineDelta` is already whole notches
    /// and a trackpad `PixelDelta` only becomes a notch after the host's
    /// own fractional accumulator (`WieApp::wheel_notches`) has seen enough of
    /// it — pushing raw deltas would make `lZ` report a fraction of a notch
    /// that no Windows mouse driver ever produces. Signed, because a
    /// DirectInput `lZ` is relative and a down-scroll must read negative.
    wheel_notches: (i32, i32),
}

impl WindowMirror {
    /// Replace the whole projection (the sync point's write).
    pub(crate) fn replace(
        &mut self,
        windows: Vec<MirrorWindow>,
        focus: Hwnd,
        capture: Hwnd,
        menu_dirty: bool,
    ) {
        self.windows = windows;
        self.focus = focus;
        self.capture = capture;
        self.menu_dirty = menu_dirty;
    }

    /// Run `f` over the mirrored window slice.
    pub(crate) fn with_windows<T>(&self, f: impl FnOnce(&[MirrorWindow]) -> T) -> T {
        f(&self.windows)
    }

    /// The mirrored `(focus, capture, menu_dirty)` triple.
    pub(crate) fn meta(&self) -> (Hwnd, Hwnd, bool) {
        (self.focus, self.capture, self.menu_dirty)
    }

    /// Set the mirrored menu-dirty flag (the rebuild path resets it).
    pub(crate) fn set_menu_dirty(&mut self, dirty: bool) {
        self.menu_dirty = dirty;
    }

    /// Append one host keyboard write (the host never touches the big lock).
    pub(crate) fn push_key_write(&mut self, vk: u16, pressed: bool) {
        self.key_writes.push((vk, pressed));
    }

    /// Drain the pending host keyboard writes (the guest readers apply them
    /// into `WindowState::keyboard_state` before reading).
    pub(crate) fn drain_key_writes(&mut self) -> Vec<(u16, bool)> {
        std::mem::take(&mut self.key_writes)
    }

    /// Record the latest host cursor position (the host never touches the
    /// big lock).
    pub(crate) fn set_cursor_pos(&mut self, x: i32, y: i32) {
        self.cursor_pos = Some((x, y));
    }

    /// Read the latest host cursor position (a copy, NOT a drain — cursor
    /// position is level-triggered, so repeated `GetCursorPos` calls report
    /// the same position until the host pushes a newer one).
    pub(crate) fn cursor_pos(&self) -> Option<(i32, i32)> {
        self.cursor_pos
    }

    /// Publish the host's pressed-mouse-button mask (Win32 `MK_*` bits — the
    /// host never touches the big lock). The whole mask is replaced, not
    /// merged: the host tracks the full set of held buttons, so a missing bit
    /// is a release.
    pub(crate) fn set_mouse_buttons(&mut self, mk: u16) {
        self.mouse_buttons = mk;
    }

    /// Read the published mouse-button mask (a copy, NOT a drain — see
    /// [`Self::mouse_buttons`]).
    pub(crate) fn mouse_buttons(&self) -> u16 {
        self.mouse_buttons
    }

    /// Accumulate wheel movement as whole notches (the host never touches the
    /// big lock). Saturating so a pathological event stream cannot wrap a
    /// `lZ` into the opposite direction.
    pub(crate) fn add_wheel_notches(&mut self, horizontal: bool, notches: i32) {
        if horizontal {
            self.wheel_notches.0 = self.wheel_notches.0.saturating_add(notches);
        } else {
            self.wheel_notches.1 = self.wheel_notches.1.saturating_add(notches);
        }
    }

    /// Drain the pending wheel notches — a relative axis has
    /// consumed-on-read semantics, exactly like the DirectInput mouse's
    /// `lX`/`lY` baseline, so a guest that reads twice sees the movement once.
    pub(crate) fn drain_wheel_notches(&mut self) -> (i32, i32) {
        std::mem::take(&mut self.wheel_notches)
    }
}
