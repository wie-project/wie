//! Handles dynamic `UXTHEME.dll` exports.
//!
//! WIE exposes no theme engine, so every entry point is a graceful no-op that
//! reports "themes off": apps fall back to classic rendering.

use crate::gdi32::{ArgReg, finish_after_discarding, read_arg};
use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

crate::soft_dispatch::soft_exports! {
    ("closethemedata", handle_close_theme_data),
    ("getwindowtheme", handle_get_window_theme),
    ("isthemeactive", handle_is_theme_active),
    ("openthemedata", handle_open_theme_data),
}

/// Soft dispatch for `uxtheme.dll` exports not in the dense `WinApiId` table.
///
/// The dense path still owns `SetWindowTheme`; these are the fallback no-ops.
pub fn dispatch_uxtheme(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    crate::soft_dispatch::dispatch_soft_table(ctx, DISPATCH, name)
}

/// `HTHEME OpenThemeData(HWND hwnd, LPCWSTR pszClassList)` — NULL (no theme).
fn handle_open_theme_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    finish_after_discarding(ctx, 2, 0)
}

/// `HRESULT CloseThemeData(HTHEME hTheme)` — S_OK.
fn handle_close_theme_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    finish_after_discarding(ctx, 1, 0)
}

/// `BOOL IsThemeActive(void)` — FALSE (themes off).
fn handle_is_theme_active(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    finish_after_discarding(ctx, 0, 0)
}

/// `HTHEME GetWindowTheme(HWND hwnd)` — NULL (no theme attached).
fn handle_get_window_theme(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    finish_after_discarding(ctx, 1, 0)
}

/// Handles dynamic `UXTHEME.dll!SetWindowTheme`.
pub fn handle_set_window_theme(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = read_arg(engine, ArgReg::Rcx, "SetWindowTheme")?;

    let _sub_app_name_va = read_arg(engine, ArgReg::Rdx, "SetWindowTheme")?;

    let _sub_id_list_va = read_arg(engine, ArgReg::R8, "SetWindowTheme")?;

    // HRESULT S_OK.
    let return_value = 0;

    ctx.finish(return_value)
}
