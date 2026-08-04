//! BUTTON-class painting (push buttons) and the shared control-paint entry
//! (split from `controls.rs`).

use anyhow::Result;

use super::edit::paint_edit;
use super::listbox::paint_item_lines;
use super::listbox::render_control_text;
use super::r#static::paint_label;
use super::{
    COLOR_BTNFACE, COLOR_BTNFACE_PRESSED, COLOR_BTNHIGHLIGHT, COLOR_BTNSHADOW, COLOR_WINDOW,
    ControlClassKind, ControlState, PaintCtx, PaintFont, Rect, TextGeom, control_items,
    control_sel_index, control_state,
};
use crate::gdi32::ResolvedWindow;
use crate::gdi32::fill_rect_surface;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::{FontEngine, FontKey, ResolvedFont};
use crate::state::WindowFlags;
use crate::user32::{WinApiState, find_window};

/// Paint a control into its ancestor's surface at its parent-relative offset.
pub(super) fn paint_control(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    hwnd: u64,
    kind: ControlClassKind,
) -> Result<()> {
    tracing::trace!(target: "wiegui", kind = ?kind, hwnd, "control paint");
    let Some(info) = resolve_window_ancestor(state, hwnd) else {
        return Ok(());
    };
    let text = find_window(state, hwnd).map_or_else(String::new, |w| w.control_text.clone());
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));

    // The status bar's raised strip (BTNFACE face + the light/dark client
    // edges + the part grooves) renders WITHOUT a font, so the strip is
    // visible even before any text is set; the per-part text is drawn after
    // the font resolution in the match below (Task 3.1).
    if kind == ControlClassKind::StatusBar {
        paint_status_bar_strip(state, &info, width, height);
        // The grooves follow the SB_SETPARTS layout, which is font-free too;
        // the part_rights clone here mirrors the one in paint_status_bar_parts.
        let part_rights = match control_state(state, hwnd) {
            Some(ControlState::StatusBar { part_rights, .. }) => part_rights.clone(),
            _ => Vec::new(),
        };
        paint_status_bar_separators(state, &info, width, height, &part_rights);
    }

    let pressed = find_window(state, hwnd).is_some_and(|w| w.flags.contains(WindowFlags::PRESSED));
    let items = control_items(state, hwnd).to_vec();
    let sel_index = control_sel_index(state, hwnd);

    // Controls draw with the font a WM_SETFONT stored on the window (notepad
    // sends one to its EDIT right after creation); a window without one — or
    // with an unknown HFONT — falls back to the system default (sans-serif
    // 16 px). Both resolve through the same engine cache. The engine is taken
    // out of gdi state so the paint can pass `&mut state` and `&mut font_engine`
    // side by side (a plain field cannot be split-borrowed alongside `state`);
    // it is put back unconditionally after the body. This is safe under the
    // single shared WinApiState mutex: every API handler — this WM_PAINT and
    // any concurrent one on another host thread — runs while holding it, so
    // the take and the put cannot interleave.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = FontKey::default();
    let key_and_resolved = match crate::gdi32::window_font_resolution(state, hwnd, &mut font_engine)
    {
        Some(key_and_resolved) => Some(key_and_resolved),
        None => font_engine
            .resolve(&default_key, 16)
            .map(|resolved| (default_key, resolved)),
    };
    let result = (|| -> Result<()> {
        let Some((key, resolved)) = &key_and_resolved else {
            // No system font: paint faces/borders but skip the text.
            return Ok(());
        };
        match kind {
            ControlClassKind::Button => {
                paint_face_and_border(state, &info, width, height, pressed);
                // The ampersand is a mnemonic marker, not caption glyph.
                let caption = strip_mnemonics(&text);
                let tx = centered_text_x(&info, width, &caption, &mut font_engine, resolved, key);
                paint_label(
                    &mut PaintCtx { state, engine },
                    &info,
                    &caption,
                    TextGeom { tx, width, height },
                    pressed,
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved,
                        key,
                    },
                )?;
            }
            ControlClassKind::Static => {
                // COLOR_BTNFACE, not COLOR_WINDOW: a label sits on the dialog
                // face and must not show as a white box (full WM_CTLCOLOR* is
                // deferred).
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x,
                    info.offset_y,
                    width,
                    height,
                    COLOR_BTNFACE,
                );
                let caption = strip_mnemonics(&text);
                let tx = info.offset_x.saturating_add(2);
                paint_label(
                    &mut PaintCtx { state, engine },
                    &info,
                    &caption,
                    TextGeom { tx, width, height },
                    false,
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved,
                        key,
                    },
                )?;
            }
            ControlClassKind::Edit => {
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x,
                    info.offset_y,
                    width,
                    height,
                    COLOR_WINDOW,
                );
                stroke_border(state, &info, width, height, 0x0000_0000);
                let tx = info.offset_x.saturating_add(2);
                paint_edit(
                    &mut PaintCtx { state, engine },
                    &info,
                    &text,
                    TextGeom { tx, width, height },
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved,
                        key,
                    },
                )?;
            }
            ControlClassKind::ListBox => {
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x,
                    info.offset_y,
                    width,
                    height,
                    COLOR_WINDOW,
                );
                stroke_border(state, &info, width, height, 0x0000_0000);
                paint_item_lines(
                    &mut PaintCtx { state, engine },
                    &info,
                    &items,
                    width,
                    height,
                    sel_index,
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved,
                        key,
                    },
                )?;
            }
            ControlClassKind::ComboBox => {
                paint_face_and_border(state, &info, width, height, false);
                let first = items.first().map_or("", String::as_str);
                let tx = info.offset_x.saturating_add(4);
                paint_label(
                    &mut PaintCtx { state, engine },
                    &info,
                    first,
                    TextGeom { tx, width, height },
                    false,
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved,
                        key,
                    },
                )?;
            }
            // The strip face/edges were painted before the font resolution;
            // this arm only draws each part's text clipped to its cell.
            ControlClassKind::StatusBar => {
                paint_status_bar_parts(
                    state,
                    engine,
                    &info,
                    hwnd,
                    &mut font_engine,
                    resolved,
                    key,
                )?;
            }
        }
        Ok(())
    })();
    state.gdi_state().font_engine = font_engine;
    result
}

/// Paint a STATUSCLASSNAMEW strip: the BTNFACE face plus the classic raised
/// client edge — a light (COLOR_BTNHIGHLIGHT) line along the top and the
/// shadow (COLOR_BTNSHADOW) line along the bottom (Task 3.1).
fn paint_status_bar_strip(state: &mut WinApiState, info: &ResolvedWindow, width: i32, height: i32) {
    if width <= 0 || height <= 0 {
        return;
    }
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        width,
        height,
        COLOR_BTNFACE,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        width,
        1,
        COLOR_BTNHIGHLIGHT,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y.saturating_add(height).saturating_sub(1),
        width,
        1,
        COLOR_BTNSHADOW,
    );
}

/// Draw the classic comctl32 vertical groove at every part boundary.
///
/// Real Windows divides status-bar parts with a sunken groove: a BTNSHADOW
/// vertical line at the boundary column with a BTNHIGHLIGHT line immediately
/// to its right — shadow-left/highlight-right, the `EDGE_SUNKEN` direction,
/// mirroring the raised client edge (`paint_status_bar_strip`). The lines
/// span only the interior rows (one below the top highlight, one above the
/// bottom shadow) so the corners join the client edges cleanly. The last
/// part always extends to the right edge of the strip and gets no right
/// separator; a boundary at or past the strip's edge leaves no room either.
fn paint_status_bar_separators(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    width: i32,
    height: i32,
    part_rights: &[i32],
) {
    if part_rights.is_empty() || height < 3 {
        return;
    }
    let top = info.offset_y.saturating_add(1);
    let rows = height.saturating_sub(2);
    let interior = part_rights.len().saturating_sub(1);
    for right in part_rights.iter().take(interior) {
        // -1 (SB_SETPARTS) means "extend to the right edge".
        let boundary = if *right < 0 { width } else { *right };
        if boundary >= width {
            continue;
        }
        let bx = info.offset_x.saturating_add(boundary);
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            bx,
            top,
            1,
            rows,
            COLOR_BTNSHADOW,
        );
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            bx.saturating_add(1),
            top,
            1,
            rows,
            COLOR_BTNHIGHLIGHT,
        );
    }
}

/// Draw each status-bar part's text, left-aligned in its cell with a small
/// horizontal inset and vertically centered, CLIPPED to the cell so a long
/// text cannot bleed into the next part. The last part always extends to the
/// right edge; with no `SB_SETPARTS` the whole strip is one part.
fn paint_status_bar_parts(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    info: &ResolvedWindow,
    hwnd: u64,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    // The control's own client size (the part cells are laid out inside it).
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));
    let part_rights = match control_state(state, hwnd) {
        Some(ControlState::StatusBar { part_rights, .. }) => part_rights.clone(),
        _ => Vec::new(),
    };
    let parts = if part_rights.is_empty() {
        1
    } else {
        part_rights.len()
    };
    let line_h = resolved.line_height();
    // Vertically centered between the strip's edges, at least one row below
    // the top border so the ink never touches the client edge.
    let ty = info
        .offset_y
        .saturating_add(height.saturating_sub(line_h).saturating_div(2))
        .max(info.offset_y.saturating_add(1));
    let strip_bottom = info.offset_y.saturating_add(height);
    let mut left = 0_i32;
    for index in 0..parts {
        let right = if part_rights.is_empty() {
            width
        } else {
            let value = part_rights.get(index).copied().unwrap_or(width);
            if value < 0 { width } else { value }
        };
        let cell_left = left;
        left = right;
        let text = crate::comctl32::status_part_text(state, hwnd, index);
        if text.is_empty() {
            continue;
        }
        let tx = info.offset_x.saturating_add(cell_left).saturating_add(3);
        render_control_text(
            &mut PaintCtx { state, engine },
            info.hwnd,
            Rect {
                x: tx,
                y: ty,
                cx: i32::try_from(info.width).unwrap_or(0),
                cy: i32::try_from(info.height).unwrap_or(0),
            },
            &text,
            0, // COLOR_BTNTEXT / COLOR_WINDOWTEXT: black
            Some((
                info.offset_x.saturating_add(cell_left),
                info.offset_y,
                info.offset_x.saturating_add(right),
                strip_bottom,
            )),
            &mut PaintFont {
                engine: font_engine,
                resolved,
                key,
            },
        )?;
    }
    Ok(())
}

/// Fill a control's face (COLOR_BTNFACE) and draw its 1 px BTNSHADOW border.
fn paint_face_and_border(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    width: i32,
    height: i32,
    pressed: bool,
) {
    let face = if pressed {
        COLOR_BTNFACE_PRESSED
    } else {
        COLOR_BTNFACE
    };
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        width,
        height,
        face,
    );
    stroke_border(state, info, width, height, COLOR_BTNSHADOW);
}

/// Draw a 1 px border around a control's rect.
fn stroke_border(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    width: i32,
    height: i32,
    color: u32,
) {
    if width <= 0 || height <= 0 {
        return;
    }
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(width).saturating_sub(1);
    let bottom = y.saturating_add(height).saturating_sub(1);
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        y,
        width,
        1,
        color,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        bottom,
        width,
        1,
        color,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        y,
        1,
        height,
        color,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        right,
        y,
        1,
        height,
        color,
    );
}

/// Remove `&` mnemonic markers from a caption so they are not rendered
/// literally (the underline + Alt activation are deferred). `&&` is the
/// escaped form of a literal ampersand, matching Windows.
#[must_use]
fn strip_mnemonics(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut iter = text.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '&' {
            if iter.peek() == Some(&'&') {
                out.push('&');
                iter.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// X origin for a horizontally centered single-line control caption.
fn centered_text_x(
    info: &ResolvedWindow,
    width: i32,
    text: &str,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> i32 {
    let text_w = font_engine.text_advance(resolved, key, text, text.chars().count());
    info.offset_x
        .saturating_add(width.saturating_sub(text_w).saturating_div(2))
}

#[cfg(test)]
mod tests {
    use super::strip_mnemonics;

    #[test]
    fn strip_mnemonics_drops_single_marker() {
        assert_eq!(strip_mnemonics("&About"), "About");
        assert_eq!(strip_mnemonics("&Quit"), "Quit");
        assert_eq!(strip_mnemonics("Plain"), "Plain");
    }

    #[test]
    fn strip_mnemonics_keeps_doubled_ampersand() {
        // `&&` is the escaped form and renders as a literal `&`.
        assert_eq!(strip_mnemonics("A&&B"), "A&B");
        assert_eq!(strip_mnemonics("&&"), "&");
        assert_eq!(strip_mnemonics("&A&&B"), "A&B");
    }

    #[test]
    fn strip_mnemonics_empty() {
        assert_eq!(strip_mnemonics(""), "");
    }
}
