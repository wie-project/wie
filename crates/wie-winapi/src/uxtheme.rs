use crate::HandlerContext;
use anyhow::{Context, Result};

use crate::WinApiHandlerResult;

/// Handles dynamic `UXTHEME.dll!SetWindowTheme`.
pub fn handle_set_window_theme(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _window_handle = engine
        .read_rcx()
        .context("failed to read RCX for SetWindowTheme")?;

    let _sub_app_name_ptr = engine
        .read_rdx()
        .context("failed to read RDX for SetWindowTheme")?;

    let _sub_id_list_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetWindowTheme")?;

    // HRESULT S_OK.
    let return_value = 0;

    ctx.finish(return_value)
}
