//! Class-registry handlers: RegisterClass/UnregisterClass, ValidateRect,
//! SetWindowLong, GetWindowDC (split from `message.rs`).

use crate::user32::{
    Context, FAKE_DEVICE_CONTEXT_HANDLE, HandlerContext, Result, WinApiHandlerResult,
    WindowClassRecord, checked_field_address, read_guest_ansi_lossy, read_guest_u32,
    read_guest_u64, read_guest_utf16_lossy, register_window_class,
};

/// Handles `USER32.dll!RegisterClassA`.
pub fn handle_register_class_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    let class_ptr = engine
        .read_rcx()
        .context("failed to read RCX for RegisterClassA")?;

    if class_ptr == 0 {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from RegisterClassA")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    }

    // WNDCLASSA on Win64:
    //  +0x00 style (u32)
    //  +0x04 (padding — lpfnWndProc must be 8-byte aligned)
    //  +0x08 lpfnWndProc (u64)
    //  +0x10 cbClsExtra (i32)
    //  +0x14 cbWndExtra (i32)
    //  +0x18 hInstance (u64)
    //  +0x20 hIcon (u64)
    //  +0x28 hCursor (u64)
    //  +0x30 hbrBackground (u64)
    //  +0x38 lpszMenuName (u64)
    //  +0x40 lpszClassName (u64)

    let style = read_guest_u32(engine, class_ptr)?;
    let window_proc = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 8, "WNDCLASS.lpfnWndProc"),
    )?;
    let _cls_extra = read_guest_u32(
        engine,
        checked_field_address(class_ptr, 0x10, "WNDCLASS.cbClsExtra"),
    )?;
    let _wnd_extra = read_guest_u32(
        engine,
        checked_field_address(class_ptr, 0x14, "WNDCLASS.cbWndExtra"),
    )?;
    let instance_handle = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x18, "WNDCLASS.hInstance"),
    )?;
    let icon = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x20, "WNDCLASS.hIcon"),
    )?;
    let cursor = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x28, "WNDCLASS.hCursor"),
    )?;
    let background_brush = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x30, "WNDCLASS.hbrBackground"),
    )?;
    let menu_name = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x38, "WNDCLASS.lpszMenuName"),
    )?;
    let class_name_ptr = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x40, "WNDCLASS.lpszClassName"),
    )?;

    let class_name = if class_name_ptr == 0 {
        String::new()
    } else {
        read_guest_ansi_lossy(engine, class_name_ptr, 256)
            .context("failed to read WNDCLASS.lpszClassName for RegisterClassA")?
    };

    let atom = register_window_class(
        state,
        WindowClassRecord {
            atom: 0,
            class_name,
            window_proc,
            style,
            instance_handle,
            icon_handle: icon,
            cursor_handle: cursor,
            background_brush,
            small_icon_handle: 0,
            menu_name,
            unicode: false,
        },
    )
    .context("failed to register window class for RegisterClassA")?;

    let ra = engine
        .return_from_win64_api(atom)
        .context("failed to return from RegisterClassA")?;
    Ok(WinApiHandlerResult {
        return_address: ra,
        return_value: atom,
    })
}

/// Handles `USER32.dll!RegisterClassW`.
pub fn handle_register_class_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    let class_ptr = engine
        .read_rcx()
        .context("failed to read RCX for RegisterClassW")?;

    if class_ptr == 0 {
        let ra = engine
            .return_from_win64_api(0)
            .context("failed to return from RegisterClassW")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value: 0,
        });
    }

    // WNDCLASSW on Win64 (same layout as WNDCLASSA, but lpszClassName
    // and lpszMenuName point to UTF-16 strings):
    //  +0x00 style (u32)
    //  +0x04 (padding — lpfnWndProc must be 8-byte aligned)
    //  +0x08 lpfnWndProc (u64)
    //  +0x10 cbClsExtra (i32)
    //  +0x14 cbWndExtra (i32)
    //  +0x18 hInstance (u64)
    //  +0x20 hIcon (u64)
    //  +0x28 hCursor (u64)
    //  +0x30 hbrBackground (u64)
    //  +0x38 lpszMenuName (u64)
    //  +0x40 lpszClassName (u64)

    let style = read_guest_u32(engine, class_ptr)?;
    let window_proc = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 8, "WNDCLASSW.lpfnWndProc"),
    )?;
    let _cls_extra = read_guest_u32(
        engine,
        checked_field_address(class_ptr, 0x10, "WNDCLASSW.cbClsExtra"),
    )?;
    let _wnd_extra = read_guest_u32(
        engine,
        checked_field_address(class_ptr, 0x14, "WNDCLASSW.cbWndExtra"),
    )?;
    let instance_handle = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x18, "WNDCLASSW.hInstance"),
    )?;
    let icon = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x20, "WNDCLASSW.hIcon"),
    )?;
    let cursor = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x28, "WNDCLASSW.hCursor"),
    )?;
    let background_brush = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x30, "WNDCLASSW.hbrBackground"),
    )?;
    let menu_name = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x38, "WNDCLASSW.lpszMenuName"),
    )?;
    let class_name_ptr = read_guest_u64(
        engine,
        checked_field_address(class_ptr, 0x40, "WNDCLASSW.lpszClassName"),
    )?;

    let class_name = if class_name_ptr == 0 {
        String::new()
    } else {
        read_guest_utf16_lossy(engine, class_name_ptr, 256)
            .context("failed to read WNDCLASSW.lpszClassName for RegisterClassW")?
    };

    let atom = register_window_class(
        state,
        WindowClassRecord {
            atom: 0,
            class_name,
            window_proc,
            style,
            instance_handle,
            icon_handle: icon,
            cursor_handle: cursor,
            background_brush,
            small_icon_handle: 0,
            menu_name,
            unicode: true,
        },
    )
    .context("failed to register window class for RegisterClassW")?;

    let ra = engine
        .return_from_win64_api(atom)
        .context("failed to return from RegisterClassW")?;
    Ok(WinApiHandlerResult {
        return_address: ra,
        return_value: atom,
    })
}

/// Handles `USER32.dll!UnregisterClassA`.
pub fn handle_unregister_class_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from UnregisterClassA")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// Handles `USER32.dll!UnregisterClassW`.
pub fn handle_unregister_class_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from UnregisterClassW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// Handles `USER32.dll!ValidateRect`.
pub fn handle_validate_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from ValidateRect")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// Handles `USER32.dll!SetWindowLongA`.
pub fn handle_set_window_long_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from SetWindowLongA")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}

/// Handles `USER32.dll!SetWindowLongW`.
pub fn handle_set_window_long_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from SetWindowLongW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}

/// Handles `USER32.dll!GetWindowDC`.
pub fn handle_get_window_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dc = FAKE_DEVICE_CONTEXT_HANDLE;
    let return_address = engine
        .return_from_win64_api(dc)
        .context("failed to return from GetWindowDC")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: dc,
    })
}
