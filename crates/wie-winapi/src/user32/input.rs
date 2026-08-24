use super::{
    Context, HandlerContext, Result, TME_CANCEL, TME_HOVER, TME_LEAVE, WinApiHandlerResult,
    read_guest_bytes, with_typed_read, with_typed_write, write_guest_bytes,
};
use crate::gdi32::{ArgReg, read_arg};
use crate::guest_layout::{TrackMouseEvent, WinPoint};

/// Handles `USER32.dll!GetAsyncKeyState`.
pub fn handle_get_async_key_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let virtual_key_raw = read_arg(engine, ArgReg::Rcx, "GetAsyncKeyState")?;

    let virtual_key = usize::try_from(virtual_key_raw & 0xff).unwrap_or(0);

    // Bit 15: key is currently down.  Bit 0: key was pressed since last call.
    let key_state = state.window_state().keyboard_state.get(virtual_key);
    let mut result = u64::from(key_state & 0x80);
    if result != 0 {
        result |= 1; // most-significant bit set → key down
    }

    ctx.finish(result)
}
/// Handles dynamic `USER32.dll!TrackMouseEvent`.
///
/// Records the tracking request on the target window; the host forwards
/// `WM_MOUSEHOVER` / `WM_MOUSELEAVE` only for tracked windows (Windows sends
/// neither without a `TrackMouseEvent` request).
pub fn handle_track_mouse_event(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let track_mouse_event_va = read_arg(engine, ArgReg::Rcx, "TrackMouseEvent")?;

    let mut tracking = false;
    if track_mouse_event_va != 0 {
        // One shared-lock borrow instead of two per-field reads; the layout
        // + pinned offsets live in `crate::guest_layout::TrackMouseEvent`. A
        // read failure keeps the old tolerant semantics (treated as all-zero).
        let (flags, hwnd_track) =
            with_typed_read::<TrackMouseEvent, _, _>(engine, track_mouse_event_va, |tme| {
                Ok((tme.flags, tme.track_window_handle))
            })
            .unwrap_or((0, 0));

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

    ctx.finish(return_value)
}
/// Handles `USER32.dll!GetCursorPos`.
pub fn handle_get_cursor_pos(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let point_va = read_arg(engine, ArgReg::Rcx, "GetCursorPos")?;

    if point_va != 0 {
        // One shared-lock borrow instead of two per-field writes; the POINT
        // layout lives in `crate::guest_layout::WinPoint`.
        with_typed_write::<WinPoint, _, _>(engine, point_va, |point| {
            point.x = 0;
            point.y = 0;
            Ok(())
        })
        .context("failed to write POINT for GetCursorPos")?;
    }

    ctx.finish(1)
}
/// Handles `USER32.dll!ClipCursor` (accept clip rect or release when NULL).
pub fn handle_clip_cursor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_va = read_arg(engine, ArgReg::Rcx, "ClipCursor")?;

    // No host cursor clipping; always succeed so editor drag paths continue.
    tracing::debug!(rect_va, "ClipCursor");

    ctx.finish(1)
}
/// Handles `USER32.dll!GetClipCursor`.
pub fn handle_get_clip_cursor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_va = read_arg(engine, ArgReg::Rcx, "GetClipCursor")?;

    let success = rect_va != 0;
    if success {
        // Full desktop-ish clip rect.
        super::write_window_rect(engine, rect_va, 0, 0, 1920, 1080)?;
    }

    let return_value = u64::from(success);
    ctx.finish(return_value)
}
/// Handles `USER32.dll!SetCursor`.
pub fn handle_set_cursor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let cursor_handle = read_arg(engine, ArgReg::Rcx, "SetCursor")?;

    let previous_cursor = state.window_state().cursor_handle;
    state.window_state().cursor_handle = cursor_handle;

    ctx.finish(previous_cursor)
}
/// Handles `USER32.dll!GetCursor`.
pub fn handle_get_cursor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let state = &mut *ctx.state;
    let return_value = state.window_state().cursor_handle;

    ctx.finish(return_value)
}
/// Handles `USER32.dll!SetKeyboardState`.
pub fn handle_set_keyboard_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let keyboard_state_va = read_arg(engine, ArgReg::Rcx, "SetKeyboardState")?;

    let success = keyboard_state_va != 0;

    if success {
        read_guest_bytes(
            engine,
            keyboard_state_va,
            &mut state.window_state().keyboard_state,
        )
        .context("failed to read SetKeyboardState buffer")?;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// Handles `USER32.dll!GetKeyboardState`.
pub fn handle_get_keyboard_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let keyboard_state_va = read_arg(engine, ArgReg::Rcx, "GetKeyboardState")?;

    let success = keyboard_state_va != 0;

    if success {
        write_guest_bytes(
            engine,
            keyboard_state_va,
            &state.window_state().keyboard_state,
        )
        .context("failed to write GetKeyboardState buffer")?;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// Handles `USER32.dll!GetKeyState`.
pub fn handle_get_key_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let virtual_key_raw = read_arg(engine, ArgReg::Rcx, "GetKeyState")?;

    let virtual_key = usize::try_from(virtual_key_raw & 0xff)
        .context("GetKeyState virtual key does not fit usize")?;

    let key_state = state.window_state().keyboard_state.get(virtual_key);

    // WinAPI uses the high bit of SHORT to indicate a pressed key.
    let return_value = if (key_state & 0x80) != 0 {
        u64::from(0x8000_u16)
    } else {
        0
    };

    ctx.finish(return_value)
}
/// Handles `USER32.dll!MapVirtualKeyA`.
pub fn handle_map_virtual_key_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let code = read_arg(engine, ArgReg::Rcx, "MapVirtualKeyA")?;

    let map_type = read_arg(engine, ArgReg::Rdx, "MapVirtualKeyA")?;

    let code_low = code & u64::from(u32::MAX);

    let return_value = match map_type {
        // MAPVK_VK_TO_VSC / MAPVK_VSC_TO_VK / MAPVK_VSC_TO_VK_EX
        0 | 1 | 3 | 4 => code_low,

        // MAPVK_VK_TO_CHAR: approximate printable ASCII keys.
        2 if (0x20..=0x7e).contains(&code_low) => code_low,

        _ => 0,
    };

    ctx.finish(return_value)
}
