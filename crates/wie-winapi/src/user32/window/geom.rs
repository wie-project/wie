//! Window geometry, system metrics, scrolling, and move/resize/z-order helpers
//! (split from the former `window.rs`).

use super::class::{find_window, find_window_mut};
use crate::state::WindowFlags;
use crate::user32::{
    Context, FAKE_DESKTOP_WINDOW_HANDLE, FAKE_PROCESS_ID, FAKE_SYSTEM_COLOR_BRUSH_BASE,
    FAKE_THREAD_ID, FAKE_WINDOW_HANDLE, HandlerContext, Result, WinApiHandlerResult, WinApiState,
    checked_field_address, is_known_window, low_i32, read_guest_i32, read_guest_u32,
    read_guest_u64, window_client_size, write_guest_i32, write_guest_u32, write_window_rect,
};

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

    let x = low_i32(x_raw, "MoveWindow x")?;
    let y = low_i32(y_raw, "MoveWindow y")?;
    let width = low_i32(width_raw, "MoveWindow width")?;
    let height = low_i32(height_raw, "MoveWindow height")?;

    // MoveWindow returns nonzero for the legacy fake window (which drives the
    // host winit window through the single-window WindowState fields) and for
    // any known window record; only an unknown handle fails (returns 0).
    let success = if window_handle == FAKE_WINDOW_HANDLE {
        state.window_state().window_x = x;
        state.window_state().window_y = y;
        state.window_state().window_width = width;
        state.window_state().window_height = height;

        if repaint_raw != 0
            && let Some(window) = find_window_mut(state, window_handle)
        {
            window.invalidated = false;
        }
        true
    } else if let Some(window) = find_window_mut(state, window_handle) {
        // Mirror the SetWindowPlacement geometry update (see
        // `handle_set_window_placement`): the outer rect lands on the record
        // and the client rect tracks the new size. MoveWindow carries no
        // visibility state, so `visible` stays untouched (the placement
        // branch derives it from showCmd).
        window.x = x;
        window.y = y;
        window.width = width;
        window.height = height;
        window.client_rect = (0, 0, width, height);

        // MoveWindow(…, bRepaint = TRUE) requests a redraw, so no paint stays
        // pending afterwards; bRepaint = FALSE leaves the previous
        // invalidation in place.
        if repaint_raw != 0 {
            window.invalidated = false;
        }
        true
    } else {
        false
    };

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

        // COLOR_INFOBK (24) — the tooltip background (#FFFFE1, Windows 2000+).
        24 => 0x00ff_ffe1,

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
        if window.parent_handle == crate::handles::Hwnd::from(parent) {
            return true;
        }
        if window.parent_handle == crate::handles::Hwnd::NULL
            || window.parent_handle == crate::handles::Hwnd::from(current)
        {
            return false;
        }
        current = window.parent_handle.as_u64();
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
/// Size of the x64 `WINDOWPLACEMENT` struct in bytes: `UINT length` @0,
/// `UINT flags` @4, `UINT showCmd` @8, `POINT ptMinPosition` @12, `POINT
/// ptMaxPosition` @20, `RECT rcNormalPosition` @28 (44 bytes total).
///
/// Both placement handlers and their tests agree on this value: `Get` writes
/// it back as `length`, `Set` rejects structs below it.
pub(crate) const WINDOWPLACEMENT_LENGTH: u32 = 44;

/// Handles `USER32.dll!GetWindowPlacement`.
pub fn handle_get_window_placement(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowPlacement")?;

    let placement_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowPlacement")?;

    // Geometry comes from the window record when one exists; the legacy fake
    // window reads the single-window state fields (like MoveWindow).
    let placement = if window_handle == FAKE_WINDOW_HANDLE {
        let window = state.window_state();
        Some((
            window.window_x,
            window.window_y,
            window.window_width,
            window.window_height,
            window.window_visible,
        ))
    } else {
        find_window(state, window_handle).map(|window| {
            (
                window.x,
                window.y,
                window.width,
                window.height,
                window.visible,
            )
        })
    };

    // `success` must mirror the `filter(|_| placement_ptr != 0)` gate on the
    // write path below: a known window with a NULL placement pointer fails in
    // both places, so no branch can ever write through the NULL pointer. Keep
    // the two pointer checks in lockstep if this is refactored.
    let success = placement.is_some() && placement_ptr != 0;

    if let Some((x, y, width, height, visible)) = placement.filter(|_| placement_ptr != 0) {
        // rcNormalPosition is the outer window rect in screen coordinates
        // (GetWindowRect semantics).
        let right = x
            .checked_add(width)
            .context("GetWindowPlacement right coordinate overflow")?;

        let bottom = y
            .checked_add(height)
            .context("GetWindowPlacement bottom coordinate overflow")?;

        // showCmd: a visible window reports SW_SHOWNORMAL (1), a hidden one
        // SW_HIDE (0). Minimized/maximized placement is not tracked (IsIconic
        // /IsZoomed always report false), so SW_SHOWNORMAL is the only
        // restored state we can express.
        let show_cmd = u32::from(visible);

        write_guest_u32(engine, placement_ptr, WINDOWPLACEMENT_LENGTH)
            .context("failed to write WINDOWPLACEMENT.length")?;

        write_guest_u32(
            engine,
            checked_field_address(placement_ptr, 4, "WINDOWPLACEMENT.flags"),
            0,
        )
        .context("failed to write WINDOWPLACEMENT.flags")?;

        write_guest_u32(
            engine,
            checked_field_address(placement_ptr, 8, "WINDOWPLACEMENT.showCmd"),
            show_cmd,
        )
        .context("failed to write WINDOWPLACEMENT.showCmd")?;

        // ptMinPosition / ptMaxPosition: zero without minimized/maximized
        // tracking.
        for (offset, name) in [
            (12, "WINDOWPLACEMENT.ptMinPosition.x"),
            (16, "WINDOWPLACEMENT.ptMinPosition.y"),
            (20, "WINDOWPLACEMENT.ptMaxPosition.x"),
            (24, "WINDOWPLACEMENT.ptMaxPosition.y"),
        ] {
            write_guest_i32(
                engine,
                checked_field_address(placement_ptr, offset, name),
                0,
            )
            .with_context(|| format!("failed to write {name}"))?;
        }

        write_window_rect(
            engine,
            checked_field_address(placement_ptr, 28, "WINDOWPLACEMENT.rcNormalPosition"),
            x,
            y,
            right,
            bottom,
        )
        .context("failed to write WINDOWPLACEMENT.rcNormalPosition")?;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetWindowPlacement")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!SetWindowPlacement`.
pub fn handle_set_window_placement(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowPlacement")?;

    let placement_ptr = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowPlacement")?;

    let known = window_handle == FAKE_WINDOW_HANDLE || find_window(state, window_handle).is_some();
    let mut success = known && placement_ptr != 0;

    if success {
        let length = read_guest_u32(engine, placement_ptr)
            .context("failed to read WINDOWPLACEMENT.length")?;

        // A length below the x64 WINDOWPLACEMENT size (a 32-bit struct) is
        // rejected, mirroring real Windows.
        success = length >= WINDOWPLACEMENT_LENGTH;
    }

    if success {
        let show_cmd = read_guest_u32(
            engine,
            checked_field_address(placement_ptr, 8, "WINDOWPLACEMENT.showCmd"),
        )
        .context("failed to read WINDOWPLACEMENT.showCmd")?;

        let left = read_guest_i32(
            engine,
            checked_field_address(placement_ptr, 28, "WINDOWPLACEMENT.rcNormalPosition.left"),
        )
        .context("failed to read WINDOWPLACEMENT.rcNormalPosition.left")?;

        let top = read_guest_i32(
            engine,
            checked_field_address(placement_ptr, 32, "WINDOWPLACEMENT.rcNormalPosition.top"),
        )
        .context("failed to read WINDOWPLACEMENT.rcNormalPosition.top")?;

        let right = read_guest_i32(
            engine,
            checked_field_address(placement_ptr, 36, "WINDOWPLACEMENT.rcNormalPosition.right"),
        )
        .context("failed to read WINDOWPLACEMENT.rcNormalPosition.right")?;

        let bottom = read_guest_i32(
            engine,
            checked_field_address(placement_ptr, 40, "WINDOWPLACEMENT.rcNormalPosition.bottom"),
        )
        .context("failed to read WINDOWPLACEMENT.rcNormalPosition.bottom")?;

        let width = right
            .checked_sub(left)
            .context("SetWindowPlacement width overflow")?;

        let height = bottom
            .checked_sub(top)
            .context("SetWindowPlacement height overflow")?;

        // Any nonzero showCmd (SW_SHOWNORMAL / SW_SHOWMINIMIZED /
        // SW_SHOWMAXIMIZED) shows the window; SW_HIDE hides it. Minimized and
        // maximized placement are not tracked, so the window reports as
        // restored afterwards.
        let visible = show_cmd != 0;

        // Whether the applied rect differs from the current record. An
        // unchanged rect skips the host-forwarding step (a no-op move); the
        // record update below still runs, matching the pre-existing behavior.
        let mut geometry_changed = false;

        if window_handle == FAKE_WINDOW_HANDLE {
            let window = state.window_state();
            geometry_changed = window.window_x != left
                || window.window_y != top
                || window.window_width != width
                || window.window_height != height;
            window.window_x = left;
            window.window_y = top;
            window.window_width = width;
            window.window_height = height;
            window.window_visible = visible;
        } else if let Some(window) = find_window_mut(state, window_handle) {
            // Reposition the window record (the MoveWindow geometry update).
            geometry_changed = window.x != left
                || window.y != top
                || window.width != width
                || window.height != height;
            window.x = left;
            window.y = top;
            window.width = width;
            window.height = height;
            window.visible = visible;
            // Showing the window invalidates it with erase (real Windows),
            // mirroring ShowWindow(SW_SHOW): the first paint cycle fills the
            // client with the class-brush background before the guest paints.
            // SetWindowPlacement is how notepad-style apps show their main
            // window (instead of ShowWindow), so without this the top-level
            // frame stays zeroed (black) until something else triggers an
            // erase. Hidden windows never paint.
            if visible {
                window.invalidated = true;
                window.flags.insert(WindowFlags::ERASE_BACKGROUND);
            }
            // rcNormalPosition is the outer window rect; the client rect
            // keeps its window-relative origin and tracks the new size.
            window.client_rect = (0, 0, width, height);
        }

        // Host-forwarding: record the new geometry for the winit window and
        // wake the host presenter so it applies the move without waiting for
        // the next frame publish. The consumer seam is
        // `GuestHandle::take_host_geometry_request` (wie-runtime session
        // window layer).
        if geometry_changed {
            let ws = state.window_state();
            ws.host_geometry_request = Some((left, top, width, height));
            // Tag the request with the hwnd it was applied to, so the host
            // can route the move to the matching winit window (multi-window
            // presentation).
            ws.host_geometry_hwnd = Some(window_handle);
            if let Some(wake) = state.present().wake.as_ref() {
                wake();
            }
        }
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetWindowPlacement")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        FAKE_WINDOW_HANDLE, find_window_mut, handle_get_client_rect, handle_move_window,
        read_guest_i32, sys_color, window_client_size,
    };

    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    use ahash::HashMap;
    use ahash::HashMapExt;
    use std::sync::{Arc, Mutex};

    use crate::guest_heap::GuestHeap;
    use crate::present::MessageQueue;
    use crate::sync_obj::SyncState;
    use crate::thread::ThreadState;
    use crate::user32::{
        CreateWindowRequest, WS_CHILD, WindowClassIdentifier, controls, create_window_record,
    };
    use crate::vfs::VolumeConfig;
    use crate::{
        DllStateMap, FileIoState, GuestStdinMode, HandlerContext, HeapState, KernelState,
        ModuleState, ProcessState, WinApiEnvironment, WinApiState,
    };

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    // STACK_VA + STACK_SIZE - 0x100 (leave room for a dummy return address).
    const STACK_TOP: u64 = 0x100_FF00;
    /// A writable buffer address inside the mapped test memory (RECT writes).
    const RECT_BUF: u64 = 0x3000;
    /// Built-in EDIT control class (resolves to `ControlClassKind::Edit`).
    const EDIT_CLASS: &str = "Edit";

    /// Minimal engine for handler unit tests: maps guest pages and a stack
    /// with a valid return address (`return_from_win64_api` reads it).
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn test_environment() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 0,
        }
    }

    /// Zero-heavy default state; the geometry handlers only touch the window
    /// records and the message queue.
    fn test_state() -> WinApiState {
        let mut heap = GuestHeap::new(0x2000, 0x10000);
        heap.attach_guest_control(0x2000);
        WinApiState {
            heap_state: HeapState {
                heap,
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: crate::FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: GuestStdinMode::InjectOnly,
                ucrt_files: HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: crate::RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: Vec::new(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: crate::ModuleHandle::from(
                    crate::dll_loader::REAL_MODULE_HANDLE_BASE,
                ),
            },
        }
    }

    /// Create a WS_CHILD window record (child windows skip class-menu
    /// resolution) with the given class name and initial size.
    fn push_child_window(state: &mut WinApiState, class: &str, width: i32, height: i32) -> u64 {
        let (hwnd, _, _) = create_window_record(
            state,
            CreateWindowRequest {
                class_identifier: WindowClassIdentifier::Name(class.to_owned()),
                title: String::new(),
                style: WS_CHILD,
                extended_style: 0,
                parent_handle: 0,
                menu_handle: 0,
                instance_handle: 0,
                x: 0,
                y: 0,
                width,
                height,
            },
            true,
        )
        .expect("create window record");
        assert_ne!(hwnd, 0, "create_window_record must allocate a handle");
        hwnd
    }

    /// Seed MoveWindow's register args (rcx..r9) and stack args (nHeight at
    /// rsp+0x28, bRepaint at rsp+0x30 — the Win64 ABI's 5th/6th slots).
    fn write_move_window_args(
        cpu: &mut IcedCpu,
        handle: u64,
        x: u64,
        y: u64,
        width: u64,
        height: u64,
        repaint: u64,
    ) {
        cpu.write_rcx(handle).ok();
        cpu.write_rdx(x).ok();
        cpu.write_r8(y).ok();
        cpu.write_r9(width).ok();
        cpu.write_rsp(STACK_TOP).ok();
        cpu.mem_write(STACK_TOP + 0x28, &height.to_le_bytes())
            .expect("write MoveWindow height");
        cpu.mem_write(STACK_TOP + 0x30, &repaint.to_le_bytes())
            .expect("write MoveWindow repaint");
    }

    /// Drive `handle_move_window` end to end and return its return value.
    /// `args` is `(handle, x, y, width, height, repaint)`.
    fn run_move_window(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        args: (u64, u64, u64, u64, u64, u64),
    ) -> u64 {
        let (handle, x, y, width, height, repaint) = args;
        write_move_window_args(engine, handle, x, y, width, height, repaint);
        handle_move_window(&mut HandlerContext::new(engine, test_environment(), state))
            .expect("MoveWindow handler")
            .return_value
    }

    fn record(state: &WinApiState, hwnd: u64) -> &crate::WindowRecord {
        state
            .try_window_state()
            .expect("window state initialised")
            .windows
            .iter()
            .find(|window| window.handle == crate::handles::Hwnd::from(hwnd))
            .expect("window record exists")
    }

    /// COLOR_INFOBK (24) is the tooltip background — pinned so a future edit
    /// to the system-color table cannot silently drop the fidelity fix.
    #[test]
    fn sys_color_infobk_is_tooltip_yellow() {
        assert_eq!(sys_color(24), 0x00ff_ffe1);
    }

    /// RNotepad's WM_SIZE handler calls MoveWindow on its multiline EDIT; the
    /// record must track the new geometry so GetClientRect and the paint row
    /// math follow the real size (pre-fix the record kept its creation size).
    #[test]
    fn move_window_updates_real_child_window_record() {
        let mut engine = test_engine();
        let mut state = test_state();
        let hwnd = push_child_window(&mut state, "MoveWindowChild", 64, 32);

        let ret = run_move_window(&mut engine, &mut state, (hwnd, 0, 0, 320, 480, 1));

        assert_eq!(ret, 1, "MoveWindow on a known window returns TRUE");
        let window = record(&state, hwnd);
        assert_eq!((window.x, window.y), (0, 0));
        assert_eq!((window.width, window.height), (320, 480));
        assert_eq!(window.client_rect, (0, 0, 320, 480));

        // The paint path sizes the client from the record:
        assert_eq!(window_client_size(&mut state, hwnd), (320, 480));

        // GetClientRect reports the moved size (the reported symptom).
        engine.write_rcx(hwnd).ok();
        engine.write_rdx(RECT_BUF).ok();
        engine.write_rsp(STACK_TOP).ok();
        let get_client_rect_ret = handle_get_client_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetClientRect handler")
        .return_value;
        assert_eq!(get_client_rect_ret, 1);
        let left = read_guest_i32(&mut engine, RECT_BUF).expect("RECT.left");
        let top = read_guest_i32(&mut engine, RECT_BUF + 4).expect("RECT.top");
        let right = read_guest_i32(&mut engine, RECT_BUF + 8).expect("RECT.right");
        let bottom = read_guest_i32(&mut engine, RECT_BUF + 12).expect("RECT.bottom");
        assert_eq!((left, top, right, bottom), (0, 0, 320, 480));
    }

    /// MoveWindow returns nonzero for a known window and 0 for an unknown
    /// handle (real Windows returns 0 only when the window does not exist).
    #[test]
    fn move_window_returns_true_for_known_window_and_zero_for_unknown() {
        let mut engine = test_engine();
        let mut state = test_state();
        let hwnd = push_child_window(&mut state, "MoveWindowReturn", 100, 100);

        assert_eq!(
            run_move_window(&mut engine, &mut state, (hwnd, 0, 0, 200, 150, 1)),
            1
        );
        assert_eq!(
            run_move_window(
                &mut engine,
                &mut state,
                (0x0000_0011_2200_0000, 0, 0, 200, 150, 1)
            ),
            0
        );
    }

    /// bRepaint = TRUE clears a pending invalidation (a redraw was requested,
    /// so nothing stays pending); bRepaint = FALSE leaves it in place.
    #[test]
    fn move_window_repaint_flag_clears_pending_invalidation() {
        let mut engine = test_engine();
        let mut state = test_state();
        let hwnd = push_child_window(&mut state, "MoveWindowRepaint", 10, 10);
        find_window_mut(&mut state, hwnd)
            .expect("window record")
            .invalidated = true;

        // bRepaint = FALSE: the pending paint stands.
        run_move_window(&mut engine, &mut state, (hwnd, 0, 0, 100, 100, 0));
        assert!(
            record(&state, hwnd).invalidated,
            "no repaint keeps the pending invalidation"
        );

        // bRepaint = TRUE: the requested redraw consumes the pending flag.
        run_move_window(&mut engine, &mut state, (hwnd, 0, 0, 100, 100, 1));
        assert!(
            !record(&state, hwnd).invalidated,
            "repaint consumes the pending invalidation"
        );
    }

    /// MoveWindow carries no visibility state: unlike SetWindowPlacement
    /// (which derives `visible` from showCmd), it must not flip the record's
    /// visibility.
    #[test]
    fn move_window_does_not_change_visibility() {
        let mut engine = test_engine();
        let mut state = test_state();
        let hidden = push_child_window(&mut state, "HiddenChild", 10, 10);

        run_move_window(&mut engine, &mut state, (hidden, 0, 0, 50, 50, 1));

        assert!(
            !record(&state, hidden).visible,
            "a hidden window stays hidden after MoveWindow"
        );
    }

    /// Regression probe for the reported symptom: a multiline EDIT grown to
    /// 480 px tall (MoveWindow from the parent's WM_SIZE) fits several rows.
    /// Uses the same `layout_visible_lines` row math the control paint path
    /// applies to the record's client size.
    #[test]
    fn move_window_grown_multiline_edit_fits_multiple_rows() {
        let mut engine = test_engine();
        let mut state = test_state();
        // Notepad's pattern: an EDIT child created small, then sized by
        // MoveWindow from the WM_SIZE handler.
        let edit = push_child_window(&mut state, EDIT_CLASS, 64, 32);
        let ret = run_move_window(&mut engine, &mut state, (edit, 0, 0, 320, 480, 1));
        assert_eq!(ret, 1);

        let window = record(&state, edit);
        assert_eq!((window.width, window.height), (320, 480));
        assert!(
            window.control_kind.is_some(),
            "the Edit class resolves to a built-in control"
        );
        let width = window.width;
        let height = window.height;

        // The control paint path lays rows out at client width minus the 2 px
        // side margins, then clips rows to the client height (edit.rs).
        let rows = controls::layout_visible_lines(
            "alpha\nbeta\ngamma\ndelta\nepsilon",
            width.saturating_sub(4),
            16,
            0,
            true,
            0,
            &mut |_ch| 8,
        );
        let visible = rows.iter().filter(|row| row.y < height).count();
        assert!(
            visible >= 5,
            "5 lines at 16 px each fit a 480 px edit; saw {visible}"
        );
    }

    /// The legacy FAKE_WINDOW_HANDLE path drives the single-window WindowState
    /// geometry fields (the host winit window) and returns TRUE.
    #[test]
    fn move_window_fake_handle_updates_window_state() {
        let mut engine = test_engine();
        let mut state = test_state();

        let ret = run_move_window(
            &mut engine,
            &mut state,
            (FAKE_WINDOW_HANDLE, 10, 20, 640, 480, 1),
        );

        assert_eq!(ret, 1, "MoveWindow on the fake window returns TRUE");
        let window = state.window_state();
        assert_eq!(
            (
                window.window_x,
                window.window_y,
                window.window_width,
                window.window_height,
            ),
            (10, 20, 640, 480)
        );
    }
}
