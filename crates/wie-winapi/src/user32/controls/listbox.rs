//! LISTBOX/COMBOBOX item logic (selection notifications, hit-testing) and the
//! shared text-rendering helper (split from `controls.rs`).

use anyhow::Result;

use super::paint::fill_rect_clipped;
use super::{
    control_items, deliver_command, Dimension, PaintCtx, PaintFont, COLOR_HIGHLIGHT,
    COLOR_HIGHLIGHTTEXT,
};
use crate::gdi32::render_text_into_surface;
use crate::gdi32::{IRect, ResolvedWindow};
use crate::user32::{find_window, make_command_wparam, WinApiState, LBN_SELCHANGE};

/// Draw the first visible LISTBOX items (one line each), filling the
/// selected item's row with COLOR_HIGHLIGHT and rendering its glyphs in
/// COLOR_HIGHLIGHTTEXT (Windows' selected-listbox look).
pub(super) fn paint_item_lines(
    ctx: &mut PaintCtx<'_>,
    info: &ResolvedWindow,
    items: &[String],
    area: Dimension,
    sel_index: i32,
    font: &mut PaintFont<'_>,
) -> Result<()> {
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(area.width);
    let bottom = y.saturating_add(area.height);
    let line_h = font.resolved.line_height();
    for (index, item) in items.iter().enumerate() {
        let line_y = y.saturating_add(i32::try_from(index).unwrap_or(0).saturating_mul(line_h));
        if line_y >= bottom {
            break;
        }
        let selected = sel_index >= 0 && i32::try_from(index).unwrap_or(-1) == sel_index;
        if selected {
            fill_rect_clipped(
                ctx.state,
                info,
                area,
                IRect::from_xywh(x, line_y, area.width, line_h),
                COLOR_HIGHLIGHT,
            );
        }
        let color = if selected { COLOR_HIGHLIGHTTEXT } else { 0 };
        render_control_text(
            ctx,
            info.hwnd,
            IRect::from_xywh(
                x.saturating_add(2),
                line_y,
                i32::try_from(info.width).unwrap_or(0),
                i32::try_from(info.height).unwrap_or(0),
            ),
            item,
            color,
            Some(IRect {
                left: x,
                top: y,
                right,
                bottom,
            }),
            font,
        )?;
    }
    Ok(())
}

/// Render control text into the ancestor surface (TRANSPARENT background).
pub(super) fn render_control_text(
    ctx: &mut PaintCtx<'_>,
    top_hwnd: crate::handles::Hwnd,
    rect: IRect,
    text: &str,
    color: u32,
    clip: Option<IRect>,
    font: &mut PaintFont<'_>,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    ctx.state.present().ensure_surface(
        top_hwnd,
        u32::try_from(rect.width()).unwrap_or(0),
        u32::try_from(rect.height()).unwrap_or(0),
    );
    let Some(surface) = ctx.state.present().surfaces.get_mut(&top_hwnd) else {
        return Ok(());
    };
    render_text_into_surface(
        ctx.engine,
        font.engine,
        &mut surface.pixels,
        surface.width,
        surface.height,
        rect.left,
        rect.top,
        text,
        color,
        // The rasterizer's clip is a plain (l,t,r,b) tuple; the control paint
        // path reasons in `IRect`s, so the bundle unwraps at the boundary.
        clip.map(|c| (c.left, c.top, c.right, c.bottom)),
        font.resolved,
        font.key,
    )
}

/// Send `LBN_SELCHANGE` as WM_COMMAND(MAKEWPARAM(id, LBN_SELCHANGE)) to the
/// parent — the LISTBOX selection changed.
pub(super) fn listbox_notify_change(state: &mut WinApiState, hwnd: u64) -> Result<Option<u64>> {
    let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
    let command_wparam = make_command_wparam(id, LBN_SELCHANGE);
    deliver_command(state, hwnd, command_wparam)
}

/// Which LISTBOX item row a client-relative click (packed `lParam`) falls on.
/// `None` when the click is outside the item rows or the list is empty.
#[must_use]
pub(super) fn listbox_hit_item(state: &WinApiState, hwnd: u64, long_parameter: u64) -> Option<i32> {
    let y_raw = u16::try_from((long_parameter >> 16) & 0xFFFF).unwrap_or(0);
    let y = i32::from(i16::from_ne_bytes(y_raw.to_ne_bytes()));
    let count = control_items(state, hwnd).len();
    if y < 0 || count == 0 {
        return None;
    }
    // One 16 px line per item, flush at the control's top edge.
    let row = y.saturating_div(16);
    if row >= i32::try_from(count).unwrap_or(0) {
        return None;
    }
    Some(row)
}
