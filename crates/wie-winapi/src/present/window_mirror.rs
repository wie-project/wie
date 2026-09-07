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
}
