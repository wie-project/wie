use super::{
    Context, HandlerContext, Result, TME_CANCEL, TME_HOVER, TME_LEAVE, WinApiHandlerResult,
    checked_field_address, read_guest_bytes, read_guest_u32, read_guest_u64, write_guest_bytes,
    write_guest_i32,
};

/// Handles `USER32.dll!GetAsyncKeyState`.
pub fn handle_get_async_key_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let virtual_key_raw = engine
        .read_rcx()
        .context("failed to read RCX for GetAsyncKeyState")?;

    let virtual_key = usize::try_from(virtual_key_raw & 0xff).unwrap_or(0);

    // Bit 15: key is currently down.  Bit 0: key was pressed since last call.
    let key_state = state
        .window_state()
        .keyboard_state
        .get(virtual_key)
        .copied()
        .unwrap_or(0);
    let mut result = u64::from(key_state & 0x80);
    if result != 0 {
        result |= 1; // most-significant bit set → key down
    }

    let return_address = engine
        .return_from_win64_api(result)
        .context("failed to return from GetAsyncKeyState")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: result,
    })
}
/// Handles dynamic `USER32.dll!TrackMouseEvent`.
///
/// Records the tracking request on the target window; the host forwards
/// `WM_MOUSEHOVER` / `WM_MOUSELEAVE` only for tracked windows (Windows sends
/// neither without a `TrackMouseEvent` request).
pub fn handle_track_mouse_event(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let track_mouse_event_ptr = engine
        .read_rcx()
        .context("failed to read RCX for TrackMouseEvent")?;

    let mut tracking = false;
    if track_mouse_event_ptr != 0 {
        // TRACKMOUSEEVENT (Win64):
        //  +0x00 cbSize (u32)
        //  +0x04 dwFlags (u32)
        //  +0x08 hwndTrack (u64)
        //  +0x10 dwHoverTime (u32)
        let flags = read_guest_u32(
            engine,
            checked_field_address(track_mouse_event_ptr, 4, "TRACKMOUSEEVENT.dwFlags"),
        )
        .ok()
        .unwrap_or(0);
        let hwnd_track = read_guest_u64(
            engine,
            checked_field_address(track_mouse_event_ptr, 8, "TRACKMOUSEEVENT.hwndTrack"),
        )
        .ok()
        .unwrap_or(0);

        if hwnd_track != 0 && super::is_known_window(state, hwnd_track) {
            if flags & TME_CANCEL != 0 {
                // Cancel tracking (TME_CANCEL with no TME_* arm is a release).
                if let Some(window) = super::find_window_mut(state, hwnd_track) {
                    window.mouse_tracking = false;
                }
            } else if flags & (TME_HOVER | TME_LEAVE) != 0
                && let Some(window) = super::find_window_mut(state, hwnd_track)
            {
                window.mouse_tracking = true;
            }
            tracking = true;
        }
    }

    let return_value = u64::from(tracking);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from TrackMouseEvent")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetCursorPos`.
pub fn handle_get_cursor_pos(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let point_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetCursorPos")?;

    if point_ptr != 0 {
        // POINT:
        // LONG x; offset 0
        // LONG y; offset 4
        write_guest_i32(engine, point_ptr, 0)?;
        write_guest_i32(engine, checked_field_address(point_ptr, 4, "POINT.y"), 0)?;
    }

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from GetCursorPos")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `USER32.dll!ClipCursor` (accept clip rect or release when NULL).
pub fn handle_clip_cursor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for ClipCursor")?;

    // No host cursor clipping; always succeed so editor drag paths continue.
    tracing::debug!(rect_ptr, "ClipCursor");

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from ClipCursor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `USER32.dll!GetClipCursor`.
pub fn handle_get_clip_cursor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetClipCursor")?;

    let success = rect_ptr != 0;
    if success {
        // Full desktop-ish clip rect.
        super::write_window_rect(engine, rect_ptr, 0, 0, 1920, 1080)?;
    }

    let return_value = u64::from(success);
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetClipCursor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetCursor`.
pub fn handle_set_cursor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let cursor_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetCursor")?;

    let previous_cursor = state.window_state().cursor_handle;
    state.window_state().cursor_handle = cursor_handle;

    let return_address = engine
        .return_from_win64_api(previous_cursor)
        .context("failed to return from SetCursor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: previous_cursor,
    })
}
/// Handles `USER32.dll!GetCursor`.
pub fn handle_get_cursor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().cursor_handle;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetCursor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetKeyboardState`.
pub fn handle_set_keyboard_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let keyboard_state_ptr = engine
        .read_rcx()
        .context("failed to read RCX for SetKeyboardState")?;

    let success = keyboard_state_ptr != 0;

    if success {
        read_guest_bytes(
            engine,
            keyboard_state_ptr,
            &mut state.window_state().keyboard_state,
        )
        .context("failed to read SetKeyboardState buffer")?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetKeyboardState")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetKeyboardState`.
pub fn handle_get_keyboard_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let keyboard_state_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetKeyboardState")?;

    let success = keyboard_state_ptr != 0;

    if success {
        write_guest_bytes(
            engine,
            keyboard_state_ptr,
            &state.window_state().keyboard_state,
        )
        .context("failed to write GetKeyboardState buffer")?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetKeyboardState")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!GetKeyState`.
pub fn handle_get_key_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let virtual_key_raw = engine
        .read_rcx()
        .context("failed to read RCX for GetKeyState")?;

    let virtual_key = usize::try_from(virtual_key_raw & 0xff)
        .context("GetKeyState virtual key does not fit usize")?;

    let key_state = state
        .window_state()
        .keyboard_state
        .get(virtual_key)
        .copied()
        .unwrap_or(0);

    // WinAPI uses the high bit of SHORT to indicate a pressed key.
    let return_value = if (key_state & 0x80) != 0 {
        u64::from(0x8000_u16)
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetKeyState")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!MapVirtualKeyA`.
pub fn handle_map_virtual_key_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let code = engine
        .read_rcx()
        .context("failed to read RCX for MapVirtualKeyA")?;

    let map_type = engine
        .read_rdx()
        .context("failed to read RDX for MapVirtualKeyA")?;

    let code_low = code & u64::from(u32::MAX);

    let return_value = match map_type {
        // MAPVK_VK_TO_VSC / MAPVK_VSC_TO_VK / MAPVK_VSC_TO_VK_EX
        0 | 1 | 3 | 4 => code_low,

        // MAPVK_VK_TO_CHAR: approximate printable ASCII keys.
        2 if (0x20..=0x7e).contains(&code_low) => code_low,

        _ => 0,
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from MapVirtualKeyA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
