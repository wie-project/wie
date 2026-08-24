//! Class-registry handlers: RegisterClass/UnregisterClass, ValidateRect,
//! SetWindowLong, GetWindowDC (split from `message.rs`).

use crate::gdi32::{ArgReg, read_arg};
use crate::guest_layout::WndClass;
use crate::user32::{
    FAKE_DEVICE_CONTEXT_HANDLE, HandlerContext, Result, WinApiHandlerResult, WindowClassRecord,
    read_guest_ansi_lossy, read_guest_utf16_lossy, register_window_class, with_typed_read,
};
use anyhow::Context as _;

/// Handles `USER32.dll!RegisterClassA`.
pub fn handle_register_class_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    register_class_impl(ctx, false)
}

/// Handles `USER32.dll!RegisterClassW`.
pub fn handle_register_class_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    register_class_impl(ctx, true)
}

/// Shared `RegisterClassA/W` implementation.
///
/// Win64 ABI: `rcx` = `lpwcx` (guest `WNDCLASS`). The struct layout is shared
/// by both variants; only the pointed-to strings differ in encoding.
fn register_class_impl(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let api_name = if wide {
        "RegisterClassW"
    } else {
        "RegisterClassA"
    };
    let struct_tag = if wide { "WNDCLASSW" } else { "WNDCLASSA" };

    let class_va = read_arg(engine, ArgReg::Rcx, api_name)?;

    if class_va == 0 {
        return ctx.finish(0);
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
        class_name_va,
    ) = with_typed_read::<WndClass, _, _>(engine, class_va, |wc| {
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
    .with_context(|| format!("failed to read {struct_tag} for {api_name}"))?;

    let class_name = if class_name_va == 0 {
        String::new()
    } else if wide {
        read_guest_utf16_lossy(engine, class_name_va, 256)
            .context("failed to read WNDCLASSW.lpszClassName for RegisterClassW")?
    } else {
        read_guest_ansi_lossy(engine, class_name_va, 256)
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
            unicode: wide,
        },
    )
    .with_context(|| format!("failed to register window class for {api_name}"))?;

    ctx.finish(atom)
}

/// Handles `USER32.dll!UnregisterClassA`.
pub fn handle_unregister_class_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(1)
}

/// Handles `USER32.dll!UnregisterClassW`.
pub fn handle_unregister_class_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(1)
}

/// Handles `USER32.dll!ValidateRect`.
pub fn handle_validate_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(1)
}

/// Handles `USER32.dll!SetWindowLongA`.
pub fn handle_set_window_long_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(0)
}

/// Handles `USER32.dll!SetWindowLongW`.
pub fn handle_set_window_long_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(0)
}

/// Handles `USER32.dll!GetWindowDC`.
pub fn handle_get_window_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let dc = FAKE_DEVICE_CONTEXT_HANDLE;
    ctx.finish(dc)
}
