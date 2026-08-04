//! LISTBOX/COMBOBOX item logic (selection notifications, hit-testing) and the
//! shared text-rendering helper (split from `controls.rs`).

use anyhow::Result;

use super::paint::fill_rect_clipped;
use super::{COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, control_items, deliver_command};
use crate::gdi32::ResolvedWindow;
use crate::gdi32::render_text_into_surface;
use crate::gdi32::{FontEngine, FontKey, ResolvedFont};
use crate::user32::{LBN_SELCHANGE, WinApiState, find_window, make_command_wparam};

/// Draw the first visible LISTBOX items (one line each), filling the
/// selected item's row with COLOR_HIGHLIGHT and rendering its glyphs in
/// COLOR_HIGHLIGHTTEXT (Windows' selected-listbox look).
pub(super) fn paint_item_lines(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    info: &ResolvedWindow,
    items: &[String],
    width: i32,
    height: i32,
    sel_index: i32,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(width);
    let bottom = y.saturating_add(height);
    let line_h = resolved.line_height();
    for (index, item) in items.iter().enumerate() {
        let line_y = y.saturating_add(i32::try_from(index).unwrap_or(0).saturating_mul(line_h));
        if line_y >= bottom {
            break;
        }
        let selected = sel_index >= 0 && i32::try_from(index).unwrap_or(-1) == sel_index;
        if selected {
            fill_rect_clipped(
                state,
                info,
                width,
                height,
                x,
                line_y,
                width,
                line_h,
                COLOR_HIGHLIGHT,
            );
        }
        let color = if selected { COLOR_HIGHLIGHTTEXT } else { 0 };
        render_control_text(
            state,
            engine,
            info.hwnd,
            info.width,
            info.height,
            x.saturating_add(2),
            line_y,
            item,
            color,
            Some((x, y, right, bottom)),
            font_engine,
            resolved,
            key,
        )?;
    }
    Ok(())
}

/// Render control text into the ancestor surface (TRANSPARENT background).
pub(super) fn render_control_text(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    top_hwnd: crate::handles::Hwnd,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    clip: Option<(i32, i32, i32, i32)>,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    state.present().ensure_surface(top_hwnd, width, height);
    let Some(surface) = state.present().surfaces.get_mut(&top_hwnd) else {
        return Ok(());
    };
    render_text_into_surface(
        engine,
        font_engine,
        &mut surface.pixels,
        surface.width,
        surface.height,
        x,
        y,
        text,
        color,
        clip,
        resolved,
        key,
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
