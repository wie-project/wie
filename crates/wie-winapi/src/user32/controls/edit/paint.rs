//! The EDIT paint path and its invalidation machinery: `paint_edit` (text
//! rows, selection, the 1 px caret bar), the row-level invalidation band
//! (`edit_invalidate_rows`/`span`/`mutation`/`caret`, `edit_dirty_band`), and
//! the classic scrollbar chrome. Split from the monolithic `edit.rs`; the
//! `pub(super)` items are the cross-file surface imported through
//! `super::paint::…` (the mutation/keyboard/messages paths mark the dirty
//! bands, `controls::button` reads `edit_dirty_band`, and `paint_edit` is
//! called from `controls::paint`).

use anyhow::Result;

use crate::gdi32::{IRect, ResolvedWindow};
use crate::state::WindowFlags;
use crate::user32::controls::{
    COLOR_BTNFACE, COLOR_BTNHIGHLIGHT, COLOR_BTNSHADOW, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT,
    ControlState, Dimension, ES_MULTILINE, EditInvalidRows, EditInvalidation, PaintCtx, PaintFont,
    TextGeom, control_state, invalidate_and_request_paint,
};
use crate::user32::{WinApiState, find_window};

use super::math::{
    VisibleSegment, edit_scroll_context, edit_text_area, layout_visible_lines, line_char_len,
    line_from_char, line_index_of,
};
use super::state::{
    ES_ALIGN_MASK, SCROLLBAR_WIDTH, WS_HSCROLL, edit_state_mut, edit_wrap_from_style,
};
use crate::user32::controls::listbox::render_control_text;
use crate::user32::controls::paint::fill_rect_clipped;

// ── Row-level invalidation (the edit optimization lane) ──────────────────
//
// The mutating ops mark the VISUAL rows they touched dirty (a
// `ControlState::Edit::invalid_rows` band); `paint_edit` clips its row loop
// to the band and the paint erase covers the same rows, so a caret blink or
// a typed character repaints only the changed rows instead of the whole
// EDIT. Structural changes (a scroll move, WM_SETFONT, a whole-text
// replacement, a resize reflow) reset the band to full. The band's rows are
// resolved with the stored font through the shared `edit_text_area` seam —
// the same greedy wrap walk the paint and the scroll math run — so the
// clipped rows, the erased band, and the painted rows always agree.

/// The window geometry an EDIT's invalidation math needs: its text, client
/// size, and creation style (`None` when the window is gone).
fn edit_geometry(state: &WinApiState, hwnd: u64) -> Option<(String, i32, i32, u32)> {
    state.try_window_state().and_then(|ws| {
        ws.windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .map(|w| (w.control_text.clone(), w.width, w.height, w.style))
    })
}

/// The caret character index of `hwnd`'s EDIT state (0 when never touched).
#[must_use]
fn edit_caret_of(state: &WinApiState, hwnd: u64) -> usize {
    match control_state(state, hwnd) {
        Some(ControlState::Edit { caret, .. }) => *caret,
        _ => 0,
    }
}

/// The visual-row span of the char range [lo, hi) of `text` — the first and
/// last visual rows holding any character of the range (a caret-only range,
/// lo == hi, spans the single row of that position; a position on a line's
/// trailing `\n` belongs to that line). Walks the same greedy wrap rule as
/// `visual_rows`/`layout_visible_lines` at the same wrap column, so the rows
/// it reports are exactly the rows the paint lays out — the seam that keeps
/// the clipped row band and the painted rows in agreement.
#[must_use]
fn span_row_range<F>(
    text: &str,
    wrap_width: i32,
    wrap: bool,
    lo: usize,
    hi: usize,
    advance: &mut F,
) -> (usize, usize)
where
    F: FnMut(char) -> i32,
{
    let mut first_row = usize::MAX;
    let mut last_row = 0_usize;
    let mut line_start_char = 0_usize;
    let mut visual = 0_usize;
    let mut found = false;
    for line_text in text.split('\n') {
        let row_start = visual;
        let line_end_char = line_start_char.saturating_add(line_text.chars().count());
        // Count this line's visual rows with the same greedy wrap walk.
        let mut x = 0_i32;
        for ch in line_text.chars() {
            let w = advance(ch);
            if wrap && x > 0 && x.saturating_add(w) > wrap_width {
                visual = visual.saturating_add(1);
                x = 0;
            }
            x = x.saturating_add(w);
        }
        visual = visual.saturating_add(1);
        // A line covers the positions [line_start, line_end_char], where
        // line_end_char is its trailing `\n` (or the text end). The line
        // overlaps the range when any position of [lo, hi) falls inside that
        // span (a caret-only range tests its single position).
        let overlaps = if lo == hi {
            lo >= line_start_char && lo <= line_end_char
        } else {
            lo <= line_end_char && hi > line_start_char
        };
        if overlaps {
            first_row = first_row.min(row_start);
            last_row = last_row.max(visual.saturating_sub(1));
            found = true;
        }
        line_start_char = line_end_char.saturating_add(1);
    }
    if !found {
        // A position past the text (or empty text) lands on the last row.
        (visual.saturating_sub(1), visual.saturating_sub(1))
    } else {
        (first_row, last_row)
    }
}

/// The pending row band of an EDIT when it is still valid against the
/// CURRENT layout: it must have been computed at the current wrap width AND
/// the control must have painted before (the first paint covers everything —
/// the surface behind a never-painted control is undefined, so a partial
/// repaint would leave holes). `None` = paint every visible row.
#[must_use]
fn band_is_current(
    invalid: EditInvalidation,
    wrap_width: i32,
    painted_before: bool,
) -> Option<EditInvalidRows> {
    match invalid {
        EditInvalidation::Band(band) if band.wrap_width == wrap_width && painted_before => {
            Some(band)
        }
        _ => None,
    }
}

/// The client-relative y band (top, bottom-exclusive) an EDIT must erase and
/// repaint on its next paint: the pending row band's rows, or the WHOLE
/// client for a full repaint (no pending band, a structural change, a stale
/// band whose wrap width no longer matches the layout — a resize reflowed
/// it — or the first paint of a never-painted control). `paint_edit` clips
/// its row loop to the same band, so a partial repaint never leaves stale
/// pixels and never wipes the untouched rows. `line_h`/`advance` come from
/// the caller's resolved font (the same resolution the paint uses); the
/// returned y is client-relative, the caller adds its own offset.
pub(crate) fn edit_dirty_band<F>(
    state: &WinApiState,
    hwnd: u64,
    text: &str,
    client: Dimension,
    line_h: i32,
    style: u32,
    advance: &mut F,
) -> (i32, i32)
where
    F: FnMut(char) -> i32,
{
    let (invalid, first_visible, caret, last_paint_rows) = match control_state(state, hwnd) {
        Some(ControlState::Edit {
            invalid_rows,
            first_visible_line,
            caret,
            last_paint_rows,
            ..
        }) => (*invalid_rows, *first_visible_line, *caret, *last_paint_rows),
        _ => (EditInvalidation::Full, 0, 0, 0),
    };
    let (width, height) = (client.width, client.height);
    let area = edit_text_area(text, width, height, line_h, style, caret, advance);
    let Some(band) = band_is_current(invalid, area.wrap_width, last_paint_rows > 0) else {
        return (0, height);
    };
    // Clamp the band to the visible text rows; an off-screen band (a stale
    // range below the last row) is not visible — but a full erase is the
    // safe fallback and never leaves stale pixels.
    let first = first_visible.min(area.total.saturating_sub(1));
    let lo = band.lo.max(first);
    let hi = band.hi.min(area.total.saturating_sub(1));
    if lo > hi {
        return (0, height);
    }
    let base_y = if style & ES_MULTILINE != 0 {
        0
    } else {
        height.saturating_sub(line_h).saturating_div(2).max(0)
    };
    let top = base_y.saturating_add(
        i32::try_from(lo.saturating_sub(first))
            .unwrap_or(0)
            .saturating_mul(line_h),
    );
    let bottom = base_y.saturating_add(
        i32::try_from(hi.saturating_sub(first))
            .unwrap_or(0)
            .saturating_add(1)
            .saturating_mul(line_h),
    );
    (top.max(0).min(height), bottom.max(0).min(height))
}

/// Mark the visual rows `lo..=hi` dirty for the next paint, unioning with
/// any pending band (two mutations before one paint both repaint). A pending
/// full repaint — or a pending band computed at a different wrap width (the
/// layout reflowed, so the old rows no longer exist as numbered) — stays
/// full. Marks the window invalidated so the next paint cycle consumes the
/// band.
pub(super) fn edit_invalidate_rows(
    state: &mut WinApiState,
    hwnd: u64,
    lo: usize,
    hi: usize,
    wrap_width: i32,
) {
    let ws = state.window_state();
    let style = ws
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .map_or(0, |w| w.style);
    let ControlState::Edit { invalid_rows, .. } =
        edit_state_mut(&mut ws.control_states, hwnd, style)
    else {
        return;
    };
    let next = match *invalid_rows {
        EditInvalidation::Full => EditInvalidation::Full,
        EditInvalidation::Band(pending) if pending.wrap_width != wrap_width => {
            EditInvalidation::Full
        }
        EditInvalidation::Band(pending) => EditInvalidation::Band(EditInvalidRows {
            lo: pending.lo.min(lo),
            hi: pending.hi.max(hi),
            wrap_width,
        }),
        EditInvalidation::Clean => EditInvalidation::Band(EditInvalidRows { lo, hi, wrap_width }),
    };
    *invalid_rows = next;
    // Every row-band edit mutation (typing, caret move, selection, paste,
    // undo, the caret blink) funnels through here — mark the window and bump
    // the content revision so the idle reconcile republishes the surface.
    invalidate_and_request_paint(state, hwnd);
}

/// Mark the whole EDIT dirty for the next paint — every structural change: a
/// scroll move, a font change, a whole-text replacement, a resize reflow.
/// Sticky: a later mutation band cannot narrow a pending full repaint. Marks
/// the window invalidated.
pub(super) fn edit_invalidate_full(state: &mut WinApiState, hwnd: u64) {
    if let Some(ControlState::Edit { invalid_rows, .. }) = state
        .window_state()
        .control_states
        .get_mut(&crate::handles::Hwnd::from(hwnd))
    {
        *invalid_rows = EditInvalidation::Full;
    }
    // Structural edit changes (whole-text replacement, scroll, font, resize)
    // funnel through here — mark the window and bump the content revision
    // (the repaint latch).
    invalidate_and_request_paint(state, hwnd);
}

/// Reset an EDIT's pending invalidation to [`EditInvalidation::Full`]
/// WITHOUT marking the window — callers that already invalidate (or must
/// not, e.g. a `redraw = 0` `WM_SETFONT`) control the window flag
/// themselves. Non-seeding: a control with no Edit state yet is untouched.
pub(crate) fn edit_reset_invalid_rows(state: &mut WinApiState, hwnd: u64) {
    if let Some(ControlState::Edit { invalid_rows, .. }) = state
        .window_state()
        .control_states
        .get_mut(&crate::handles::Hwnd::from(hwnd))
    {
        *invalid_rows = EditInvalidation::Full;
    }
}

/// Mark the visual rows of the char span [lo, hi) of `hwnd`'s text dirty for
/// the next paint — every row holding any character of the span (a
/// caret-only span, lo == hi, dirties the single row of that position).
/// Resolved with the stored control font through the shared
/// `edit_text_area` seam — the same resolution the paint and the scroll math
/// use, so the clipped rows and the painted rows always agree. Unions with
/// any pending band. Marks the window invalidated. No-op when the window (or
/// its Edit state) is gone.
pub(super) fn edit_invalidate_span(state: &mut WinApiState, hwnd: u64, lo: usize, hi: usize) {
    let Some((text, width, height, style)) = edit_geometry(state, hwnd) else {
        return;
    };
    // The font engine is taken out of gdi state so the advance closure can
    // run next to `state` (the established pattern, now structural via
    // `with_font_engine`); it is put back unconditionally. Safe under the
    // single shared WinApiState mutex — the take and the put cannot
    // interleave with another handler's.
    let band = state.with_font_engine(|state, font_engine| {
        let key_and_resolved =
            crate::gdi32::window_font_resolution_or_default(state, hwnd, font_engine);
        match &key_and_resolved {
            Some((key, resolved)) => {
                let line_h = resolved.line_height();
                let caret = edit_caret_of(state, hwnd);
                let wrap = edit_wrap_from_style(style);
                let advance = &mut |ch: char| font_engine.char_advance(resolved, key, ch);
                let area = edit_text_area(&text, width, height, line_h, style, caret, advance);
                let (first, last) = span_row_range(&text, area.wrap_width, wrap, lo, hi, advance);
                Some((first, last, area.wrap_width))
            }
            None => None,
        }
    });
    let Some((first, last, wrap_width)) = band else {
        return;
    };
    edit_invalidate_rows(state, hwnd, first, last, wrap_width);
}

/// Mark the rows a text mutation dirtied for the next paint: every visual
/// row of the logical line holding the edit start (a wrapped line reflows as
/// a whole), or the whole EDIT when the edit crossed a line boundary
/// (`\n` inserted/removed — every row below the edit shifts position, and a
/// SHORTER replacement vacates rows that must be erased, so the pending band
/// cannot narrow to the new text's extent).
pub(super) fn edit_invalidate_mutation(
    state: &mut WinApiState,
    hwnd: u64,
    char_index: usize,
    crossed_lines: bool,
) {
    if crossed_lines {
        // A line-structure change reflows every row below the edit; the
        // band-limited span [line_start, new-text-end] would leave the rows
        // vacated by a SHORTER replacement painted with the old text (the
        // Time/Date full-selection stale-rows bug). Full is the safe scope —
        // the design's "structural change resets the band to full".
        edit_invalidate_full(state, hwnd);
        return;
    }
    let Some((text, _, _, _)) = edit_geometry(state, hwnd) else {
        return;
    };
    let line = line_from_char(&text, char_index);
    let line_start = line_index_of(&text, line).unwrap_or(0);
    let line_end = line_start.saturating_add(line_char_len(&text, line).unwrap_or(0));
    edit_invalidate_span(state, hwnd, line_start, line_end);
}

/// Narrow the caret-blink repaint to the caret's rows: the blink only toggles
/// the 1 px × line-height caret bar, so the rows holding the caret are all
/// that is dirty (a row repaint erases the bar and redraws the row's text
/// under it). The repaint covers BOTH the row where the last paint drew the
/// bar (`last_caret_drawn_row`) and the caret's CURRENT row: when the caret
/// moved since the last paint, the old bar is still on the surface at the
/// old row, and a repaint of only the new row would leave it there forever
/// (the stuck/ghost caret). Marks the window invalidated like
/// `edit_invalidate_rows`. Falls back to a plain full-window invalidate when
/// the layout cannot be resolved.
pub(super) fn edit_invalidate_caret(state: &mut WinApiState, hwnd: u64) {
    let Some(context) = edit_scroll_context(state, hwnd) else {
        // The caret blink is a visible change even when the layout cannot be
        // resolved (the fallback full invalidate) — mark the window and bump
        // the latch.
        invalidate_and_request_paint(state, hwnd);
        return;
    };
    let last_drawn = match control_state(state, hwnd) {
        Some(ControlState::Edit {
            last_caret_drawn_row,
            ..
        }) => *last_caret_drawn_row,
        _ => None,
    };
    let lo = last_drawn
        .unwrap_or(context.caret_row)
        .min(context.caret_row);
    let hi = last_drawn
        .unwrap_or(context.caret_row)
        .max(context.caret_row);
    edit_invalidate_rows(state, hwnd, lo, hi, context.wrap_width);
}

/// The selection `[sel_start, sel_end)` intersected with `row`, as row-local
/// char offsets (lo == hi when the row holds no selected characters).
#[must_use]
fn selection_overlap(row: &VisibleSegment, sel_start: usize, sel_end: usize) -> (usize, usize) {
    let lo = sel_start.max(row.char_start);
    let hi = sel_end.min(row.char_end);
    (
        lo.saturating_sub(row.char_start),
        hi.saturating_sub(row.char_start),
    )
}
/// The thumb length and leading-edge offset for a classic scrollbar:
/// `track` is the travel axis length in px, `position` the scroll offset
/// within `span` (total − visible rows, or the horizontal overflow in px),
/// and `visible` the visible share. The thumb scales with visible/total
/// (floored at 16 px so a large range keeps a grab handle and capped at the
/// track). Shared with the mouse's gutter presses/thumb drags.
#[must_use]
pub(super) fn scrollbar_thumb(
    track: i32,
    position: usize,
    span: usize,
    visible: usize,
) -> (i32, i32) {
    let total = span.saturating_add(visible).max(1);
    let thumb = i32::try_from(
        i64::try_from(visible)
            .unwrap_or(0)
            .saturating_mul(i64::from(track))
            .saturating_div(i64::try_from(total).unwrap_or(1)),
    )
    .unwrap_or(0)
    .max(16)
    .min(track);
    let travel = track.saturating_sub(thumb);
    let offset = i32::try_from(
        i64::try_from(position.min(span))
            .unwrap_or(0)
            .saturating_mul(i64::from(travel))
            .saturating_div(i64::try_from(span.max(1)).unwrap_or(1)),
    )
    .unwrap_or(0);
    (thumb, offset)
}

/// Paint the classic vertical scrollbar in the right gutter: a BTNFACE track
/// with BTNHIGHLIGHT (left) / BTNSHADOW (right) edges and a raised thumb
/// positioned by `first_visible_line` over the total/visible span.
fn paint_vertical_scrollbar(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    size: Dimension,
    first_visible_line: usize,
    total: usize,
    visible: usize,
) {
    let gutter_x = info
        .offset_x
        .saturating_add(size.width.saturating_sub(SCROLLBAR_WIDTH));
    let track = size.height;
    let (thumb, thumb_pos) = scrollbar_thumb(
        track,
        first_visible_line,
        total.saturating_sub(visible),
        visible,
    );
    // Track + its light/dark outer edges.
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(gutter_x, info.offset_y, SCROLLBAR_WIDTH, track),
        COLOR_BTNFACE,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(gutter_x, info.offset_y, 1, track),
        COLOR_BTNHIGHLIGHT,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(
            gutter_x.saturating_add(SCROLLBAR_WIDTH.saturating_sub(1)),
            info.offset_y,
            1,
            track,
        ),
        COLOR_BTNSHADOW,
    );
    // The raised thumb: BTNFACE with light top/left and shadow bottom/right.
    let thumb_y = info.offset_y.saturating_add(thumb_pos);
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(gutter_x, thumb_y, SCROLLBAR_WIDTH, thumb),
        COLOR_BTNFACE,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(gutter_x, thumb_y, SCROLLBAR_WIDTH, 1),
        COLOR_BTNHIGHLIGHT,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(
            gutter_x,
            thumb_y.saturating_add(thumb.saturating_sub(1)),
            SCROLLBAR_WIDTH,
            1,
        ),
        COLOR_BTNSHADOW,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(gutter_x, thumb_y, 1, thumb),
        COLOR_BTNHIGHLIGHT,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(
            gutter_x.saturating_add(SCROLLBAR_WIDTH.saturating_sub(1)),
            thumb_y,
            1,
            thumb,
        ),
        COLOR_BTNSHADOW,
    );
}

/// Paint the classic horizontal scrollbar in the bottom strip of a wrap-off
/// EDIT: BTNFACE with a light top and shadow bottom edge and a raised thumb
/// positioned by `first_visible_column` over the max-line-width span.
fn paint_horizontal_scrollbar(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    size: Dimension,
    first_visible_column: usize,
    max_line_width: i32,
    wrap_width: i32,
) {
    let strip_y = info
        .offset_y
        .saturating_add(size.height.saturating_sub(SCROLLBAR_WIDTH));
    let span = max_line_width.saturating_sub(wrap_width).max(0);
    let (thumb, thumb_pos) = scrollbar_thumb(
        size.width,
        first_visible_column,
        usize::try_from(span).unwrap_or(0),
        usize::try_from(wrap_width.max(0)).unwrap_or(0),
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(info.offset_x, strip_y, size.width, SCROLLBAR_WIDTH),
        COLOR_BTNFACE,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(info.offset_x, strip_y, size.width, 1),
        COLOR_BTNHIGHLIGHT,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(
            info.offset_x,
            strip_y.saturating_add(SCROLLBAR_WIDTH.saturating_sub(1)),
            size.width,
            1,
        ),
        COLOR_BTNSHADOW,
    );
    let thumb_x = info.offset_x.saturating_add(thumb_pos);
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(thumb_x, strip_y, thumb, SCROLLBAR_WIDTH),
        COLOR_BTNFACE,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(thumb_x, strip_y, 1, SCROLLBAR_WIDTH),
        COLOR_BTNHIGHLIGHT,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(
            thumb_x.saturating_add(thumb.saturating_sub(1)),
            strip_y,
            1,
            SCROLLBAR_WIDTH,
        ),
        COLOR_BTNSHADOW,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(thumb_x, strip_y, thumb, 1),
        COLOR_BTNHIGHLIGHT,
    );
    fill_rect_clipped(
        state,
        info,
        size,
        IRect::from_xywh(
            thumb_x,
            strip_y.saturating_add(SCROLLBAR_WIDTH.saturating_sub(1)),
            thumb,
            1,
        ),
        COLOR_BTNSHADOW,
    );
}

/// EDIT paint: text rows (wrap-aware), selection highlight, and the caret bar.
///
/// Glyphs are proportional, so the caret and selection x positions are the
/// SUMMED advances of the preceding characters (matching the rendered text
/// exactly). The row loop is clipped to the pending invalid row band
/// (`ControlState::Edit::invalid_rows`) — a caret blink or a typed character
/// repaints only the rows it touched, while the paint erase covers the same
/// band — so the untouched rows keep their pixels. Each row is drawn in the
/// compositing order below (see the comment in the loop for why the selected
/// run must be re-rendered rather than drawn once per-glyph), and the caret
/// bar (1 px, full line height) is only drawn while the control has focus.
pub(crate) fn paint_edit(
    ctx: &mut PaintCtx<'_>,
    info: &ResolvedWindow,
    text: &str,
    geom: TextGeom,
    font: &mut PaintFont<'_>,
) -> Result<()> {
    let len = text.chars().count();
    let focused = find_window(ctx.state, info.dc_window.as_u64())
        .is_some_and(|w| w.flags.contains(WindowFlags::FOCUSED));
    let (
        sel_start,
        sel_end,
        caret,
        first_visible_line,
        first_visible_column,
        caret_on,
        invalid_rows,
        last_paint_rows,
        last_caret_drawn_row,
    ) = match control_state(ctx.state, info.dc_window.as_u64()) {
        Some(ControlState::Edit {
            caret,
            sel_start,
            sel_end,
            first_visible_line,
            first_visible_column,
            caret_on,
            invalid_rows,
            last_paint_rows,
            last_caret_drawn_row,
            ..
        }) => (
            (*sel_start).min(*sel_end),
            (*sel_start).max(*sel_end),
            *caret,
            *first_visible_line,
            *first_visible_column,
            *caret_on,
            *invalid_rows,
            *last_paint_rows,
            *last_caret_drawn_row,
        ),
        _ => (0, 0, 0, 0, 0, true, EditInvalidation::Full, 0, None),
    };
    let (sel_start, sel_end, caret) = (sel_start.min(len), sel_end.min(len), caret.min(len));
    let line_h = font.resolved.line_height();
    // The control's client extent — the width/height pair every paint helper
    // below shares (the scrollbars, the clipped fills).
    let control = Dimension {
        width: geom.width,
        height: geom.height,
    };
    // The multiline/wrap/alignment decisions read the LIVE creation style
    // from the window record, not `style_bits`: the read-only `control_state`
    // accessor never refreshes `style_bits`, whose lazy seed starts at 0 — so
    // the very FIRST paint (WM_PAINT right after creation, before any
    // keyboard/input message ran a mutating accessor) would otherwise render
    // a multiline EDIT as single-line (and an ES_CENTER/RIGHT edit as
    // left-aligned). The caret/selection fields are correctly maintained and
    // stay on the control state.
    let edit_style = ctx
        .state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == info.dc_window)
        .map_or(0, |w| w.style);
    let multiline = edit_style & ES_MULTILINE != 0;
    // Wrap is on when the multiline EDIT has no horizontal scrollbar (notepad
    // toggles word wrap by dropping the horizontal styles); long lines are
    // horizontally clipped otherwise.
    let wrap = multiline && edit_style & WS_HSCROLL == 0;
    // Single-line edits keep their vertical centering; multiline rows start
    // at the top of the client rect.
    let base_y = if multiline {
        info.offset_y
    } else {
        info.offset_y
            .saturating_add(geom.height.saturating_sub(line_h).saturating_div(2))
            .max(info.offset_y)
    };
    // The wrap column and scrollbar visibility come from the SAME shared
    // resolution the scroll math uses (`edit_text_area`), so the painted rows
    // and the scroll offsets always agree — including the V-scrollbar gutter
    // reservation and the H-scrollbar bottom strip.
    let area = {
        let mut advance = |ch: char| font.engine.char_advance(font.resolved, font.key, ch);
        edit_text_area(
            text,
            geom.width,
            geom.height,
            line_h,
            edit_style,
            caret,
            &mut advance,
        )
    };
    // Text is clipped to the text area: the right edge stops before the V
    // gutter (a wrap-off line's tail must not bleed into the scrollbar) and
    // the bottom stops before the H strip.
    let v_gutter = if area.v_scroll_visible {
        SCROLLBAR_WIDTH
    } else {
        0
    };
    let h_strip = if area.h_scroll_visible {
        SCROLLBAR_WIDTH
    } else {
        0
    };
    let text_right = info
        .offset_x
        .saturating_add(geom.width)
        .saturating_sub(v_gutter);
    let text_bottom = info
        .offset_y
        .saturating_add(geom.height)
        .saturating_sub(h_strip);
    let clip = Some(IRect {
        left: info.offset_x,
        top: info.offset_y,
        right: text_right,
        bottom: text_bottom,
    });
    let has_selection = focused && sel_start != sel_end;
    // The row band to repaint: the pending invalid rows (when still valid
    // against the current layout), or every visible row — a full repaint, a
    // stale band (the wrap width changed underneath it), or the first paint
    // (the surface behind a never-painted control is undefined, so a partial
    // repaint would leave holes). The paint erase covers the same band.
    let first_row = if multiline { first_visible_line } else { 0 };
    let (band_lo, band_hi) =
        match band_is_current(invalid_rows, area.wrap_width, last_paint_rows > 0) {
            Some(band) => (band.lo, band.hi),
            None => (0, usize::MAX),
        };
    let rows = layout_visible_lines(
        text,
        area.wrap_width,
        line_h,
        first_row,
        wrap,
        edit_style & ES_ALIGN_MASK,
        &mut |ch| font.engine.char_advance(font.resolved, font.key, ch),
    );
    // A wrap-off EDIT scrolled right shifts every row (and its caret/selection
    // x) by the horizontal offset.
    let h_shift = if area.h_scroll_visible {
        i32::try_from(first_visible_column).unwrap_or(0)
    } else {
        0
    };
    // `layout_visible_lines` emits rows from `first_row` on, so segment i
    // holds visual row `first_row + i`; the band maps to segment indices
    // directly (rows above the viewport saturate to segment 0, which is
    // merely an over-invalidation and never a stale pixel).
    let seg_lo = band_lo.saturating_sub(first_row);
    let seg_hi = band_hi.saturating_sub(first_row);
    let mut painted_rows = 0_usize;
    let mut caret_drawn = false;
    // The row the bar landed on in THIS paint (starts as the surface state:
    // a paint that does not draw the bar must leave the recorded row
    // untouched — the bar may still be elsewhere on the surface, or already
    // erased by a previous repaint).
    let mut drawn_row = last_caret_drawn_row;
    for (i, row) in rows.iter().enumerate() {
        if i < seg_lo || i > seg_hi {
            continue;
        }
        let y = base_y.saturating_add(row.y);
        if y >= text_bottom {
            break;
        }
        painted_rows = painted_rows.saturating_add(1);
        let x = geom.tx.saturating_sub(h_shift).saturating_add(row.x);
        // The selected cells are drawn in a FIXED compositing order — the
        // COLOR_HIGHLIGHT fill, then the whole row in COLOR_WINDOWTEXT, then
        // the selected run re-rendered in COLOR_HIGHLIGHTTEXT — and that
        // re-render is load-bearing, NOT a per-glyph single pass. The
        // rasterizer blends with coverage alpha (`blend_pixel`: source-over),
        // so a selected glyph's final pixels are
        // WHITE-over-(COLOR_WINDOWTEXT-over-COLOR_HIGHLIGHT): a single
        // white-over-HIGHLIGHT pass would compute different anti-aliased edge
        // pixels, and the micro-suite and the paint pixel tests assert exact
        // pixels. The two-pass stays; each glyph's rasterize is a font-cache
        // hit and the extra blend covers only the selected cells.
        // Pass 1: fill the selected cells with COLOR_HIGHLIGHT (behind text).
        let (sel_lo, sel_hi) = if has_selection {
            selection_overlap(row, sel_start, sel_end)
        } else {
            (0, 0)
        };
        let sel_x = if sel_lo < sel_hi {
            let lo_x = x.saturating_add(font.engine.text_advance(
                font.resolved,
                font.key,
                &row.text,
                sel_lo,
            ));
            let hi_x = x.saturating_add(font.engine.text_advance(
                font.resolved,
                font.key,
                &row.text,
                sel_hi,
            ));
            fill_rect_clipped(
                ctx.state,
                info,
                control,
                IRect::from_xywh(lo_x, y, hi_x.saturating_sub(lo_x), line_h),
                COLOR_HIGHLIGHT,
            );
            Some((lo_x, sel_lo, sel_hi))
        } else {
            None
        };
        // Pass 2: the whole row in the normal text color.
        if !row.text.is_empty() {
            render_control_text(
                ctx,
                info.hwnd,
                IRect::from_xywh(
                    x,
                    y,
                    i32::try_from(info.width).unwrap_or(0),
                    i32::try_from(info.height).unwrap_or(0),
                ),
                &row.text,
                0,
                clip,
                font,
            )?;
        }
        // Pass 3: re-render the selected run in COLOR_HIGHLIGHTTEXT.
        if let Some((sel_x, sel_lo, sel_hi)) = sel_x {
            let selected: String = row
                .text
                .chars()
                .skip(sel_lo)
                .take(sel_hi.saturating_sub(sel_lo))
                .collect();
            render_control_text(
                ctx,
                info.hwnd,
                IRect::from_xywh(
                    sel_x,
                    y,
                    i32::try_from(info.width).unwrap_or(0),
                    i32::try_from(info.height).unwrap_or(0),
                ),
                &selected,
                COLOR_HIGHLIGHTTEXT,
                clip,
                font,
            )?;
        }
        // Pass 4: the 1 px caret bar at the caret's glyph cell. The caret
        // belongs to the first row ending at or past it — a wrap-boundary
        // caret lands at the END of the row before the break. It draws only
        // in the blink ON phase (the focus timer toggles `caret_on`). The
        // row where the bar lands is RECORDED on the state: the blink tick
        // repaints that row too, so a caret that moved since this paint
        // cannot leave the bar behind on the surface (the stale/ghost
        // caret).
        if focused && caret_on && !caret_drawn && caret >= row.char_start && caret <= row.char_end {
            let local = caret.saturating_sub(row.char_start);
            let caret_x = x.saturating_add(font.engine.text_advance(
                font.resolved,
                font.key,
                &row.text,
                local,
            ));
            fill_rect_clipped(
                ctx.state,
                info,
                control,
                IRect::from_xywh(caret_x, y, 1, line_h),
                0x0000_0000,
            );
            caret_drawn = true;
            drawn_row = Some(first_row.saturating_add(i));
        }
    }
    // Scrollbar chrome: painted LAST so it overdraws the border/text at the
    // client edges (the classic scrollbars are window chrome, not text area).
    if area.v_scroll_visible {
        paint_vertical_scrollbar(
            ctx.state,
            info,
            control,
            first_visible_line,
            area.total,
            area.visible,
        );
    }
    if area.h_scroll_visible {
        paint_horizontal_scrollbar(
            ctx.state,
            info,
            control,
            first_visible_column,
            area.max_line_width,
            area.wrap_width,
        );
    }
    // Consume the band: the next paint starts clean (the window's own
    // `invalidated` flag drives the next cycle), and the coverage counter
    // records how many rows this paint drew — the row-level invalidation
    // gate (typing one char paints ≤ the rows it changed; a structural
    // change still paints every visible row).
    if let Some(ControlState::Edit {
        invalid_rows,
        last_paint_rows,
        last_caret_drawn_row,
        ..
    }) = ctx
        .state
        .window_state()
        .control_states
        .get_mut(&info.dc_window)
    {
        *invalid_rows = EditInvalidation::Clean;
        *last_paint_rows = painted_rows;
        *last_caret_drawn_row = drawn_row;
    }
    Ok(())
}
