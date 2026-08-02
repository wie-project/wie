//! STATIC-class painting (labels; text-only) (split from `controls.rs`).

use anyhow::Result;

use super::listbox::render_control_text;
use crate::gdi32::ResolvedWindow;
use crate::gdi32::{FontEngine, FontKey, ResolvedFont};
use crate::user32::WinApiState;

/// Draw a single line of control text, vertically centered, black on the
/// control's face. `tx` is the caller-computed left edge (centered or padded);
/// the glyphs are clipped to the control's rect.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_label(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    info: &ResolvedWindow,
    text: &str,
    tx: i32,
    width: i32,
    height: i32,
    pressed: bool,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let line_h = resolved.line_height();
    let ty = info
        .offset_y
        .saturating_add(height.saturating_sub(line_h).saturating_div(2))
        .max(info.offset_y);
    // Pressed buttons offset their caption one pixel down/right (classic 3D).
    let (tx, ty) = if pressed {
        (tx.saturating_add(1), ty.saturating_add(1))
    } else {
        (tx, ty)
    };
    let right = info.offset_x.saturating_add(width);
    let bottom = info.offset_y.saturating_add(height);
    render_control_text(
        state,
        engine,
        info.hwnd,
        info.width,
        info.height,
        tx,
        ty,
        text,
        0, // COLOR_BTNTEXT / COLOR_WINDOWTEXT: black
        Some((info.offset_x, info.offset_y, right, bottom)),
        font_engine,
        resolved,
        key,
    )
}
