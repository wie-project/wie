//! Winit application handler — displays the guest window and forwards input.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wie_cpu::stretch_nearest;
use wie_runtime::GuestHandle;
use wie_runtime::MenuNode;
use wie_runtime::RuntimeSession;
use wie_runtime::{GuiControl, run_windowed};
use wie_winapi::handles::Hwnd;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
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
        self.menu_bar.rebuild(&items);
        self.last_menu_items = items;
    }
}

/// Debounce window for resize: how long without a `Resized` event before we
/// consider the drag settled and post the final `WM_SIZE` to the guest.
///
/// 50 ms: short enough that release→crisp-frame feels instant (the guest
/// repaint is the dominant cost, ~55 ms at 886×776), long enough to absorb
/// macOS's trailing `Resized` events so the guest reallocates its DIB once.
const RESIZE_SETTLE_MS: u64 = 50;

/// Per-window host state. `Some` iff the winit window exists — the "window
/// exists iff hwnd known" invariant is now a type instead of per-arm checks.
struct WindowRuntime {
    /// Guest HWND this window mirrors.
    hwnd: Hwnd,
    window: Arc<Window>,
    /// At-most-one present backend (wgpu XOR softbuffer), by construction.
    surface: Option<PresentBackend>,
    /// B2: present generation of the last frame actually presented; frames
    /// with an unchanged generation AND unchanged window size are skipped.
    last_presented_generation: Option<u64>,
    /// B2: window size at the last present (drag-stretch must not be skipped).
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

/// The present backend for a window. At most one variant is set — the two
/// backends are mutually exclusive per window.
#[expect(
    clippy::large_enum_variant,
    reason = "one instance per window, constructed once; the softbuffer surface's size is irrelevant"
)]
enum PresentBackend {
    /// P4a: wgpu (Metal) present backend, used by default on macOS. When set,
    /// `Softbuffer` stays unset. Falls back to softbuffer if wgpu init fails
    /// or `WIE_PRESENT` names softbuffer.
    #[cfg(target_os = "macos")]
    Wgpu(crate::gui::present_wgpu::WgpuPresenter),
    /// softbuffer (CPU copy) — the fallback and the non-macOS path.
    Softbuffer(softbuffer::Surface<Arc<Window>, Arc<Window>>),
}

/// Initialize the present backend per `WIE_PRESENT`: "softbuffer" keeps the
/// CPU path, anything else (default) uses wgpu on macOS. On wgpu init failure
/// we fall back to softbuffer rather than showing a black window.
#[cfg(target_os = "macos")]
fn init_present_backend(window: &Arc<Window>) -> Option<PresentBackend> {
    let use_wgpu = !matches!(
        std::env::var("WIE_PRESENT").ok().as_deref(),
        Some("softbuffer")
    );
    if use_wgpu {
        match crate::gui::present_wgpu::WgpuPresenter::init(window.clone()) {
            Ok(presenter) => Some(PresentBackend::Wgpu(presenter)),
            Err(e) => {
                tracing::error!(
                    target: "wiegui",
                    error = %e,
                    "wgpu init failed; falling back to softbuffer"
                );
                init_softbuffer(window)
            }
        }
    } else {
        init_softbuffer(window)
    }
}

#[cfg(not(target_os = "macos"))]
fn init_present_backend(window: &Arc<Window>) -> Option<PresentBackend> {
    init_softbuffer(window)
}

fn init_softbuffer(window: &Arc<Window>) -> Option<PresentBackend> {
    let ctx = softbuffer::Context::new(window.clone()).ok()?;
    softbuffer::Surface::new(&ctx, window.clone())
        .ok()
        .map(PresentBackend::Softbuffer)
}

struct WieApp {
    handle: Option<GuestHandle>,
    /// Window-bound state; `Some` iff the winit window exists.
    runtime: Option<WindowRuntime>,
    /// B2: wake-coalescing flag, shared with the guest-thread wake callback.
    /// Set on every publish; the first `Frame` event after a publish group
    /// swaps it and requests a redraw, duplicates skip.
    pending_frame: Arc<std::sync::atomic::AtomicBool>,
    /// Currently pressed mouse buttons (MK_* bits) — from MouseInput events.
    mouse_buttons: u16,
    /// Last reported cursor position in client coords (x, y).
    cursor_pos: (f64, f64),
    /// Currently held modifier keys (shift/ctrl/alt) — from ModifiersChanged.
    modifiers: winit::keyboard::ModifiersState,
    /// macOS application menu bar mirroring the guest window's menu.
    #[cfg(target_os = "macos")]
    menu_bar: crate::gui::menu_bar::MacMenuBar,
    /// Menu items the menu bar was last rebuilt with (cheap change check).
    #[cfg(target_os = "macos")]
    last_menu_items: Vec<MenuNode>,
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
                    // B2: skip redundant presents. When neither the
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
                    let src_w = frame.width.max(1);
                    let src_h = frame.height.max(1);
                    // B9(d): time copy ③ (write_texture + present, or
                    // copy_from_slice / stretch_nearest + softbuffer present)
                    // — only when frame timing is on.
                    let present_t0 = if handle.frame_timing_enabled() {
                        Some(Instant::now())
                    } else {
                        None
                    };
                    if let Some(backend) = rt.surface.as_mut() {
                        match backend {
                            #[cfg(target_os = "macos")]
                            PresentBackend::Wgpu(presenter) => {
                                // P4a: wgpu path — no CPU copy; the blit pass
                                // nearest-scales via the sampler when the window
                                // size differs from the frame size (identical
                                // nearest semantics to stretch_nearest).
                                if let Err(e) = presenter.present(&frame, dst_w, dst_h) {
                                    tracing::error!(target: "wiegui", error = %e, "wgpu present failed");
                                }
                            }
                            PresentBackend::Softbuffer(surface) => {
                                let nzw = std::num::NonZeroU32::new(dst_w)
                                    .unwrap_or(std::num::NonZeroU32::MIN);
                                let nzh = std::num::NonZeroU32::new(dst_h)
                                    .unwrap_or(std::num::NonZeroU32::MIN);
                                let _ = surface.resize(nzw, nzh);
                                if let Ok(mut buf) = surface.buffer_mut() {
                                    if src_w == dst_w && src_h == dst_h {
                                        // Full copy only. softbuffer's AppKit
                                        // backend (cg.rs) allocates a fresh
                                        // zeroed buffer on every `buffer_mut()`
                                        // call — the previous frame's pixels
                                        // are never retained — so a
                                        // region-only copy would black out
                                        // everything outside the region
                                        // (visible as a black flash after
                                        // resize and as element-shaped bands
                                        // on partial control repaints). The
                                        // frame's `region` field remains for
                                        // guest-side bookkeeping and tests,
                                        // but the presented buffer is always
                                        // fully rewritten.
                                        let n = buf.len().min(frame.pixels.len());
                                        buf[..n].copy_from_slice(&frame.pixels[..n]);
                                    } else {
                                        stretch_nearest(
                                            &mut buf,
                                            &frame.pixels,
                                            src_w,
                                            src_h,
                                            dst_w,
                                            dst_h,
                                        );
                                    }
                                    let _ = buf.present();
                                }
                            }
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
                let msg = match button {
                    winit::event::MouseButton::Left => {
                        if pressed {
                            input::WM_LBUTTONDOWN
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
                let (target, rx, ry) = self.mouse_target(handle);
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
                let (target, rx, ry) = self.mouse_target(handle);
                let (px, py) = self.cursor_pos_i32();
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
                // P4a: keep the wgpu swapchain matching the window's physical
                // size. This is purely the host surface — the guest-visible
                // WM_SIZE bookkeeping below is untouched.
                #[cfg(target_os = "macos")]
                if let Some(PresentBackend::Wgpu(presenter)) = rt.surface.as_mut() {
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
        let Some(rt) = &mut self.runtime else {
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
                // B9(b): wake → event-loop latency.
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
                            self.runtime = Some(WindowRuntime {
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
                    // B2: coalesce wake storms. Every publish sets the flag;
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

/// Run the guest with a winit window.
pub fn run_gui_windowed(path: &std::path::Path) -> Result<()> {
    let event_loop = EventLoop::<WieEvent>::with_user_event()
        .build()
        .context("build event loop")?;
    let proxy = event_loop.create_proxy();
    // B2: wake-coalescing flag, shared between the guest-thread wake callback
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
                                // B2: mark the pending frame BEFORE sending so
                                // the first Frame event always does the work.
                                pending.store(true, std::sync::atomic::Ordering::SeqCst);
                                let _ = proxy.send_event(WieEvent::Frame {
                                    published_at: Instant::now(),
                                });
                            }));
                        }

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

    // Window is created lazily when the first frame arrives (in user_event).
    let mut app = WieApp {
        handle: Some(handle),
        runtime: None,
        pending_frame,
        mouse_buttons: 0,
        cursor_pos: (0.0, 0.0),
        modifiers: winit::keyboard::ModifiersState::default(),
        #[cfg(target_os = "macos")]
        menu_bar: crate::gui::menu_bar::MacMenuBar::new(proxy.clone()),
        #[cfg(target_os = "macos")]
        last_menu_items: Vec::new(),
    };

    event_loop
        .run_app(&mut app)
        .map_err(|e| anyhow::anyhow!("event loop: {e}"))
}
