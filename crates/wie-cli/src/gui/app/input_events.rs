//! winit `WindowEvent` → guest-message translation: the body of
//! `ApplicationHandler::window_event`, moved out of `app.rs` (file-size cap).
//!
//! Raw key/mouse mapping lives in [`crate::gui::input`]; this module owns the
//! guest-message construction (the WM_* posts) and the per-window present
//! (`RedrawRequested`) and resize (`Resized`/settle) accounting that share the
//! dispatch. The trait impl in `super` delegates here, so the event path is
//! behavior-identical to a single in-place handler.

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::WindowId;

use super::{RESIZE_SETTLE_MS, WHEEL_DELTA, WieApp};
use crate::gui::input;

// Whether a present actually drew its frame — the RedrawRequested arm records
// the frame as presented ONLY when it reached the screen (a skipped present
// must stay retryable).
use crate::gui::present_wgpu::PresentOutcome;

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

impl WieApp {
    /// The winit `WindowEvent` handler body — guest-message translation plus
    /// the present/resize dispatch, on the event-loop thread.
    pub(super) fn dispatch_window_event(
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
            WindowEvent::Occluded(occluded) => {
                let occluded = *occluded;
                if let Some(rt) = self.windows.get_mut(&window_id) {
                    rt.occluded = occluded;
                    if occluded {
                        // Fully covered: every surface acquire fails. The
                        // parked-frame re-arm is throttled to a few slow
                        // tries (`OCCLUDED_RETRY_MS`) rather than cleared —
                        // a frame published while the window was briefly
                        // covered (menu interaction) still gets its chance,
                        // and the bounded cadence cannot spin.
                    } else {
                        // Visible again: a frame may be parked in the
                        // presenter by the bounded-retry present — re-request
                        // the redraw so the parked frame draws, with a fresh
                        // slow-retry budget. winit emits this on macOS when
                        // the occlusion state clears, which is also how the
                        // un-occlusion acceptance test draws its parked frame.
                        rt.occluded_retries = 0;
                        rt.window.request_redraw();
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
                            // The frame reached the screen — the next present
                            // starts with a fresh retry budget.
                            rt.note_present_drawn();
                        }
                        PresentOutcome::NotDrawn { retry } => {
                            // The frame never reached the screen (occluded,
                            // out-of-date surface, or a transient acquire
                            // failure). Keep last_presented_* stale — the
                            // ptr_eq skip above would otherwise reject the
                            // retry of this same frame. One scheduling
                            // decision covers every case (see
                            // `WindowRuntime::schedule_present_retry`): the
                            // first skip retries immediately, repeat skips
                            // re-arm at a throttled cadence, and the frame
                            // parks once the bound is spent — so a single
                            // lost present (a modal dialog's first composite
                            // frame, the status-bar toggle) still reaches
                            // the screen, while a persistent skip (an
                            // occluded window) cannot spin the event loop.
                            if retry {
                                rt.schedule_present_retry();
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
                        ((x * WHEEL_DELTA) as i32, (y * WHEEL_DELTA) as i32)
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
}
