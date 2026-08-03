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
            state.window_state().host_geometry_request = Some((left, top, width, height));
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
