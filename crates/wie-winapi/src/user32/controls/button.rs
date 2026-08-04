//! BUTTON-class painting (push buttons) and the shared control-paint entry
//! (split from `controls.rs`).

use anyhow::Result;

use super::edit::edit_dirty_band;
use super::edit::paint_edit;
use super::listbox::paint_item_lines;
use super::listbox::render_control_text;
use super::r#static::paint_label;
use super::{
    COLOR_BTNFACE, COLOR_BTNFACE_PRESSED, COLOR_BTNHIGHLIGHT, COLOR_BTNSHADOW, COLOR_WINDOW,
    ControlClassKind, ControlState, Dimension, LabelInvalidRect, LabelInvalidation, PaintCtx,
    PaintFont, TextGeom, control_items, control_sel_index, control_state, control_state_mut,
};
use crate::gdi32::fill_rect_surface;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::{FontEngine, FontKey, ResolvedFont};
use crate::gdi32::{IRect, ResolvedWindow};
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
    let size = find_window(state, hwnd).map_or(
        Dimension {
            width: 0,
            height: 0,
        },
        |w| Dimension {
            width: w.width,
            height: w.height,
        },
    );

    // The status bar's raised strip (BTNFACE face + the light/dark client
    // edges + the part grooves) renders WITHOUT a font, so the strip is
    // visible even before any text is set; the per-part text is drawn after
    // the font resolution in the match below (Task 3.1).
    if kind == ControlClassKind::StatusBar {
        paint_status_bar_strip(state, &info, size);
        // The grooves follow the SB_SETPARTS layout, which is font-free too;
        // the part_rights clone here mirrors the one in paint_status_bar_parts.
        let part_rights = match control_state(state, hwnd) {
            Some(ControlState::StatusBar { part_rights, .. }) => part_rights.clone(),
            _ => Vec::new(),
        };
        paint_status_bar_separators(state, &info, size, &part_rights);
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
                // The repaint scope: the pending face/caption rect, or the
                // whole client — a clean scope, a structural change, a stale
                // rect whose size no longer matches, or the first paint. The
                // erase covers exactly the scope, so the published frame's
                // region is the true changed area (the B3 dirty-region
                // machinery the EDIT's row band feeds).
                let dirty = control_dirty_rect(state, hwnd, size);
                if dirty == IRect::from_xywh(0, 0, size.width, size.height) {
                    // Full repaint: face + border + caption (the pre-scope
                    // path, byte-identical).
                    paint_face_and_border(state, &info, size, pressed);
                } else {
                    // Partial repaint: erase only the dirty rect with the
                    // current face color. The border is redrawn only when the
                    // dirty rect covers a border pixel (the erase overpaints
                    // it otherwise); it is unchanged by press/text scopes.
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
                        info.offset_x.saturating_add(dirty.left),
                        info.offset_y.saturating_add(dirty.top),
                        dirty.width(),
                        dirty.height(),
                        face,
                    );
                    if rect_touches_border(dirty, size) {
                        stroke_border(state, &info, size, COLOR_BTNSHADOW);
                    }
                }
                // The caption is always redrawn: the dirty rect is the union
                // of the old and new caption rects (or the face), so the new
                // glyphs land inside the erased area and the previous glyphs
                // are gone.
                // The ampersand is a mnemonic marker, not caption glyph.
                let caption = strip_mnemonics(&text);
                let tx =
                    centered_text_x(&info, size.width, &caption, &mut font_engine, resolved, key);
                paint_label(
                    &mut PaintCtx { state, engine },
                    &info,
                    &caption,
                    TextGeom {
                        tx,
                        width: size.width,
                        height: size.height,
                    },
                    pressed,
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved,
                        key,
                    },
                )?;
                consume_control_invalidation(state, hwnd);
            }
            ControlClassKind::Static => {
                // COLOR_BTNFACE, not COLOR_WINDOW: a label sits on the dialog
                // face and must not show as a white box (full WM_CTLCOLOR* is
                // deferred). The erase covers only the pending caption rect
                // (or the whole client for a full repaint), so the region
                // reports the true changed area.
                let dirty = control_dirty_rect(state, hwnd, size);
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x.saturating_add(dirty.left),
                    info.offset_y.saturating_add(dirty.top),
                    dirty.width(),
                    dirty.height(),
                    COLOR_BTNFACE,
                );
                let caption = strip_mnemonics(&text);
                let tx = info.offset_x.saturating_add(2);
                paint_label(
                    &mut PaintCtx { state, engine },
                    &info,
                    &caption,
                    TextGeom {
                        tx,
                        width: size.width,
                        height: size.height,
                    },
                    false,
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved,
                        key,
                    },
                )?;
                consume_control_invalidation(state, hwnd);
            }
            ControlClassKind::Edit => {
                // Erase only the dirty rows — the pending invalid row band,
                // or the whole client for a full repaint — so a caret blink
                // or a typed character does not wipe the untouched rows
                // (`paint_edit` redraws exactly the same band). The border is
                // stroked after the erase exactly like the full fill was, so
                // the edge pixels are preserved.
                let edit_style = find_window(state, hwnd).map_or(0, |w| w.style);
                let (band_top, band_bottom) = {
                    let mut advance = |ch: char| font_engine.char_advance(resolved, key, ch);
                    edit_dirty_band(
                        state,
                        hwnd,
                        &text,
                        size,
                        resolved.line_height(),
                        edit_style,
                        &mut advance,
                    )
                };
                if band_bottom > band_top {
                    fill_rect_surface(
                        state,
                        info.hwnd,
                        info.width,
                        info.height,
                        info.offset_x,
                        info.offset_y.saturating_add(band_top),
                        size.width,
                        band_bottom.saturating_sub(band_top),
                        COLOR_WINDOW,
                    );
                }
                stroke_border(state, &info, size, 0x0000_0000);
                let tx = info.offset_x.saturating_add(2);
                paint_edit(
                    &mut PaintCtx { state, engine },
                    &info,
                    &text,
                    TextGeom {
                        tx,
                        width: size.width,
                        height: size.height,
                    },
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved,
                        key,
                    },
                )?;
            }
            ControlClassKind::ListBox => {
                // The repaint scope: the pending dirty rect (a scroll's
                // old+new visible bands, a selection change's rows, an item
                // append's row), or the whole client — a clean scope, a stale
                // rect whose size no longer matches, or the first paint. The
                // erase covers exactly the scope, so the published frame's
                // region is the true changed area (the same B3 dirty-region
                // machinery the EDIT/button bands feed). Only the rows inside
                // the scope render (`paint_item_lines` takes the rect), so a
                // partial repaint never wipes the untouched rows.
                let dirty = control_dirty_rect(state, hwnd, size);
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x.saturating_add(dirty.left),
                    info.offset_y.saturating_add(dirty.top),
                    dirty.width(),
                    dirty.height(),
                    COLOR_WINDOW,
                );
                // The erase overpainted the 1 px border wherever the band
                // reaches it — re-stroke only those edges (the row bands span
                // the client's full width and start at its top edge, so a
                // mid-client band wipes the left/right edges but not the top
                // or bottom ones).
                stroke_border_partial(state, &info, size, dirty, 0x0000_0000);
                paint_item_lines(
                    &mut PaintCtx { state, engine },
                    &info,
                    &items,
                    size,
                    sel_index,
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved,
                        key,
                    },
                    dirty,
                )?;
                consume_control_invalidation(state, hwnd);
            }
            ControlClassKind::ComboBox => {
                paint_face_and_border(state, &info, size, false);
                let first = items.first().map_or("", String::as_str);
                let tx = info.offset_x.saturating_add(4);
                paint_label(
                    &mut PaintCtx { state, engine },
                    &info,
                    first,
                    TextGeom {
                        tx,
                        width: size.width,
                        height: size.height,
                    },
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
                let (status_key, status_resolved) =
                    status_bar_part_font(state, hwnd, &mut font_engine, key, resolved);
                paint_status_bar_parts(
                    state,
                    engine,
                    &info,
                    hwnd,
                    &mut PaintFont {
                        engine: &mut font_engine,
                        resolved: &status_resolved,
                        key: &status_key,
                    },
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
fn paint_status_bar_strip(state: &mut WinApiState, info: &ResolvedWindow, size: Dimension) {
    if size.width <= 0 || size.height <= 0 {
        return;
    }
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        size.width,
        size.height,
        COLOR_BTNFACE,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        size.width,
        1,
        COLOR_BTNHIGHLIGHT,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y.saturating_add(size.height).saturating_sub(1),
        size.width,
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
    size: Dimension,
    part_rights: &[i32],
) {
    if part_rights.is_empty() || size.height < 3 {
        return;
    }
    let top = info.offset_y.saturating_add(1);
    let rows = size.height.saturating_sub(2);
    let interior = part_rights.len().saturating_sub(1);
    for right in part_rights.iter().take(interior) {
        // -1 (SB_SETPARTS) means "extend to the right edge".
        let boundary = if *right < 0 { size.width } else { *right };
        if boundary >= size.width {
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

/// Horizontal inset (px) on each side of a status-bar part's text cell, so
/// the ink never touches the cell boundary or the next part's groove.
const STATUS_BAR_TEXT_INSET: i32 = 3;

/// The px size a status bar with NO `WM_SETFONT` draws its part text at.
///
/// Real Windows defaults such a bar to the DEFAULT_GUI_FONT (~12 px); WIE's
/// 16 px system default is ~25% wider, and RNotepad lays its parts out for
/// the smaller font WITHOUT measuring the text: `DIALOG_StatusBarAlignParts`
/// sets the EOL cell ("Windows (CR + LF)") to a FIXED 120 px box
/// (`max(client_w - 120, 240)` right edge, part 0 at `max(client_w - 240,
/// 120)`). At 16 px that text is ~125 px and the last glyph clips at the 120
/// px boundary — the ")" lands under the next part. Resolving the no-font
/// bar at the same 13 px the guest uses for its own UI font keeps the fixed
/// geometry working, exactly like real Windows' default-GUI-font behavior.
const STATUS_BAR_DEFAULT_FONT_PX: i32 = 13;

/// The font the status-bar part text renders with.
///
/// A bar with a `WM_SETFONT` stored font keeps the paint's normal resolution
/// (the stored font or the 16 px system default fallback). A bar without one
/// — RNotepad never sends WM_SETFONT to its status bar — drops to
/// [`STATUS_BAR_DEFAULT_FONT_PX`] instead of the 16 px default (see the
/// constant's doc). The caller's `key`/`resolved` are owned so the override
/// can return a freshly resolved pair.
fn status_bar_part_font(
    state: &mut WinApiState,
    hwnd: u64,
    font_engine: &mut FontEngine,
    key: &FontKey,
    resolved: &ResolvedFont,
) -> (FontKey, ResolvedFont) {
    let stored = find_window(state, hwnd)
        .map(|w| w.font_handle)
        .unwrap_or(crate::handles::Hfont::NULL);
    if stored != crate::handles::Hfont::NULL {
        return (key.clone(), resolved.clone());
    }
    font_engine
        .resolve(key, STATUS_BAR_DEFAULT_FONT_PX)
        .map_or((key.clone(), resolved.clone()), |smaller| {
            (key.clone(), smaller)
        })
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
    font: &mut PaintFont<'_>,
) -> Result<()> {
    // The control's own client size (the part cells are laid out inside it).
    let size = find_window(state, hwnd).map_or(
        Dimension {
            width: 0,
            height: 0,
        },
        |w| Dimension {
            width: w.width,
            height: w.height,
        },
    );
    let part_rights = match control_state(state, hwnd) {
        Some(ControlState::StatusBar { part_rights, .. }) => part_rights.clone(),
        _ => Vec::new(),
    };
    let parts = if part_rights.is_empty() {
        1
    } else {
        part_rights.len()
    };
    let line_h = font.resolved.line_height();
    // Vertically centered between the strip's edges, at least one row below
    // the top border so the ink never touches the client edge.
    let ty = info
        .offset_y
        .saturating_add(size.height.saturating_sub(line_h).saturating_div(2))
        .max(info.offset_y.saturating_add(1));
    let strip_bottom = info.offset_y.saturating_add(size.height);
    let mut left = 0_i32;
    for index in 0..parts {
        let right = if part_rights.is_empty() {
            size.width
        } else {
            let value = part_rights.get(index).copied().unwrap_or(size.width);
            if value < 0 { size.width } else { value }
        };
        let cell_left = left;
        left = right;
        let text = crate::comctl32::status_part_text(state, hwnd, index);
        if text.is_empty() {
            continue;
        }
        let tx = info
            .offset_x
            .saturating_add(cell_left)
            .saturating_add(STATUS_BAR_TEXT_INSET);
        // The clip mirrors the left inset on the right: text stops
        // `STATUS_BAR_TEXT_INSET` px before the cell edge (real Windows
        // status-bar cells carry the same margin), so the last glyph never
        // touches the boundary or the next part's groove.
        let clip_right = right.saturating_sub(STATUS_BAR_TEXT_INSET);
        render_control_text(
            &mut PaintCtx { state, engine },
            info.hwnd,
            IRect::from_xywh(
                tx,
                ty,
                i32::try_from(info.width).unwrap_or(0),
                i32::try_from(info.height).unwrap_or(0),
            ),
            &text,
            0, // COLOR_BTNTEXT / COLOR_WINDOWTEXT: black
            Some(IRect {
                left: info.offset_x.saturating_add(cell_left),
                top: info.offset_y,
                right: info.offset_x.saturating_add(clip_right),
                bottom: strip_bottom,
            }),
            font,
        )?;
    }
    Ok(())
}

/// Fill a control's face (COLOR_BTNFACE) and draw its 1 px BTNSHADOW border.
fn paint_face_and_border(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    size: Dimension,
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
        size.width,
        size.height,
        face,
    );
    stroke_border(state, info, size, COLOR_BTNSHADOW);
}

/// Draw a 1 px border around a control's rect.
fn stroke_border(state: &mut WinApiState, info: &ResolvedWindow, size: Dimension, color: u32) {
    if size.width <= 0 || size.height <= 0 {
        return;
    }
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(size.width).saturating_sub(1);
    let bottom = y.saturating_add(size.height).saturating_sub(1);
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        y,
        size.width,
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
        size.width,
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
        size.height,
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
        size.height,
        color,
    );
}

/// Re-stroke ONLY the 1 px border edges a partial repaint overpainted — the
/// LISTBOX's row bands span the client's full width and start at its top
/// edge, so a mid-client erase wipes the left/right edges (and the top/bottom
/// only when the band reaches them) but must not re-draw untouched edges:
/// the border fill marks the surface dirty, so an over-eager full re-stroke
/// would widen the published region beyond the true changed band. The edges
/// are clipped to the dirty rect's vertical extent so the re-stroked pixels
/// always land inside the erased area.
fn stroke_border_partial(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    size: Dimension,
    dirty: IRect,
    color: u32,
) {
    if size.width <= 0 || size.height <= 0 {
        return;
    }
    let (x, y) = (info.offset_x, info.offset_y);
    let band_top = dirty.top.max(0);
    let band_bottom = dirty.bottom.min(size.height);
    if dirty.top <= 0 {
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            x,
            y,
            size.width,
            1,
            color,
        );
    }
    if dirty.bottom >= size.height {
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            x,
            y.saturating_add(size.height).saturating_sub(1),
            size.width,
            1,
            color,
        );
    }
    if dirty.left <= 0 && band_bottom > band_top {
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            x,
            y.saturating_add(band_top),
            1,
            band_bottom.saturating_sub(band_top),
            color,
        );
    }
    if dirty.right >= size.width && band_bottom > band_top {
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            x.saturating_add(size.width).saturating_sub(1),
            y.saturating_add(band_top),
            1,
            band_bottom.saturating_sub(band_top),
            color,
        );
    }
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

// ── Rect-level invalidation (the label-control optimization lane) ────────
//
// The BUTTON/STATIC/LISTBOX repaint scopes mirror the EDIT's row bands at
// rect granularity: the mutating ops mark the sub-rect they changed (a pressed
// state change marks the face, a caption change the caption rect, a listbox
// scroll the old+new visible bands), and `paint_control` erases exactly that
// rect — the erase fill is what feeds the B3 dirty-region accumulator, so the
// published frame's `region` reports the true changed area instead of the full
// control rect. The first paint (and every structural change — a font change,
// a resize) stays full: the surface behind a never-painted control is
// undefined, so a partial repaint would leave holes.

/// The client-relative rect a BUTTON/STATIC/LISTBOX must erase and repaint on
/// its next paint: the pending invalidation rect (when still valid against the
/// CURRENT size), or the WHOLE client — a clean/full scope, a stale rect whose
/// size stamp no longer matches (a resize reflowed the layout), or the first
/// paint of a never-painted control. `paint_control` erases exactly the
/// returned rect. Mirrors `edit_dirty_band` for the rect-scope controls.
fn control_dirty_rect(state: &WinApiState, hwnd: u64, size: Dimension) -> IRect {
    let pending = match control_state(state, hwnd) {
        Some(ControlState::Button { invalidation, .. }) => Some(*invalidation),
        Some(ControlState::Static { invalidation }) => Some(*invalidation),
        Some(ControlState::ListBox { invalidation, .. }) => Some(*invalidation),
        _ => None,
    };
    match pending {
        Some(LabelInvalidation::Rect(rect))
            if rect.width == size.width && rect.height == size.height =>
        {
            rect.rect
        }
        _ => IRect::from_xywh(0, 0, size.width, size.height),
    }
}

/// Union `rect` (client-relative, computed against `width`×`height`) into a
/// label control's pending invalidation. A pending full repaint stays full
/// (sticky — a structural change can never be narrowed by a later partial
/// mark); a pending rect computed at a different size is stale (the layout
/// reflowed) and escalates to full; two rects at the same size union.
fn union_label_invalid(
    current: LabelInvalidation,
    rect: IRect,
    width: i32,
    height: i32,
) -> LabelInvalidation {
    match current {
        LabelInvalidation::Full => LabelInvalidation::Full,
        LabelInvalidation::Rect(pending) if pending.width != width || pending.height != height => {
            LabelInvalidation::Full
        }
        LabelInvalidation::Rect(pending) => LabelInvalidation::Rect(LabelInvalidRect {
            rect: union_rect(pending.rect, rect),
            width,
            height,
        }),
        LabelInvalidation::Clean => LabelInvalidation::Rect(LabelInvalidRect {
            rect,
            width,
            height,
        }),
    }
}

/// The smallest axis-aligned rect covering both inputs.
fn union_rect(a: IRect, b: IRect) -> IRect {
    IRect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

/// The overlap of two rects (empty when they do not overlap).
fn intersect_rect(a: IRect, b: IRect) -> IRect {
    IRect {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    }
}

/// Whether `rect` (client-relative) covers any pixel of a control's 1 px
/// border — the erase filled it with the face color, so the border must be
/// re-stroked.
fn rect_touches_border(rect: IRect, size: Dimension) -> bool {
    rect.left <= 0 || rect.top <= 0 || rect.right >= size.width || rect.bottom >= size.height
}

/// Mark `rect` (client-relative) as dirty on a BUTTON/STATIC/LISTBOX control
/// for the next paint, unioning with any pending scope, and mark the window
/// invalidated. No-op for other kinds and for a degenerate (empty) rect — a
/// control with nothing visible to repaint falls back to the window's own
/// full invalidation.
pub(super) fn invalidate_control_rect(state: &mut WinApiState, hwnd: u64, rect: IRect) {
    let kind = find_window(state, hwnd).and_then(|w| w.control_kind);
    if !matches!(
        kind,
        Some(ControlClassKind::Button | ControlClassKind::Static | ControlClassKind::ListBox)
    ) {
        return;
    }
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));
    {
        let control = control_state_mut(state, hwnd);
        match control {
            ControlState::Button { invalidation, .. }
            | ControlState::Static { invalidation }
            | ControlState::ListBox { invalidation, .. } => {
                *invalidation = union_label_invalid(*invalidation, rect, width, height);
            }
            _ => {}
        }
    }
    super::invalidate(state, hwnd);
}

/// Reset a rect-scope control's pending invalidation to
/// [`LabelInvalidation::Full`] WITHOUT marking the window — callers that
/// already invalidate (or must not, e.g. a `redraw = 0` `WM_SETFONT`) control
/// the window flag themselves. Non-seeding: a control with no state yet is
/// untouched.
pub(super) fn label_reset_invalid_full(state: &mut WinApiState, hwnd: u64) {
    if let Some(
        ControlState::Button { invalidation, .. }
        | ControlState::Static { invalidation }
        | ControlState::ListBox { invalidation, .. },
    ) = state
        .window_state()
        .control_states
        .get_mut(&crate::handles::Hwnd::from(hwnd))
    {
        *invalidation = LabelInvalidation::Full;
    }
}

/// BUTTON pressed-state change (press/release, Space, BM_SETSTATE): the
/// whole face — the interior inside the 1 px border — repaints with the
/// pressed (or released) face color and the caption shifts one px. The
/// border color does not change, so the interior is the true painted region;
/// the caption shift stays inside it for any control taller than one text
/// line (a shorter control renders clipped anyway).
pub(super) fn button_invalidate_pressed(state: &mut WinApiState, hwnd: u64) {
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));
    let interior = IRect::from_xywh(
        1,
        1,
        width.saturating_sub(2).max(0),
        height.saturating_sub(2).max(0),
    );
    invalidate_control_rect(state, hwnd, interior);
}

/// Mark the caption rect a WM_SETTEXT change dirties: the union of the OLD
/// caption's rect and the NEW caption's rect (client-relative), so the next
/// paint erases the previous glyphs too. Resolves the stored control font
/// exactly like the paint path (centered for a BUTTON, padded-left for a
/// STATIC, vertically centered for both); falls back to a full repaint when
/// the font cannot be resolved. The rect is clamped to the control — a
/// caption wider than the control clips at it, matching the paint.
///
/// `pub(crate)` (not `pub(super)` like the other helpers): the SetWindowText
/// handlers in `user32::window` call it outside the control dispatch.
pub(crate) fn label_invalidate_text_change(
    state: &mut WinApiState,
    hwnd: u64,
    old_text: &str,
    new_text: &str,
) {
    let kind = find_window(state, hwnd).and_then(|w| w.control_kind);
    if !matches!(
        kind,
        Some(ControlClassKind::Button | ControlClassKind::Static)
    ) {
        return;
    }
    let (width, height, button) = find_window(state, hwnd).map_or((0, 0, false), |w| {
        (
            w.width,
            w.height,
            w.control_kind == Some(ControlClassKind::Button),
        )
    });
    // The font engine is taken out of gdi state so the caption measurement
    // can run next to `state` (the established pattern); it is put back
    // unconditionally. Safe under the single shared WinApiState mutex — the
    // take and the put cannot interleave with another handler's.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = FontKey::default();
    let key_and_resolved = match crate::gdi32::window_font_resolution(state, hwnd, &mut font_engine)
    {
        Some(key_and_resolved) => Some(key_and_resolved),
        None => font_engine
            .resolve(&default_key, 16)
            .map(|resolved| (default_key, resolved)),
    };
    let rect = match &key_and_resolved {
        Some((key, resolved)) => {
            let line_h = resolved.line_height();
            // The vertically centered caption band — the same `paint_label`
            // geometry, client-relative.
            let top = height.saturating_sub(line_h).saturating_div(2).max(0);
            let mut rect_of = |caption: &str| {
                let text_w =
                    font_engine.text_advance(resolved, key, caption, caption.chars().count());
                let left = if button {
                    width.saturating_sub(text_w).saturating_div(2).max(0)
                } else {
                    2
                };
                IRect::from_xywh(left, top, text_w, line_h)
            };
            let union = union_rect(
                rect_of(&strip_mnemonics(old_text)),
                rect_of(&strip_mnemonics(new_text)),
            );
            // A pressed button shifts its caption one px down/right; pad so
            // the erase covers both the shifted and unshifted ink.
            let pad = if button { 1 } else { 0 };
            IRect {
                left: union.left.saturating_sub(pad),
                top: union.top.saturating_sub(pad),
                right: union.right.saturating_add(pad),
                bottom: union.bottom.saturating_add(pad),
            }
        }
        None => IRect::from_xywh(0, 0, width, height),
    };
    state.gdi_state().font_engine = font_engine;
    // Clamp to the control: a caption wider than the control clips at it
    // (the paint's own clip), so the region must not exceed the control.
    let rect = intersect_rect(rect, IRect::from_xywh(0, 0, width, height));
    invalidate_control_rect(state, hwnd, rect);
}

/// Consume a BUTTON/STATIC/LISTBOX paint: the invalidation scope was applied
/// (the erase covered it), so the next paint starts clean. The window's own
/// `invalidated` flag still drives the next cycle.
fn consume_control_invalidation(state: &mut WinApiState, hwnd: u64) {
    let control = control_state_mut(state, hwnd);
    match control {
        ControlState::Button { invalidation, .. }
        | ControlState::Static { invalidation }
        | ControlState::ListBox { invalidation, .. } => {
            *invalidation = LabelInvalidation::Clean;
        }
        _ => {}
    }
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

    // ── Rect-level invalidation (the label-control optimization lane) ──────

    use super::{union_label_invalid, union_rect};
    use crate::gdi32::IRect;
    use crate::user32::controls::{LabelInvalidRect, LabelInvalidation};

    /// Two partial marks before one paint union into one rect — the same
    /// region-accumulation semantics the present dirty accumulator applies.
    #[test]
    fn two_marks_union_into_one_pending_rect() {
        let first = IRect {
            left: 4,
            top: 6,
            right: 20,
            bottom: 18,
        };
        let second = IRect {
            left: 30,
            top: 10,
            right: 50,
            bottom: 22,
        };
        let pending = union_label_invalid(LabelInvalidation::Clean, first, 100, 40);
        let pending = union_label_invalid(pending, second, 100, 40);
        assert_eq!(
            pending,
            LabelInvalidation::Rect(LabelInvalidRect {
                rect: IRect {
                    left: 4,
                    top: 6,
                    right: 50,
                    bottom: 22,
                },
                width: 100,
                height: 40,
            }),
            "the pending scope is the union of both marks"
        );
    }

    /// A pending full repaint is sticky: a later partial mark must not narrow
    /// it (the first paint / a structural change covers everything).
    #[test]
    fn full_is_sticky_against_later_partial_marks() {
        let rect = IRect {
            left: 1,
            top: 1,
            right: 9,
            bottom: 9,
        };
        let pending = union_label_invalid(LabelInvalidation::Full, rect, 100, 40);
        assert_eq!(pending, LabelInvalidation::Full);
    }

    /// A pending rect computed at a different control size is stale — the
    /// layout reflowed underneath it — and escalates to a full repaint.
    #[test]
    fn size_stale_rect_escalates_to_full() {
        let rect = IRect {
            left: 1,
            top: 1,
            right: 9,
            bottom: 9,
        };
        let pending = union_label_invalid(LabelInvalidation::Clean, rect, 100, 40);
        let pending = union_label_invalid(pending, rect, 120, 60);
        assert_eq!(
            pending,
            LabelInvalidation::Full,
            "a mark at a new size must not trust the old stamp"
        );
    }

    /// The union helper covers both input rects exactly (axis-aligned
    /// min/max), including a rect that extends past the other.
    #[test]
    fn union_rect_covers_both_inputs() {
        let a = IRect {
            left: 10,
            top: 20,
            right: 30,
            bottom: 40,
        };
        let b = IRect {
            left: 25,
            top: 35,
            right: 60,
            bottom: 50,
        };
        assert_eq!(
            union_rect(a, b),
            IRect {
                left: 10,
                top: 20,
                right: 60,
                bottom: 50,
            }
        );
    }
}
