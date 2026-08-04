//! USER32 rectangle helpers — pure rect math.

use anyhow::{Context, Result};

use crate::guest_memory::{
    checked_field_address, read_i32 as read_guest_i32, write_i32 as write_guest_i32,
};
use crate::user32::low_i32;
use crate::{HandlerContext, WinApiHandlerResult};

/// Handles `USER32.dll!InflateRect` — grow (positive) or shrink (negative) a
/// RECT in place.
///
/// Real Win32 semantics: the rect grows by `dx` on left AND right and by `dy`
/// on top AND bottom — `left -= dx; top -= dy; right += dx; bottom += dy`.
/// (The design brief's shorthand "l/t/r/b += dx/dy" would shift the rect and
/// shrink its width by 2·dx, so the documented Win32 behavior wins.)
pub fn handle_inflate_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let rect_ptr = engine
        .read_rcx()
        .context("failed to read RCX for InflateRect")?;
    let dx_raw = engine
        .read_rdx()
        .context("failed to read RDX for InflateRect")?;
    let dy_raw = engine
        .read_r8()
        .context("failed to read R8 for InflateRect")?;

    let dx = low_i32(dx_raw, "InflateRect dx")?;
    let dy = low_i32(dy_raw, "InflateRect dy")?;

    let success = rect_ptr != 0;
    if success {
        // RECT (Win64): LONG left @0, top @4, right @8, bottom @12.
        for (offset, name, delta) in [
            (0, "RECT.left", -dx),
            (4, "RECT.top", -dy),
            (8, "RECT.right", dx),
            (12, "RECT.bottom", dy),
        ] {
            let value = read_guest_i32(engine, checked_field_address(rect_ptr, offset, name))
                .with_context(|| format!("failed to read {name} for InflateRect"))?;
            write_guest_i32(
                engine,
                checked_field_address(rect_ptr, offset, name),
                value.saturating_add(delta),
            )
            .with_context(|| format!("failed to write {name} for InflateRect"))?;
        }
    }

    let return_address = engine
        .return_from_win64_api(u64::from(success))
        .context("failed to return from InflateRect")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(success),
    })
}
