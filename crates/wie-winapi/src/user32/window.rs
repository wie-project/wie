use super::{
    Context, CreateWindowRequest, FAKE_DESKTOP_WINDOW_HANDLE, FAKE_PROCESS_ID,
    FAKE_SYSTEM_COLOR_BRUSH_BASE, FAKE_THREAD_ID, FAKE_WINDOW_HANDLE, GuestCallbackRequest,
    HandlerContext, Result, WM_CREATE, WM_DESTROY, WM_KILLFOCUS, WM_PAINT, WM_SETFOCUS,
    WinApiControlSignal, WinApiHandlerResult, WinApiState, WindowRecord, checked_field_address,
    create_window_record, dispatch_control_proc, get_window_long_ptr_value, is_known_window,
    low_i32, read_guest_ansi_lossy, read_guest_i32, read_guest_u64, read_guest_utf16_lossy,
    read_window_class_identifier_a, read_window_class_identifier_w, set_window_long_ptr_value,
    window_client_size, window_long_ptr_index, write_ansi_window_text, write_guest_ansi_c_string,
    write_guest_i32, write_guest_u32, write_guest_u64, write_guest_utf16_c_string,
    write_wide_window_text, write_window_rect,
};
use crate::OuterReturn;
use crate::gdi32::{IRect, ancestor_offset};

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
pub fn handle_set_window_pos(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowPos")?;

    let _insert_after = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowPos")?;

    let _x = engine
        .read_r8()
        .context("failed to read R8 for SetWindowPos")?;

    let _y = engine
        .read_r9()
        .context("failed to read R9 for SetWindowPos")?;

    // Remaining Win64 arguments are width, height and flags on the stack.
    // For now the compatibility harness accepts the requested placement
    // without maintaining a full window manager.
    let return_value = 1;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetWindowPos")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetWindowLongPtrW`.
pub fn handle_set_window_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowLongPtrW")?;

    let index_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowLongPtrW")?;

    let new_value = engine
        .read_r8()
        .context("failed to read R8 for SetWindowLongPtrW")?;

    let previous_value = set_window_long_ptr_value(
        window_handle,
        index_raw,
        new_value,
        state,
        "SetWindowLongPtrW",
    )?;

    let return_address = engine
        .return_from_win64_api(previous_value)
        .context("failed to return from SetWindowLongPtrW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_value,
    })
}
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
        find_window(state, window_handle).is_some_and(|window| window.enabled)
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

    let return_value = find_window(state, window_handle).map_or(0, |window| window.parent_handle);

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
    let return_value = state.window_state().active_window_handle;

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
    let return_value = state.window_state().foreground_window_handle;

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
        state.window_state().foreground_window_handle = window_handle;
        state.window_state().active_window_handle = window_handle;
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
        state.window_state().active_window_handle = window_handle;
    }

    let return_address = engine
        .return_from_win64_api(previous_window)
        .context("failed to return from SetActiveWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window,
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
        state.window_state().focus_window_handle = window_handle;
    }

    // Windows sends WM_KILLFOCUS(old, new) then WM_SETFOCUS(new, old) when
    // the focus actually moves. Bridge synchronously (guest WndProcs) or
    // host-side (controls) — see deliver_focus_change.
    if accepted
        && window_handle != previous_window
        && let Some(signal) = deliver_focus_change(
            state,
            engine,
            previous_window,
            window_handle,
            OuterReturn::Fixed(previous_window),
        )?
    {
        return Err(signal.into());
    }

    let return_address = engine
        .return_from_win64_api(previous_window)
        .context("failed to return from SetFocus")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window,
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
#[allow(clippy::too_many_arguments)]
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
            .find(|w| w.handle == hwnd)
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
            // Host-side WndProc: update the control's focus state directly.
            let _ = dispatch_control_proc(engine, state, hwnd, message, wparam, 0)?;
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
                window_handle: hwnd,
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
    let return_value = state.window_state().focus_window_handle;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFocus")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetCapture`.
pub fn handle_set_capture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetCapture")?;

    let previous_window = state.window_state().capture_window_handle;

    // SetCapture(NULL) releases; real windows (including child controls, whose
    // WndProc captures implicitly while pressed) become the capture owner.
    if window_handle == 0 || is_known_window(state, window_handle) {
        state.window_state().capture_window_handle = window_handle;
    }

    let return_address = engine
        .return_from_win64_api(previous_window)
        .context("failed to return from SetCapture")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_window,
    })
}
/// Handles `USER32.dll!GetCapture`.
pub fn handle_get_capture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().capture_window_handle;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetCapture")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!ReleaseCapture`.
pub fn handle_release_capture(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    state.window_state().capture_window_handle = 0;

    let return_value = 1;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ReleaseCapture")?;

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
            .find(|window| window.handle == window_handle)
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
        if window.erase_background {
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

    let rect_ptr = engine
        .read_rdx()
        .context("failed to read RDX for InvalidateRect")?;

    let erase_background = engine
        .read_r8()
        .context("failed to read R8 for InvalidateRect")?;

    let success = window_handle == 0 || is_known_window(state, window_handle);

    if let Some(window) = find_window_mut(state, window_handle) {
        window.invalidated = true;
        // bErase: OR so a TRUE erase request survives a later FALSE invalidation
        // of a different region (the update region accumulates, matching Windows).
        if erase_background != 0 {
            window.erase_background = true;
        }
    }

    // B3: accumulate the invalidation into the window's present-surface dirty
    // region (publish side only — WM_PAINT synthesis keeps using `invalidated`).
    // A NULL rect = the whole client area. Children translate to the ancestor
    // surface via the parent-chain offset so their region lands where their
    // pixels actually live. An unreadable RECT falls back to a full-surface
    // mark (conservative: a partial publish can never go stale).
    if success
        && window_handle != 0
        && let Some((top_hwnd, offset_x, offset_y)) =
            ancestor_offset(&state.window_state().windows, window_handle)
    {
        let local_rect = if rect_ptr == 0 {
            let (cw, ch) = window_client_size(state, window_handle);
            Some(IRect {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            })
        } else {
            match (
                read_guest_i32(engine, rect_ptr),
                read_guest_i32(engine, checked_field_address(rect_ptr, 4, "RECT.top")),
                read_guest_i32(engine, checked_field_address(rect_ptr, 8, "RECT.right")),
                read_guest_i32(engine, checked_field_address(rect_ptr, 12, "RECT.bottom")),
            ) {
                (Ok(left), Ok(top), Ok(right), Ok(bottom)) => Some(IRect {
                    left,
                    top,
                    right,
                    bottom,
                }),
                // Unreadable RECT: cannot prove the invalidated region.
                _ => None,
            }
        };
        match local_rect {
            Some(rect) => state.present().mark_dirty(
                top_hwnd,
                IRect {
                    left: rect.left.saturating_add(offset_x),
                    top: rect.top.saturating_add(offset_y),
                    right: rect.right.saturating_add(offset_x),
                    bottom: rect.bottom.saturating_add(offset_y),
                },
            ),
            // Conservative fallback: force a full publish.
            None => state.present().mark_dirty_full(top_hwnd),
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
                window.control_text = text;
                window.invalidated = true;
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
                window.control_text = text;
                window.invalidated = true;
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
/// Handles `USER32.dll!GetClientRect`.
pub fn handle_get_client_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetClientRect")?;

    let rect_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetClientRect")?;

    let success = is_known_window(state, window_handle) && rect_ptr != 0;

    if success {
        let (width, height) = window_client_size(state, window_handle);
        write_window_rect(engine, rect_ptr, 0, 0, width, height)?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetClientRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!MoveWindow`.
pub fn handle_move_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for MoveWindow")?;

    let x_raw = engine
        .read_rdx()
        .context("failed to read RDX for MoveWindow")?;

    let y_raw = engine
        .read_r8()
        .context("failed to read R8 for MoveWindow")?;

    let width_raw = engine
        .read_r9()
        .context("failed to read R9 for MoveWindow")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for MoveWindow")?;

    let height_arg_address = rsp
        .checked_add(0x28)
        .context("MoveWindow height argument address overflow")?;

    let repaint_arg_address = rsp
        .checked_add(0x30)
        .context("MoveWindow repaint argument address overflow")?;

    let height_raw = read_guest_u64(engine, height_arg_address)?;
    let repaint_raw = read_guest_u64(engine, repaint_arg_address)?;

    let success = window_handle == FAKE_WINDOW_HANDLE;

    if success {
        state.window_state().window_x = low_i32(x_raw, "MoveWindow x")?;
        state.window_state().window_y = low_i32(y_raw, "MoveWindow y")?;
        state.window_state().window_width = low_i32(width_raw, "MoveWindow width")?;
        state.window_state().window_height = low_i32(height_raw, "MoveWindow height")?;

        if repaint_raw != 0
            && let Some(window) = find_window_mut(state, window_handle)
        {
            window.invalidated = false;
        }
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from MoveWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!ScreenToClient`.
pub fn handle_screen_to_client(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for ScreenToClient")?;

    let point_ptr = engine
        .read_rdx()
        .context("failed to read RDX for ScreenToClient")?;

    let success = window_handle == FAKE_WINDOW_HANDLE && point_ptr != 0;

    if success {
        let x = read_guest_i32(engine, point_ptr)?;

        let y_address = checked_field_address(point_ptr, 4, "POINT.y");

        let y = read_guest_i32(engine, y_address)?;

        let client_x = x
            .checked_sub(state.window_state().window_x)
            .context("ScreenToClient x coordinate overflow")?;

        let client_y = y
            .checked_sub(state.window_state().window_y)
            .context("ScreenToClient y coordinate overflow")?;

        write_guest_i32(engine, point_ptr, client_x)?;
        write_guest_i32(engine, y_address, client_y)?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ScreenToClient")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!ClientToScreen`.
pub fn handle_client_to_screen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for ClientToScreen")?;

    let point_ptr = engine
        .read_rdx()
        .context("failed to read RDX for ClientToScreen")?;

    let success = window_handle == FAKE_WINDOW_HANDLE && point_ptr != 0;

    if success {
        let x = read_guest_i32(engine, point_ptr)?;

        let y_address = checked_field_address(point_ptr, 4, "POINT.y");

        let y = read_guest_i32(engine, y_address)?;

        let screen_x = x
            .checked_add(state.window_state().window_x)
            .context("ClientToScreen x coordinate overflow")?;

        let screen_y = y
            .checked_add(state.window_state().window_y)
            .context("ClientToScreen y coordinate overflow")?;

        write_guest_i32(engine, point_ptr, screen_x)?;
        write_guest_i32(engine, y_address, screen_y)?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ClientToScreen")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetDesktopWindow`.
pub fn handle_get_desktop_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(FAKE_DESKTOP_WINDOW_HANDLE)
        .context("failed to return from GetDesktopWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_DESKTOP_WINDOW_HANDLE,
    })
}
/// Handles `USER32.dll!GetSysColor`.
pub fn handle_get_sys_color(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let color_index = engine
        .read_rcx()
        .context("failed to read RCX for GetSysColor")?;

    let return_value = u64::from(sys_color(u32::try_from(color_index).unwrap_or(u32::MAX)));

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetSysColor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// The classic Windows system-color table, as 0RGB.
///
/// Shared by `GetSysColor` and the WM_ERASEBKGND class-brush fill (a
/// `WNDCLASS.hbrBackground` of `COLOR_x + 1` resolves through the same table).
#[must_use]
pub(crate) fn sys_color(color_index: u32) -> u32 {
    match color_index {
        // Black-like colors:
        // COLOR_BACKGROUND, COLOR_WINDOWFRAME,
        // COLOR_MENUTEXT, COLOR_WINDOWTEXT,
        // COLOR_CAPTIONTEXT, COLOR_BTNTEXT.
        1 | 6 | 7..=9 | 18 => 0x0000_0000,

        // Accent colors:
        // COLOR_ACTIVECAPTION, COLOR_HIGHLIGHT.
        // #0078D7 as 0RGB (the previous 0xD77830 was the B/R-swapped value and
        // rendered orange).
        2 | 13 => 0x0000_78D7,

        // COLOR_INACTIVECAPTION.
        3 => 0x00bf_bfbf,

        // White-like colors:
        // COLOR_WINDOW, COLOR_HIGHLIGHTTEXT, COLOR_BTNHIGHLIGHT (the 3D
        // edge highlight).
        5 | 14 | 20 => 0x00ff_ffff,

        // COLOR_ACTIVEBORDER, COLOR_INACTIVEBORDER.
        10 | 11 => 0x00b4_b4b4,

        // COLOR_APPWORKSPACE.
        12 => 0x00ab_abab,

        // COLOR_BTNSHADOW.
        16 => 0x00a0_a0a0,

        // COLOR_3DDKSHADOW (the darkest 3D edge).
        21 => 0x0069_6969,

        // COLOR_GRAYTEXT.
        17 => 0x006d_6d6d,

        // COLOR_SCROLLBAR.
        0 => 0x00c8_c8c8,

        // COLOR_MENU, COLOR_BTNFACE and neutral fallback.
        _ => 0x00f0_f0f0,
    }
}
/// Handles `USER32.dll!GetSysColorBrush`.
pub fn handle_get_sys_color_brush(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let color_index = engine
        .read_rcx()
        .context("failed to read RCX for GetSysColorBrush")?;

    let return_value = FAKE_SYSTEM_COLOR_BRUSH_BASE
        .checked_add(color_index)
        .context("GetSysColorBrush handle overflow")?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetSysColorBrush")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetRect`.
pub fn handle_set_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for SetRect")?;

    let left_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetRect")?;

    let top_raw = engine.read_r8().context("failed to read R8 for SetRect")?;

    let right_raw = engine.read_r9().context("failed to read R9 for SetRect")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for SetRect")?;

    let bottom_address = rsp
        .checked_add(0x28)
        .context("SetRect bottom argument address overflow")?;

    let bottom_raw = read_guest_u64(engine, bottom_address)?;

    let success = rect_ptr != 0;

    if success {
        write_window_rect(
            engine,
            rect_ptr,
            low_i32(left_raw, "SetRect left")?,
            low_i32(top_raw, "SetRect top")?,
            low_i32(right_raw, "SetRect right")?,
            low_i32(bottom_raw, "SetRect bottom")?,
        )?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!IsIconic`.
pub fn handle_is_iconic(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsIconic")?;

    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsIconic")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!IsZoomed`.
pub fn handle_is_zoomed(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsZoomed")?;

    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsZoomed")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetWindowThreadProcessId`.
pub fn handle_get_window_thread_process_id(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowThreadProcessId")?;

    let process_id_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowThreadProcessId")?;

    let valid_window =
        window_handle == FAKE_WINDOW_HANDLE || window_handle == FAKE_DESKTOP_WINDOW_HANDLE;

    if valid_window && process_id_ptr != 0 {
        write_guest_u32(engine, process_id_ptr, FAKE_PROCESS_ID)?;
    }

    let return_value = if valid_window { FAKE_THREAD_ID } else { 0 };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindowThreadProcessId")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetDlgCtrlID`.
pub fn handle_get_dlg_ctrl_id(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetDlgCtrlID")?;

    // A child window's control identifier is its menu handle (CreateWindowEx
    // stores the ID there). Unknown windows report -1 like real Windows.
    let return_value = if window_handle == FAKE_WINDOW_HANDLE {
        0
    } else {
        find_window(state, window_handle).map_or(u64::from(u32::MAX), |window| window.menu_handle)
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetDlgCtrlID")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!IsChild`.
pub fn handle_is_child(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_handle = engine
        .read_rcx()
        .context("failed to read RCX for IsChild")?;

    let child_handle = engine
        .read_rdx()
        .context("failed to read RDX for IsChild")?;

    let return_value = u64::from(descends_from(state, child_handle, parent_handle));

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsChild")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Whether `child` is `parent` or a descendant of `parent` (parent-chain walk).
fn descends_from(state: &mut WinApiState, child: u64, parent: u64) -> bool {
    if child == parent {
        return true;
    }
    let mut current = child;
    loop {
        let Some(window) = find_window(state, current) else {
            return false;
        };
        if window.parent_handle == parent {
            return true;
        }
        if window.parent_handle == 0 || window.parent_handle == current {
            return false;
        }
        current = window.parent_handle;
    }
}
/// Handles `USER32.dll!GetWindow`.
pub fn handle_get_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindow")?;

    let _command = engine
        .read_rdx()
        .context("failed to read RDX for GetWindow")?;

    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindow")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetWindowLongPtrA`.
pub fn handle_get_window_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowLongPtrA")?;

    let index_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowLongPtrA")?;

    let value = get_window_long_ptr_value(window_handle, index_raw, state, "GetWindowLongPtrA")?;

    let return_address = engine
        .return_from_win64_api(value)
        .context("failed to return from GetWindowLongPtrA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}
/// Handles `USER32.dll!GetWindowLongPtrW`.
pub fn handle_get_window_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowLongPtrW")?;

    let index_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowLongPtrW")?;

    let value = get_window_long_ptr_value(window_handle, index_raw, state, "GetWindowLongPtrW")?;

    let return_address = engine
        .return_from_win64_api(value)
        .context("failed to return from GetWindowLongPtrW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}
/// Handles `USER32.dll!SetWindowLongPtrA`.
pub fn handle_set_window_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowLongPtrA")?;

    let index_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowLongPtrA")?;

    let new_value = engine
        .read_r8()
        .context("failed to read R8 for SetWindowLongPtrA")?;

    let previous_value = set_window_long_ptr_value(
        window_handle,
        index_raw,
        new_value,
        state,
        "SetWindowLongPtrA",
    )?;

    let return_address = engine
        .return_from_win64_api(previous_value)
        .context("failed to return from SetWindowLongPtrA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_value,
    })
}
/// Handles `USER32.dll!AdjustWindowRectEx`.
pub fn handle_adjust_window_rect_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for AdjustWindowRectEx")?;

    let _style = engine
        .read_rdx()
        .context("failed to read RDX for AdjustWindowRectEx")?;

    let has_menu = engine
        .read_r8()
        .context("failed to read R8 for AdjustWindowRectEx")?;

    let _extended_style = engine
        .read_r9()
        .context("failed to read R9 for AdjustWindowRectEx")?;

    let success = rect_ptr != 0;

    if success {
        let left = read_guest_i32(engine, rect_ptr)?;

        let top_address = checked_field_address(rect_ptr, 4, "RECT.top");
        let right_address = checked_field_address(rect_ptr, 8, "RECT.right");
        let bottom_address = checked_field_address(rect_ptr, 12, "RECT.bottom");

        let top = read_guest_i32(engine, top_address)?;
        let right = read_guest_i32(engine, right_address)?;
        let bottom = read_guest_i32(engine, bottom_address)?;

        // Approximate classic non-client metrics:
        // 8 px frame on each side, 31 px caption,
        // and another 20 px when a menu is present.
        let menu_height = if has_menu != 0 { 20 } else { 0 };

        let adjusted_left = left
            .checked_sub(8)
            .context("AdjustWindowRectEx left overflow")?;

        let adjusted_top = top
            .checked_sub(31)
            .and_then(|value| value.checked_sub(menu_height))
            .context("AdjustWindowRectEx top overflow")?;

        let adjusted_right = right
            .checked_add(8)
            .context("AdjustWindowRectEx right overflow")?;

        let adjusted_bottom = bottom
            .checked_add(8)
            .context("AdjustWindowRectEx bottom overflow")?;

        write_window_rect(
            engine,
            rect_ptr,
            adjusted_left,
            adjusted_top,
            adjusted_right,
            adjusted_bottom,
        )?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from AdjustWindowRectEx")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!ScrollWindowEx` (no-op success stub).
pub fn handle_scroll_window_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwnd = engine
        .read_rcx()
        .context("failed to read RCX for ScrollWindowEx")?;

    // Returns TRUE on success.
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from ScrollWindowEx")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub(crate) fn find_window(state: &mut WinApiState, handle: u64) -> Option<&WindowRecord> {
    state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == handle)
}

pub(crate) fn find_window_mut(state: &mut WinApiState, handle: u64) -> Option<&mut WindowRecord> {
    state
        .window_state()
        .windows
        .iter_mut()
        .find(|window| window.handle == handle)
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
    let x = if x_raw == i32::MIN { 100 } else { x_raw };
    let y = if y_raw == i32::MIN { 100 } else { y_raw };
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
    let x = if x_raw == i32::MIN { 100 } else { x_raw };
    let y = if y_raw == i32::MIN { 100 } else { y_raw };
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
    let window_info = find_window(state, window_handle).map(|w| (w.window_proc, w.unicode));

    let Some((window_proc, unicode)) = window_info else {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from DestroyWindow")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    };

    tracing::debug!(target: "wiegui", hwnd = window_handle, "DestroyWindow");

    if window_proc == 0 {
        // No WndProc — remove the window and return success.
        state
            .window_state()
            .windows
            .retain(|w| w.handle != window_handle);
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
        .find(|window| window.handle == window_handle)
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

/// Handles `USER32.dll!GetClassLongPtrA`.
pub fn handle_get_class_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_class_long_ptr(ctx, "GetClassLongPtrA")
}

/// Handles `USER32.dll!GetClassLongPtrW`.
pub fn handle_get_class_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_class_long_ptr(ctx, "GetClassLongPtrW")
}

fn handle_get_class_long_ptr(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let index_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let atom = window_class_atom(state, window_handle);

    let return_value = if atom == 0 {
        0
    } else {
        let index = window_long_ptr_index(index_raw, api_name)?;
        state
            .window_state()
            .class_long_ptr_values
            .iter()
            .find(|(stored_atom, stored_index, _)| *stored_atom == atom && *stored_index == index)
            .map_or(0, |(_, _, value)| *value)
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `USER32.dll!SetClassLongPtrA`.
pub fn handle_set_class_long_ptr_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_class_long_ptr(ctx, "SetClassLongPtrA")
}

/// Handles `USER32.dll!SetClassLongPtrW`.
pub fn handle_set_class_long_ptr_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_set_class_long_ptr(ctx, "SetClassLongPtrW")
}

fn handle_set_class_long_ptr(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let index_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let new_value = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let atom = window_class_atom(state, window_handle);

    let return_value = if atom == 0 {
        0
    } else {
        let index = window_long_ptr_index(index_raw, api_name)?;
        set_class_long_ptr_value(state, atom, index, new_value)
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Resolve a window handle to its registered class atom (0 when unregistered).
fn window_class_atom(state: &mut WinApiState, window_handle: u64) -> u16 {
    state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == window_handle)
        .map_or(0, |window| window.class_atom)
}

/// Store a class-long value; returns the previous value.
fn set_class_long_ptr_value(state: &mut WinApiState, atom: u16, index: i64, new_value: u64) -> u64 {
    let previous_value = state
        .window_state()
        .class_long_ptr_values
        .iter()
        .find(|(stored_atom, stored_index, _)| *stored_atom == atom && *stored_index == index)
        .map_or(0, |(_, _, value)| *value);

    if let Some(entry) = state
        .window_state()
        .class_long_ptr_values
        .iter_mut()
        .find(|(stored_atom, stored_index, _)| *stored_atom == atom && *stored_index == index)
    {
        entry.2 = new_value;
    } else {
        state
            .window_state()
            .class_long_ptr_values
            .push((atom, index, new_value));
    }

    previous_value
}
