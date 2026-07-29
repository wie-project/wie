use anyhow::{Context, Result};

use crate::{HandlerContext, WinApiHandlerResult};

/// Handles `WINMM.dll!timeGetTime`.
pub fn handle_time_get_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = state.window_state().tick_count;

    state.window_state().tick_count = state
        .window_state()
        .tick_count
        .checked_add(16)
        .context("timeGetTime tick count overflow")?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from timeGetTime")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
