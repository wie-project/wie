use anyhow::{Context, Result};

use crate::{HandlerContext, WinApiHandlerResult};

/// Handles `WINMM.dll!timeGetTime`.
///
/// B5: returns the same ms-since-session-epoch value the in-guest stub reads
/// from the host-written guest clock table (slot 2), so the host fallback and
/// the stub can never disagree about how much time has passed.
pub fn handle_time_get_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_value = crate::kernel32::clock::tick_count_32();

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from timeGetTime")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
