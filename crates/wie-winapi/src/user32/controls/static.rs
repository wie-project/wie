//! STATIC-class painting (labels; text-only) (split from `controls.rs`).

use anyhow::Result;

use super::listbox::render_control_text;
use super::{PaintCtx, PaintFont, Rect, TextGeom};
use crate::gdi32::ResolvedWindow;

/// Draw a single line of control text, vertically centered, black on the
/// control's face. `geom.tx` is the caller-computed left edge (centered or
/// padded); the glyphs are clipped to the control's rect.
pub(super) fn paint_label(
    ctx: &mut PaintCtx<'_>,
    info: &ResolvedWindow,
    text: &str,
    geom: TextGeom,
    pressed: bool,
    font: &mut PaintFont<'_>,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let line_h = font.resolved.line_height();
    let ty = info
        .offset_y
        .saturating_add(geom.height.saturating_sub(line_h).saturating_div(2))
        .max(info.offset_y);
    // Pressed buttons offset their caption one pixel down/right (classic 3D).
    let (tx, ty) = if pressed {
        (geom.tx.saturating_add(1), ty.saturating_add(1))
    } else {
        (geom.tx, ty)
    };
    let right = info.offset_x.saturating_add(geom.width);
    let bottom = info.offset_y.saturating_add(geom.height);
    render_control_text(
        ctx,
        info.hwnd,
        Rect {
            x: tx,
            y: ty,
            cx: i32::try_from(info.width).unwrap_or(0),
            cy: i32::try_from(info.height).unwrap_or(0),
        },
        text,
        0, // COLOR_BTNTEXT / COLOR_WINDOWTEXT: black
        Some((info.offset_x, info.offset_y, right, bottom)),
        font,
    )
}
