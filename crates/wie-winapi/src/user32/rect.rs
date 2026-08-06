//! USER32 rectangle helpers — pure rect math.

use anyhow::{Context, Result};

use crate::guest_layout::WinRect;
use crate::user32::{low_i32, with_typed_read, with_typed_write};
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
        // Read-all → compute → write-all (one shared-lock borrow per view);
        // the RECT layout + pinned offsets live in `crate::guest_layout::WinRect`.
        let (left, top, right, bottom) =
            with_typed_read::<WinRect, _, _>(engine, rect_ptr, |rect| {
                Ok((rect.left, rect.top, rect.right, rect.bottom))
            })
            .context("failed to read RECT for InflateRect")?;

        with_typed_write::<WinRect, _, _>(engine, rect_ptr, |rect| {
            rect.left = left.saturating_add(dx.saturating_neg());
            rect.top = top.saturating_add(dy.saturating_neg());
            rect.right = right.saturating_add(dx);
            rect.bottom = bottom.saturating_add(dy);
            Ok(())
        })
        .context("failed to write RECT for InflateRect")?;
    }

    ctx.finish(u64::from(success))
}
