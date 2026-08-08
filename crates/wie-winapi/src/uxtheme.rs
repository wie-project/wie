//! Handles dynamic `UXTHEME.dll` exports.
//!
//! WIE exposes no theme engine, so every entry point is a graceful no-op that
//! reports "themes off": apps fall back to classic rendering.

use anyhow::{Context, Result};

use crate::{HandlerContext, WinApiHandlerResult};

/// Soft dispatch for `uxtheme.dll` exports not in the dense `WinApiId` table.
///
/// The dense path still owns `SetWindowTheme`; these are the fallback no-ops.
pub fn dispatch_uxtheme(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "openthemedata" => Ok(Some(handle_open_theme_data(ctx)?)),
        "closethemedata" => Ok(Some(handle_close_theme_data(ctx)?)),
        "isthemeactive" => Ok(Some(handle_is_theme_active(ctx)?)),
        "getwindowtheme" => Ok(Some(handle_get_window_theme(ctx)?)),
        _ => Ok(None),
    }
}

/// `HTHEME OpenThemeData(HWND hwnd, LPCWSTR pszClassList)` — NULL (no theme).
fn handle_open_theme_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwnd = engine
        .read_rcx()
        .context("failed to read RCX for OpenThemeData")?;
    let _class_list_va = engine
        .read_rdx()
        .context("failed to read RDX for OpenThemeData")?;
    ctx.finish(0)
}

/// `HRESULT CloseThemeData(HTHEME hTheme)` — S_OK.
fn handle_close_theme_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _theme = engine
        .read_rcx()
        .context("failed to read RCX for CloseThemeData")?;
    ctx.finish(0)
}

/// `BOOL IsThemeActive(void)` — FALSE (themes off).
fn handle_is_theme_active(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(0)
}

/// `HTHEME GetWindowTheme(HWND hwnd)` — NULL (no theme attached).
fn handle_get_window_theme(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwnd = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowTheme")?;
    ctx.finish(0)
}

/// Handles dynamic `UXTHEME.dll!SetWindowTheme`.
pub fn handle_set_window_theme(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowTheme")?;

    let _sub_app_name_va = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowTheme")?;

    let _sub_id_list_va = engine
        .read_r8()
        .context("failed to read R8 for SetWindowTheme")?;

    // HRESULT S_OK.
    let return_value = 0;

    ctx.finish(return_value)
}
