//! The host-facing surface of a session: every `wie-winapi` name host code
//! (`wie-cli`) is allowed to say, and the session entry points for the work it
//! used to do itself (ADR-0006 decision 3).
//!
//! # Why this module exists
//!
//! ADR-0006 decision 3 removes the one *structural* anomaly in the crate
//! graph: `wie-cli` depending on `wie-winapi` directly. That edge let host code
//! call into the emulator's guts without going through [`RuntimeSession`] /
//! [`GuestHandle`] — i.e. without the lock discipline and the in-guest-callback
//! invariants every other entry point obeys. That is the documented deadlock
//! class (see CLAUDE.md): a guest thread holds the big `WinApiState` mutex for
//! the whole of an API call, so host code that touches that state directly, at
//! an arbitrary moment, can wedge the process.
//!
//! With the edge gone, `wie-cli` cannot name `wie_winapi` at all — the compiler
//! enforces it, because a transitive dependency is not nameable. What the CLI
//! *legitimately* needs crosses here instead, in the three classes that
//! ADR-0006 decision 3 asked for:
//!
//! 1. **Types the host names** ([`Dimension`], [`SurfaceFrame`], [`VolumeConfig`],
//!    the dialog policies and their request/pick payloads, …). Re-exported from
//!    [`wie_winapi::port`], the published surface (ADR-0006 decision 1), so the
//!    CLI and the runtime name the same types and a rename stays a
//!    `wie-winapi`-internal change.
//! 2. **Host-side policy and path queries** — the free functions the CLI
//!    called ([`host_path_to_guest`], [`global_bottle_root`],
//!    [`profile_sigint_armed`], …). These are pure queries or idempotent host
//!    services: no guest state, no big lock, so re-exporting them *is* the fix.
//! 3. **Work that belongs to the session** — the synthesized raw-input lane.
//!    The CLI used to call the emulator's raw-input posting functions and post
//!    the resulting `WM_INPUT` messages itself. That is guest-message delivery,
//!    so it is now a [`GuestHandle`] method ([`GuestHandle::post_raw_keyboard`]
//!    and friends): same queue, same wake signal, same "never take the big
//!    lock" rule as [`GuestHandle::post_message`], and the raw-input record
//!    encoding plus its `RAWINPUT`/`WM_INPUT` vocabulary stay inside the
//!    emulator where they are pinned by tests.
//!
//! # What this module is *not*
//!
//! Not a second `port`. It re-exports; it does not decide. Every name here is
//! either already in `wie_winapi::port` or is one the CLI demonstrably needs
//! through a signature (`WindowState` fields, handlers, state accessors do
//! **not** appear). `./scripts/check.sh --only winapi-port` keeps `wie-runtime`
//! off the gated WinAPI modules; the two names in the "not yet published"
//! section below are the scoped, commented exception that gate records.

// ── Types the host names ───────────────────────────────────────────────────
//
// Every one of these is already on `wie_winapi::port`, so this is a rename-free
// pass-through: the CLI named it as `wie_winapi::…`, it now names it as
// `wie_runtime::host::…`.
pub use wie_winapi::port::{
    Dimension, DisplayMetrics, FileDialogPolicy, FontDialogPolicy, Hwnd, IRect,
    MessageQueueIdlePolicy, PageSetupDialogPolicy, PrintDialogPolicy, RawInputPost, RawMouseButton,
    RawMouseReport, SurfaceFrame, VolumeConfig, WinMsg, bottle_root_from_env, drive_d_from_env,
    guest_path_to_host, host_path_to_guest, is_legacy_keyboard_input_excluded,
    is_legacy_mouse_input_excluded, is_winapi_implemented, post_raw_keyboard_event,
    post_raw_mouse_event, seed_default_skeleton,
};

// The dialog bridges hand the host plain-data request/pick payloads (Rust
// types with no guest-visible layout), so the host callback signature — not
// the emulator — decides their shape. `port` publishes the three policies;
// these payloads are reachable through the crate root. Putting them here, not
// widening `port`, keeps the published surface unchanged.
pub use wie_winapi::{
    FileDialogPick, FileDialogRequest, PageSetupDialogPick, PageSetupDialogRequest,
    PrintDialogPick, PrintDialogRequest, PrintJobRequest, global_bottle_root,
};

// Host path policy the CLI applies to its own arguments before a session
// exists (`--gui` argv preflight, `--bottle` drive probing) and the terminal
// services `run` uses around a console session. Pure functions over host
// paths / the host tty — they never see guest state, so no session is needed.
//
// `bottle_guest_path_to_host` is `wie_winapi::bottle::guest_path_to_host`
// (bottle root + guest path → host path), aliased because `wie-winapi` exports
// a *different* function under the plain name above (`vfs::guest_path_to_host`
// takes a whole `VolumeConfig` and answers with a `HostMap`). Both are named
// here so the host never has to guess which one it is calling.
pub use wie_winapi::bottle::guest_path_to_host as bottle_guest_path_to_host;
pub use wie_winapi::console::{
    ensure_hooks_installed, profile_sigint_armed, restore_terminal, set_raw_mode,
};
pub use wie_winapi::vfs::path::{drive_letter, normalize_windows_path_separators};

use crate::GuestHandle;

/// Which mouse button a host raw-input report describes.
///
/// The host says what happened in host terms; the `RAWMOUSE` button flags stay
/// inside the emulator. Mirrors `wie_winapi`'s `RawMouseButton` deliberately:
/// that type names guest record bits, and the host has no business holding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMouseButton {
    /// Left button.
    Left,
    /// Right button.
    Right,
    /// Middle button.
    Middle,
}

impl HostMouseButton {
    /// The emulator-side button this host button encodes to.
    fn to_winapi(self) -> RawMouseButton {
        match self {
            Self::Left => RawMouseButton::Left,
            Self::Right => RawMouseButton::Right,
            Self::Middle => RawMouseButton::Middle,
        }
    }
}

/// A client-relative point in guest-logical pixels — where a mouse report
/// happened. Named so the raw-input entry points keep readable signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientPoint {
    /// Client-relative logical X.
    pub x: i32,
    /// Client-relative logical Y.
    pub y: i32,
}

impl ClientPoint {
    /// A client-relative point in guest-logical pixels.
    #[must_use]
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

// ── The synthesized raw-input lane ─────────────────────────────────────────
//
// Each method is the session's version of one raw-input posting call the CLI
// used to make, plus the `WM_INPUT` delivery the CLI used to post
// itself. `target` is the window the legacy message would have gone to,
// `sink` the event window that may have registered `RIDEV_INPUTSINK`, and
// `raw_buttons` the host's current `MK_*` mask (recorded in
// `RAWMOUSE.ulRawButtons`). The lane does its own registration filtering and
// locks only its own module mutex — never the big `WinApiState` lock — so these
// stay safe to call from the event-loop thread while a guest handler runs.
impl GuestHandle {
    /// Synthesize a keyboard `RAWKEYBOARD` and post the `WM_INPUT`s it produced.
    ///
    /// `message` is the `WM_KEYDOWN` / `WM_SYSKEYDOWN` / `WM_KEYUP` /
    /// `WM_SYSKEYUP` value the guest reads inside `RAWKEYBOARD.Message`. A
    /// no-op when no window registered the keyboard class, which is the common
    /// case.
    pub fn post_raw_keyboard(
        &self,
        target: u64,
        sink: Option<u64>,
        vkey: u16,
        pressed: bool,
        message: u32,
    ) {
        let posts =
            post_raw_keyboard_event(target, sink, self.focus_window(), vkey, pressed, message);
        self.post_raw_input_posts(posts);
    }

    /// Synthesize a relative `RAWMOUSE` movement report and post its `WM_INPUT`s.
    pub fn post_raw_mouse_move(
        &self,
        target: u64,
        sink: Option<u64>,
        dx: i32,
        dy: i32,
        raw_buttons: u32,
    ) {
        let posts = post_raw_mouse_event(
            target,
            sink,
            self.focus_window(),
            RawMouseReport::Movement { dx, dy },
            raw_buttons,
        );
        self.post_raw_input_posts(posts);
    }

    /// Synthesize a `RAWMOUSE` button report at `at` and post its `WM_INPUT`s.
    pub fn post_raw_mouse_button(
        &self,
        target: u64,
        sink: Option<u64>,
        button: HostMouseButton,
        down: bool,
        at: ClientPoint,
        raw_buttons: u32,
    ) {
        let posts = post_raw_mouse_event(
            target,
            sink,
            self.focus_window(),
            RawMouseReport::Button {
                button: button.to_winapi(),
                down,
                x: at.x,
                y: at.y,
            },
            raw_buttons,
        );
        self.post_raw_input_posts(posts);
    }

    /// Synthesize a `RAWMOUSE` wheel report at `at` and post its `WM_INPUT`s.
    ///
    /// `horizontal` selects `WM_MOUSEHWHEEL` vs `WM_MOUSEWHEEL` semantics;
    /// `notches` is whole notches, each becoming a 120-unit `usButtonData`
    /// delta.
    pub fn post_raw_mouse_wheel(
        &self,
        target: u64,
        sink: Option<u64>,
        horizontal: bool,
        notches: i32,
        at: ClientPoint,
        raw_buttons: u32,
    ) {
        let posts = post_raw_mouse_event(
            target,
            sink,
            self.focus_window(),
            RawMouseReport::Wheel {
                horizontal,
                notches,
                x: at.x,
                y: at.y,
            },
            raw_buttons,
        );
        self.post_raw_input_posts(posts);
    }

    /// Whether `hwnd` registered the keyboard class with `RIDEV_EXCLUDE`, i.e.
    /// asked for `WM_KEY*` to be suppressed in favour of `WM_INPUT`.
    #[must_use]
    pub fn legacy_keyboard_input_excluded(&self, hwnd: u64) -> bool {
        is_legacy_keyboard_input_excluded(hwnd)
    }

    /// Whether `hwnd` registered the mouse class with `RIDEV_EXCLUDE`, i.e.
    /// asked for the legacy `WM_MOUSE*` messages to be replaced by `WM_INPUT`.
    #[must_use]
    pub fn legacy_mouse_input_excluded(&self, hwnd: u64) -> bool {
        is_legacy_mouse_input_excluded(hwnd)
    }

    /// Deliver one `WM_INPUT` per resolved raw-input record.
    ///
    /// The posts are already resolved: `wParam` carries
    /// `GET_RAWINPUT_CODE_WPARAM` and `lParam` the fake `HRAWINPUT` the guest
    /// hands to `GetRawInputData`. Delivery goes through
    /// [`GuestHandle::post_message`], so it takes the queue mutex only and
    /// wakes a parked guest loop — the same path every other host post uses.
    fn post_raw_input_posts(&self, posts: Vec<RawInputPost>) {
        for post in posts {
            tracing::debug!(
                target: "wiegui",
                hwnd = post.hwnd,
                wparam = post.wparam,
                "WM_INPUT"
            );
            self.post_message(
                post.hwnd,
                WinMsg::WM_INPUT.as_u32(),
                post.wparam,
                post.lparam,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host surface is a re-export layer, so the thing worth pinning is
    /// that the names the CLI names really resolve — and that the two names
    /// `port` does not publish stay the *values* the emulator dispatches on
    /// (a host post and a guest handler disagreeing on a message id would be
    /// silent, not a compile error).
    #[test]
    fn host_surface_resolves_the_seam() {
        // §1 types the host names.
        let _: fn(&str, &str) -> bool = is_winapi_implemented;
        assert_eq!(WinMsg::WM_PAINT.as_u32(), 0x000F);
        assert_eq!(WinMsg::WM_INPUT.as_u32(), 0x00FF);
        assert_eq!(IRect::empty().left, 0);
        // §3 host vocabulary maps onto the emulator's one-to-one.
        assert_eq!(HostMouseButton::Left.to_winapi(), RawMouseButton::Left);
        assert_eq!(ClientPoint::new(3, 4).y, 4);
    }
}
