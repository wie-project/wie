use super::{
    Context, FAKE_DEVICE_CONTEXT_HANDLE, HandlerContext, Result, WinApiHandlerResult,
    checked_field_address, write_guest_i32, write_guest_u32, write_guest_u64,
};
use crate::gdi32::DcKind;

/// Handles `USER32.dll!GetDC`.
pub fn handle_get_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine.read_rcx().context("failed to read RCX for GetDC")?;

    let dc_handle = if super::is_known_window(state, window_handle) {
        state
            .gdi_state()
            .alloc_dc(DcKind::Window(crate::handles::Hwnd::from(window_handle)))
    } else {
        crate::handles::Hdc::from(FAKE_DEVICE_CONTEXT_HANDLE)
    };

    let return_address = engine
        .return_from_win64_api(dc_handle.as_u64())
        .context("failed to return from GetDC")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: dc_handle.as_u64(),
    })
}
/// Handles `USER32.dll!ReleaseDC`.
pub fn handle_release_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for ReleaseDC")?;

    let dc_handle = engine
        .read_rdx()
        .context("failed to read RDX for ReleaseDC")?;

    state
        .gdi_state()
        .remove_dc(crate::handles::Hdc::from(dc_handle));

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from ReleaseDC")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `USER32.dll!BeginPaint`.
pub fn handle_begin_paint(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for BeginPaint")?;

    let paint_ptr = engine
        .read_rdx()
        .context("failed to read RDX for BeginPaint")?;

    let known = super::is_known_window(state, window_handle);
    let return_value = if known && paint_ptr != 0 {
        let (width, height) = super::window_client_size(state, window_handle);

        // PAINTSTRUCT (Win64):
        // HDC  hdc;           0
        // BOOL fErase;        8
        // RECT rcPaint;       12  (left, top, right, bottom)
        // BOOL fRestore;      28
        // BOOL fIncUpdate;    32
        // BYTE rgbReserved[32]; 36
        let begin_dc = state
            .gdi_state()
            .alloc_dc(DcKind::Window(crate::handles::Hwnd::from(window_handle)));
        write_guest_u64(engine, paint_ptr, begin_dc.as_u64())?;
        // fErase: nonzero when the background still needs erasing — i.e. the
        // invalidation asked for an erase and no WM_ERASEBKGND consumed it
        // (a class brush that erased it clears the flag on dispatch).
        let f_erase =
            super::find_window(state, window_handle).is_some_and(|window| window.erase_background);
        write_guest_u32(
            engine,
            checked_field_address(paint_ptr, 8, "fErase"),
            u32::from(f_erase),
        )?;
        write_guest_i32(
            engine,
            checked_field_address(paint_ptr, 12, "rcPaint.left"),
            0,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(paint_ptr, 16, "rcPaint.top"),
            0,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(paint_ptr, 20, "rcPaint.right"),
            width,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(paint_ptr, 24, "rcPaint.bottom"),
            height,
        )?;
        write_guest_u32(engine, checked_field_address(paint_ptr, 28, "fRestore"), 0)?;
        write_guest_u32(
            engine,
            checked_field_address(paint_ptr, 32, "fIncUpdate"),
            0,
        )?;
        // rgbReserved left zeroed by guest or ignored.

        tracing::debug!(window_handle, width, height, "BeginPaint");
        begin_dc.as_u64()
    } else {
        tracing::debug!(window_handle, paint_ptr, known, "BeginPaint rejected");
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from BeginPaint")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!EndPaint`.
pub fn handle_end_paint(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for EndPaint")?;

    let _paint_ptr = engine
        .read_rdx()
        .context("failed to read RDX for EndPaint")?;

    let success = super::is_known_window(state, window_handle);
    if let Some(window) = super::find_window_mut(state, window_handle) {
        window.invalidated = false;
        // The paint cycle is over: a pending erase either ran (WM_ERASEBKGND)
        // or was seen by the WndProc via fErase and handled by the app.
        window.erase_background = false;
    }

    let return_value = u64::from(success);
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from EndPaint")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `USER32.dll!ScrollDC` (no-op success stub; no real pixel scroll).
pub fn handle_scroll_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hdc = engine
        .read_rcx()
        .context("failed to read RCX for ScrollDC")?;

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from ScrollDC")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
