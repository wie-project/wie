use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

/// WINMM timer/audio handle tables (`timeSetEvent`, `waveOut*`), owned by
/// this module and heap-allocated on first load via `DllId::Winmm`.
#[derive(Debug, Default)]
pub struct WinmmState {}

/// Dispatch a `WINMM.dll` export by name (case-insensitive) — the string
/// path for APIs beyond the dense `timeGetTime` row.
pub fn dispatch_winmm_extra(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let _ = (ctx, name);
    Ok(None)
}

/// Handles `WINMM.dll!timeGetTime`.
///
/// B5: returns the same ms-since-session-epoch value the in-guest stub reads
/// from the host-written guest clock table (slot 2), so the host fallback and
/// the stub can never disagree about how much time has passed.
pub fn handle_time_get_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let return_value = crate::kernel32::clock::tick_count_32();

    ctx.finish(return_value)
}
