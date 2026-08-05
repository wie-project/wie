//! Shared control-paint helpers: clipped fills and guest text copies
//! (split from `controls.rs`).

use anyhow::Result;

use super::Dimension;
use crate::gdi32::fill_rect_surface;
use crate::gdi32::{IRect, ResolvedWindow};
use crate::user32::{WinApiState, write_guest_ansi_c_string, write_guest_utf16_c_string};

/// Fill a rect with `color`, clipped to the control's own bounds so a
/// selection or caret running past the right edge cannot bleed into the
/// ancestor surface.
pub(super) fn fill_rect_clipped(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    control: Dimension,
    rect: IRect,
    color: u32,
) {
    let x0 = rect.left.max(info.offset_x);
    let y0 = rect.top.max(info.offset_y);
    let x1 = rect.right.min(info.offset_x.saturating_add(control.width));
    let y1 = rect
        .bottom
        .min(info.offset_y.saturating_add(control.height));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x0,
        y0,
        x1.saturating_sub(x0),
        y1.saturating_sub(y0),
        color,
    );
}

/// Copy a control's text into a guest buffer (WM_GETTEXT / LB_GETTEXT).
pub(super) fn write_control_text(
    engine: &mut dyn wie_cpu::CpuEngine,
    unicode: bool,
    buffer_ptr: u64,
    max_characters: u64,
    text: &str,
) -> Result<u64> {
    if buffer_ptr == 0 {
        return Ok(0);
    }
    let capacity = usize::try_from(max_characters).unwrap_or(0);
    let copied = if unicode {
        write_guest_utf16_c_string(engine, buffer_ptr, capacity, text)?
    } else {
        write_guest_ansi_c_string(engine, buffer_ptr, capacity, text)?
    };
    Ok(u64::try_from(copied).unwrap_or(0))
}
