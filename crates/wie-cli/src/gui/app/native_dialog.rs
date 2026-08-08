//! Native-alert helpers for the macOS MessageBox bridge.
//!
//! The MessageBoxA/W handlers return `MessageBoxBridgeRequested`; the runtime
//! drops the shared state lock and the registered bridge runs on the guest
//! thread. This module owns the bridge's host-side pieces: the Win32
//! `MB_*`/`ID*` constant mapping, the rfd dialog shape, the parent-window
//! resolution, and the no-parent fallback (a bare NSAlert — rfd's unparented
//! path prints a legacy `CFUserNotificationDisplayAlert` line on the main
//! thread).

use std::sync::Arc;

use wie_runtime::GuestHandle;
use winit::window::Window;

use super::ParentWindowSlots;

/// Win32 `MB_*` flag bits (the low nibble of `MB_ICONMASK`/`MB_TYPEMASK` picks
/// the button set; the next nibble the icon). The rfd bridge receives the raw
/// flag word from the guest's MessageBoxA/W call.
#[cfg(target_os = "macos")]
const MB_TYPEMASK: u32 = 0x0000_000F;
#[cfg(target_os = "macos")]
const MB_ICONMASK: u32 = 0x0000_00F0;
#[cfg(target_os = "macos")]
const MB_OK: u32 = 0x0000_0000;
#[cfg(target_os = "macos")]
const MB_OKCANCEL: u32 = 0x0000_0001;
#[cfg(target_os = "macos")]
const MB_YESNOCANCEL: u32 = 0x0000_0003;
#[cfg(target_os = "macos")]
const MB_YESNO: u32 = 0x0000_0004;
#[cfg(target_os = "macos")]
const MB_ICONERROR: u32 = 0x0000_0010;
#[cfg(target_os = "macos")]
const MB_ICONQUESTION: u32 = 0x0000_0020;
#[cfg(target_os = "macos")]
const MB_ICONWARNING: u32 = 0x0000_0030;
#[cfg(target_os = "macos")]
const MB_ICONINFORMATION: u32 = 0x0000_0040;

/// Win32 standard dialog-command ids (`WM_COMMAND` wParam lows).
#[cfg(target_os = "macos")]
const IDOK: i32 = 1;
#[cfg(target_os = "macos")]
const IDCANCEL: i32 = 2;
#[cfg(target_os = "macos")]
const IDYES: i32 = 6;
#[cfg(target_os = "macos")]
const IDNO: i32 = 7;

/// Map Win32 `MB_*` flag bits to rfd's dialog shape.
///
/// The low nibble selects the button set, the next nibble the icon. rfd has no
/// `Question` level, so `MB_ICONQUESTION` falls back to `Info`. Unknown bits
/// fall back to the MB_OK / no-icon defaults, matching real MessageBox.
#[cfg(target_os = "macos")]
pub(super) fn map_message_box_buttons(mb_type: u32) -> (rfd::MessageButtons, rfd::MessageLevel) {
    let buttons = match mb_type & MB_TYPEMASK {
        MB_OK => rfd::MessageButtons::Ok,
        MB_OKCANCEL => rfd::MessageButtons::OkCancel,
        MB_YESNOCANCEL => rfd::MessageButtons::YesNoCancel,
        MB_YESNO => rfd::MessageButtons::YesNo,
        // Unknown button bits fall back to Ok (matching real MessageBox).
        _ => rfd::MessageButtons::Ok,
    };
    let level = match mb_type & MB_ICONMASK {
        MB_ICONERROR => rfd::MessageLevel::Error,
        MB_ICONWARNING => rfd::MessageLevel::Warning,
        MB_ICONINFORMATION | MB_ICONQUESTION => rfd::MessageLevel::Info,
        // No icon bits (plain MB_OK) also read as Info.
        _ => rfd::MessageLevel::Info,
    };
    (buttons, level)
}

/// Map an rfd alert result to the Win32 id the guest expects (IDOK, IDCANCEL,
/// IDYES, IDNO).
#[cfg(target_os = "macos")]
pub(super) fn map_alert_result(result: rfd::MessageDialogResult) -> i32 {
    match result {
        rfd::MessageDialogResult::Ok => IDOK,
        rfd::MessageDialogResult::Cancel => IDCANCEL,
        rfd::MessageDialogResult::Yes => IDYES,
        rfd::MessageDialogResult::No => IDNO,
        rfd::MessageDialogResult::Custom(_) => IDCANCEL,
    }
}

/// Show a MessageBox with NO parent winit window via a direct NSAlert.
///
/// rfd's unparented `MessageDialog::show()` falls back to the legacy
/// `CFUserNotificationDisplayAlert` API on macOS, which prints a
/// "called from main application thread, will block waiting for a response"
/// line to stderr. A bare NSAlert (the same modern API rfd uses internally
/// once a parent exists) never touches that path — a MessageBox raised
/// before the first frame created a winit window stays silent.
#[cfg(target_os = "macos")]
#[expect(unsafe_code)]
pub(super) fn show_unparented_ns_alert(caption: &str, text: &str, mb_type: u32) -> i32 {
    use objc2_app_kit::{
        NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAlertStyle,
    };
    use objc2_foundation::{NSString, run_on_main};

    let (buttons, level) = map_message_box_buttons(mb_type);
    run_on_main(|mtm| {
        // SAFETY: `new(mtm)` is the main-thread-only constructor (NSAlert is
        // a MainThreadOnly class); the returned Retained owns the alert.
        let alert = unsafe { NSAlert::new(mtm) };
        // SAFETY: plain property setters on the owned alert.
        unsafe {
            alert.setMessageText(&NSString::from_str(caption));
            alert.setInformativeText(&NSString::from_str(text));
            let style = match level {
                rfd::MessageLevel::Error => NSAlertStyle::Critical,
                rfd::MessageLevel::Warning => NSAlertStyle::Warning,
                rfd::MessageLevel::Info => NSAlertStyle::Informational,
            };
            alert.setAlertStyle(style);
        }
        // First added button is the default (rightmost, Enter). The order
        // mirrors the Win32 button set so the response index maps to the id.
        let titles: &[&str] = match buttons {
            rfd::MessageButtons::Ok => &["OK"],
            rfd::MessageButtons::OkCancel => &["OK", "Cancel"],
            rfd::MessageButtons::YesNoCancel => &["Yes", "No", "Cancel"],
            rfd::MessageButtons::YesNo => &["Yes", "No"],
            // Unknown button bits fall back to Ok (matching real MessageBox).
            _ => &["OK"],
        };
        for title in titles {
            // SAFETY: appends a button to the owned alert; the returned
            // Retained<NSButton> is dropped (the alert retains it).
            unsafe {
                let _ = alert.addButtonWithTitle(&NSString::from_str(title));
            }
        }
        // SAFETY: runModal on the main thread (we are inside run_on_main)
        // blocks until the user clicks — MessageBox semantics; the guest
        // thread is blocked in the bridge.
        let response = unsafe { alert.runModal() };
        if response == NSAlertFirstButtonReturn {
            match buttons {
                rfd::MessageButtons::YesNo | rfd::MessageButtons::YesNoCancel => IDYES,
                _ => IDOK,
            }
        } else if response == NSAlertSecondButtonReturn {
            match buttons {
                rfd::MessageButtons::YesNo | rfd::MessageButtons::YesNoCancel => IDNO,
                _ => IDCANCEL,
            }
        } else {
            // Third button (and any unknown response) is the Cancel slot.
            IDCANCEL
        }
    })
}

/// Resolve the winit window a native dialog (MessageBox, file panel) should
/// parent to.
///
/// The FOCUSED window's top-level is the natural parent — a MessageBox raised
/// while a second top-level is active parents to THAT window, not the first
/// one (the per-window-slot fix). Falls back to the primary window (the
/// first guest window), then to any live host window. `None` when no host
/// window exists yet (the bridge runs before the first frame — rfd then uses
/// its unparented fallback).
#[cfg(target_os = "macos")]
pub(crate) fn resolve_dialog_parent(
    handle: &GuestHandle,
    slots: &ParentWindowSlots,
) -> Option<Arc<Window>> {
    let preferred = handle
        .focused_top_level()
        .or_else(|| handle.first_guest_window_handle());
    let Ok(map) = slots.lock() else {
        return None;
    };
    preferred
        .and_then(|hwnd| map.get(&hwnd).cloned())
        .or_else(|| map.values().next().cloned())
}

/// Bring `window`'s NSWindow to the front of its window level.
///
/// winit 0.30 exposes no window-ordering API, so the guest z-order is
/// mirrored through AppKit directly: winit's raw window handle exposes the
/// backing NSView, whose owning NSWindow answers `orderFront`. Best-effort —
/// a window whose raw handle is not AppKit, or whose view is not yet in a
/// window, is skipped. Runs on the main (event-loop) thread, where AppKit
/// ordering is valid.
#[cfg(target_os = "macos")]
#[expect(unsafe_code)]
pub(super) fn order_window_front(window: &Arc<Window>) {
    use objc2_app_kit::NSView;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: `ns_view` is the live NSView backing this winit window — winit
    // retains it for the window's lifetime, and this runs on the main thread
    // where the AppKit view hierarchy is valid.
    let view: &NSView = unsafe { &*appkit.ns_view.as_ptr().cast::<NSView>() };
    if let Some(ns_window) = view.window() {
        ns_window.orderFront(None);
    }
}
