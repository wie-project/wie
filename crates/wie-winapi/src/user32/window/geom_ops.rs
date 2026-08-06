//! Geometry-changing window operations: `MoveWindow` (move/resize) and the
//! `SetWindowPos` z-order tracking (split from `geom.rs` / the former
//! `window.rs`).

use super::class::find_window_mut;
use crate::user32::{
    Context, FAKE_WINDOW_HANDLE, HandlerContext, Result, WinApiHandlerResult, low_i32, read_int,
};

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

    let height_raw = read_int::<u64>(engine, height_arg_address)?;
    let repaint_raw = read_int::<u64>(engine, repaint_arg_address)?;

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
        read_int::<u32>(engine, flags_address).context("failed to read SetWindowPos flags")?;

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
