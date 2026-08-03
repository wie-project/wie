//! Winit application handler — displays the guest window and forwards input.

use std::sync::Arc;
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
use winit::dpi::{PhysicalPosition, PhysicalSize};
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

    /// The window under the cursor and its client-relative coordinates.
    ///
    /// A window holding the mouse capture (SetCapture — a pressed BUTTON
    /// captures while down) receives every mouse message instead of the
    /// hit-tested child, matching Windows. Falls back to the main window at
    /// (0, 0) when the hit-test finds nothing (no visible top-level window
    /// yet). Mouse messages go to the topmost child containing the cursor,
    /// with child-relative lParam coords.
    fn mouse_target(&self, handle: &GuestHandle) -> (u64, u16, u16) {
        let (cx, cy) = self.cursor_pos;
        let (x, y) = (cx.max(0.0) as i32, cy.max(0.0) as i32);
        if let Some((hwnd, rx, ry)) = handle.capture_target(x, y) {
            return (hwnd, rx as u16, ry as u16);
        }
        handle.window_at(x, y).map_or(
            (self.runtime.as_ref().map_or(0, |rt| rt.hwnd.as_u64()), 0, 0),
            |(hwnd, rx, ry)| (hwnd, rx as u16, ry as u16),
        )
    }

    /// The last cursor position as integer client coordinates (for MSG.pt).
    fn cursor_pos_i32(&self) -> (i32, i32) {
        (
            self.cursor_pos.0.max(0.0) as i32,
            self.cursor_pos.1.max(0.0) as i32,
        )
    }

    /// Rebuilds the macOS menu bar when the guest window's menu changed.
    ///
    /// Runs once per `Frame`; the cheap `Vec` compare skips the AppKit work
    /// unless the guest actually rebuilt its menu. Clicks are forwarded back
    /// through the event-loop proxy by [`MacMenuBar`] as [`WieEvent::MenuEvent`]
    /// so the guest mutation (`WM_COMMAND`) always happens on this thread.
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
    /// The winapi handler records the pending `(x, y, width, height)` on the
    /// shared state and wakes the presenter; this consumes it on the
    /// event-loop thread where winit calls must run. Guest coordinates are
    /// treated as physical pixels, matching how `first_guest_window_info`
    /// sizes the window at creation (the same `max(100)` clamp guards against
    /// a degenerate or negative rect). When no window exists yet (the move
    /// arrived before the first published frame) the request stays pending
    /// and applies once the window is created.
    fn apply_host_geometry(&self) {
        let Some(rt) = self.runtime.as_ref() else {
            return;
        };
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let Some((x, y, width, height)) = handle.take_host_geometry_request() else {
            return;
        };
        rt.window
            .set_outer_position(PhysicalPosition::new(x as f64, y as f64));
        // winit 0.30 removed set_inner_size; request_inner_size is its
        // replacement (same Into<Size> contract as with_inner_size at
        // creation, so guest pixels stay physical). The returned actual size
        // is informational — the winit Resized event carries the settle.
        let _ = rt.window.request_inner_size(PhysicalSize::new(
            width.max(100) as u32,
            height.max(100) as u32,
        ));
    }
}

/// Debounce window for resize: how long without a `Resized` event before we
/// consider the drag settled and post the final `WM_SIZE` to the guest.
///
/// 50 ms: short enough that release→crisp-frame feels instant (the guest
/// repaint is the dominant cost, ~55 ms at 886×776), long enough to absorb
/// macOS's trailing `Resized` events so the guest reallocates its DIB once.
const RESIZE_SETTLE_MS: u64 = 50;

/// Per-window host state for an active winit window — the payload of
/// [`WindowState::Active`]. The "window exists iff hwnd known" invariant is
/// now a type instead of per-arm checks.
struct WindowRuntime {
    /// Guest HWND this window mirrors.
    hwnd: Hwnd,
    window: Arc<Window>,
    /// The wgpu (Metal) present backend, created at window construction.
    surface: Option<PresentBackend>,
    /// Present generation of the last frame actually presented; frames
    /// with an unchanged generation AND unchanged window size are skipped.
    last_presented_generation: Option<u64>,
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
    /// that recreation delays close/quit handling).
    last_sent_size: Option<(u32, u32)>,
}

/// Window-bound state. The winit window is created lazily on the first
/// published frame (the guest provides the title/size), so a session starts
/// [`WindowState::Uncreated`] and moves to [`WindowState::Active`] exactly
/// once, for the rest of the session. A thin [`Option`]-equivalent so call
/// sites keep the `as_ref()` / `as_mut()` / `is_none()` shape.
enum WindowState {
    /// The winit window has not been created yet.
    Uncreated,
    /// A winit window exists and mirrors the guest window.
    Active(WindowRuntime),
}

impl WindowState {
    fn as_ref(&self) -> Option<&WindowRuntime> {
        match self {
            WindowState::Uncreated => None,
            WindowState::Active(rt) => Some(rt),
        }
    }

    fn as_mut(&mut self) -> Option<&mut WindowRuntime> {
        match self {
            WindowState::Uncreated => None,
            WindowState::Active(rt) => Some(rt),
        }
    }

    fn is_none(&self) -> bool {
        matches!(self, WindowState::Uncreated)
    }

    fn is_some(&self) -> bool {
        matches!(self, WindowState::Active(_))
    }
}

/// The wgpu (Metal) present backend for a window, created at window
/// construction. The guest frame is uploaded to a staging texture and blitted
/// to the swapchain; the pixel Arc is dropped right after the upload so the
/// guest can hand the surface buffer back zero-copy on the next paint.
type PresentBackend = crate::gui::present_wgpu::WgpuPresenter;

/// Initialize the wgpu present backend. wgpu init is expected to succeed on
/// macOS (Metal backend); a failure here means the host cannot present at all.
fn init_present_backend(window: &Arc<Window>) -> Option<PresentBackend> {
    crate::gui::present_wgpu::WgpuPresenter::init(window.clone()).ok()
}

/// The message for a left-button press: WM_LBUTTONDBLCLK when it lands on the
/// SAME target window within the double-click time window AND the double-click
/// slop rectangle of the previous press (`last`), WM_LBUTTONDOWN otherwise.
/// Every press refreshes `last`, so the host re-creates the message Windows
/// itself synthesizes from `GetDoubleClickTime` / `SM_CXDOUBLECLK` (Windows
/// tracks double-clicks per window). A free function (not a method) so the
/// MouseInput arm can call it while `self.handle` is borrowed.
fn left_press_message(
    last: &mut Option<(Instant, f64, f64, u64)>,
    cursor: (f64, f64),
    hwnd: u64,
) -> u32 {
    let now = Instant::now();
    let (x, y) = cursor;
    let dbl = last.is_some_and(|(t, lx, ly, last_hwnd)| {
        last_hwnd == hwnd
            && now.saturating_duration_since(t)
                <= Duration::from_millis(input::DOUBLE_CLICK_TIME_MS)
            && (x - lx).abs() <= input::DOUBLE_CLICK_SLOP_PX
            && (y - ly).abs() <= input::DOUBLE_CLICK_SLOP_PX
    });
    *last = Some((now, x, y, hwnd));
    if dbl {
        input::WM_LBUTTONDBLCLK
    } else {
        input::WM_LBUTTONDOWN
    }
}

/// winit application state: bridges the guest window to the present backend.
struct WieApp {
    handle: Option<GuestHandle>,
    /// Window-bound state; [`WindowState::Uncreated`] until the first frame
    /// creates the winit window, then [`WindowState::Active`] for the session.
    runtime: WindowState,
    /// Wake-coalescing flag, shared with the guest-thread wake callback.
    /// Set on every publish; the first `Frame` event after a publish group
    /// swaps it and requests a redraw, duplicates skip.
    pending_frame: Arc<std::sync::atomic::AtomicBool>,
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
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // Handle events that need early processing before the runtime guard.
        match &event {
            WindowEvent::CloseRequested => {
                if let (Some(handle), Some(rt)) = (self.handle.as_ref(), self.runtime.as_ref()) {
                    handle.post_message(rt.hwnd.as_u64(), input::WM_CLOSE, 0, 0);
                }
                if self.runtime.is_none() {
                    event_loop.exit();
                }
                return;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // Keyboard events only fire once the winit window exists, so
                // the runtime is always present here; no fresh-lookup needed.
                let Some(handle) = self.handle.as_ref() else {
                    return;
                };
                let Some(rt) = self.runtime.as_ref() else {
                    return;
                };
                let hwnd = rt.hwnd.as_u64();
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
        let Some(hwnd) = self.runtime.as_ref().map(|rt| rt.hwnd) else {
            return;
        };

        match event {
            WindowEvent::DroppedFile(path) => {
                // A host file dropped on the window: store it as the guest
                // drop list and post WM_DROPFILES with the fake HDROP. The
                // guest's handler (notepad's WM_DROPFILES) reads the path via
                // DragQueryFileW, mapped to a guest C:\ path by
                // GuestHandle::set_drop_files (drops outside the bottle/D:
                // bridge are skipped, so hdrop stays 0 and nothing is posted).
                let (px, py) = self.cursor_pos_i32();
                let hdrop = handle.set_drop_files(vec![path.clone()], (px, py));
                tracing::info!(
                    target: "wiegui",
                    host_path = %path.display(),
                    "drop: mapped to hdrop=0x{hdrop:x}"
                );
                if hdrop != 0 {
                    let lparam = input::make_lparam(px.max(0) as u16, py.max(0) as u16);
                    handle.post_message(hwnd.as_u64(), input::WM_DROPFILES, hdrop, lparam);
                }
            }
            WindowEvent::RedrawRequested => {
                let Some(rt) = self.runtime.as_mut() else {
                    return;
                };
                let frame = handle.take_frame(hwnd.as_u64());
                if let Some(frame) = frame {
                    let (dst_w, dst_h) = {
                        let s = rt.window.inner_size();
                        (s.width.max(1), s.height.max(1))
                    };
                    // Skip redundant presents. When neither the
                    // present generation (nothing repainted) nor the
                    // window size (drag-stretch) changed since the last
                    // present, the published frame is byte-identical —
                    // skip the copy + GPU upload entirely.
                    let generation = handle.present_generation();
                    if rt.last_presented_generation == Some(generation)
                        && rt.last_presented_size == Some((dst_w, dst_h))
                    {
                        tracing::debug!(
                            target: "wiegui",
                            generation,
                            "skipping unchanged frame (gen+size)"
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
                    // pixel Arc right after the staging upload.
                    if let Some(presenter) = rt.surface.as_mut() {
                        if let Err(e) = presenter.present(frame, dst_w, dst_h) {
                            tracing::error!(target: "wiegui", error = %e, "wgpu present failed");
                        }
                    }
                    if let Some(t0) = present_t0 {
                        handle.record_present_time(t0.elapsed().as_nanos());
                        tracing::debug!(
                            target: "wiegui",
                            present_us = u64::try_from(t0.elapsed().as_micros())
                                .unwrap_or(u64::MAX),
                            "host present"
                        );
                    }
                    rt.last_presented_generation = Some(generation);
                    rt.last_presented_size = Some((dst_w, dst_h));
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x.max(0.0), position.y.max(0.0));
                let mk = self.mk_flags();
                // Route to the topmost child under the cursor, if any.
                let (target, rx, ry) = self.mouse_target(handle);
                let lparam = input::make_lparam(rx, ry);
                let (px, py) = self.cursor_pos_i32();
                handle.post_message_at(target, input::WM_MOUSEMOVE, u64::from(mk), lparam, px, py);
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
                // double-clicks per window).
                let (target, rx, ry) = self.mouse_target(handle);
                let msg = match button {
                    winit::event::MouseButton::Left => {
                        if pressed {
                            left_press_message(&mut self.last_left_press, self.cursor_pos, target)
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
                let (px, py) = self.cursor_pos_i32();
                handle.post_message_at(target, msg, u64::from(mk), lparam, px, py);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (delta_x, delta_y) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        ((x * 120.0) as i32, (y * 120.0) as i32)
                    }
                    winit::event::MouseScrollDelta::PixelDelta(pos) => (pos.x as i32, pos.y as i32),
                };
                let mk = self.mk_flags();
                let (px, py) = self.cursor_pos_i32();
                // WM_MOUSEWHEEL/HWHEEL go to the FOCUS window, not the window
                // under the cursor (DefWindowProc then bubbles them up the
                // parent chain) — a multiline EDIT keeps scrolling while the
                // pointer is elsewhere. Fall back to the hit-tested window
                // when nothing has keyboard focus.
                let (target, rx, ry) = match handle.focus_window() {
                    Some(focus) => (focus, 0, 0),
                    None => self.mouse_target(handle),
                };
                if delta_y != 0 {
                    let wparam = input::make_wparam(mk, delta_y as u16);
                    handle.post_message_at(
                        target,
                        input::WM_MOUSEWHEEL,
                        wparam,
                        input::make_lparam(rx, ry),
                        px,
                        py,
                    );
                }
                if delta_x != 0 {
                    let wparam = input::make_wparam(mk, delta_x as u16);
                    handle.post_message_at(
                        target,
                        input::WM_MOUSEHWHEEL,
                        wparam,
                        input::make_lparam(rx, ry),
                        px,
                        py,
                    );
                }
            }
            WindowEvent::Moved(position) => {
                // WM_MOVE: lParam = MAKELPARAM(x, y) screen coords.
                let lparam = input::make_lparam(position.x.max(0) as u16, position.y.max(0) as u16);
                handle.post_message(hwnd.as_u64(), input::WM_MOVE, 0, lparam);
            }
            WindowEvent::CursorEntered { .. } => {
                // Windows sends WM_MOUSEHOVER only for windows that requested
                // tracking via TrackMouseEvent.
                if handle.mouse_tracking(hwnd.as_u64()) {
                    handle.post_message(hwnd.as_u64(), input::WM_MOUSEHOVER, 0, 0);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                // Windows sends WM_MOUSELEAVE only for tracked windows.
                if handle.mouse_tracking(hwnd.as_u64()) {
                    handle.post_message(hwnd.as_u64(), input::WM_MOUSELEAVE, 0, 0);
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                // Physical pixel size changed under us; the guest sees the
                // same logical size, but we need a fresh render at the new
                // scale.  Request a redraw; the next frame covers it.
                if let Some(rt) = self.runtime.as_ref() {
                    rt.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = matches!(event.state, winit::event::ElementState::Pressed);
                let vk = input::virt_key_from_physical(event.physical_key);
                // Keep the guest keyboard-state table in sync with real input.
                handle.set_key_state(vk, pressed);
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
                    handle.post_message(hwnd.as_u64(), input::WM_KEYDOWN, u64::from(vk), 0);
                    // TranslateMessage in WIE doesn't generate WM_CHAR, so
                    // we post it directly from the winit KeyEvent.text field.
                    if let Some(ref text) = event.text {
                        for c in text.chars() {
                            handle.post_message(
                                hwnd.as_u64(),
                                input::WM_CHAR,
                                u64::from(c as u32),
                                0,
                            );
                        }
                    }
                    if is_alt {
                        handle.post_message(hwnd.as_u64(), input::WM_SYSKEYDOWN, u64::from(vk), 0);
                    }
                } else {
                    handle.post_message(hwnd.as_u64(), input::WM_KEYUP, u64::from(vk), 0);
                    if is_alt {
                        handle.post_message(hwnd.as_u64(), input::WM_SYSKEYUP, u64::from(vk), 0);
                    }
                }
            }
            WindowEvent::Focused(true) => {
                handle.post_message(hwnd.as_u64(), input::WM_SETFOCUS, 0, 0);
            }
            WindowEvent::Focused(false) => {
                handle.post_message(hwnd.as_u64(), input::WM_KILLFOCUS, 0, 0);
            }
            WindowEvent::Resized(size) => {
                tracing::debug!(
                    "Resized event: {}x{} hwnd_set={}",
                    size.width,
                    size.height,
                    self.runtime.is_some()
                );
                let Some(rt) = self.runtime.as_mut() else {
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
        // Debounced resize: when no Resized event arrived for the settle
        // window, the drag has ended — post the final WM_SIZE (and a WM_PAINT
        // so the guest reallocates its DIB exactly once, at the final size).
        let Some(rt) = self.runtime.as_mut() else {
            return;
        };
        if let Some(start) = rt.last_resize
            && start.elapsed() >= Duration::from_millis(RESIZE_SETTLE_MS)
        {
            rt.last_resize = None;
            if let Some((w, h)) = rt.pending_size.take() {
                tracing::debug!(
                    "settle: pending={}x{} last_sent={:?}",
                    w,
                    h,
                    rt.last_sent_size
                );
                // Skip if the size hasn't changed since the last posted
                // WM_SIZE — macOS fires trailing Resized events after the
                // drag, and re-posting the same size would re-trigger the
                // guest's expensive DIB recreation (which also delays
                // close/quit handling).
                if rt.last_sent_size != Some((w, h)) {
                    let hwnd = rt.hwnd;
                    if let Some(handle) = self.handle.as_ref() {
                        // Update the guest-visible record now — together with
                        // the WM_SIZE post — so GetClientRect matches the size
                        // the guest is about to recreate its DIB at.
                        handle.resize_window(hwnd.as_u64(), w, h);
                        let lparam = input::make_lparam(w as u16, h as u16);
                        handle.post_message(hwnd.as_u64(), input::WM_SIZE, 0, lparam);
                        handle.post_message(hwnd.as_u64(), input::WM_PAINT, 0, 0);
                        tracing::debug!("resize settled: WM_SIZE {}x{}", w, h);
                    }
                    rt.last_sent_size = Some((w, h));
                }
            }
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
                if self.runtime.is_none() {
                    // First frame — create window with guest's title and size.
                    if let Some((hwnd, title, w, h)) = self
                        .handle
                        .as_ref()
                        .and_then(|h| h.first_guest_window_info())
                    {
                        let title: &str = &title;
                        let w = w.max(100) as u32;
                        let h = h.max(100) as u32;
                        let attrs = Window::default_attributes()
                            .with_title(title)
                            .with_inner_size(PhysicalSize::new(w, h));
                        if let Ok(window) = event_loop.create_window(attrs) {
                            let window = Arc::new(window);
                            window.focus_window();
                            self.runtime = WindowState::Active(WindowRuntime {
                                hwnd: Hwnd::from(hwnd),
                                window: window.clone(),
                                surface: init_present_backend(&window),
                                last_presented_generation: None,
                                last_presented_size: None,
                                pending_size: None,
                                last_resize: None,
                                last_sent_size: None,
                            });
                        }
                    }
                } else if let Some(rt) = self.runtime.as_ref() {
                    // Coalesce wake storms. Every publish sets the flag;
                    // the first Frame event after a publish group requests the
                    // redraw and later duplicates (which see the flag cleared)
                    // skip. A real new frame is never dropped: any new publish
                    // re-sets the flag AND enqueues another Frame event, and
                    // RedrawRequested additionally skips only when the present
                    // generation AND window size are unchanged.
                    if self
                        .pending_frame
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        rt.window.request_redraw();
                    } else {
                        tracing::debug!(target: "wiegui", "coalesced duplicate Frame event");
                    }
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
                // deliver WM_COMMAND with wParam = MAKEWPARAM(id, 0).
                let id = menu_bar::menu_id_to_guest_id(menu_event.id());
                if let Some(handle) = self.handle.as_ref()
                    && let Some(hwnd) = self.runtime.as_ref().map_or_else(
                        || handle.first_guest_window_handle(),
                        |rt| Some(rt.hwnd.as_u64()),
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

                        // Register the native-alert MessageBox bridge. rfd shows
                        // an NSAlert (dispatched to the main thread; the guest
                        // thread blocks until the user clicks — MessageBox
                        // semantics) and maps the result to the Win32 id.
                        #[cfg(target_os = "macos")]
                        handle.set_message_box_bridge(Box::new(|caption, text, mb_type| {
                            tracing::info!(
                                target: "wiegui",
                                "MessageBox: {caption}: {text} (type 0x{mb_type:x})"
                            );
                            let (buttons, level) = map_message_box_buttons(mb_type);
                            let result = rfd::MessageDialog::new()
                                .set_title(caption.to_owned())
                                .set_description(text.to_owned())
                                .set_level(level)
                                .set_buttons(buttons)
                                .show();
                            map_alert_result(result)
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

    // Window is created lazily when the first frame arrives (in user_event).
    let mut app = WieApp {
        handle: Some(handle),
        runtime: WindowState::Uncreated,
        pending_frame,
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
    use super::{map_alert_result, map_message_box_buttons};

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
}
