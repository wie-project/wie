use super::{
    Context, HandlerContext, Result, TME_CANCEL, TME_HOVER, TME_LEAVE, WinApiHandlerResult,
    read_guest_bytes, with_typed_read, with_typed_write, write_guest_bytes,
};
use crate::gdi32::{ArgReg, read_arg};
use crate::guest_layout::{TrackMouseEvent, WinPoint};
use crate::state::{MOUSE_BUTTON_SLOTS, WinApiState, mouse_button_bit};
use std::ops::Deref;

/// Handles `USER32.dll!GetAsyncKeyState`.
pub fn handle_get_async_key_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let virtual_key_raw = read_arg(engine, ArgReg::Rcx, "GetAsyncKeyState")?;

    let virtual_key = usize::try_from(virtual_key_raw & 0xff).unwrap_or(0);

    // Bit 15: key is currently down.  Bit 0: key was pressed since last call.
    let key_state = key_state_byte(state, virtual_key);
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
///
/// Reports the latest host-pushed cursor position in guest-logical screen
/// pixels (see `WinApiState::cursor_pos`); `None` before the first host
/// push keeps the legacy `(0, 0)`. Level-triggered: the mirror read is a
/// copy, so repeated calls report the same position.
pub fn handle_get_cursor_pos(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let point_va = read_arg(engine, ArgReg::Rcx, "GetCursorPos")?;

    if point_va != 0 {
        let (x, y) = state.cursor_pos().unwrap_or((0, 0));
        // One shared-lock borrow instead of two per-field writes; the POINT
        // layout lives in `crate::guest_layout::WinPoint`.
        with_typed_write::<WinPoint, _, _>(engine, point_va, |point| {
            point.x = x;
            point.y = y;
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

    let display = ctx.state.display;
    let success = rect_va != 0;
    if success {
        // Full desktop-ish clip rect.
        super::write_window_rect(engine, rect_va, 0, 0, display.width, display.height)?;
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
        let key_state = key_state_bytes(state);
        write_guest_bytes(engine, keyboard_state_va, &key_state)
            .context("failed to write GetKeyboardState buffer")?;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// The 256-byte array `GetKeyboardState` hands the guest: the stored keyboard
/// rows, with the five mouse rows projected from the mirror's live `MK_*` mask.
///
/// Built as a copy so the stored array keeps exactly one meaning — real keys,
/// as the host key seam and `SetKeyboardState` write it — and the guest's view
/// of the mouse stays derived from the single host push (semantics documented
/// on [`key_state_byte`]).
fn key_state_bytes(state: &mut WinApiState) -> [u8; 256] {
    state.drain_key_writes();

    let mut array: [u8; 256] = state
        .window_state()
        .keyboard_state
        .deref()
        .try_into()
        .unwrap_or([0_u8; 256]);
    let mouse_buttons = state.mouse_buttons();
    for (vk, bit, _) in MOUSE_BUTTON_SLOTS {
        let slot = usize::from(vk);
        let Some(byte) = array.get_mut(slot) else {
            continue;
        };
        *byte = mouse_slot_byte(mouse_buttons, bit);
    }
    array
}
/// Handles `USER32.dll!GetKeyState`.
pub fn handle_get_key_state(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let virtual_key_raw = read_arg(engine, ArgReg::Rcx, "GetKeyState")?;

    let virtual_key = usize::try_from(virtual_key_raw & 0xff)
        .context("GetKeyState virtual key does not fit usize")?;

    let key_state = key_state_byte(state, virtual_key);

    // WinAPI uses the high bit of SHORT to indicate a pressed key.
    let return_value = if (key_state & 0x80) != 0 {
        u64::from(0x8000_u16)
    } else {
        0
    };

    ctx.finish(return_value)
}
/// The Windows semantics WIE implements for the three key-state readers that
/// carry mouse virtual-key slots — `GetKeyState`, `GetAsyncKeyState` and
/// `GetKeyboardState` — written down because each of the three documents a
/// different subset of the rule and none of them mentions the mouse.
///
/// **The array is the whole answer; the mask is never a per-thread copy.**
/// `GetKeyboardState` returns a `PBYTE` a guest indexes by virtual-key code,
/// and the mouse rows are ordinary rows of it. A guest polling a click with
/// `kbd[VK_LBUTTON] & 0x80` inside its message loop is the common idiom, so
/// those rows carry the live button state. Since the rows are projected from
/// the mirror on every read and never stored, the array, the single-key readers
/// and DirectInput's `GetDeviceState` cannot disagree: one host push, three
/// consistent views.
///
/// **The foreground-thread zero rule is deliberately not implemented.** All
/// three are documented to return zero when the calling thread is not the
/// foreground thread (i.e. has no keyboard focus). WIE has one input focus for
/// the whole guest — the winit window — and no per-thread keyboard state to
/// report, so the "calling thread" part of the rule has nothing to read.
/// Implementing the zero rule as "zero for any non-UI thread" would make a
/// worker thread that polls `GetKeyState(VK_LBUTTON)` read up, which is the
/// very divergence this projection removes, reintroduced under a new name. A
/// guest whose click detection breaks once it moves that poll off its UI thread
/// would break on real Windows too, where the same poll also requires the
/// focus — so the deviation is a missing *liveness* check, not a wrong *state*
/// report, and the state is the part that must be right.
///
/// **Toggle state stays 0 for mouse buttons.** `GetKeyState`'s low-order bit
/// reports the toggle state of Caps/Num/Scroll lock; a mouse button has no
/// toggle, so a held button is `0x8000` and never `0x8001`.
fn key_state_byte(state: &mut WinApiState, virtual_key: usize) -> u8 {
    // Host key writes reach the array first, so the two sources are read in
    // exactly one place and neither reader can forget a step.
    state.drain_key_writes();

    // The five mouse slots are derived from the presenter mirror's live `MK_*`
    // mask, never read out of the keyboard array: the host never wrote them, so
    // the array could only report them permanently up. A guest that pressed
    // `SetKeyboardState`'s mouse slots does not get a second voice either.
    if let Some(bit) = u8::try_from(virtual_key).ok().and_then(mouse_button_bit) {
        return mouse_slot_byte(state.mouse_buttons(), bit);
    }

    state.window_state().keyboard_state.get(virtual_key)
}

/// One keyboard-state byte for a mouse button, `0x80` down / `0` up, from the
/// mirror's `MK_*` mask.
fn mouse_slot_byte(mouse_buttons: u16, bit: u16) -> u8 {
    if mouse_buttons & bit == 0 { 0 } else { 0x80 }
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
