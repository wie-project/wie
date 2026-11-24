//! The all-control paint entry point, plus the shared control-paint helpers
//! and the rect-level repaint-scope machinery it consumes.
//!
//! `paint_control` is the entry point for ALL SIX built-in control kinds
//! (BUTTON, STATIC, EDIT, LISTBOX, COMBOBOX, and the comctl32 status bar), so
//! it does not belong to any one control's module. It lived in `button.rs`
//! until the control tree was split by kind; the per-kind painters it calls
//! (`super::button` for the BUTTON face/border/mnemonic drawing,
//! `super::statusbar` for the bar) stay with their own kind.
//!
//! The repaint-scope helpers (`control_dirty_rect`, `union_label_invalid`,
//! `invalidate_control_rect`, the label mutators) are here for the same
//! reason: they compute the rect `paint_control` erases, and are shared by
//! BUTTON, STATIC and LISTBOX alike.

use anyhow::Result;

use super::button::{
    centered_text_x, paint_face_and_border, strip_mnemonics, stroke_border, stroke_border_partial,
};
use super::edit::{edit_dirty_band, paint_edit};
use super::listbox::paint_item_lines;
use super::r#static::paint_label;
use super::statusbar::{
    paint_status_bar_parts, paint_status_bar_separators, paint_status_bar_strip,
    status_bar_part_font,
};
use super::{
    COLOR_BTNFACE, COLOR_BTNFACE_PRESSED, COLOR_BTNSHADOW, COLOR_WINDOW, ControlClassKind,
    ControlState, Dimension, LabelInvalidRect, LabelInvalidation, PaintCtx, PaintFont, TextGeom,
    control_items, control_sel_index, control_state, control_state_mut, edit_reset_invalid_rows,
};
use crate::gdi32::fill_rect_surface;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::subtract_rect;
use crate::gdi32::{IRect, ResolvedWindow, intersect_rect, union_rect};
use crate::state::WindowFlags;
use crate::user32::{
    WinApiState, find_window, find_window_mut, write_guest_ansi_c_string,
    write_guest_utf16_c_string,
};

/// The direct child of `top` on the ancestor chain of `w` — the window in
/// `top`'s immediate child list that is `w` itself (when `w` is a direct
/// child) or the ancestor of `w` at that level. `None` when `w` is not in
/// `top`'s subtree (a different surface, or an orphan).
fn z_child_of(windows: &[crate::WindowRecord], top: u64, w: u64) -> Option<u64> {
    let mut current = w;
    loop {
        let window = windows.iter().find(|win| win.handle.as_u64() == current)?;
        let parent = window.parent_handle.as_u64();
        if parent == top {
            return Some(current);
        }
        if parent == 0 || parent == current {
            return None;
        }
        current = parent;
    }
}

/// The position of `w` inside `top`'s surface (accumulated child offsets).
fn surface_position_of(windows: &[crate::WindowRecord], top: u64, w: u64) -> (i32, i32) {
    let (mut x, mut y) = (0_i32, 0_i32);
    let mut current = w;
    loop {
        let Some(window) = windows.iter().find(|win| win.handle.as_u64() == current) else {
            return (x, y);
        };
        x = x.saturating_add(window.x);
        y = y.saturating_add(window.y);
        let parent = window.parent_handle.as_u64();
        if parent == top {
            return (x, y);
        }
        if parent == 0 || parent == current {
            return (x, y);
        }
        current = parent;
    }
}

/// The surface rects of the VISIBLE windows that composite ABOVE
/// `dc_window` in the surface of `top_hwnd` — a paint of `dc_window` must
/// never overwrite them.
///
/// Windows clips a window's update region to exclude the windows above it in
/// the z-order; WIE's shared-surface compositing (children paint directly
/// into the top-level surface, z-order-blind) must mirror that. Without it a
/// z-order-lower sibling's repaint destroys an overlapping dialog: the owner
/// EDIT's full-width `COLOR_WINDOW` band erase wipes the FindDialog's face
/// (the live "dialog upper region turns white" bug). Two windows in the same
/// surface are ordered by their z-child under the top-level: the
/// later-created z-child (and everything beneath it) composites on top. A
/// window on the SAME z-child branch as the painter is its ancestor (paints
/// below it) or its descendant (paints on top of it, own paint) — neither is
/// a clip for the painter.
pub(crate) fn above_window_rects(state: &WinApiState, top_hwnd: u64, dc_window: u64) -> Vec<IRect> {
    let Some(ws) = state.try_window_state() else {
        return Vec::new();
    };
    let windows = &ws.windows;
    let Some(dc_zchild) = z_child_of(windows, top_hwnd, dc_window) else {
        return Vec::new();
    };
    let Some(dc_zindex) = windows.iter().position(|w| w.handle.as_u64() == dc_zchild) else {
        return Vec::new();
    };
    let mut clip = Vec::new();
    for window in windows {
        if !window.visible || window.handle.as_u64() == dc_window {
            continue;
        }
        let Some(zchild) = z_child_of(windows, top_hwnd, window.handle.as_u64()) else {
            continue;
        };
        if zchild == dc_zchild {
            // Same branch: ancestor of the painter or painter's descendant.
            continue;
        }
        let above = windows
            .iter()
            .position(|w| w.handle.as_u64() == zchild)
            .is_none_or(|index| index > dc_zindex);
        if !above {
            continue;
        }
        let (x, y) = surface_position_of(windows, top_hwnd, window.handle.as_u64());
        clip.push(IRect::from_xywh(x, y, window.width, window.height));
    }
    clip
}

/// Decompose `rects` around the windows that composite above `dc_window` in
/// the surface of `top_hwnd` — the paint must skip the pixels those windows
/// own (see [`above_window_rects`]). Passed rects are in surface
/// coordinates; a rect with no overlap is unchanged.
pub(crate) fn clip_rects_around_above(
    state: &WinApiState,
    top_hwnd: u64,
    dc_window: u64,
    rects: Vec<IRect>,
) -> Vec<IRect> {
    let above = above_window_rects(state, top_hwnd, dc_window);
    if above.is_empty() {
        return rects;
    }
    let mut out = rects;
    for window in above {
        out = subtract_rect(out, window);
    }
    out
}

/// Fill a rect with `color`, clipped to the control's own bounds so a
/// selection or caret running past the right edge cannot bleed into the
/// ancestor surface — and around the windows that composite above the
/// control, so the fill cannot overwrite an overlapping dialog (the
/// z-order-aware update-region clip).
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
    let rects = clip_rects_around_above(
        state,
        info.hwnd.as_u64(),
        info.dc_window.as_u64(),
        vec![IRect {
            left: x0,
            top: y0,
            right: x1,
            bottom: y1,
        }],
    );
    for rect in rects {
        if rect.width() <= 0 || rect.height() <= 0 {
            continue;
        }
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            rect.left,
            rect.top,
            rect.width(),
            rect.height(),
            color,
        );
    }
}

/// Fill a control-paint rect in `info`'s surface, decomposed around the
/// windows that composite above `info.dc_window` (the z-order-aware
/// update-region clip — see [`clip_rects_around_above`]). The rect is in
/// SURFACE coordinates (already offset by the control's position) and
/// pre-clipped to the control's own bounds by the caller.
pub(crate) fn fill_surface_rect_above_clipped(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    rect: IRect,
    color: u32,
) {
    let rects = clip_rects_around_above(
        state,
        info.hwnd.as_u64(),
        info.dc_window.as_u64(),
        vec![rect],
    );
    for rect in rects {
        if rect.width() <= 0 || rect.height() <= 0 {
            continue;
        }
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            rect.left,
            rect.top,
            rect.width(),
            rect.height(),
            color,
        );
    }
}

/// Copy a control's text into a guest buffer (WM_GETTEXT / LB_GETTEXT).
pub(super) fn write_control_text(
    engine: &mut dyn wie_cpu::CpuEngine,
    unicode: bool,
    buffer_va: u64,
    max_characters: u64,
    text: &str,
) -> Result<u64> {
    if buffer_va == 0 {
        return Ok(0);
    }
    let capacity = usize::try_from(max_characters).unwrap_or(0);
    let copied = if unicode {
        write_guest_utf16_c_string(engine, buffer_va, capacity, text)?
    } else {
        write_guest_ansi_c_string(engine, buffer_va, capacity, text)?
    };
    Ok(u64::try_from(copied).unwrap_or(0))
}

/// Reset every EDIT's pending row band to Full in `root`'s window subtree.
///
/// The erase machinery (`user32::message::synth::erase_window_background`)
/// calls this after a full-surface erase: the erase painted over the
/// controls beneath the erased window, destroying their paint base, so an
/// EDIT's band-limited repaint would leave the erased rows blank (the Go To
/// line-N blank-rows bug). A full repaint is always correct; the band
/// optimization is only valid while the surface base survives.
pub(crate) fn reset_edit_bands_in_subtree(state: &mut WinApiState, root: u64) {
    // Snapshot (handle, parent, kind) so the subtree walk does not borrow
    // `state` mutably while the resets below do.
    let snapshot: Vec<(u64, u64, Option<ControlClassKind>)> = state
        .window_state()
        .windows
        .iter()
        .map(|window| {
            (
                window.handle.as_u64(),
                window.parent_handle.as_u64(),
                window.control_kind,
            )
        })
        .collect();
    let edit_subtree: Vec<u64> = snapshot
        .iter()
        .filter(|(handle, _, kind)| {
            *kind == Some(ControlClassKind::Edit) && {
                // Is `root` an ancestor-or-self of this window?
                let mut current = *handle;
                loop {
                    if current == root {
                        break true;
                    }
                    let Some(&(_, next_parent, _)) = snapshot.iter().find(|(h, ..)| *h == current)
                    else {
                        break false;
                    };
                    if next_parent == 0 || next_parent == current {
                        break false;
                    }
                    current = next_parent;
                }
            }
        })
        .map(|(handle, ..)| *handle)
        .collect();
    for hwnd in edit_subtree {
        edit_reset_invalid_rows(state, hwnd);
    }
}

pub(crate) fn invalidate(state: &mut WinApiState, hwnd: u64) {
    if let Some(window) = find_window_mut(state, hwnd) {
        window.invalidated = true;
    }
}

/// Mark a window for a future synthesized WM_PAINT and bump the owning
/// top-level's content revision (the repaint latch) — the pair every control
/// mutation's invalidation function ends with (the EDIT's row bands, the
/// BUTTON/STATIC/LISTBOX rect scopes, the SETCURSEL/ADDSTRING arms). The
/// window-layer callers that manage `invalidated` themselves keep calling
/// `PresentState::request_paint` alone.
pub(crate) fn invalidate_and_request_paint(state: &mut WinApiState, hwnd: u64) {
    invalidate(state, hwnd);
    crate::present::PresentState::request_paint(state, hwnd);
}

/// Paint a control into its ancestor's surface at its parent-relative offset.
pub(crate) fn paint_control(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    hwnd: u64,
    kind: ControlClassKind,
) -> Result<()> {
    // Defense-in-depth: a hidden control must never paint (real Windows
    // discards a hidden window's invalid region). The dispatch arm and the
    // paint synthesizer both gate visibility, but a direct paint path would
    // otherwise let a just-hidden status bar draw its strip over the control
    // that grew into its space.
    if find_window(state, hwnd).is_some_and(|w| !w.visible) {
        return Ok(());
    }
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
    // `with_font_engine` puts it back unconditionally. Safe under the single
    // shared WinApiState mutex: every API handler — this WM_PAINT and any
    // concurrent one on another host thread — runs while holding it, so the
    // take and the put cannot interleave.
    state.with_font_engine(|state, font_engine| {
        let key_and_resolved =
            crate::gdi32::window_font_resolution_or_default(state, hwnd, font_engine);
        (|| -> Result<()> {
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
                        fill_surface_rect_above_clipped(
                            state,
                            &info,
                            IRect::from_xywh(
                                info.offset_x.saturating_add(dirty.left),
                                info.offset_y.saturating_add(dirty.top),
                                dirty.width(),
                                dirty.height(),
                            ),
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
                        centered_text_x(&info, size.width, &caption, font_engine, resolved, key);
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
                            engine: font_engine,
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
                    fill_surface_rect_above_clipped(
                        state,
                        &info,
                        IRect::from_xywh(
                            info.offset_x.saturating_add(dirty.left),
                            info.offset_y.saturating_add(dirty.top),
                            dirty.width(),
                            dirty.height(),
                        ),
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
                            engine: font_engine,
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
                        // The band erase is the z-order-sensitive fill: a
                        // full-width white band here would wipe an overlapping
                        // window composited above the EDIT in the same surface
                        // (the FindDialog — the live "dialog turns white" bug),
                        // so the erase is decomposed around the above windows.
                        fill_surface_rect_above_clipped(
                            state,
                            &info,
                            IRect::from_xywh(
                                info.offset_x,
                                info.offset_y.saturating_add(band_top),
                                size.width,
                                band_bottom.saturating_sub(band_top),
                            ),
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
                            engine: font_engine,
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
                    fill_surface_rect_above_clipped(
                        state,
                        &info,
                        IRect::from_xywh(
                            info.offset_x.saturating_add(dirty.left),
                            info.offset_y.saturating_add(dirty.top),
                            dirty.width(),
                            dirty.height(),
                        ),
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
                            engine: font_engine,
                            resolved,
                            key,
                        },
                        dirty,
                    )?;
                    consume_control_invalidation(state, hwnd);
                }
                ControlClassKind::ComboBox => {
                    paint_face_and_border(state, &info, size, false);
                    // Real Windows shows the *selected* item's text, and
                    // `sel_index` is already bound above — so honour it. Falling
                    // back to the first item when nothing is selected preserves
                    // what a freshly-created combo displayed before.
                    let shown = sel_index
                        .try_into()
                        .ok()
                        .and_then(|i: usize| items.get(i))
                        .or_else(|| items.first())
                        .map_or("", String::as_str);
                    let tx = info.offset_x.saturating_add(4);
                    paint_label(
                        &mut PaintCtx { state, engine },
                        &info,
                        shown,
                        TextGeom {
                            tx,
                            width: size.width,
                            height: size.height,
                        },
                        false,
                        &mut PaintFont {
                            engine: font_engine,
                            resolved,
                            key,
                        },
                    )?;
                }
                // The strip face/edges were painted before the font resolution;
                // this arm only draws each part's text clipped to its cell.
                ControlClassKind::StatusBar => {
                    let (status_key, status_resolved) =
                        status_bar_part_font(state, hwnd, font_engine, key, resolved);
                    paint_status_bar_parts(
                        state,
                        engine,
                        &info,
                        hwnd,
                        &mut PaintFont {
                            engine: font_engine,
                            resolved: &status_resolved,
                            key: &status_key,
                        },
                    )?;
                }
            }
            Ok(())
        })()
    })
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
pub(super) fn control_dirty_rect(state: &WinApiState, hwnd: u64, size: Dimension) -> IRect {
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
pub(super) fn union_label_invalid(
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

// The smallest axis-aligned rect covering both inputs.
// (Shared `crate::gdi32::union_rect` — see `gdi32/blit.rs`.)

/// Whether `rect` (client-relative) covers any pixel of a control's 1 px
/// border — the erase filled it with the face color, so the border must be
/// re-stroked.
pub(super) fn rect_touches_border(rect: IRect, size: Dimension) -> bool {
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
    // Every rect-scoped control change funnels through here — a button's
    // pressed face, a label's caption, a listbox's rows — so this is the
    // button/label/listbox arm of the repaint latch: mark the window and
    // bump the content revision of the owning top-level.
    super::invalidate_and_request_paint(state, hwnd);
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
    // can run next to `state` (the established pattern, now structural via
    // `with_font_engine`); it is put back unconditionally. Safe under the
    // single shared WinApiState mutex — the take and the put cannot
    // interleave with another handler's.
    let rect = state.with_font_engine(|state, font_engine| {
        let key_and_resolved =
            crate::gdi32::window_font_resolution_or_default(state, hwnd, font_engine);
        match &key_and_resolved {
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
        }
    });
    // Clamp to the control: a caption wider than the control clips at it
    // (the paint's own clip), so the region must not exceed the control.
    let rect = intersect_rect(rect, IRect::from_xywh(0, 0, width, height));
    invalidate_control_rect(state, hwnd, rect);
}

/// Consume a BUTTON/STATIC/LISTBOX paint: the invalidation scope was applied
/// (the erase covered it), so the next paint starts clean. The window's own
/// `invalidated` flag still drives the next cycle.
pub(super) fn consume_control_invalidation(state: &mut WinApiState, hwnd: u64) {
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
    use super::{strip_mnemonics, union_label_invalid};
    use crate::gdi32::{IRect, union_rect};
    use crate::user32::controls::{LabelInvalidRect, LabelInvalidation};

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
