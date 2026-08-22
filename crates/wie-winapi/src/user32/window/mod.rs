//! Window lifecycle: creation/destruction, text, focus, show/enable state, and
//! the class-name registry (split from the former `window.rs`).
//!
//! Geometry and system-metrics helpers live in [`geom`]; the geometry-changing
//! operations (`MoveWindow` move/resize, `SetWindowPos` z-order) in
//! [`geom_ops`]; window text/font helpers in [`text`]; `CreateWindowExA/W` in
//! [`create`]; the window-manager state (show/enable/focus/invalidation/
//! destroy) in [`mgr`]. Capture, the window/class long-pointer accessors and
//! the window-lookup helpers live in [`class`].

use super::{
    Context, HandlerContext, Result, WinApiHandlerResult, with_typed_write,
    write_guest_ansi_c_string, write_guest_utf16_c_string,
};
use crate::guest_layout::WinRect;

mod class;
mod create;
mod geom;
mod geom_ops;
mod mgr;
mod text;

pub(crate) use class::{find_window, find_window_mut};
pub use class::{
    handle_get_capture, handle_get_class_long_ptr_a, handle_get_class_long_ptr_w,
    handle_get_window_long_ptr_a, handle_get_window_long_ptr_w, handle_release_capture,
    handle_set_capture, handle_set_class_long_ptr_a, handle_set_class_long_ptr_w,
    handle_set_window_long_ptr_a, handle_set_window_long_ptr_w,
};
pub use create::{handle_create_window_ex_a, handle_create_window_ex_w};
pub use geom::{
    handle_adjust_window_rect, handle_adjust_window_rect_ex, handle_client_to_screen,
    handle_get_client_rect, handle_get_desktop_window, handle_get_dlg_ctrl_id,
    handle_get_sys_color, handle_get_sys_color_brush, handle_get_window,
    handle_get_window_placement, handle_get_window_thread_process_id, handle_is_child,
    handle_is_iconic, handle_is_zoomed, handle_screen_to_client, handle_scroll_window_ex,
    handle_set_rect, handle_set_window_placement,
};
pub use geom_ops::{handle_move_window, handle_set_window_pos};
pub use mgr::{
    handle_destroy_window, handle_enable_window, handle_get_active_window, handle_get_focus,
    handle_get_foreground_window, handle_get_parent, handle_invalidate_rect, handle_is_window,
    handle_is_window_enabled, handle_is_window_visible, handle_redraw_window,
    handle_set_active_window, handle_set_focus, handle_set_foreground_window, handle_show_window,
    handle_update_window,
};
pub use text::{
    handle_get_window_text_a, handle_get_window_text_length_a, handle_get_window_text_length_w,
    handle_get_window_text_w, handle_set_window_text_a, handle_set_window_text_w,
};
// Test-only: the placement tests assert against the same 44-byte struct size
// the handlers write/validate. The lib build does not reference it through
// this path (geom.rs uses the const directly), so gate the re-export.
#[cfg(test)]
pub(crate) use geom::WINDOWPLACEMENT_LENGTH;
pub(crate) use geom::sys_color;
pub(crate) use mgr::deliver_focus_change;
pub(crate) use text::{set_window_font, window_font};

/// Handles `USER32.dll!GetWindowRect`.
pub fn handle_get_window_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetWindowRect")?;

    let rect_va = engine
        .read_rdx()
        .context("failed to read RDX for GetWindowRect")?;

    let window = find_window(state, window_handle);
    let success = window.is_some() && rect_va != 0;

    if let Some(window) = window.filter(|_| rect_va != 0) {
        let right = window
            .x
            .checked_add(window.width)
            .context("GetWindowRect right coordinate overflow")?;

        let bottom = window
            .y
            .checked_add(window.height)
            .context("GetWindowRect bottom coordinate overflow")?;

        // One shared-lock borrow instead of four per-field writes; the RECT
        // layout + pinned offsets live in `crate::guest_layout::WinRect`.
        with_typed_write::<WinRect, _, _>(engine, rect_va, |rect| {
            rect.left = window.x;
            rect.top = window.y;
            rect.right = right;
            rect.bottom = bottom;
            Ok(())
        })
        .context("failed to write GetWindowRect RECT")?;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// `USER_DEFAULT_SCREEN_DPI` (shellscalingapi.h) — the DPI of a 100% display.
const USER_DEFAULT_SCREEN_DPI: u64 = 96;

/// Handles dynamic `USER32.dll!GetDpiForWindow`.
pub fn handle_get_dpi_for_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetDpiForWindow")?;

    // Standard 100% Windows DPI.
    let return_value = USER_DEFAULT_SCREEN_DPI;

    ctx.finish(return_value)
}
/// Handles dynamic `USER32.dll!AdjustWindowRectExForDpi`.
pub fn handle_adjust_window_rect_ex_for_dpi(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_va = engine
        .read_rcx()
        .context("failed to read RCX for AdjustWindowRectExForDpi")?;

    let _style = engine
        .read_rdx()
        .context("failed to read RDX for AdjustWindowRectExForDpi")?;

    let _has_menu = engine
        .read_r8()
        .context("failed to read R8 for AdjustWindowRectExForDpi")?;

    let _extended_style = engine
        .read_r9()
        .context("failed to read R9 for AdjustWindowRectExForDpi")?;

    // The fifth argument, dpi, is on the Win64 stack. For now the fake desktop
    // uses 96 DPI, so preserving the supplied client rectangle is sufficient.
    let return_value = u64::from(rect_va != 0);

    ctx.finish(return_value)
}

/// Handles `USER32.dll!GetClassNameA`.
pub fn handle_get_class_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_class_name(ctx, "GetClassNameA", false)
}

/// Handles `USER32.dll!GetClassNameW`.
pub fn handle_get_class_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_class_name(ctx, "GetClassNameW", true)
}

pub(crate) fn handle_get_class_name(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let window_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let buffer_va = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let max_count = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let class_name = state
        .window_state()
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(window_handle))
        .map_or(String::new(), |window| window.class_name.clone());

    let capacity = usize::try_from(max_count)
        .with_context(|| format!("{api_name} buffer capacity does not fit usize"))?;

    // GetClassName returns the character count copied (excluding the NUL), or
    // zero on failure — both writers already report that.
    let copied = if class_name.is_empty() {
        0
    } else if unicode {
        write_guest_utf16_c_string(engine, buffer_va, capacity, &class_name)
            .with_context(|| format!("failed to write class name for {api_name}"))?
    } else {
        write_guest_ansi_c_string(engine, buffer_va, capacity, &class_name)
            .with_context(|| format!("failed to write class name for {api_name}"))?
    };

    let return_value =
        u64::try_from(copied).with_context(|| format!("{api_name} length does not fit u64"))?;

    ctx.finish(return_value)
}
