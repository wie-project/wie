use super::{
    Context, FAKE_DEVICE_CONTEXT_HANDLE, HandlerContext, Result, WinApiHandlerResult,
    checked_field_address, write_guest_i32, write_guest_u32, write_guest_u64,
};

/// Handles `USER32.dll!GetDC`.
pub fn handle_get_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine.read_rcx().context("failed to read RCX for GetDC")?;

    let return_address = engine
        .return_from_win64_api(FAKE_DEVICE_CONTEXT_HANDLE)
        .context("failed to return from GetDC")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_DEVICE_CONTEXT_HANDLE,
    })
}
/// Handles `USER32.dll!ReleaseDC`.
pub fn handle_release_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for ReleaseDC")?;

    let _device_context_handle = engine
        .read_rdx()
        .context("failed to read RDX for ReleaseDC")?;

    // ReleaseDC returns 1 when the device context was released.
    let return_value = 1;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from ReleaseDC")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
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
        write_guest_u64(engine, paint_ptr, FAKE_DEVICE_CONTEXT_HANDLE)?;
        write_guest_u32(engine, checked_field_address(paint_ptr, 8, "fErase")?, 1)?;
        write_guest_i32(
            engine,
            checked_field_address(paint_ptr, 12, "rcPaint.left")?,
            0,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(paint_ptr, 16, "rcPaint.top")?,
            0,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(paint_ptr, 20, "rcPaint.right")?,
            width,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(paint_ptr, 24, "rcPaint.bottom")?,
            height,
        )?;
        write_guest_u32(engine, checked_field_address(paint_ptr, 28, "fRestore")?, 0)?;
        write_guest_u32(
            engine,
            checked_field_address(paint_ptr, 32, "fIncUpdate")?,
            0,
        )?;
        // rgbReserved left zeroed by guest or ignored.

        tracing::debug!(window_handle, width, height, "BeginPaint");
        FAKE_DEVICE_CONTEXT_HANDLE
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
    if success {
        state.window_state.window_invalidated = false;
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
