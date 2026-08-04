//! Window geometry, system metrics, scrolling, and move/resize/z-order helpers
//! (split from the former `window.rs`).

use super::class::{find_window, find_window_mut};
use crate::guest_layout::{WinPoint, WinRect};
use crate::state::WindowFlags;
use crate::user32::{
    Context, FAKE_DESKTOP_WINDOW_HANDLE, FAKE_PROCESS_ID, FAKE_SYSTEM_COLOR_BRUSH_BASE,
    FAKE_THREAD_ID, FAKE_WINDOW_HANDLE, HandlerContext, Result, WinApiHandlerResult, WinApiState,
    WindowPlacement, is_known_window, low_i32, read_guest_u32, read_guest_u64, window_client_size,
    with_typed_read, with_typed_write, write_guest_u32,
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
        // One shared-lock borrow instead of four per-field writes; the RECT
        // layout + pinned offsets live in `crate::guest_layout::WinRect`.
        with_typed_write::<WinRect, _, _>(engine, rect_ptr, |rect| {
            rect.left = 0;
            rect.top = 0;
            rect.right = width;
            rect.bottom = height;
            Ok(())
        })
        .context("failed to write GetClientRect RECT")?;
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
            // bRepaint = TRUE invalidates the window (real Windows generates
            // a WM_PAINT after the move) — the region stays dirty until the
            // next paint cycle repaints it.
            window.invalidated = true;
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

        // MoveWindow(…, bRepaint = TRUE) invalidates the window — a WM_PAINT
        // is generated after the move (the moved window must repaint at its
        // new geometry); bRepaint = FALSE leaves the previous invalidation in
        // place. (The pre-fix semantics were inverted: TRUE *cleared* the
        // invalidation, so a resized control never repainted — e.g. the
        // multiline EDIT kept the status bar's stale pixels after the
        // View > Status Bar toggle until a click repainted it.)
        if repaint_raw != 0 {
            window.invalidated = true;
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
        let (x, y) =
            with_typed_read::<WinPoint, _, _>(engine, point_ptr, |point| Ok((point.x, point.y)))
                .context("failed to read POINT for ScreenToClient")?;

        let client_x = x
            .checked_sub(state.window_state().window_x)
            .context("ScreenToClient x coordinate overflow")?;

        let client_y = y
            .checked_sub(state.window_state().window_y)
            .context("ScreenToClient y coordinate overflow")?;

        with_typed_write::<WinPoint, _, _>(engine, point_ptr, |point| {
            point.x = client_x;
            point.y = client_y;
            Ok(())
        })
        .context("failed to write POINT for ScreenToClient")?;
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
        let (x, y) =
            with_typed_read::<WinPoint, _, _>(engine, point_ptr, |point| Ok((point.x, point.y)))
                .context("failed to read POINT for ClientToScreen")?;

        let screen_x = x
            .checked_add(state.window_state().window_x)
            .context("ClientToScreen x coordinate overflow")?;

        let screen_y = y
            .checked_add(state.window_state().window_y)
            .context("ClientToScreen y coordinate overflow")?;

        with_typed_write::<WinPoint, _, _>(engine, point_ptr, |point| {
            point.x = screen_x;
            point.y = screen_y;
            Ok(())
        })
        .context("failed to write POINT for ClientToScreen")?;
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
        with_typed_write::<WinRect, _, _>(engine, rect_ptr, |rect| {
            rect.left = low_i32(left_raw, "SetRect left")?;
            rect.top = low_i32(top_raw, "SetRect top")?;
            rect.right = low_i32(right_raw, "SetRect right")?;
            rect.bottom = low_i32(bottom_raw, "SetRect bottom")?;
            Ok(())
        })
        .context("failed to write RECT for SetRect")?;
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
        // Read-all → compute → write-all (one shared-lock borrow per view).
        let (left, top, right, bottom) =
            with_typed_read::<WinRect, _, _>(engine, rect_ptr, |rect| {
                Ok((rect.left, rect.top, rect.right, rect.bottom))
            })
            .context("failed to read RECT for AdjustWindowRectEx")?;

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

        with_typed_write::<WinRect, _, _>(engine, rect_ptr, |rect| {
            rect.left = adjusted_left;
            rect.top = adjusted_top;
            rect.right = adjusted_right;
            rect.bottom = adjusted_bottom;
            Ok(())
        })
        .context("failed to write RECT for AdjustWindowRectEx")?;
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

        // One shared-lock borrow instead of ten per-field writes. The view
        // starts zeroed, so flags and the min/max positions read as zero —
        // the same bytes the old per-field path wrote explicitly.
        with_typed_write::<WindowPlacement, _, _>(engine, placement_ptr, |placement| {
            placement.length = WINDOWPLACEMENT_LENGTH;
            placement.show_cmd = show_cmd;
            placement.rc_left = x;
            placement.rc_top = y;
            placement.rc_right = right;
            placement.rc_bottom = bottom;
            Ok(())
        })
        .context("failed to write WINDOWPLACEMENT")?;
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
        // Read the whole struct with one shared-lock borrow instead of five
        // per-field reads. The length gate above (Set rejects a pre-44
        // 32-bit struct) still runs before the view is created.
        let (show_cmd, left, top, right, bottom) =
            with_typed_read::<WindowPlacement, _, _>(engine, placement_ptr, |placement| {
                Ok((
                    placement.show_cmd,
                    placement.rc_left,
                    placement.rc_top,
                    placement.rc_right,
                    placement.rc_bottom,
                ))
            })
            .context("failed to read WINDOWPLACEMENT")?;

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
        FAKE_WINDOW_HANDLE, find_window_mut, handle_get_client_rect, handle_move_window, sys_color,
        window_client_size,
    };

    use crate::user32::read_guest_i32;
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
        handle_create_window_ex_a, handle_destroy_window, handle_set_window_pos,
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

    /// Seed `CreateWindowExA`'s register + stack args (the Win64 ABI: four
    /// register args, then x/y/width/height/hWndParent/hMenu/hInstance/
    /// lpParam in the first eight stack slots). An unresolved class atom
    /// yields a window with NO guest WndProc, so the handler completes
    /// synchronously (returns the HWND without a WM_CREATE bridge).
    fn write_create_window_ex_args(
        cpu: &mut IcedCpu,
        parent: u64,
        style: u64,
    ) -> Result<(), &'static str> {
        cpu.write_rcx(0).map_err(|_| "ex_style")?;
        cpu.write_rdx(5).map_err(|_| "class atom")?;
        cpu.write_r8(0).map_err(|_| "title")?;
        cpu.write_r9(style).map_err(|_| "style")?;
        cpu.write_rsp(STACK_TOP).map_err(|_| "rsp")?;
        for (offset, value) in [
            (0x28, 10_u64),  // X
            (0x30, 20_u64),  // Y
            (0x38, 200_u64), // nWidth
            (0x40, 100_u64), // nHeight
            (0x48, parent),  // hWndParent
            (0x50, 0_u64),   // hMenu
            (0x58, 0_u64),   // hInstance
            (0x60, 0_u64),   // lpParam
        ] {
            cpu.mem_write(STACK_TOP + offset, &value.to_le_bytes())
                .map_err(|_| "stack arg")?;
        }
        Ok(())
    }

    /// Create a window through the real `CreateWindowExA` handler (not the
    /// bare `create_window_record`) so the host-visible window-set and
    /// z-order registrations run. Returns the new HWND.
    fn create_window_via_handler(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        parent: u64,
    ) -> u64 {
        write_create_window_ex_args(engine, parent, 0).expect("write CreateWindowExA args");
        handle_create_window_ex_a(&mut HandlerContext::new(engine, test_environment(), state))
            .expect("CreateWindowExA handler")
            .return_value
    }

    /// Seed `SetWindowPos`'s register + stack args (rcx=hwnd, rdx=insertAfter,
    /// r8=X, r9=Y, then cx/cy/uFlags in the first three stack slots).
    fn write_set_window_pos_args(cpu: &mut IcedCpu, hwnd: u64, insert_after: u64, flags: u32) {
        cpu.write_rcx(hwnd).ok();
        cpu.write_rdx(insert_after).ok();
        cpu.write_r8(0).ok();
        cpu.write_r9(0).ok();
        cpu.write_rsp(STACK_TOP).ok();
        for (offset, value) in [
            (0x28, 200_u64), // cx
            (0x30, 100_u64), // cy
            (0x38, u64::from(flags)),
        ] {
            cpu.mem_write(STACK_TOP + offset, &value.to_le_bytes())
                .expect("write SetWindowPos stack arg");
        }
    }

    /// Drive `SetWindowPos` end to end and return its return value.
    fn run_set_window_pos(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        hwnd: u64,
        insert_after: u64,
        flags: u32,
    ) -> u64 {
        write_set_window_pos_args(engine, hwnd, insert_after, flags);
        handle_set_window_pos(&mut HandlerContext::new(engine, test_environment(), state))
            .expect("SetWindowPos handler")
            .return_value
    }

    /// The current guest z-order (back-to-front) as raw u64s.
    fn z_order(state: &mut WinApiState) -> Vec<u64> {
        state
            .present()
            .z_order
            .iter()
            .map(|hwnd| hwnd.as_u64())
            .collect()
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
        // bRepaint = TRUE leaves the window invalidated — the paint cycle
        // repaints it at the new geometry (the status-bar-toggle regression:
        // a cleared flag left the moved EDIT's old pixels until a click).
        assert!(
            record(&state, hwnd).invalidated,
            "MoveWindow(…, TRUE) must invalidate the moved window"
        );

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
    fn move_window_repaint_flag_invalidates_the_window() {
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

        // bRepaint = TRUE: the requested redraw leaves the window
        // invalidated — the paint cycle repaints it at its new geometry
        // (pre-fix the flag was CLEARED, so a moved control never repainted
        // until an unrelated event; the status-bar-toggle regression).
        run_move_window(&mut engine, &mut state, (hwnd, 0, 0, 100, 100, 1));
        assert!(
            record(&state, hwnd).invalidated,
            "repaint (TRUE) must leave the window invalidated for the paint cycle"
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

    /// The reconcile-on-change latch, create side: creating a top-level
    /// (parentless) window through the real handler bumps the window-set
    /// revision and stacks it at the TOP of the z-order; a CHILD window
    /// (parented) must not — it composites into its parent's surface and
    /// owns no host window.
    #[test]
    fn create_window_ex_top_level_bumps_rev_and_z_order() {
        let mut engine = test_engine();
        let mut state = test_state();
        let set_rev_before = state.present().windows_rev;

        let main = create_window_via_handler(&mut engine, &mut state, 0);
        assert_ne!(main, 0, "CreateWindowExA returns a handle");
        let rev_after_main = state.present().windows_rev;
        assert_eq!(
            rev_after_main,
            set_rev_before + 1,
            "a top-level create must bump the window-set revision"
        );

        let second = create_window_via_handler(&mut engine, &mut state, 0);
        assert_eq!(
            state.present().windows_rev,
            rev_after_main + 1,
            "each top-level create bumps the revision"
        );

        // A child of the second top-level: no host window, no revision.
        let child = create_window_via_handler(&mut engine, &mut state, second);
        assert_ne!(child, 0);
        assert_eq!(
            state.present().windows_rev,
            rev_after_main + 1,
            "a CHILD create must not bump the window-set revision"
        );

        // Z-order: creation order, newest topmost.
        assert_eq!(
            z_order(&mut state),
            vec![main, second],
            "the z-order tracks top-level creation order, back-to-front"
        );
    }

    /// The reconcile-on-change latch, destroy side: DestroyWindow of a
    /// top-level unregisters it (the host drops the stale winit window on
    /// the wake) and bumps the window-set revision; a child destroy leaves
    /// the set untouched.
    #[test]
    fn destroy_window_top_level_unregisters_and_bumps_rev() {
        let mut engine = test_engine();
        let mut state = test_state();
        let main = create_window_via_handler(&mut engine, &mut state, 0);
        let second = create_window_via_handler(&mut engine, &mut state, 0);
        let set_rev_before = state.present().windows_rev;

        // Destroy the topmost (second) window through the handler.
        engine.write_rcx(second).ok();
        engine.write_rsp(STACK_TOP).ok();
        let ret = handle_destroy_window(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("DestroyWindow handler")
        .return_value;
        assert_eq!(ret, 1, "DestroyWindow of a known window returns TRUE");

        assert_eq!(
            state.present().windows_rev,
            set_rev_before + 1,
            "a top-level destroy must bump the window-set revision"
        );
        assert_eq!(
            z_order(&mut state),
            vec![main],
            "the destroyed top-level leaves the z-order"
        );

        // Destroy a CHILD: the window set is untouched (children own no
        // host window), so the revision must not move.
        let child = create_window_via_handler(&mut engine, &mut state, main);
        let set_rev_after_child = state.present().windows_rev;
        engine.write_rcx(child).ok();
        engine.write_rsp(STACK_TOP).ok();
        handle_destroy_window(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("DestroyWindow of a child");
        assert_eq!(
            state.present().windows_rev,
            set_rev_after_child,
            "a child destroy never changes the top-level set"
        );
    }

    /// The z-order testable core: `SetWindowPos` HWND_TOP / HWND_BOTTOM
    /// reorder the guest's top-level list (the host presenter mirrors it),
    /// while SWP_NOZORDER leaves it alone. The revision bumps only on a real
    /// z-change.
    #[test]
    fn set_window_pos_reorders_the_guest_z_order() {
        let mut engine = test_engine();
        let mut state = test_state();
        let a = create_window_via_handler(&mut engine, &mut state, 0);
        let b = create_window_via_handler(&mut engine, &mut state, 0);
        let c = create_window_via_handler(&mut engine, &mut state, 0);
        assert_eq!(z_order(&mut state), vec![a, b, c]);
        let z_rev_before = state.present().z_rev;

        // HWND_TOP (0): bring the backmost window to the front.
        assert_eq!(
            run_set_window_pos(&mut engine, &mut state, a, 0, 0),
            1,
            "SetWindowPos returns TRUE"
        );
        assert_eq!(
            z_order(&mut state),
            vec![b, c, a],
            "HWND_TOP moves the window to the top of the z-order"
        );
        assert_eq!(
            state.present().z_rev,
            z_rev_before + 1,
            "a real z-change bumps the z-order revision"
        );

        // HWND_BOTTOM (1): send the topmost window to the back.
        run_set_window_pos(&mut engine, &mut state, a, 1, 0);
        assert_eq!(
            z_order(&mut state),
            vec![a, b, c],
            "HWND_BOTTOM moves the window to the back of the z-order"
        );

        // SWP_NOZORDER (0x0004): the caller said "do not change z-order".
        let rev = state.present().z_rev;
        run_set_window_pos(&mut engine, &mut state, b, 0, 0x0004);
        assert_eq!(
            z_order(&mut state),
            vec![a, b, c],
            "SWP_NOZORDER leaves the z-order untouched"
        );
        assert_eq!(
            state.present().z_rev,
            rev,
            "a no-zorder SetWindowPos must not bump the revision"
        );

        // HWND_TOPMOST (-1) / HWND_NOTOPMOST (-2) map to top / bottom
        // (topmost style is not tracked — minimal model).
        run_set_window_pos(&mut engine, &mut state, c, 0xFFFF_FFFF_FFFF_FFFF, 0);
        assert_eq!(
            z_order(&mut state),
            vec![a, b, c],
            "HWND_TOPMOST reads as top"
        );
        run_set_window_pos(&mut engine, &mut state, c, 0xFFFF_FFFF_FFFF_FFFE, 0);
        assert_eq!(
            z_order(&mut state),
            vec![c, a, b],
            "HWND_NOTOPMOST reads as bottom"
        );
    }
}
