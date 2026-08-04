//! Winit application handler — displays the guest windows and forwards input.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wie_runtime::GuestHandle;
use wie_runtime::MenuNode;
use wie_runtime::RuntimeSession;
use wie_runtime::{GuiControl, run_windowed};
use wie_winapi::handles::Hwnd;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::gui::input;
#[cfg(target_os = "macos")]
use crate::gui::menu_bar;

/// Custom events from the guest thread to the host event loop.
///
/// `pub(crate)` so the [`menu_bar`] module can name it in its event-loop
/// proxy.
pub(crate) enum WieEvent {
    /// A frame was published and the wake callback fired. `published_at` is
    /// the wake timestamp — the event loop measures wake→redraw latency.
    Frame { published_at: Instant },
    GuestExited {
        #[allow(dead_code)]
        code: i32,
    },
    /// A macOS menu-bar item was clicked; the muda event carries the guest
    /// menu item id as a string-form [`muda::MenuId`].
    #[cfg(target_os = "macos")]
    MenuEvent(muda::MenuEvent),
}

impl WieApp {
    /// Win32 MK_* flags for mouse-message wParam: pressed buttons plus
    /// held Shift/Ctrl (Windows sends these to let apps interpret clicks).
    fn mk_flags(&self) -> u16 {
        let mut mk = self.mouse_buttons;
        if self.modifiers.shift_key() {
            mk |= input::MK_SHIFT;
        }
        if self.modifiers.control_key() {
            mk |= input::MK_CONTROL;
        }
        mk
    }

    /// The window under the cursor and its client-relative coordinates, for a
    /// mouse event delivered to `event_hwnd` (the guest HWND of the winit
    /// window the event arrived on).
    ///
    /// A window holding the mouse capture (SetCapture — a pressed BUTTON
    /// captures while down) receives every mouse message instead of the
    /// hit-tested child, matching Windows; capture is desktop-global, so the
    /// capture path runs first regardless of which top-level the event
    /// arrived on. Without capture, the hit-test descends the EVENT window's
    /// own subtree via `window_at_in` — a second top-level (a dialog in its
    /// own winit window) must resolve its own controls, not the main
    /// window's. Falls back to the primary window at (0, 0) when the guest
    /// tree yields nothing (no visible window under the cursor). `sf` is the
    /// event window's device scale factor: winit reports PHYSICAL pixels, the
    /// guest hit-test expects LOGICAL 96-DPI client pixels.
    fn mouse_target(&self, handle: &GuestHandle, event_hwnd: u64, sf: f64) -> (u64, u16, u16) {
        let (cx, cy) = self.cursor_pos;
        let (x, y) = (
            input::physical_to_logical(cx.max(0.0), sf) as i32,
            input::physical_to_logical(cy.max(0.0), sf) as i32,
        );
        if let Some((hwnd, rx, ry)) = handle.capture_target(x, y) {
            return (hwnd, rx as u16, ry as u16);
        }
        handle.window_at_in(event_hwnd, x, y).map_or(
            (self.primary_hwnd.map_or(0, |h| h.as_u64()), 0, 0),
            |(hwnd, rx, ry)| (hwnd, rx as u16, ry as u16),
        )
    }

    /// The last cursor position as integer client coordinates in LOGICAL
    /// 96-DPI pixels (for MSG.pt).
    fn cursor_pos_i32(&self) -> (i32, i32) {
        let sf = self.scale_factor();
        (
            input::physical_to_logical(self.cursor_pos.0.max(0.0), sf) as i32,
            input::physical_to_logical(self.cursor_pos.1.max(0.0), sf) as i32,
        )
    }

    /// Rebuilds the macOS menu bar when the focused guest window's menu
    /// changed.
    ///
    /// The bar mirrors the FOCUSED guest window's menu (macOS has one global
    /// bar; Windows has one per window) — `window_menu_items` resolves the
    /// focus. Runs once per `Frame` and as a best-effort immediate attempt on
    /// winit `Focused(true)`; the cheap `Vec` compare skips the AppKit work
    /// unless the menu actually changed. Clicks are forwarded back through
    /// the event-loop proxy by [`MacMenuBar`] as [`WieEvent::MenuEvent`] so
    /// the guest mutation (`WM_COMMAND`) always happens on this thread.
    #[cfg(target_os = "macos")]
    fn sync_menu_bar(&mut self) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let items = handle.window_menu_items();
        if items == self.last_menu_items {
            return;
        }
        tracing::debug!("menu items: {:?}", items);
        self.menu_bar.rebuild(items.as_slice());
        self.last_menu_items = items;
    }
    /// Apply a guest-requested host-window move/resize (`SetWindowPlacement`)
    /// to the winit window.
    ///
    /// The winapi handler records the pending `(hwnd, x, y, width, height)` on
    /// the shared state and wakes the presenter; this consumes it on the
    /// event-loop thread where winit calls must run. Guest coordinates are
    /// LOGICAL 96-DPI pixels, so each value is multiplied by the window's
    /// device scale factor ([`input::logical_to_physical`]) to reach winit's
    /// physical space — the same `max(100)` clamp guards against a
    /// degenerate or negative rect. When no winit window exists yet (the
    /// move arrived before the first published frame) the request stays
    /// pending and applies once the window is created.
    fn apply_host_geometry(&self) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        // No winit window at all: leave the request pending — the frame
        // handler re-runs this once reconciliation creates one (the startup
        // restore must not be dropped).
        if self.windows.is_empty() {
            return;
        }
        let Some((hwnd, x, y, width, height)) = handle.take_host_geometry_request() else {
            return;
        };
        // Apply to the winit window mirroring the request's hwnd; fall back
        // to the primary window when that hwnd isn't registered (a
        // SetWindowPlacement on a child/owned window has no host entry).
        let Some(rt) = self
            .windows
            .values()
            .find(|rt| rt.hwnd.as_u64() == hwnd)
            .or_else(|| self.primary_runtime())
        else {
            return;
        };
        let sf = rt.scale_factor;
        rt.window.set_outer_position(PhysicalPosition::new(
            input::logical_to_physical(x as f64, sf),
            input::logical_to_physical(y as f64, sf),
        ));
        // winit 0.30 removed set_inner_size; request_inner_size is its
        // replacement (same Into<Size> contract as with_inner_size at
        // creation, so the guest's logical size must be scaled to physical
        // here). The returned actual size is informational — the winit
        // Resized event carries the settle.
        let _ = rt.window.request_inner_size(PhysicalSize::new(
            input::logical_to_physical(width.max(100) as f64, sf) as u32,
            input::logical_to_physical(height.max(100) as f64, sf) as u32,
        ));
    }

    /// The primary window's device scale factor (physical px per logical
    /// 96-DPI px), defaulting to 1.0 before any winit window exists (no
    /// scaling has happened yet). Mouse/keyboard use the EVENT window's
    /// factor; this remains for the few paths still in primary space (the
    /// drop-point conversion).
    fn scale_factor(&self) -> f64 {
        self.primary_runtime().map_or(1.0, |rt| rt.scale_factor)
    }

    /// The runtime of the primary host window — the first window created,
    /// mirroring the guest's main window. The fallback target when an event's
    /// own window isn't registered (geometry, no-hit mouse fallback).
    fn primary_runtime(&self) -> Option<&WindowRuntime> {
        let hwnd = self.primary_hwnd?;
        self.windows.values().find(|rt| rt.hwnd == hwnd)
    }

    /// Reconcile the host window registry against the guest's top-level
    /// windows: create a winit window for each top-level that has none, and
    /// destroy the winit windows of top-levels the guest no longer has.
    ///
    /// Runs on every `Frame` event; a cheap no-op when the guest window set
    /// is unchanged (the top-level snapshot is a filter over the window
    /// records). The first-window-on-first-publish behavior is preserved: the
    /// first published frame still creates the first host window on this
    /// exact event, from the guest's own title/size.
    fn reconcile_windows(&mut self, event_loop: &ActiveEventLoop) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let top_levels = handle.guest_top_level_windows();
        let live_hwnds: HashMap<u64, ()> =
            top_levels.iter().map(|(hwnd, ..)| (*hwnd, ())).collect();
        // Destroy winit windows for top-levels the guest no longer has.
        // Dropping the last `Arc<Window>` clone closes the winit window
        // (winit 0.30 `Window` drops the underlying window).
        let stale: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, rt)| !live_hwnds.contains_key(&rt.hwnd.as_u64()))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(rt) = self.windows.remove(&id) {
                tracing::debug!(
                    target: "wiegui",
                    hwnd = rt.hwnd.as_u64(),
                    "destroyed host window for closed guest top-level"
                );
            }
        }
        if self.windows.is_empty() {
            self.primary_hwnd = None;
        }
        // Create winit windows for new top-levels, in guest creation order
        // (the top-level snapshot preserves the `windows` record order), so
        // the main window is created first.
        for (hwnd, title, width, height) in top_levels {
            if self.windows.values().any(|rt| rt.hwnd.as_u64() == hwnd) {
                continue;
            }
            let is_first = self.primary_hwnd.is_none();
            let width = width.max(100) as u32;
            let height = height.max(100) as u32;
            let attrs = window_attributes(&title, width, height);
            let Ok(window) = event_loop.create_window(attrs) else {
                tracing::error!(target: "wiegui", "create_window failed for a guest top-level");
                continue;
            };
            let window = Arc::new(window);
            if is_first {
                // Publish the first window to the MessageBox bridge so its
                // rfd dialog can parent to it (the NSAlert path instead of
                // the legacy CFUserNotification fallback). A poisoned mutex
                // leaves the bridge unparented — harmless, the fallback still
                // works.
                if let Ok(mut slot) = self.window_slot.lock() {
                    *slot = Some(window.clone());
                }
            }
            // winit reports the device scale factor (physical px per logical
            // px); the input and resize paths divide winit's physical
            // coordinates by it.
            let scale_factor = window.scale_factor();
            window.focus_window();
            let id = window.id();
            let rt = WindowRuntime {
                hwnd: Hwnd::from(hwnd),
                window: window.clone(),
                surface: init_present_backend(&window),
                last_presented_pixels: None,
                last_presented_size: None,
                pending_size: None,
                last_resize: None,
                last_sent_size: None,
                scale_factor,
            };
            if is_first {
                self.primary_hwnd = Some(rt.hwnd);
            }
            tracing::debug!(
                target: "wiegui",
                window_id = ?id,
                hwnd = hwnd,
                "created host window for guest top-level"
            );
            self.windows.insert(id, rt);
        }
    }
}

/// Debounce window for resize: how long without a `Resized` event before we
/// consider the drag settled and post the final `WM_SIZE` to the guest.
///
/// 50 ms: short enough that release→crisp-frame feels instant (the guest
/// repaint is the dominant cost, ~55 ms at 886×776), long enough to absorb
/// macOS's trailing `Resized` events so the guest reallocates its DIB once.
const RESIZE_SETTLE_MS: u64 = 50;

/// Per-window host state for one winit window — the payload of each entry in
/// the [`WindowRegistry`]. One entry exists per guest top-level window; the
/// "window exists iff hwnd known" invariant is now a type instead of
/// per-arm checks.
struct WindowRuntime {
    /// Guest HWND this window mirrors.
    hwnd: Hwnd,
    window: Arc<Window>,
    /// The wgpu (Metal) present backend, created at window construction.
    surface: Option<PresentBackend>,
    /// Pixels Arc of the last frame actually presented; a frame whose pixels
    /// Arc is pointer-equal to this one AND whose window size is unchanged
    /// since the last present is byte-identical — skip the re-upload.
    ///
    /// Publishes move a fresh Arc every time ([`present::publish`] wraps the
    /// painted buffer in `Arc::from`), and this entry keeps the compared-to
    /// allocation alive until the next present, so `Arc::ptr_eq` inequality
    /// exactly means the window republished. Holding the Arc also pins the
    /// allocation: a freed-and-reused header address would otherwise fake a
    /// pointer match on a genuinely republished frame.
    last_presented_pixels: Option<Arc<Vec<u32>>>,
    /// Window size at the last present (drag-stretch must not be skipped).
    last_presented_size: Option<(u32, u32)>,
    /// Latest window size from winit during a resize drag.
    pending_size: Option<(u32, u32)>,
    /// When the last `Resized` event arrived (None = not resizing).
    last_resize: Option<Instant>,
    /// The size most recently posted to the guest as WM_SIZE.  Used to skip
    /// duplicate settles (macOS fires trailing `Resized` events after the
    /// drag, each would otherwise re-trigger the guest's expensive DIB
    /// recreation at the same size — and the guest thread being busy with
    /// that recreation delays close/quit handling).  Guest sizes are
    /// LOGICAL 96-DPI pixels.
    last_sent_size: Option<(u32, u32)>,
    /// The window's device scale factor (physical pixels per logical
    /// 96-DPI pixel), read at creation and refreshed on
    /// `ScaleFactorChanged`.  The guest space is logical; every winit
    /// physical value crossing the window boundary divides by this.
    scale_factor: f64,
}

/// One host winit window per guest top-level window, keyed by winit
/// `WindowId`. Reconciled from the guest on every published frame (see
/// [`WieApp::reconcile_windows`]). A session starts empty — no winit window
/// exists until the first frame — and entries are created/destroyed as the
/// guest opens and closes top-level windows.
type WindowRegistry = HashMap<WindowId, WindowRuntime>;

/// The wgpu (Metal) present backend for a window, created at window
/// construction. The guest frame is uploaded to a staging texture and blitted
/// to the swapchain; the pixel Arc is dropped right after the upload so the
/// guest can hand the surface buffer back zero-copy on the next paint.
type PresentBackend = crate::gui::present_wgpu::WgpuPresenter;

/// Whether a present actually drew its frame — the RedrawRequested handler
/// records the frame as presented ONLY when it reached the screen (a skipped
/// present must stay retryable).
use crate::gui::present_wgpu::PresentOutcome;

/// Initialize the wgpu present backend. wgpu init is expected to succeed on
/// macOS (Metal backend); a failure here means the host cannot present at all.
fn init_present_backend(window: &Arc<Window>) -> Option<PresentBackend> {
    crate::gui::present_wgpu::WgpuPresenter::init(window.clone()).ok()
}

/// Build the winit window attributes for the guest's first window.
///
/// The guest reports LOGICAL 96-DPI pixels; winit multiplies the
/// `LogicalSize` by the display's device scale factor, so on a Retina (2×)
/// display the guest's 640×480 window becomes a 1280×960 physical surface.
fn window_attributes(title: &str, width: u32, height: u32) -> winit::window::WindowAttributes {
    Window::default_attributes()
        .with_title(title)
        .with_inner_size(LogicalSize::new(width, height))
}

/// The guest-LOGICAL size a physical winit `inner_size` corresponds to at
/// `scale_factor`: physical ÷ sf with the crate's standard rounding (see
/// [`input::physical_to_logical`]). This is the size posted as WM_SIZE and
/// written to the guest-visible window record.
fn guest_size_from_physical(width: u32, height: u32, scale_factor: f64) -> (u32, u32) {
    (
        input::physical_to_logical(f64::from(width), scale_factor) as u32,
        input::physical_to_logical(f64::from(height), scale_factor) as u32,
    )
}

/// The message for a left-button press: WM_LBUTTONDBLCLK when it lands on the
/// SAME target window within the double-click time window AND the double-click
/// slop rectangle of the previous press (`last`), WM_LBUTTONDOWN otherwise.
/// Every press refreshes `last`, so the host re-creates the message Windows
/// itself synthesizes from `GetDoubleClickTime` / `SM_CXDOUBLECLK` (Windows
/// tracks double-clicks per window). `slop` is the [`input::DOUBLE_CLICK_SLOP_PX`]
/// distance scaled to the window's PHYSICAL pixels — winit reports cursor
/// positions physically, and both presses are compared in that space. A free
/// function (not a method) so the MouseInput arm can call it while
/// `self.handle` is borrowed.
fn left_press_message(
    last: &mut Option<(Instant, f64, f64, u64)>,
    cursor: (f64, f64),
    hwnd: u64,
    slop: f64,
) -> u32 {
    let now = Instant::now();
    let (x, y) = cursor;
    let dbl = last.is_some_and(|(t, lx, ly, last_hwnd)| {
        last_hwnd == hwnd
            && now.saturating_duration_since(t)
                <= Duration::from_millis(input::DOUBLE_CLICK_TIME_MS)
            && (x - lx).abs() <= slop
            && (y - ly).abs() <= slop
    });
    *last = Some((now, x, y, hwnd));
    if dbl {
        input::WM_LBUTTONDBLCLK
    } else {
        input::WM_LBUTTONDOWN
    }
}

/// winit application state: bridges the guest windows to the present backend.
struct WieApp {
    handle: Option<GuestHandle>,
    /// One winit window per guest top-level window; [`WindowRegistry`] is
    /// empty until the first frame creates the first window, then reconciled
    /// from the guest on every publish (see [`WieApp::reconcile_windows`]).
    windows: WindowRegistry,
    /// Guest HWND of the primary (first-created) host window — the window
    /// that mirrors the guest's main window. Input events that are not yet
    /// per-window (keyboard, focus, and the mouse hit-test fallback — L2's
    /// input-routing lane) target it.
    primary_hwnd: Option<Hwnd>,
    /// Wake-coalescing flag, shared with the guest-thread wake callback.
    /// Set on every publish; the first `Frame` event after a publish group
    /// swaps it and requests a redraw, duplicates skip.
    pending_frame: Arc<std::sync::atomic::AtomicBool>,
    /// Shared slot for the winit window Arc, filled once at the first window
    /// creation (on the event-loop thread) and read by the guest-thread
    /// MessageBox bridge to parent its rfd dialog to the window.
    window_slot: Arc<Mutex<Option<Arc<Window>>>>,
    /// Currently pressed mouse buttons (MK_* bits) — from MouseInput events.
    mouse_buttons: u16,
    /// Last reported cursor position in client coords (x, y).
    cursor_pos: (f64, f64),
    /// The last left-button press (time, position, target window), for
    /// double-click detection — a second press on the same window within the
    /// time window and slop rectangle posts WM_LBUTTONDBLCLK instead of
    /// WM_LBUTTONDOWN (the message Windows itself would synthesize).
    last_left_press: Option<(Instant, f64, f64, u64)>,
    /// Currently held modifier keys (shift/ctrl/alt) — from ModifiersChanged.
    modifiers: winit::keyboard::ModifiersState,
    /// macOS application menu bar mirroring the guest window's menu.
    #[cfg(target_os = "macos")]
    menu_bar: crate::gui::menu_bar::MacMenuBar,
    /// Menu items the menu bar was last rebuilt with (cheap change check).
    #[cfg(target_os = "macos")]
    last_menu_items: Arc<Vec<MenuNode>>,
}

impl ApplicationHandler<WieEvent> for WieApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // Window is created lazily in the Frame handler with the guest's
        // actual title and size (from CreateWindowExA parameters).
        tracing::debug!("resumed: waiting for guest window");
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // Handle events that need early processing before the runtime guard.
        match &event {
            WindowEvent::CloseRequested => {
                // WM_CLOSE to the event window's own guest HWND, so closing
                // any top-level (the main window OR a dialog) asks the guest
                // to destroy exactly that window.
                if let Some(handle) = self.handle.as_ref()
                    && let Some(rt) = self.windows.get(&window_id)
                {
                    handle.post_message(rt.hwnd.as_u64(), input::WM_CLOSE, 0, 0);
                }
                return;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // The guest's SetFocus state is authoritative: keys go to the
                // focus window (whatever top-level it lives in), falling back
                // to the event window's own hwnd when nothing has focus.
                let Some(handle) = self.handle.as_ref() else {
                    return;
                };
                let Some(event_hwnd) = self.windows.get(&window_id).map(|rt| rt.hwnd.as_u64())
                else {
                    return;
                };
                let hwnd = handle.focus_window().unwrap_or(event_hwnd);
                let pressed = matches!(event.state, winit::event::ElementState::Pressed);
                let vk = input::virt_key_from_physical(event.physical_key);
                // Feed the guest keyboard-state table so GetKeyState /
                // GetAsyncKeyState / IsDialogMessage (Shift+Tab) see
                // real key state, not stale zeros.
                handle.set_key_state(vk, pressed);
                let is_alt = matches!(
                    event.physical_key,
                    winit::keyboard::PhysicalKey::Code(
                        winit::keyboard::KeyCode::AltLeft | winit::keyboard::KeyCode::AltRight
                    )
                );
                tracing::debug!(
                    "keyboard vk={:#04x} pressed={} alt={} hwnd={:#x}",
                    vk,
                    pressed,
                    is_alt,
                    hwnd
                );
                if pressed {
                    handle.post_message(hwnd, input::WM_KEYDOWN, u64::from(vk), 0);
                    if let Some(ref text) = event.text {
                        for c in text.chars() {
                            handle.post_message(hwnd, input::WM_CHAR, u64::from(c as u32), 0);
                        }
                    }
                    if is_alt {
                        handle.post_message(hwnd, input::WM_SYSKEYDOWN, u64::from(vk), 0);
                    }
                } else {
                    handle.post_message(hwnd, input::WM_KEYUP, u64::from(vk), 0);
                    if is_alt {
                        handle.post_message(hwnd, input::WM_SYSKEYUP, u64::from(vk), 0);
                    }
                }
                return;
            }
            _ => {}
        }

        let Some(ref handle) = self.handle else {
            return;
        };
        // Event-window context: the guest HWND and device scale factor of the
        // winit window the event arrived on (per-entry — a second top-level
        // can sit on a display with a different factor). `None` for a stale
        // event from a window reconciliation already destroyed; those arms
        // fall back to the primary window.
        let event_hwnd = self.windows.get(&window_id).map(|rt| rt.hwnd.as_u64());
        let event_sf = self
            .windows
            .get(&window_id)
            .map_or(1.0, |rt| rt.scale_factor);
        // The primary window's guest HWND — the no-hit fallback for mouse
        // routing.
        let primary_hwnd = self.primary_hwnd.map_or(0, |h| h.as_u64());

        match event {
            WindowEvent::DroppedFile(path) => {
                // A host file dropped on the window: store it as the guest
                // drop list and post WM_DROPFILES with the fake HDROP. The
                // guest's handler (notepad's WM_DROPFILES) reads the path via
                // DragQueryFileW, mapped to a guest C:\ path by
                // GuestHandle::set_drop_files (drops outside the bottle/D:
                // bridge are skipped, so hdrop stays 0 and nothing is posted).
                // WM_DROPFILES targets the top-level window the file was
                // dropped ON (the event window), where top-level-relative
                // MSG.pt is the same space as lParam.
                let (px, py) = self.cursor_pos_i32();
                let hdrop = handle.set_drop_files(vec![path.clone()], (px, py));
                tracing::info!(
                    target: "wiegui",
                    host_path = %path.display(),
                    "drop: mapped to hdrop=0x{hdrop:x}"
                );
                if hdrop != 0 {
                    let lparam = input::make_lparam(px.max(0) as u16, py.max(0) as u16);
                    handle.post_message(
                        event_hwnd.unwrap_or(primary_hwnd),
                        input::WM_DROPFILES,
                        hdrop,
                        lparam,
                    );
                }
            }
            WindowEvent::RedrawRequested => {
                let Some(rt) = self.windows.get_mut(&window_id) else {
                    return;
                };
                let frame = handle.take_frame(rt.hwnd.as_u64());
                if let Some(frame) = frame {
                    let (dst_w, dst_h) = {
                        let s = rt.window.inner_size();
                        (s.width.max(1), s.height.max(1))
                    };
                    // Skip redundant presents, per window. When the frame's
                    // pixels Arc is pointer-equal to the last one presented
                    // (this window did NOT republish — some other window
                    // woke us) AND the window size is unchanged (no
                    // drag-stretch), the frame is byte-identical — skip the
                    // copy + GPU upload entirely. The Arc compare is exact:
                    // `publish` wraps the painted buffer in a fresh Arc every
                    // time, and this entry holds the compared-to allocation
                    // alive (see `last_presented_pixels`).
                    let is_unchanged = rt
                        .last_presented_pixels
                        .as_ref()
                        .is_some_and(|prev| Arc::ptr_eq(prev, &frame.pixels))
                        && rt.last_presented_size == Some((dst_w, dst_h));
                    if is_unchanged {
                        tracing::debug!(
                            target: "wiegui",
                            hwnd = rt.hwnd.as_u64(),
                            "skipping unchanged frame (ptr_eq+size)"
                        );
                        return;
                    }
                    tracing::debug!(
                        "redraw: frame={}x{} dst={}x{}",
                        frame.width,
                        frame.height,
                        dst_w,
                        dst_h,
                    );
                    let present_t0 = if handle.frame_timing_enabled() {
                        Some(Instant::now())
                    } else {
                        None
                    };
                    // wgpu path — no CPU copy; the blit pass nearest-scales via
                    // the sampler when the window size differs from the frame
                    // size (identical nearest semantics to stretch_nearest).
                    // The frame is moved so the present backend can drop the
                    // pixel Arc right after the staging upload; snapshot the
                    // pixels Arc for the per-window skip BEFORE the move.
                    let presented_pixels = Arc::clone(&frame.pixels);
                    let outcome = if let Some(presenter) = rt.surface.as_mut() {
                        match presenter.present(frame, dst_w, dst_h) {
                            Ok(outcome) => outcome,
                            Err(e) => {
                                tracing::error!(target: "wiegui", error = %e, "wgpu present failed");
                                PresentOutcome::NotDrawn { retry: false }
                            }
                        }
                    } else {
                        // No present backend (init failed): nothing was drawn.
                        // Leave the frame un-presented so a later retry can
                        // still draw it.
                        PresentOutcome::NotDrawn { retry: false }
                    };
                    if let Some(t0) = present_t0 {
                        handle.record_present_time(t0.elapsed().as_nanos());
                        tracing::debug!(
                            target: "wiegui",
                            present_us = u64::try_from(t0.elapsed().as_micros())
                                .unwrap_or(u64::MAX),
                            "host present"
                        );
                    }
                    match outcome {
                        PresentOutcome::Drawn => {
                            rt.last_presented_pixels = Some(presented_pixels);
                            rt.last_presented_size = Some((dst_w, dst_h));
                        }
                        PresentOutcome::NotDrawn { retry } => {
                            // The frame never reached the screen. Keep
                            // last_presented_* stale — the ptr_eq skip above
                            // would otherwise reject the retry of this same
                            // frame — and re-request the redraw when the skip
                            // is transient (occluded/out-of-date surface). This
                            // is what makes a modal dialog's FIRST composite
                            // frame appear: the dialog publishes once (into the
                            // owner surface) and then the guest parks in its
                            // in-guest modal loop, so a lost present has no
                            // follow-up publish to re-arm the redraw — the
                            // dialog stays invisible until a mouse event
                            // repaints it. The backend skips the redundant
                            // staging re-upload on the retry, so this is cheap.
                            if retry {
                                rt.window.request_redraw();
                            }
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x.max(0.0), position.y.max(0.0));
                let mk = self.mk_flags();
                // Route to the topmost child of the EVENT window under the
                // cursor (window_at_in roots at the event window's hwnd).
                let (target, rx, ry) =
                    self.mouse_target(handle, event_hwnd.unwrap_or(primary_hwnd), event_sf);
                let lparam = input::make_lparam(rx, ry);
                // MSG.pt must share lParam's CHILD-relative space: a guest
                // reading MSG.pt (e.g. the wndproc's own hit-testing) would
                // otherwise mix child-relative lParam with top-level-relative
                // pt in one message and mis-map clicks on child windows (the
                // EDIT caret landing on a huge char index). WM_DROPFILES is
                // the exception: it targets the top-level window, where
                // top-level-relative pt is the same space.
                handle.post_message_at(
                    target,
                    input::WM_MOUSEMOVE,
                    u64::from(mk),
                    lparam,
                    i32::from(rx),
                    i32::from(ry),
                );
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = matches!(state, winit::event::ElementState::Pressed);
                // Track pressed buttons for MK_* flags in subsequent messages.
                let bit = match button {
                    winit::event::MouseButton::Left => input::MK_LBUTTON,
                    winit::event::MouseButton::Right => input::MK_RBUTTON,
                    winit::event::MouseButton::Middle => input::MK_MBUTTON,
                    _ => return,
                };
                if pressed {
                    self.mouse_buttons |= bit;
                } else {
                    self.mouse_buttons &= !bit;
                }
                // The target is resolved first so the double-click detection
                // can require both presses on the same window (Windows tracks
                // double-clicks per window). The slop is the EVENT window's
                // physical space (left_press_message borrows `last` mutably,
                // and event_sf is read before the call).
                let (target, rx, ry) =
                    self.mouse_target(handle, event_hwnd.unwrap_or(primary_hwnd), event_sf);
                let msg = match button {
                    winit::event::MouseButton::Left => {
                        if pressed {
                            let slop = input::double_click_slop(event_sf);
                            left_press_message(
                                &mut self.last_left_press,
                                self.cursor_pos,
                                target,
                                slop,
                            )
                        } else {
                            input::WM_LBUTTONUP
                        }
                    }
                    winit::event::MouseButton::Right => {
                        if pressed {
                            input::WM_RBUTTONDOWN
                        } else {
                            input::WM_RBUTTONUP
                        }
                    }
                    winit::event::MouseButton::Middle => {
                        if pressed {
                            input::WM_MBUTTONDOWN
                        } else {
                            input::WM_MBUTTONUP
                        }
                    }
                    _ => return,
                };
                let mk = self.mk_flags();
                let lparam = input::make_lparam(rx, ry);
                // MSG.pt in lParam's child-relative space (see CursorMoved).
                handle.post_message_at(
                    target,
                    msg,
                    u64::from(mk),
                    lparam,
                    i32::from(rx),
                    i32::from(ry),
                );
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (delta_x, delta_y) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        ((x * 120.0) as i32, (y * 120.0) as i32)
                    }
                    winit::event::MouseScrollDelta::PixelDelta(pos) => (pos.x as i32, pos.y as i32),
                };
                let mk = self.mk_flags();
                // WM_MOUSEWHEEL/HWHEEL go to the FOCUS window, not the window
                // under the cursor (DefWindowProc then bubbles them up the
                // parent chain) — a multiline EDIT keeps scrolling while the
                // pointer is elsewhere. Fall back to the EVENT window's
                // hit-test when nothing has keyboard focus.
                let (target, rx, ry) = match handle.focus_window() {
                    Some(focus) => (focus, 0, 0),
                    None => self.mouse_target(handle, event_hwnd.unwrap_or(primary_hwnd), event_sf),
                };
                if delta_y != 0 {
                    let wparam = input::make_wparam(mk, delta_y as u16);
                    handle.post_message_at(
                        target,
                        input::WM_MOUSEWHEEL,
                        wparam,
                        input::make_lparam(rx, ry),
                        i32::from(rx),
                        i32::from(ry),
                    );
                }
                if delta_x != 0 {
                    let wparam = input::make_wparam(mk, delta_x as u16);
                    handle.post_message_at(
                        target,
                        input::WM_MOUSEHWHEEL,
                        wparam,
                        input::make_lparam(rx, ry),
                        i32::from(rx),
                        i32::from(ry),
                    );
                }
            }
            WindowEvent::Moved(position) => {
                // WM_MOVE: lParam = MAKELPARAM(x, y) screen coords — guest
                // LOGICAL 96-DPI pixels, so divide winit's physical position
                // by the event window's device scale factor.
                let Some(rt) = self.windows.get(&window_id) else {
                    return;
                };
                let (px, py) = (
                    input::physical_to_logical(f64::from(position.x.max(0)), rt.scale_factor)
                        as u16,
                    input::physical_to_logical(f64::from(position.y.max(0)), rt.scale_factor)
                        as u16,
                );
                let lparam = input::make_lparam(px, py);
                handle.post_message(rt.hwnd.as_u64(), input::WM_MOVE, 0, lparam);
            }
            WindowEvent::CursorEntered { .. } => {
                // Windows sends WM_MOUSEHOVER only for windows that requested
                // tracking via TrackMouseEvent — tracked per window, so check
                // the EVENT window.
                let Some(hwnd) = event_hwnd else {
                    return;
                };
                if handle.mouse_tracking(hwnd) {
                    handle.post_message(hwnd, input::WM_MOUSEHOVER, 0, 0);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                // Windows sends WM_MOUSELEAVE only for tracked windows.
                let Some(hwnd) = event_hwnd else {
                    return;
                };
                if handle.mouse_tracking(hwnd) {
                    handle.post_message(hwnd, input::WM_MOUSELEAVE, 0, 0);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The window moved to a display (or setting) with a different
                // device scale factor. The guest sees the same LOGICAL size,
                // so refresh the factor the input and resize paths divide by,
                // then request a fresh render at the new scale — the next
                // frame covers it.
                if let Some(rt) = self.windows.get_mut(&window_id) {
                    rt.scale_factor = scale_factor;
                    rt.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // Dead in practice (the early KeyboardInput arm above returns
                // first) — kept in sync with it: keys go to the guest focus
                // window, falling back to the event window.
                let pressed = matches!(event.state, winit::event::ElementState::Pressed);
                let vk = input::virt_key_from_physical(event.physical_key);
                // Keep the guest keyboard-state table in sync with real input.
                handle.set_key_state(vk, pressed);
                let hwnd = handle
                    .focus_window()
                    .unwrap_or(event_hwnd.unwrap_or(primary_hwnd));
                let is_alt = matches!(
                    event.physical_key,
                    winit::keyboard::PhysicalKey::Code(
                        winit::keyboard::KeyCode::AltLeft | winit::keyboard::KeyCode::AltRight
                    )
                );
                tracing::debug!(
                    "keyboard vk={:#04x} pressed={} alt={} text={:?}",
                    vk,
                    pressed,
                    is_alt,
                    event.text
                );
                if pressed {
                    handle.post_message(hwnd, input::WM_KEYDOWN, u64::from(vk), 0);
                    // TranslateMessage in WIE doesn't generate WM_CHAR, so
                    // we post it directly from the winit KeyEvent.text field.
                    if let Some(ref text) = event.text {
                        for c in text.chars() {
                            handle.post_message(hwnd, input::WM_CHAR, u64::from(c as u32), 0);
                        }
                    }
                    if is_alt {
                        handle.post_message(hwnd, input::WM_SYSKEYDOWN, u64::from(vk), 0);
                    }
                } else {
                    handle.post_message(hwnd, input::WM_KEYUP, u64::from(vk), 0);
                    if is_alt {
                        handle.post_message(hwnd, input::WM_SYSKEYUP, u64::from(vk), 0);
                    }
                }
            }
            WindowEvent::Focused(true) => {
                // WM_SETFOCUS to the window that gained macOS focus.
                handle.post_message(event_hwnd.unwrap_or(primary_hwnd), input::WM_SETFOCUS, 0, 0);
                // macOS has ONE global menu bar: swap it to the newly focused
                // guest window's menu. Best-effort — the guest may not have
                // processed WM_SETFOCUS yet (its SetFocus lands
                // asynchronously), so this may read the previous focus; the
                // per-Frame sync corrects it as soon as the guest repaints
                // (focus rect/caret), and the cheap Vec compare makes a stale
                // read a no-op.
                #[cfg(target_os = "macos")]
                self.sync_menu_bar();
            }
            WindowEvent::Focused(false) => {
                handle.post_message(
                    event_hwnd.unwrap_or(primary_hwnd),
                    input::WM_KILLFOCUS,
                    0,
                    0,
                );
            }
            WindowEvent::Resized(size) => {
                tracing::debug!(
                    "Resized event: {}x{} window={window_id:?}",
                    size.width,
                    size.height,
                );
                let Some(rt) = self.windows.get_mut(&window_id) else {
                    return;
                };
                // Keep the wgpu swapchain matching the window's physical
                // size. This is purely the host surface — the guest-visible
                // WM_SIZE bookkeeping below is untouched.
                if let Some(presenter) = rt.surface.as_mut() {
                    presenter.resize(size.width.max(1), size.height.max(1));
                }
                let w = size.width.max(1);
                let h = size.height.max(1);
                // Debounce: remember the latest size; the final WM_SIZE is
                // posted only once the drag settles (see about_to_wait),
                // so the guest reallocates its DIB exactly once.
                //
                // Do NOT update the guest-visible window record here.  The
                // record must stay at the size the guest has actually been
                // told (last WM_SIZE) until the settle posts the new one —
                // otherwise a mid-drag timer WM_PAINT resolves BitBlt to
                // the growing size, ensure_surface reallocates zero-padded,
                // the old-size DIB only partially fills it, and the
                // published frame has a zero strip ("bg doesn't cover").
                rt.pending_size = Some((w, h));
                rt.last_resize = Some(Instant::now());
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(RESIZE_SETTLE_MS),
                ));
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Debounced resize, per window: when no Resized event arrived for the
        // settle window, the drag has ended — post the final WM_SIZE (and a
        // WM_PAINT so the guest reallocates its DIB exactly once, at the
        // final size). Each window settles independently (a dialog can be
        // settling while the main window idles).
        let mut any_settled = false;
        for rt in self.windows.values_mut() {
            let Some(start) = rt.last_resize else {
                continue;
            };
            if start.elapsed() < Duration::from_millis(RESIZE_SETTLE_MS) {
                continue;
            }
            rt.last_resize = None;
            any_settled = true;
            let Some((w, h)) = rt.pending_size.take() else {
                continue;
            };
            // The guest DIB is LOGICAL 96-DPI: post the PHYSICAL inner
            // size divided by the device scale factor.
            let (lw, lh) = guest_size_from_physical(w, h, rt.scale_factor);
            tracing::debug!(
                "settle: pending={}x{} guest={}x{} last_sent={:?}",
                w,
                h,
                lw,
                lh,
                rt.last_sent_size
            );
            // Skip if the size hasn't changed since the last posted
            // WM_SIZE — macOS fires trailing Resized events after the
            // drag, and re-posting the same size would re-trigger the
            // guest's expensive DIB recreation (which also delays
            // close/quit handling).
            if rt.last_sent_size != Some((lw, lh)) {
                let hwnd = rt.hwnd;
                if let Some(handle) = self.handle.as_ref() {
                    // Update the guest-visible record now — together with
                    // the WM_SIZE post — so GetClientRect matches the size
                    // the guest is about to recreate its DIB at.
                    handle.resize_window(hwnd.as_u64(), lw, lh);
                    let lparam = input::make_lparam(lw as u16, lh as u16);
                    handle.post_message(hwnd.as_u64(), input::WM_SIZE, 0, lparam);
                    handle.post_message(hwnd.as_u64(), input::WM_PAINT, 0, 0);
                    tracing::debug!("resize settled: WM_SIZE {}x{}", lw, lh);
                }
                rt.last_sent_size = Some((lw, lh));
            }
        }
        if any_settled {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: WieEvent) {
        match event {
            WieEvent::Frame { published_at } => {
                // Wake → event-loop latency.
                tracing::debug!(
                    target: "wiegui",
                    wake_to_user_us = u64::try_from(published_at.elapsed().as_micros())
                        .unwrap_or(u64::MAX),
                    "Frame event"
                );
                // Reconcile the host window registry against the guest's
                // top-level windows. On the first frame this creates the
                // first window from the guest's own title/size (identical to
                // the pre-registry first-window path); later frames create
                // windows for newly opened top-levels (dialogs) and destroy
                // the winit windows of closed ones.
                self.reconcile_windows(event_loop);
                // Coalesce wake storms. Every publish sets the flag; the
                // first Frame event after a publish group requests the
                // redraws and later duplicates (which see the flag cleared)
                // skip. A real new frame is never dropped: any new publish
                // re-sets the flag AND enqueues another Frame event, and each
                // window's RedrawRequested additionally skips only when its
                // own frame is unchanged (per-window ptr_eq skip).
                if self
                    .pending_frame
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    for rt in self.windows.values() {
                        rt.window.request_redraw();
                    }
                } else {
                    tracing::debug!(target: "wiegui", "coalesced duplicate Frame event");
                }
                // Apply any guest-requested geometry (SetWindowPlacement) to
                // the host window. The winapi handler set a pending request
                // and woke the presenter, so the move lands even without a
                // repaint; a cheap no-op when nothing is pending. Runs on the
                // first frame too — the window was just created above, so a
                // startup restore is applied immediately.
                self.apply_host_geometry();
                #[cfg(target_os = "macos")]
                self.sync_menu_bar();
            }
            #[cfg(target_os = "macos")]
            WieEvent::MenuEvent(menu_event) => {
                // Decode the string-form muda id back to the guest item id and
                // deliver WM_COMMAND with wParam = MAKEWPARAM(id, 0). The menu
                // bar mirrors the primary window's menu (menu polish is L4's
                // lane), so the command goes to the primary window's hwnd.
                let id = menu_bar::menu_id_to_guest_id(menu_event.id());
                tracing::debug!(
                    target: "wiegui",
                    menu_id = menu_event.id().0,
                    guest_id = id,
                    "MenuEvent"
                );
                if let Some(handle) = self.handle.as_ref()
                    && let Some(hwnd) = self.primary_hwnd.map_or_else(
                        || handle.first_guest_window_handle(),
                        |hwnd| Some(hwnd.as_u64()),
                    )
                {
                    handle.post_message(hwnd, input::WM_COMMAND, u64::from(id), 0);
                }
            }
            WieEvent::GuestExited { code: _ } => {
                event_loop.exit();
            }
        }
    }
}

/// Map Win32 `MB_*` flag bits to rfd's dialog shape.
///
/// The low nibble selects the button set, the next nibble the icon. rfd has no
/// `Question` level, so `MB_ICONQUESTION` falls back to `Info`. Unknown bits
/// fall back to the MB_OK / no-icon defaults, matching real MessageBox.
#[cfg(target_os = "macos")]
fn map_message_box_buttons(mb_type: u32) -> (rfd::MessageButtons, rfd::MessageLevel) {
    let buttons = match mb_type & 0x0F {
        0x1 => rfd::MessageButtons::OkCancel,
        0x3 => rfd::MessageButtons::YesNoCancel,
        0x4 => rfd::MessageButtons::YesNo,
        _ => rfd::MessageButtons::Ok, // 0x0 = MB_OK
    };
    let level = match mb_type & 0xF0 {
        0x10 => rfd::MessageLevel::Error,
        0x30 => rfd::MessageLevel::Warning,
        0x40 => rfd::MessageLevel::Info,
        // 0x20 = MB_ICONQUESTION and 0x00 = no icon both read as Info.
        _ => rfd::MessageLevel::Info,
    };
    (buttons, level)
}

/// Map an rfd alert result to the Win32 id the guest expects
/// (IDOK=1, IDCANCEL=2, IDYES=6, IDNO=7).
#[cfg(target_os = "macos")]
fn map_alert_result(result: rfd::MessageDialogResult) -> i32 {
    match result {
        rfd::MessageDialogResult::Ok => 1,
        rfd::MessageDialogResult::Cancel => 2,
        rfd::MessageDialogResult::Yes => 6,
        rfd::MessageDialogResult::No => 7,
        rfd::MessageDialogResult::Custom(_) => 2,
    }
}

/// Run the guest with a winit window.
///
/// `input_script` is a parsed input-script path (see
/// [`crate::gui::input_script`]); `None` runs without scripted input. The
/// script is read and parsed up front so a bad path or syntax fails before
/// the guest thread and event loop start.
pub fn run_gui_windowed(
    path: &std::path::Path,
    input_script: Option<std::path::PathBuf>,
) -> Result<()> {
    let script_steps = match &input_script {
        Some(script_path) => Some(crate::gui::input_script::read_script(script_path)?),
        None => None,
    };
    let event_loop = EventLoop::<WieEvent>::with_user_event()
        .build()
        .context("build event loop")?;
    let proxy = event_loop.create_proxy();
    // Wake-coalescing flag, shared between the guest-thread wake callback
    // and the host event loop. Cloned for the guest thread; the original moves
    // into `WieApp`.
    let pending_frame = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pending_frame_guest = pending_frame.clone();
    // Shared slot for the winit window Arc: the MessageBox bridge (registered
    // on the guest thread BEFORE the window exists) reads it to parent its
    // rfd dialog; WieApp fills it once the window is created. Clone for the
    // guest thread; the original moves into `WieApp`.
    let window_slot: Arc<Mutex<Option<Arc<Window>>>> = Arc::new(Mutex::new(None));
    let window_slot_guest = window_slot.clone();

    let handle_rx = {
        let (tx, rx) = mpsc::channel::<GuestHandle>();
        let proxy = proxy.clone();
        let control = Arc::new(GuiControl::new());
        let path = path.to_owned();

        thread::Builder::new()
            .name("wie-guest-primary".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                match RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle) {
                    Ok(mut session) => {
                        // Interactive file dialogs: GetOpenFileNameW /
                        // GetSaveFileNameW build the host dialog (path EDIT +
                        // directory LISTBOX + OK/Cancel) and run its in-guest
                        // modal loop instead of returning a scripted answer.
                        crate::gui::file_dialog::enable_interactive_file_dialogs(&mut session);
                        // FindTextW / ReplaceTextW are modeless and always
                        // enabled (see gui/find_dialog.rs); this call pins the
                        // wiring point next to the file dialog's.
                        crate::gui::find_dialog::enable_interactive_find_dialogs(&mut session);
                        // ChooseFontW builds the host font dialog (family +
                        // size + effects + OK/Cancel) and runs the file
                        // dialog's in-guest modal loop.
                        crate::gui::font_dialog::enable_interactive_font_dialogs(&mut session);
                        let handle = session.guest_handle();

                        // Register wake callback.
                        {
                            let proxy = proxy.clone();
                            let pending = pending_frame_guest.clone();
                            handle.set_wake(Box::new(move || {
                                // Mark the pending frame BEFORE sending so
                                // the first Frame event always does the work.
                                pending.store(true, std::sync::atomic::Ordering::SeqCst);
                                let _ = proxy.send_event(WieEvent::Frame {
                                    published_at: Instant::now(),
                                });
                            }));
                        }

                        // Register the native-alert MessageBox bridge. rfd's
                        // parented dialog uses the modern NSAlert API
                        // (dispatched to the main thread; the guest thread
                        // blocks until the user clicks — MessageBox
                        // semantics) and maps the result to the Win32 id.
                        // With no parent, rfd falls back to the legacy
                        // CFUserNotificationDisplayAlert path, which prints
                        // "will block waiting for a response" on the main
                        // thread — the window slot below switches to NSAlert
                        // once the winit window exists.
                        #[cfg(target_os = "macos")]
                        handle.set_message_box_bridge(Box::new({
                            let window_slot = window_slot_guest.clone();
                            move |caption, text, mb_type| {
                                tracing::info!(
                                    target: "wiegui",
                                    "MessageBox: {caption}: {text} (type 0x{mb_type:x})"
                                );
                                let (buttons, level) = map_message_box_buttons(mb_type);
                                // Parent to the winit window when it exists
                                // (it always does by the time a MessageBox
                                // fires — the bridge is just registered
                                // earlier). set_parent consumes the builder,
                                // so apply it before the chain.
                                let parent = window_slot.lock().ok().and_then(|slot| slot.clone());
                                let mut dialog = rfd::MessageDialog::new();
                                if let Some(parent) = &parent {
                                    dialog = dialog.set_parent(parent.as_ref());
                                }
                                let result = dialog
                                    .set_title(caption.to_owned())
                                    .set_description(text.to_owned())
                                    .set_level(level)
                                    .set_buttons(buttons)
                                    .show();
                                map_alert_result(result)
                            }
                        }));

                        let _ = tx.send(handle);

                        // Run the guest.
                        let _result = run_windowed(&mut session, &control);
                        let code = control.exit_code.load(std::sync::atomic::Ordering::SeqCst);
                        let _ = proxy.send_event(WieEvent::GuestExited { code });
                    }
                    Err(e) => tracing::error!("session: {e}"),
                }
            })
            .context("spawn guest thread")?;

        rx
    };

    let handle = handle_rx.recv().context("recv handle")?;

    // Scripted input driver: posts WM_KEYDOWN/WM_CHAR/WM_COMMAND to the guest
    // on its own schedule once the guest window exists.
    if let Some(steps) = script_steps {
        crate::gui::input_script::spawn(handle.clone(), steps)?;
    }

    // Windows are created lazily when frames arrive (in user_event):
    // reconciliation creates a winit window per guest top-level.
    let mut app = WieApp {
        handle: Some(handle),
        windows: WindowRegistry::new(),
        primary_hwnd: None,
        pending_frame,
        window_slot,
        mouse_buttons: 0,
        cursor_pos: (0.0, 0.0),
        last_left_press: None,
        modifiers: winit::keyboard::ModifiersState::default(),
        #[cfg(target_os = "macos")]
        menu_bar: crate::gui::menu_bar::MacMenuBar::new(proxy.clone()),
        #[cfg(target_os = "macos")]
        last_menu_items: Arc::new(Vec::new()),
    };

    event_loop
        .run_app(&mut app)
        .map_err(|e| anyhow::anyhow!("event loop: {e}"))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{
        guest_size_from_physical, map_alert_result, map_message_box_buttons, window_attributes,
    };

    /// `MB_*` button bits select the rfd button set; the bridge receives the
    /// raw flag word, so this mapping is the winapi crate's documented seam.
    #[test]
    fn map_message_box_buttons_selects_button_set() {
        assert!(matches!(
            map_message_box_buttons(0x00).0,
            rfd::MessageButtons::Ok
        ));
        assert!(matches!(
            map_message_box_buttons(0x01).0,
            rfd::MessageButtons::OkCancel
        ));
        assert!(matches!(
            map_message_box_buttons(0x04).0,
            rfd::MessageButtons::YesNo
        ));
        assert!(matches!(
            map_message_box_buttons(0x03).0,
            rfd::MessageButtons::YesNoCancel
        ));
        // Icon bits must not disturb the button set.
        assert!(matches!(
            map_message_box_buttons(0x04 | 0x20).0,
            rfd::MessageButtons::YesNo
        ));
        // Unknown button bits fall back to Ok.
        assert!(matches!(
            map_message_box_buttons(0x02).0,
            rfd::MessageButtons::Ok
        ));
    }

    /// `MB_ICON*` bits select the rfd level. rfd has no Question level, so
    /// MB_ICONQUESTION reads as Info.
    #[test]
    fn map_message_box_buttons_selects_level() {
        assert!(matches!(
            map_message_box_buttons(0x10).1,
            rfd::MessageLevel::Error
        ));
        assert!(matches!(
            map_message_box_buttons(0x30).1,
            rfd::MessageLevel::Warning
        ));
        assert!(matches!(
            map_message_box_buttons(0x40).1,
            rfd::MessageLevel::Info
        ));
        assert!(matches!(
            map_message_box_buttons(0x20).1,
            rfd::MessageLevel::Info
        ));
        // No icon bits (plain MB_OK) also reads as Info.
        assert!(matches!(
            map_message_box_buttons(0x00).1,
            rfd::MessageLevel::Info
        ));
    }

    /// The user's alert choice maps to the Win32 id the guest sees.
    #[test]
    fn map_alert_result_maps_to_win32_ids() {
        assert_eq!(map_alert_result(rfd::MessageDialogResult::Ok), 1, "IDOK");
        assert_eq!(
            map_alert_result(rfd::MessageDialogResult::Cancel),
            2,
            "IDCANCEL"
        );
        assert_eq!(map_alert_result(rfd::MessageDialogResult::Yes), 6, "IDYES");
        assert_eq!(map_alert_result(rfd::MessageDialogResult::No), 7, "IDNO");
        assert_eq!(
            map_alert_result(rfd::MessageDialogResult::Custom("x".to_owned())),
            2,
            "an unknown custom result cancels"
        );
    }

    // -----------------------------------------------------------------------
    // F1 scale plumbing (fidelity lane 1): the guest space is LOGICAL 96-DPI
    // pixels; winit reports PHYSICAL. These pin the window-boundary
    // conversions the input/resize/geometry paths share.
    // -----------------------------------------------------------------------

    /// The guest's first window is created at its LOGICAL 96-DPI size —
    /// winit multiplies by the device scale factor itself.
    #[test]
    fn window_attributes_use_logical_size() {
        let attrs = window_attributes("notepad", 640, 480);
        assert_eq!(
            attrs.inner_size,
            Some(winit::dpi::Size::Logical(winit::dpi::LogicalSize::new(
                640.0, 480.0
            )))
        );
        let attrs = window_attributes("t", 100, 100);
        assert_eq!(
            attrs.inner_size,
            Some(winit::dpi::Size::Logical(winit::dpi::LogicalSize::new(
                100.0, 100.0
            )))
        );
    }

    /// The WM_SIZE settle posts the guest LOGICAL size: physical inner_size
    /// ÷ sf with the crate's rounding.
    #[test]
    fn settle_posts_logical_size() {
        // Retina 2×: a 1280×960 physical window is a 640×480 logical window.
        assert_eq!(guest_size_from_physical(1280, 960, 2.0), (640, 480));
        // Non-integer division rounds half away from zero (320.5 → 321).
        assert_eq!(guest_size_from_physical(641, 481, 2.0), (321, 241));
        // Scale factor 1.0 reproduces today's physical-as-logical posting.
        assert_eq!(guest_size_from_physical(640, 480, 1.0), (640, 480));
        assert_eq!(guest_size_from_physical(886, 776, 1.0), (886, 776));
    }
}
