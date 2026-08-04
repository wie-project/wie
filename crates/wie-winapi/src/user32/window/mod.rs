//! Window lifecycle: creation/destruction, text, focus, show/enable state, and
//! the class-name registry (split from the former `window.rs`).
//!
//! Geometry and system-metrics helpers live in [`geom`]; capture, the
//! window/class long-pointer accessors and the window-lookup helpers live in
//! [`class`].

use super::{
    Context, CreateWindowRequest, FAKE_WINDOW_HANDLE, GuestCallbackRequest, HandlerContext, Result,
    WM_CREATE, WM_DESTROY, WM_KILLFOCUS, WM_PAINT, WM_SETFOCUS, WinApiControlSignal,
    WinApiHandlerResult, WinApiState, create_window_record, dispatch_control_proc, is_known_window,
    read_guest_ansi_lossy, read_guest_i32, read_guest_u32, read_guest_u64, read_guest_utf16_lossy,
    read_window_class_identifier_a, read_window_class_identifier_w, write_ansi_window_text,
    write_guest_ansi_c_string, write_guest_i32, write_guest_u32, write_guest_u64,
    write_guest_utf16_c_string, write_wide_window_text, write_window_rect,
};
use crate::OuterReturn;
use crate::state::WindowFlags;

mod class;
mod geom;

pub(crate) use class::{find_window, find_window_mut};
pub use class::{
    handle_get_capture, handle_get_class_long_ptr_a, handle_get_class_long_ptr_w,
    handle_get_window_long_ptr_a, handle_get_window_long_ptr_w, handle_release_capture,
    handle_set_capture, handle_set_class_long_ptr_a, handle_set_class_long_ptr_w,
    handle_set_window_long_ptr_a, handle_set_window_long_ptr_w,
};
// Test-only: the placement tests assert against the same 44-byte struct size
// the handlers write/validate. The lib build does not reference it through
// this path (geom.rs uses the const directly), so gate the re-export.
#[cfg(test)]
pub(crate) use geom::WINDOWPLACEMENT_LENGTH;
pub(crate) use geom::sys_color;
pub use geom::{
    handle_adjust_window_rect_ex, handle_client_to_screen, handle_get_client_rect,
    handle_get_desktop_window, handle_get_dlg_ctrl_id, handle_get_sys_color,
    handle_get_sys_color_brush, handle_get_window, handle_get_window_placement,
    handle_get_window_thread_process_id, handle_is_child, handle_is_iconic, handle_is_zoomed,
    handle_move_window, handle_screen_to_client, handle_scroll_window_ex, handle_set_rect,
    handle_set_window_placement,
};

/// Handles `USER32.dll!GetWindowRect`.
pub fn handle_get_window_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowRect")?;

    let rect_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowRect")?;

    let window = find_window(state, window_handle);
    let success = window.is_some() && rect_ptr != 0;

    if let Some(window) = window.filter(|_| rect_ptr != 0) {
        let right = window
            .x
            .checked_add(window.width)
            .context("GetWindowRect right coordinate overflow")?;

        let bottom = window
            .y
            .checked_add(window.height)
            .context("GetWindowRect bottom coordinate overflow")?;

        write_window_rect(engine, rect_ptr, window.x, window.y, right, bottom)
            .context("failed to write GetWindowRect RECT")?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindowRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles dynamic `USER32.dll!GetDpiForWindow`.
pub fn handle_get_dpi_for_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetDpiForWindow")?;

    // Standard 100% Windows DPI.
    let return_value = 96;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetDpiForWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles dynamic `USER32.dll!AdjustWindowRectExForDpi`.
pub fn handle_adjust_window_rect_ex_for_dpi(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for AdjustWindowRectExForDpi")?;

    let _style = engine
        .read_rdx()
        .context("failed to read RDX for AdjustWindowRectExForDpi")?;

    let _has_menu = engine
        .read_r8()
        .context("failed to read R8 for AdjustWindowRectExForDpi")?;

    let _extended_style = engine
        .read_r9()
        .context("failed to read R9 for AdjustWindowRectExForDpi")?;

    // The fifth argument, dpi, is on the Win64 stack. For now the fake desktop
    // uses 96 DPI, so preserving the supplied client rectangle is sufficient.
    let return_value = u64::from(rect_ptr != 0);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from AdjustWindowRectExForDpi")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetWindowPos`.
///
/// Only the z-order (`hWndInsertAfter`) is tracked here: HWND_TOP /
/// HWND_BOTTOM reorder the guest's top-level list (the host presenter
/// mirrors that list into its NSWindows). The geometry (x/y/cx/cy) moves are
/// handled by `SetWindowPlacement` / `MoveWindow`, so they are ignored —
/// matching the pre-lane stub, which accepted the placement without
/// maintaining a full window manager.
pub fn handle_set_window_pos(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowPos")?;
    let insert_after = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowPos")?;
    let _x = engine
        .read_r8()
        .context("failed to read R8 for SetWindowPos")?;
    let _y = engine
        .read_r9()
        .context("failed to read R9 for SetWindowPos")?;

    // Remaining Win64 arguments are cx, cy and flags on the stack.
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for SetWindowPos")?;
    let flags_address = rsp
        .checked_add(0x38)
        .context("SetWindowPos flags address overflow")?;
    let flags =
        read_guest_u32(engine, flags_address).context("failed to read SetWindowPos flags")?;

    // SWP_NOZORDER (0x0004): the caller explicitly leaves the z-order alone.
    if flags & SWP_NOZORDER == 0 {
        let hwnd = crate::handles::Hwnd::from(window_handle);
        // hWndInsertAfter read as its SIGNED sentinel values: HWND_TOP = 0,
        // HWND_BOTTOM = 1, HWND_TOPMOST = -1, HWND_NOTOPMOST = -2. Any other
        // value is a window handle (place the window below that window) —
        // not tracked here, keeping the minimal z-order model.
        match insert_after {
            0 | HWND_TOPMOST => {
                state.present().z_order_to_top(hwnd);
            }
            1 | HWND_NOTOPMOST => {
                state.present().z_order_to_bottom(hwnd);
            }
            _ => {}
        }
    }

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from SetWindowPos")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// `SetWindowPos` flag bit that leaves the z-order untouched.
const SWP_NOZORDER: u32 = 0x0004;

/// `hWndInsertAfter` sentinel: place the window above all non-topmost
/// windows (`(HWND)-1` as u64).
const HWND_TOPMOST: u64 = 0xFFFF_FFFF_FFFF_FFFF;

/// `hWndInsertAfter` sentinel: remove the topmost style (`(HWND)-2` as u64).
const HWND_NOTOPMOST: u64 = 0xFFFF_FFFF_FFFF_FFFE;

/// Handles `USER32.dll!IsWindow`.
pub fn handle_is_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsWindow")?;

    // Any runtime-known window is a valid window — including child controls,
    // not just the legacy fake top-level handle.
    let return_value = u64::from(is_known_window(state, window_handle));

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!IsWindowVisible`.
pub fn handle_is_window_visible(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsWindowVisible")?;

    let return_value = u64::from(if window_handle == FAKE_WINDOW_HANDLE {
        state.window_state().window_visible
    } else {
        find_window(state, window_handle).is_some_and(|window| window.visible)
    });

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsWindowVisible")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!IsWindowEnabled`.
pub fn handle_is_window_enabled(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsWindowEnabled")?;

    let return_value = u64::from(if window_handle == FAKE_WINDOW_HANDLE {
        state.window_state().window_enabled
    } else {
        find_window(state, window_handle)
            .is_some_and(|window| window.flags.contains(WindowFlags::ENABLED))
    });

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsWindowEnabled")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetParent`.
pub fn handle_get_parent(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetParent")?;

    let return_value =
        find_window(state, window_handle).map_or(0, |window| window.parent_handle.as_u64());

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetParent")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetActiveWindow`.
pub fn handle_get_active_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().active_window_handle.as_u64();

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetActiveWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetForegroundWindow`.
pub fn handle_get_foreground_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().foreground_window_handle.as_u64();

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetForegroundWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!ShowWindow`.
pub fn handle_show_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for ShowWindow")?;

    let show_command = engine
        .read_rdx()
        .context("failed to read RDX for ShowWindow")?;

    let previously_visible = state.window_state().window_visible;

    if window_handle == FAKE_WINDOW_HANDLE {
        // SW_HIDE is zero. Other commands make the window visible in the
        // current single-window model.
        state.window_state().window_visible = show_command != 0;
    } else if let Some(window) = find_window_mut(state, window_handle) {
        // Real window records track their own visibility (used by the host
        // mouse hit-test in `GuestHandle::window_at`).
        window.visible = show_command != 0;
        // Showing a window invalidates it with erase (real Windows): the
        // first paint cycle fills the client with the class-brush background
        // before the guest paints. Without this the owner surface stays
        // zeroed (black) until something else triggers an erase — a modal
        // dialog close, which then visibly "changes the background color".
        if show_command != 0 {
            window.invalidated = true;
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
    }

    let return_value = u64::from(previously_visible);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ShowWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!EnableWindow`.
pub fn handle_enable_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for EnableWindow")?;

    let enable_raw = engine
        .read_rdx()
        .context("failed to read RDX for EnableWindow")?;

    let previously_disabled = !state.window_state().window_enabled;

    if window_handle == FAKE_WINDOW_HANDLE {
        state.window_state().window_enabled = enable_raw != 0;
    }

    // EnableWindow returns nonzero when the window was previously disabled.
    let return_value = u64::from(previously_disabled);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from EnableWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetForegroundWindow`.
pub fn handle_set_foreground_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetForegroundWindow")?;

    let success = window_handle == FAKE_WINDOW_HANDLE;

    if success {
        state.window_state().foreground_window_handle = crate::handles::Hwnd::from(window_handle);
        state.window_state().active_window_handle = crate::handles::Hwnd::from(window_handle);
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetForegroundWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetActiveWindow`.
pub fn handle_set_active_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetActiveWindow")?;

    let previous_window = state.window_state().active_window_handle;

    if window_handle == 0 || window_handle == FAKE_WINDOW_HANDLE {
        state.window_state().active_window_handle = crate::handles::Hwnd::from(window_handle);
    }

    let return_address = engine
        .return_from_win64_api(previous_window.as_u64())
        .context("failed to return from SetActiveWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window.as_u64(),
    })
}
/// Handles `USER32.dll!SetFocus`.
pub fn handle_set_focus(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetFocus")?;

    let previous_window = state.window_state().focus_window_handle;
    let accepted = window_handle == 0 || is_known_window(state, window_handle);

    if accepted {
        state.window_state().focus_window_handle = crate::handles::Hwnd::from(window_handle);
    }

    // Windows sends WM_KILLFOCUS(old, new) then WM_SETFOCUS(new, old) when
    // the focus actually moves. Bridge synchronously (guest WndProcs) or
    // host-side (controls) — see deliver_focus_change.
    if accepted
        && crate::handles::Hwnd::from(window_handle) != previous_window
        && let Some(signal) = deliver_focus_change(
            state,
            engine,
            previous_window.as_u64(),
            window_handle,
            OuterReturn::Fixed(previous_window.as_u64()),
        )?
    {
        return Err(signal.into());
    }

    let return_address = engine
        .return_from_win64_api(previous_window.as_u64())
        .context("failed to return from SetFocus")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window.as_u64(),
    })
}

/// Deliver the focus-change message pair (`WM_KILLFOCUS` to `old_focus`, then
/// `WM_SETFOCUS` to `new_focus`) when the focus actually moved.
///
/// Control targets (host-side WndProc) are dispatched directly; a target with
/// a guest WndProc or dialog proc must be bridged through the
/// `GuestCallbackRequested` mechanism. The bridge is one-shot per API stop, so
/// at most one of the two is bridged — the other is dispatched host-side or
/// posted to the guest's queue (correct order, delivered before any input).
///
/// Returns the bridge signal when one is required (the caller returns it as
/// `Err(..)`); `Ok(None)` means both were delivered and the caller completes
/// normally.
pub(crate) fn deliver_focus_change(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    old_focus: u64,
    new_focus: u64,
    outer_return: OuterReturn,
) -> Result<Option<WinApiControlSignal>> {
    if old_focus == new_focus {
        return Ok(None);
    }

    // WM_KILLFOCUS first (Windows order), then WM_SETFOCUS. KILLFOCUS gets the
    // bridge slot when it needs one; a guest-WndProc SETFOCUS is then queued.
    let mut bridged = None;
    for (hwnd, message, wparam) in [
        (old_focus, WM_KILLFOCUS, new_focus),
        (new_focus, WM_SETFOCUS, old_focus),
    ] {
        if hwnd == 0 || !is_known_window(state, hwnd) {
            continue;
        }
        let (window_proc, dialog_proc, dialog_unicode, unicode, is_control) = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .map_or((0, 0, false, false, false), |w| {
                (
                    w.window_proc,
                    w.dialog_proc,
                    w.dialog_unicode,
                    w.unicode,
                    w.control_kind.is_some(),
                )
            });
        if is_control {
            // A guest-subclassed control (GWLP_WNDPROC replaced by the guest)
            // must see the focus message through its subclass proc, exactly
            // like a guest WndProc window — the subclass forwards it through
            // CallWindowProcW back into the host default. Plain controls
            // update their focus state host-side.
            let subclass = super::get_window_long_ptr_value(
                hwnd,
                super::GWLP_WNDPROC_RAW,
                state,
                "deliver_focus_change",
            )?;
            if subclass == 0 {
                let _ = dispatch_control_proc(engine, state, hwnd, message, wparam, 0)?;
                continue;
            }
            let request = GuestCallbackRequest {
                callback_address: subclass,
                window_handle: hwnd,
                message,
                word_parameter: wparam,
                long_parameter: 0,
                unicode,
                outer_return,
            };
            if bridged.is_none() {
                bridged = Some(WinApiControlSignal::GuestCallbackRequested { request });
            } else {
                // The bridge is one-shot: post the second message so it
                // arrives after the bridged one (queue order).
                let mut queue = state.lock_message_queue();
                let time = queue.next_message_time;
                queue.next_message_time = time
                    .checked_add(1)
                    .context("focus-change message timestamp overflow")?;
                queue.messages.push(super::QueuedWindowMessage {
                    window_handle: crate::handles::Hwnd::from(hwnd),
                    message,
                    word_parameter: wparam,
                    long_parameter: 0,
                    time,
                    point_x: 0,
                    point_y: 0,
                });
            }
            continue;
        }
        if window_proc == 0 && dialog_proc == 0 {
            continue;
        }
        let callback_address = if window_proc != 0 {
            window_proc
        } else {
            dialog_proc
        };
        let callback_unicode = if window_proc != 0 {
            unicode
        } else {
            dialog_unicode
        };
        let request = GuestCallbackRequest {
            callback_address,
            window_handle: hwnd,
            message,
            word_parameter: wparam,
            long_parameter: 0,
            unicode: callback_unicode,
            outer_return,
        };
        if bridged.is_none() {
            bridged = Some(WinApiControlSignal::GuestCallbackRequested { request });
        } else {
            // The bridge is one-shot: post the second message so it arrives
            // after the bridged one (queue order).
            let mut queue = state.lock_message_queue();
            let time = queue.next_message_time;
            queue.next_message_time = time
                .checked_add(1)
                .context("focus-change message timestamp overflow")?;
            queue.messages.push(super::QueuedWindowMessage {
                window_handle: crate::handles::Hwnd::from(hwnd),
                message,
                word_parameter: wparam,
                long_parameter: 0,
                time,
                point_x: 0,
                point_y: 0,
            });
        }
    }
    Ok(bridged)
}
/// Handles `USER32.dll!GetFocus`.
pub fn handle_get_focus(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().focus_window_handle.as_u64();

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFocus")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!UpdateWindow`.
pub fn handle_update_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for UpdateWindow")?;

    let (window_proc, unicode) = {
        let windows = &state.window_state().windows;
        windows
            .iter()
            .find(|window| window.handle == crate::handles::Hwnd::from(window_handle))
            .map_or((0, false), |window| (window.window_proc, window.unicode))
    };

    if let Some(window) = find_window_mut(state, window_handle)
        && window.invalidated
    {
        window.invalidated = false;

        // UpdateWindow paints synchronously: erase the background first when
        // the invalidation requested it and a class brush exists (the
        // message-loop path synthesizes WM_ERASEBKGND ahead of WM_PAINT; here
        // the erase runs inline, DefWindowProc semantics).
        if window.flags.contains(WindowFlags::ERASE_BACKGROUND) {
            super::message::erase_window_background(state, window_handle);
        }

        // Synchronous WM_PAINT: the runtime bridges into the guest WndProc
        // and completes UpdateWindow with a fixed TRUE once it returns.
        if window_proc != 0 {
            return Err(WinApiControlSignal::GuestCallbackRequested {
                request: GuestCallbackRequest {
                    callback_address: window_proc,
                    window_handle,
                    message: WM_PAINT,
                    word_parameter: 0,
                    long_parameter: 0,
                    unicode,
                    outer_return: OuterReturn::Fixed(1),
                },
            }
            .into());
        }
    }

    // No update region or no guest WndProc: succeed without painting.
    let return_value = 1;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from UpdateWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!InvalidateRect`.
pub fn handle_invalidate_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for InvalidateRect")?;

    let erase_background = engine
        .read_r8()
        .context("failed to read R8 for InvalidateRect")?;

    let success = window_handle == 0 || is_known_window(state, window_handle);

    if let Some(window) = find_window_mut(state, window_handle) {
        window.invalidated = true;
        // bErase: OR so a TRUE erase request survives a later FALSE invalidation
        // of a different region (the update region accumulates, matching Windows).
        if erase_background != 0 {
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from InvalidateRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!RedrawWindow`.
pub fn handle_redraw_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for RedrawWindow")?;

    let _update_rect_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RedrawWindow")?;

    let _update_region = engine
        .read_r8()
        .context("failed to read R8 for RedrawWindow")?;

    let _flags = engine
        .read_r9()
        .context("failed to read R9 for RedrawWindow")?;

    let success = window_handle == 0 || is_known_window(state, window_handle);

    if let Some(window) = find_window_mut(state, window_handle) {
        window.invalidated = false;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from RedrawWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetWindowTextA`.
pub fn handle_set_window_text_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowTextA")?;

    let text_ptr = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowTextA")?;

    let known = window_handle == FAKE_WINDOW_HANDLE || is_known_window(state, window_handle);
    let success = known && text_ptr != 0;

    if success {
        let text = read_guest_ansi_lossy(engine, text_ptr, 32_768)
            .context("failed to read SetWindowTextA text")?;
        if window_handle == FAKE_WINDOW_HANDLE {
            state.window_state().window_title = text;
        } else if let Some(window) = find_window_mut(state, window_handle) {
            // Controls repaint with their new caption; other windows get the
            // title updated.
            if window.control_kind.is_some() {
                // A label control's old caption must survive the replacement:
                // the text-change invalidation measures both captions so the
                // next paint erases the previous glyphs (same as the WM_SETTEXT
                // dispatch arm).
                let old_text = if matches!(
                    window.control_kind,
                    Some(crate::user32::controls::ControlClassKind::Button)
                        | Some(crate::user32::controls::ControlClassKind::Static)
                ) {
                    window.control_text.clone()
                } else {
                    String::new()
                };
                let kind = window.control_kind;
                window.control_text = text.clone();
                window.invalidated = true;
                // Real Windows clears an EDIT's undo buffer when the program
                // sets the text — WM_UNDO must not revert past it (the
                // WM_SETTEXT dispatch arm does the same).
                crate::user32::controls::edit_clear_undo_buffer(state, window_handle);
                if matches!(
                    kind,
                    Some(crate::user32::controls::ControlClassKind::Button)
                        | Some(crate::user32::controls::ControlClassKind::Static)
                ) {
                    crate::user32::controls::label_invalidate_text_change(
                        state,
                        window_handle,
                        &old_text,
                        &text,
                    );
                }
            } else {
                window.title = text;
            }
        }
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetWindowTextA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetWindowTextW`.
pub fn handle_set_window_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowTextW")?;

    let text_ptr = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowTextW")?;

    let known = window_handle == FAKE_WINDOW_HANDLE || is_known_window(state, window_handle);
    let success = known && text_ptr != 0;

    if success {
        let text = read_guest_utf16_lossy(engine, text_ptr, 32_768)
            .context("failed to read SetWindowTextW text")?;
        if window_handle == FAKE_WINDOW_HANDLE {
            state.window_state().window_title = text;
        } else if let Some(window) = find_window_mut(state, window_handle) {
            if window.control_kind.is_some() {
                // A label control's old caption must survive the replacement
                // (see the ANSI variant above).
                let old_text = if matches!(
                    window.control_kind,
                    Some(crate::user32::controls::ControlClassKind::Button)
                        | Some(crate::user32::controls::ControlClassKind::Static)
                ) {
                    window.control_text.clone()
                } else {
                    String::new()
                };
                let kind = window.control_kind;
                window.control_text = text.clone();
                window.invalidated = true;
                // SetWindowText clears an EDIT's undo buffer (see the ANSI
                // variant above).
                crate::user32::controls::edit_clear_undo_buffer(state, window_handle);
                if matches!(
                    kind,
                    Some(crate::user32::controls::ControlClassKind::Button)
                        | Some(crate::user32::controls::ControlClassKind::Static)
                ) {
                    crate::user32::controls::label_invalidate_text_change(
                        state,
                        window_handle,
                        &old_text,
                        &text,
                    );
                }
            } else {
                window.title = text;
            }
        }
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetWindowTextW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetWindowTextA`.
pub fn handle_get_window_text_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowTextA")?;

    let buffer_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowTextA")?;

    let max_characters = engine
        .read_r8()
        .context("failed to read R8 for GetWindowTextA")?;

    let text = resolve_window_text(state, window_handle);

    let return_value = if text.is_empty() {
        0
    } else {
        write_ansi_window_text(engine, buffer_ptr, max_characters, &text)?
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindowTextA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetWindowTextW`.
pub fn handle_get_window_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowTextW")?;

    let buffer_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowTextW")?;

    let max_characters = engine
        .read_r8()
        .context("failed to read R8 for GetWindowTextW")?;

    let text = resolve_window_text(state, window_handle);

    let return_value = if text.is_empty() {
        0
    } else {
        write_wide_window_text(engine, buffer_ptr, max_characters, &text)?
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindowTextW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `USER32.dll!GetWindowTextLengthW`.
pub fn handle_get_window_text_length_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowTextLengthW")?;

    let text = resolve_window_text(state, window_handle);

    // Count of UTF-16 units — exactly what GetWindowTextW would copy,
    // excluding the terminating NUL (mirrors write_guest_utf16_c_string's
    // length semantics). Empty/unknown text resolves to "" → 0.
    let length = u64::try_from(text.encode_utf16().count())
        .context("window text length does not fit u64")?;

    let return_address = engine
        .return_from_win64_api(length)
        .context("failed to return from GetWindowTextLengthW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: length,
    })
}

/// Handles `USER32.dll!GetWindowTextLengthA`.
pub fn handle_get_window_text_length_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowTextLengthA")?;

    let text = resolve_window_text(state, window_handle);

    // Count of CP1252 characters — what Windows GetWindowTextLengthA reports
    // (the ACP is 1252, one byte per char). `encode_acp` maps each char to a
    // single CP1252 byte (unmappable chars fall back to '?', matching the
    // A write path), so the encoded length is the ANSI character count
    // (mirrors write_guest_ansi_c_string's length semantics).
    // Empty/unknown text resolves to "" → 0.
    let length = u64::try_from(crate::vfs::encode_acp(&text).len())
        .context("window text length does not fit u64")?;

    let return_address = engine
        .return_from_win64_api(length)
        .context("failed to return from GetWindowTextLengthA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: length,
    })
}

/// The text GetWindowTextA/W reports for a window: the control buffer for
/// built-in controls, the title otherwise (the legacy fake window's title
/// lives in `WindowState.window_title`).
fn resolve_window_text(state: &mut WinApiState, window_handle: u64) -> String {
    if window_handle == FAKE_WINDOW_HANDLE {
        return state.window_state().window_title.clone();
    }
    find_window(state, window_handle).map_or_else(String::new, |window| {
        if window.control_kind.is_some() {
            window.control_text.clone()
        } else {
            window.title.clone()
        }
    })
}

/// WM_SETFONT (DefWindowProc semantics): store `font_handle` on the window so
/// the paint paths draw its text with the guest-selected font, and mark the
/// window invalidated when `redraw` is non-zero (the next repaint cycle then
/// re-renders with the new font). Real Windows sends WM_ERASEBKGND before that
/// repaint, so a redraw also requests the pending erase — the same idiom
/// SetWindowPlacement and the resize path use. Callers return the WM_SETFONT
/// result (0).
///
/// Shared by the control dispatch (`dispatch_control_proc`), `DefWindowProc`,
/// and the no-WndProc `SendMessage` fallthrough so ANY window — control or
/// not — stores the font it is told to use.
pub(crate) fn set_window_font(state: &mut WinApiState, hwnd: u64, font_handle: u64, redraw: u64) {
    if let Some(window) = find_window_mut(state, hwnd) {
        window.font_handle = crate::handles::Hfont::from(font_handle);
        if redraw != 0 {
            window.invalidated = true;
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
    }
}

/// WM_GETFONT (DefWindowProc semantics): the HFONT stored on the window, or 0
/// when never set (or the window is unknown).
#[must_use]
pub(crate) fn window_font(state: &mut WinApiState, hwnd: u64) -> u64 {
    find_window(state, hwnd).map_or(0, |window| window.font_handle.as_u64())
}

/// Handles `USER32.dll!CreateWindowExA`.
pub fn handle_create_window_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    // Read 4 register args
    let ex_style = engine
        .read_rcx()
        .context("failed to read RCX for CreateWindowExA")?;
    let class_value = engine
        .read_rdx()
        .context("failed to read RDX for CreateWindowExA")?;
    let window_title = engine
        .read_r8()
        .context("failed to read R8 for CreateWindowExA")?;
    let style_raw = engine
        .read_r9()
        .context("failed to read R9 for CreateWindowExA")?;

    // Read 8 stack args
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for CreateWindowExA")?;

    let stack_arg = |offset: u64, name: &str| -> Result<u64> {
        rsp.checked_add(offset)
            .with_context(|| format!("CreateWindowExA: {name} address overflow"))
    };

    let x_raw = read_guest_i32(engine, stack_arg(0x28, "X")?)?;
    let y_raw = read_guest_i32(engine, stack_arg(0x30, "Y")?)?;
    let width_raw = read_guest_i32(engine, stack_arg(0x38, "nWidth")?)?;
    let height_raw = read_guest_i32(engine, stack_arg(0x40, "nHeight")?)?;
    let parent_handle = read_guest_u64(engine, stack_arg(0x48, "hWndParent")?)?;
    let menu_handle = read_guest_u64(engine, stack_arg(0x50, "hMenu")?)?;
    let instance_handle = read_guest_u64(engine, stack_arg(0x58, "hInstance")?)?;
    let create_params = read_guest_u64(engine, stack_arg(0x60, "lpParam")?)?;

    // Read window title if present
    let title = if window_title == 0 {
        String::new()
    } else {
        read_guest_ansi_lossy(engine, window_title, 512)
            .context("failed to read CreateWindowExA window title")?
    };

    // Convert style/ex_style to u32 once (avoids repeated `as` conversions).
    let style = u32::try_from(style_raw).context("CreateWindowExA: style does not fit u32")?;
    let ex_style = u32::try_from(ex_style).context("CreateWindowExA: ex_style does not fit u32")?;

    // Handle CW_USEDEFAULT (0x8000_0000 stored as i32 = i32::MIN on the stack).
    // A CHILD window with CW_USEDEFAULT x/y is placed at (0,0) of the parent's
    // client area; only a top-level window gets the cascaded (100,100).
    let x = if x_raw == i32::MIN {
        if parent_handle != 0 { 0 } else { 100 }
    } else {
        x_raw
    };
    let y = if y_raw == i32::MIN {
        if parent_handle != 0 { 0 } else { 100 }
    } else {
        y_raw
    };
    let width = if width_raw == i32::MIN {
        640
    } else {
        width_raw
    };
    let height = if height_raw == i32::MIN {
        480
    } else {
        height_raw
    };

    let class_identifier = read_window_class_identifier_a(engine, class_value)
        .context("failed to read window class identifier for CreateWindowExA")?;

    let (hwnd, window_proc, class_unicode) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier,
            title,
            style,
            extended_style: ex_style,
            parent_handle,
            menu_handle,
            instance_handle,
            x,
            y,
            width,
            height,
        },
        false,
    )
    .context("failed to create window record for CreateWindowExA")?;

    if hwnd == 0 {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from CreateWindowExA")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    }

    // A new top-level window (no parent) enters the host-visible window set
    // and the top of the guest z-order. The revision fingerprint makes the
    // presenter's Frame handler reconcile exactly on this create — and on no
    // other frame (the reconcile-on-change latch). Children composite into
    // their parent's surface and own no host window, so only parentless
    // windows register.
    if parent_handle == 0 {
        state
            .present()
            .register_top_level(crate::handles::Hwnd::from(hwnd));
    }

    // Update the window record with client rect
    if let Some(window) = find_window_mut(state, hwnd) {
        window.client_rect = (0, 0, width, height);
    }

    // If there is a window procedure, send WM_CREATE via guest callback
    if window_proc != 0 {
        // Allocate CREATESTRUCT in guest memory (0x50 bytes)
        let cs_va = state.heap_state.heap.alloc_coherent(engine, 0x50);
        if cs_va == 0 {
            // Allocation failed — return HWND without WM_CREATE
            let ra = engine
                .return_from_win64_api(hwnd)
                .context("failed to return from CreateWindowExA")?;
            return Ok(WinApiHandlerResult {
                return_address: ra,
                return_value: hwnd,
            });
        }

        // CREATESTRUCT on Win64 layout:
        //  +0x00 lpCreateParams (u64)
        //  +0x08 hInstance (u64)
        //  +0x10 hMenu (u64)
        //  +0x18 hwndParent (u64)
        //  +0x20 cy (i32)
        //  +0x24 cx (i32)
        //  +0x28 y (i32)
        //  +0x2C x (i32)
        //  +0x30 style (u32)
        //  +0x38 lpszName (u64)
        //  +0x40 lpszClass (u64)
        //  +0x48 dwExStyle (u32)

        write_guest_u64(engine, cs_va.wrapping_add(0x00), create_params)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x08), instance_handle)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x10), menu_handle)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x18), parent_handle)?;
        write_guest_i32(engine, cs_va.wrapping_add(0x20), height)?;
        write_guest_i32(engine, cs_va.wrapping_add(0x24), width)?;
        write_guest_i32(engine, cs_va.wrapping_add(0x28), y)?;
        write_guest_i32(engine, cs_va.wrapping_add(0x2C), x)?;
        write_guest_u32(engine, cs_va.wrapping_add(0x30), style)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x38), window_title)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x40), class_value)?;
        write_guest_u32(engine, cs_va.wrapping_add(0x48), ex_style)?;

        return Err(WinApiControlSignal::GuestCallbackRequested {
            request: GuestCallbackRequest {
                callback_address: window_proc,
                window_handle: hwnd,
                message: WM_CREATE,
                word_parameter: 0,
                long_parameter: cs_va,
                unicode: class_unicode,
                outer_return: OuterReturn::CreateWindow(hwnd),
            },
        }
        .into());
    }

    // No window procedure — return the HWND directly
    let ra = engine
        .return_from_win64_api(hwnd)
        .context("failed to return from CreateWindowExA")?;
    Ok(WinApiHandlerResult {
        return_address: ra,
        return_value: hwnd,
    })
}

/// Handles `USER32.dll!CreateWindowExW`.
pub fn handle_create_window_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    // Read 4 register args
    let ex_style = engine
        .read_rcx()
        .context("failed to read RCX for CreateWindowExW")?;
    let class_value = engine
        .read_rdx()
        .context("failed to read RDX for CreateWindowExW")?;
    let window_title = engine
        .read_r8()
        .context("failed to read R8 for CreateWindowExW")?;
    let style_raw = engine
        .read_r9()
        .context("failed to read R9 for CreateWindowExW")?;

    // Read 8 stack args
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for CreateWindowExW")?;

    let stack_arg = |offset: u64, name: &str| -> Result<u64> {
        rsp.checked_add(offset)
            .with_context(|| format!("CreateWindowExW: {name} address overflow"))
    };

    let x_raw = read_guest_i32(engine, stack_arg(0x28, "X")?)?;
    let y_raw = read_guest_i32(engine, stack_arg(0x30, "Y")?)?;
    let width_raw = read_guest_i32(engine, stack_arg(0x38, "nWidth")?)?;
    let height_raw = read_guest_i32(engine, stack_arg(0x40, "nHeight")?)?;
    let parent_handle = read_guest_u64(engine, stack_arg(0x48, "hWndParent")?)?;
    let menu_handle = read_guest_u64(engine, stack_arg(0x50, "hMenu")?)?;
    let instance_handle = read_guest_u64(engine, stack_arg(0x58, "hInstance")?)?;
    let create_params = read_guest_u64(engine, stack_arg(0x60, "lpParam")?)?;

    // Read window title if present (UTF-16 for the W variant)
    let title = if window_title == 0 {
        String::new()
    } else {
        read_guest_utf16_lossy(engine, window_title, 512)
            .context("failed to read CreateWindowExW window title")?
    };

    // Handle CW_USEDEFAULT (0x8000_0000) — stored as a 32-bit DWORD in the
    // stack slot, so it reads back as i32::MIN through read_guest_i32.
    // A CHILD window with CW_USEDEFAULT x/y is placed at (0,0) of the
    // parent's client area; only a top-level window gets the cascaded
    // (100,100).
    let x = if x_raw == i32::MIN {
        if parent_handle != 0 { 0 } else { 100 }
    } else {
        x_raw
    };
    let y = if y_raw == i32::MIN {
        if parent_handle != 0 { 0 } else { 100 }
    } else {
        y_raw
    };
    let width = if width_raw == i32::MIN {
        640
    } else {
        width_raw
    };
    let height = if height_raw == i32::MIN {
        480
    } else {
        height_raw
    };

    // Convert style/ex_style to u32 once (avoids repeated `as` conversions).
    let style = u32::try_from(style_raw).context("CreateWindowExW: style does not fit u32")?;
    let ex_style = u32::try_from(ex_style).context("CreateWindowExW: ex_style does not fit u32")?;

    let class_identifier = read_window_class_identifier_w(engine, class_value)
        .context("failed to read window class identifier for CreateWindowExW")?;

    let (hwnd, window_proc, class_unicode) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier,
            title,
            style,
            extended_style: ex_style,
            parent_handle,
            menu_handle,
            instance_handle,
            x,
            y,
            width,
            height,
        },
        true,
    )
    .context("failed to create window record for CreateWindowExW")?;

    if hwnd == 0 {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from CreateWindowExW")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    }

    // A new top-level window registers with the host-visible window set and
    // the z-order (see the ANSI variant for the full rationale).
    if parent_handle == 0 {
        state
            .present()
            .register_top_level(crate::handles::Hwnd::from(hwnd));
    }

    // Update the window record with client rect
    if let Some(window) = find_window_mut(state, hwnd) {
        window.client_rect = (0, 0, width, height);
    }

    // If there is a window procedure, send WM_CREATE via guest callback
    if window_proc != 0 {
        // Allocate CREATESTRUCT in guest memory (0x50 bytes)
        let cs_va = state.heap_state.heap.alloc_coherent(engine, 0x50);
        if cs_va == 0 {
            let ra = engine
                .return_from_win64_api(hwnd)
                .context("failed to return from CreateWindowExW")?;
            return Ok(WinApiHandlerResult {
                return_address: ra,
                return_value: hwnd,
            });
        }

        // CREATESTRUCT on Win64 layout (same as CreateWindowExA)
        write_guest_u64(engine, cs_va.wrapping_add(0x00), create_params)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x08), instance_handle)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x10), menu_handle)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x18), parent_handle)?;
        write_guest_i32(engine, cs_va.wrapping_add(0x20), height)?;
        write_guest_i32(engine, cs_va.wrapping_add(0x24), width)?;
        write_guest_i32(engine, cs_va.wrapping_add(0x28), y)?;
        write_guest_i32(engine, cs_va.wrapping_add(0x2C), x)?;
        write_guest_u32(engine, cs_va.wrapping_add(0x30), style)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x38), window_title)?;
        write_guest_u64(engine, cs_va.wrapping_add(0x40), class_value)?;
        write_guest_u32(engine, cs_va.wrapping_add(0x48), ex_style)?;

        return Err(WinApiControlSignal::GuestCallbackRequested {
            request: GuestCallbackRequest {
                callback_address: window_proc,
                window_handle: hwnd,
                message: WM_CREATE,
                word_parameter: 0,
                long_parameter: cs_va,
                unicode: class_unicode,
                outer_return: OuterReturn::CreateWindow(hwnd),
            },
        }
        .into());
    }

    // No window procedure — return the HWND directly
    let ra = engine
        .return_from_win64_api(hwnd)
        .context("failed to return from CreateWindowExW")?;
    Ok(WinApiHandlerResult {
        return_address: ra,
        return_value: hwnd,
    })
}

/// Handles `USER32.dll!DestroyWindow`.
pub fn handle_destroy_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for DestroyWindow")?;

    // Extract window info before any mutation.
    let window_info =
        find_window(state, window_handle).map(|w| (w.window_proc, w.unicode, w.parent_handle));

    let Some((window_proc, unicode, parent_handle)) = window_info else {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from DestroyWindow")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    };

    // A destroyed top-level window publishes no frame, so the host window
    // registry would never wake to drop the stale winit window — request the
    // sync explicitly (the same wake the publish path fires). Children live
    // inside their parent's surface and have no host window of their own.
    if parent_handle == crate::handles::Hwnd::NULL {
        state.present().request_host_sync();
        // The top-level also leaves the host-visible window set and z-order.
        // The window-set revision bumps so the presenter's Frame handler
        // reconciles the stale host window away on the wake above — the
        // destroy side of the reconcile-on-change latch.
        state
            .present()
            .unregister_top_level(crate::handles::Hwnd::from(window_handle));
    }

    tracing::debug!(target: "wiegui", hwnd = window_handle, "DestroyWindow");

    if window_proc == 0 {
        // No WndProc — remove the window and return success.
        state
            .window_state()
            .windows
            .retain(|w| w.handle != crate::handles::Hwnd::from(window_handle));
        let ra = engine
            .return_from_win64_api(1)
            .context("failed to return from DestroyWindow")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 1,
        });
    }

    // Send WM_DESTROY to the window procedure.
    Err(WinApiControlSignal::GuestCallbackRequested {
        request: GuestCallbackRequest {
            callback_address: window_proc,
            window_handle,
            message: WM_DESTROY,
            word_parameter: 0,
            long_parameter: 0,
            unicode,
            outer_return: OuterReturn::Fixed(1),
        },
    }
    .into())
}

/// Handles `USER32.dll!GetClassNameA`.
pub fn handle_get_class_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_class_name(ctx, "GetClassNameA", false)
}

/// Handles `USER32.dll!GetClassNameW`.
pub fn handle_get_class_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_class_name(ctx, "GetClassNameW", true)
}

pub(crate) fn handle_get_class_name(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let buffer_ptr = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let max_count = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let class_name = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(window_handle))
        .map_or(String::new(), |window| window.class_name.clone());

    let capacity = usize::try_from(max_count)
        .with_context(|| format!("{api_name} buffer capacity does not fit usize"))?;

    // GetClassName returns the character count copied (excluding the NUL), or
    // zero on failure — both writers already report that.
    let copied = if class_name.is_empty() {
        0
    } else if unicode {
        write_guest_utf16_c_string(engine, buffer_ptr, capacity, &class_name)
            .with_context(|| format!("failed to write class name for {api_name}"))?
    } else {
        write_guest_ansi_c_string(engine, buffer_ptr, capacity, &class_name)
            .with_context(|| format!("failed to write class name for {api_name}"))?
    };

    let return_value =
        u64::try_from(copied).with_context(|| format!("{api_name} length does not fit u64"))?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
