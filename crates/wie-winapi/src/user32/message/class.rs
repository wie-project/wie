//! Class-registry handlers: RegisterClass/UnregisterClass, ValidateRect,
//! SetWindowLong, GetWindowDC (split from `message.rs`).

use crate::guest_layout::WndClass;
use crate::user32::{
    Context, FAKE_DEVICE_CONTEXT_HANDLE, HandlerContext, Result, WinApiHandlerResult,
    WindowClassRecord, read_guest_ansi_lossy, read_guest_utf16_lossy, register_window_class,
    with_typed_read,
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

    // One shared-lock borrow instead of ten per-field reads; the layout and
    // its pinned offsets live in `crate::guest_layout::WndClass` (Win64
    // WNDCLASS: `lpszClassName` @0x40 — the 40-byte figure sometimes quoted
    // is the Win32 size, 4-byte pointers).
    let (
        style,
        window_proc,
        instance_handle,
        icon,
        cursor,
        background_brush,
        menu_name,
        class_name_ptr,
    ) = with_typed_read::<WndClass, _, _>(engine, class_ptr, |wc| {
        Ok((
            wc.style,
            wc.window_proc,
            wc.instance_handle,
            wc.icon_handle,
            wc.cursor_handle,
            wc.background_brush,
            wc.menu_name,
            wc.class_name_ptr,
        ))
    })
    .context("failed to read WNDCLASSA for RegisterClassA")?;

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

    // WNDCLASSW shares the WNDCLASSA layout (see the A variant above); only
    // the pointed-to strings are UTF-16.
    let (
        style,
        window_proc,
        instance_handle,
        icon,
        cursor,
        background_brush,
        menu_name,
        class_name_ptr,
    ) = with_typed_read::<WndClass, _, _>(engine, class_ptr, |wc| {
        Ok((
            wc.style,
            wc.window_proc,
            wc.instance_handle,
            wc.icon_handle,
            wc.cursor_handle,
            wc.background_brush,
            wc.menu_name,
            wc.class_name_ptr,
        ))
    })
    .context("failed to read WNDCLASSW for RegisterClassW")?;

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
