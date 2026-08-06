//! The EDIT's line/layout/scroll math — the pure text geometry the mutation,
//! keyboard, mouse, paint, and message paths share: the logical-line model,
//! the wrap-aware visual-row layout, the scroll context, and the row/char
//! span mapping. Split from the monolithic `edit.rs`; the `pub(super)` items
//! are the cross-file surface imported through `super::math::…`.

use crate::user32::WinApiState;
use crate::user32::controls::{ControlState, ES_MULTILINE};

use super::state::{SCROLLBAR_WIDTH, edit_wrap_from_style, scrollbar_visible};

/// The character index of the start of `line` (None when out of range).
pub(super) fn line_index_of(text: &str, line: usize) -> Option<usize> {
    let mut index = 0;
    for (i, line_text) in text.split('\n').enumerate() {
        if i == line {
            return Some(index);
        }
        index = index
            .saturating_add(line_text.chars().count())
            .saturating_add(1);
    }
    None
}

/// The line containing `char_index`. A position exactly on a line's trailing
/// `\n` belongs to that line; a position past the end clamps to the last line.
pub(super) fn line_from_char(text: &str, char_index: usize) -> usize {
    let mut index = 0_usize;
    let mut line = 0_usize;
    for line_text in text.split('\n') {
        let line_end = index.saturating_add(line_text.chars().count());
        if char_index <= line_end {
            return line;
        }
        index = line_end.saturating_add(1);
        line += 1;
    }
    line.saturating_sub(1)
}

/// The number of characters in `line`, excluding its `\n` (None when out of
/// range). `text.split('\n').count()` is the line count, so a trailing empty
/// line and an empty text both report 1 line.
pub(super) fn line_char_len(text: &str, line: usize) -> Option<usize> {
    text.split('\n')
        .nth(line)
        .map(|line_text| line_text.chars().count())
}

/// One visual row captured while walking the logical lines — the segment's
/// slice bounds and whole-text char offset, plus its visual-line index (the
/// y is re-based only after the scroll offset is clamped).
struct VisualRow {
    visual: usize,
    chars: Vec<char>,
    seg_start: usize,
    seg_end: usize,
    line_start_char: usize,
}

/// A visual row of an EDIT's text ready to paint: one logical line, or one
/// wrapped slice of it, at a client-relative position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisibleSegment {
    /// The row's text (a substring of one logical line; empty for a blank
    /// line).
    pub text: String,
    /// Client-relative y: (visual line − first visible) × line height.
    pub y: i32,
    /// Client-relative x after ES_LEFT/CENTER/RIGHT alignment.
    pub x: i32,
    /// Whole-text char index of the row's first character.
    pub char_start: usize,
    /// Whole-text char index one past the row's last character.
    pub char_end: usize,
}

/// Lay the EDIT text out into paint rows.
///
/// With `wrap` set, a logical line longer than `width` splits into visual
/// segments at the width (greedy char-level wrap; a single over-wide glyph
/// still emits its own row). `first_visible` counts VISUAL rows — wrapped
/// segments included — and clamps to the last row when it points past the
/// end. `advance` measures one glyph's advance so callers feed real font
/// metrics while tests pass a fixed width. `alignment` is the ES_LEFT (0) /
/// ES_CENTER (1) / ES_RIGHT (2) bit value; rows wider than the column always
/// start at the left edge.
#[must_use]
pub(crate) fn layout_visible_lines<F>(
    text: &str,
    width: i32,
    line_height: i32,
    first_visible: usize,
    wrap: bool,
    alignment: u32,
    advance: &mut F,
) -> Vec<VisibleSegment>
where
    F: FnMut(char) -> i32,
{
    let mut rows: Vec<VisualRow> = Vec::new();
    let mut line_start_char = 0_usize;
    let mut visual = 0_usize;
    for line_text in text.split('\n') {
        let chars: Vec<char> = line_text.chars().collect();
        let line_len = chars.len();
        let mut seg_start = 0_usize;
        let mut x = 0_i32;
        for (i, ch) in chars.iter().enumerate() {
            let w = advance(*ch);
            if wrap && x > 0 && x.saturating_add(w) > width {
                rows.push(VisualRow {
                    visual,
                    chars: chars.clone(),
                    seg_start,
                    seg_end: i,
                    line_start_char,
                });
                visual = visual.saturating_add(1);
                seg_start = i;
                x = 0;
            }
            x = x.saturating_add(w);
        }
        // Every line emits at least one row, so blank lines still occupy
        // their vertical slot and no glyph is dropped.
        rows.push(VisualRow {
            visual,
            chars,
            seg_start,
            seg_end: line_len,
            line_start_char,
        });
        visual = visual.saturating_add(1);
        line_start_char = line_start_char.saturating_add(line_len).saturating_add(1);
    }
    // Clamp the scroll offset to the last row (empty text still has one).
    let first = first_visible.min(visual.saturating_sub(1));
    let mut segments = Vec::with_capacity(rows.len());
    for row in rows {
        if row.visual < first {
            continue;
        }
        let y = i32::try_from(row.visual.saturating_sub(first))
            .unwrap_or(0)
            .saturating_mul(line_height);
        let row_text: String = row
            .chars
            .iter()
            .skip(row.seg_start)
            .take(row.seg_end.saturating_sub(row.seg_start))
            .collect();
        let row_width = row_text
            .chars()
            .fold(0_i32, |acc, ch| acc.saturating_add(advance(ch)));
        let x = match alignment {
            0x1 => (width.saturating_sub(row_width)).saturating_div(2).max(0),
            0x2 => width.saturating_sub(row_width).max(0),
            _ => 0,
        };
        segments.push(VisibleSegment {
            text: row_text,
            y,
            x,
            char_start: row.line_start_char.saturating_add(row.seg_start),
            char_end: row.line_start_char.saturating_add(row.seg_end),
        });
    }
    segments
}
/// The number of visual rows that fit in a client of `client_height` px at
/// `line_height` px per row. Floor division — a partial row at the bottom is
/// clipped. A degenerate (zero-height) client still reports one row so the
/// caret row stays reachable and the clamp never goes below the last row.
#[must_use]
pub(crate) fn visible_line_count(client_height: i32, line_height: i32) -> usize {
    if line_height <= 0 {
        return 1;
    }
    usize::try_from(client_height.saturating_div(line_height))
        .unwrap_or(0)
        .max(1)
}

/// Clamp a scroll offset so the viewport stays inside the text: `first` must
/// be at most `total − visible` (0 when the text fits entirely). The upper
/// bound keeps the LAST visual row on screen — scrolling past it would leave
/// a gap at the bottom.
#[must_use]
pub(crate) fn clamp_scroll_offset(
    first: usize,
    total_visual_lines: usize,
    visible_lines: usize,
) -> usize {
    first.min(total_visual_lines.saturating_sub(visible_lines))
}

/// One pass over the EDIT text producing the wrap-aware visual-row bookkeeping
/// the scroll math needs: the TOTAL row count and the row holding
/// `char_index`. Walks the same greedy width rule as `layout_visible_lines`
/// (so the scroll clamp and the painted rows always agree); a caret exactly at
/// a wrap boundary belongs to the row that ENDS at it — the row `paint_edit`
/// draws the caret bar on.
#[must_use]
pub(crate) fn visual_rows<F>(
    text: &str,
    width: i32,
    wrap: bool,
    char_index: usize,
    advance: &mut F,
) -> (usize, usize)
where
    F: FnMut(char) -> i32,
{
    let mut rows = 0_usize;
    let mut caret_row = 0_usize;
    let mut caret_found = false;
    let mut line_start_char = 0_usize;
    for line_text in text.split('\n') {
        let mut x = 0_i32;
        for (i, ch) in line_text.chars().enumerate() {
            let w = advance(ch);
            if wrap && x > 0 && x.saturating_add(w) > width {
                let seg_end = line_start_char.saturating_add(i);
                if !caret_found && char_index <= seg_end {
                    caret_row = rows;
                    caret_found = true;
                }
                rows = rows.saturating_add(1);
                x = 0;
            }
            x = x.saturating_add(w);
        }
        let row_end = line_start_char.saturating_add(line_text.chars().count());
        if !caret_found && char_index <= row_end {
            caret_row = rows;
            caret_found = true;
        }
        rows = rows.saturating_add(1);
        line_start_char = row_end.saturating_add(1);
    }
    if !caret_found {
        // A caret past every row end (a corrupted index) lands on the last row.
        caret_row = rows.saturating_sub(1);
    }
    (rows, caret_row)
}

/// The widest line's advance sum in px — the horizontal scroll range's far
/// end for a wrap-off EDIT (a line's intrinsic width; independent of the
/// viewport).
#[must_use]
fn max_line_advance<F>(text: &str, advance: &mut F) -> i32
where
    F: FnMut(char) -> i32,
{
    text.split('\n')
        .map(|line| {
            line.chars()
                .fold(0_i32, |acc, ch| acc.saturating_add(advance(ch)))
        })
        .max()
        .unwrap_or(0)
}

/// The resolved text-area geometry of a multiline EDIT — shared by the scroll
/// math (`edit_scroll_context`) and the paint (`paint_edit`) so their row
/// counts, wrap columns, and scrollbar visibility ALWAYS agree (the F5
/// deferral's stated reason). The vertical scrollbar, when shown, reserves a
/// `SCROLLBAR_WIDTH` gutter in the right of the client, shrinking the wrap
/// column; the horizontal scrollbar (wrap-off edits with WS_HSCROLL) reserves
/// a bottom strip that the visible-row count reflects.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EditTextArea {
    /// The wrap column: client width minus the 2 px side margins minus the
    /// vertical gutter when the V scrollbar is shown.
    pub wrap_width: i32,
    /// Whether the vertical scrollbar is shown (the content overflows the
    /// viewport after the gutter reservation).
    pub v_scroll_visible: bool,
    /// Whether the horizontal scrollbar is shown (a wrap-off line overflows
    /// the text area).
    pub h_scroll_visible: bool,
    /// The wrap-aware visual row count at `wrap_width` — the V scroll range's
    /// far end.
    pub total: usize,
    /// The visual row holding the caret.
    pub caret_row: usize,
    /// The row count visible in the client (the H strip, when shown, is not
    /// part of the text area).
    pub visible: usize,
    /// The widest line's advance sum in px.
    pub max_line_width: i32,
}

/// Resolve an EDIT's text-area geometry (see [`EditTextArea`]).
///
/// Visibility runs in a FIXED order with no feedback: the V scrollbar is
/// decided on the no-gutter width first (reserving its gutter can only add
/// wrap rows, so a shown scrollbar never needs to hide); the H scrollbar then
/// compares the widest line against the remaining text area. `style` is the
/// window's creation style; the wrap decision derives from it here, so the
/// callers cannot disagree about it.
pub(crate) fn edit_text_area<F>(
    text: &str,
    width: i32,
    client_height: i32,
    line_h: i32,
    style: u32,
    caret: usize,
    advance: &mut F,
) -> EditTextArea
where
    F: FnMut(char) -> i32,
{
    let wrap = edit_wrap_from_style(style);
    let no_gutter = width.saturating_sub(4);
    let total_no_gutter = visual_rows(text, no_gutter, wrap, caret, advance).0;
    let v_visible = scrollbar_visible(
        style,
        total_no_gutter,
        visible_line_count(client_height, line_h),
    );
    let v_gutter = if v_visible { SCROLLBAR_WIDTH } else { 0 };
    let max_line_width = max_line_advance(text, advance);
    // The H bar is a multiline no-wrap EDIT's (WS_HSCROLL implies no wrap);
    // a single-line EDIT auto-scrolls instead and never shows chrome.
    let h_visible =
        style & ES_MULTILINE != 0 && !wrap && max_line_width > no_gutter.saturating_sub(v_gutter);
    let h_strip = if h_visible { SCROLLBAR_WIDTH } else { 0 };
    let wrap_width = no_gutter.saturating_sub(v_gutter);
    let (total, caret_row) = visual_rows(text, wrap_width, wrap, caret, advance);
    EditTextArea {
        wrap_width,
        v_scroll_visible: v_visible,
        h_scroll_visible: h_visible,
        total,
        caret_row,
        visible: visible_line_count(client_height.saturating_sub(h_strip), line_h),
        max_line_width,
    }
}

/// The wrap-aware vertical scroll context of a multiline EDIT: how many visual
/// rows fit in the client (`visible`), the full row count (`total`), and the
/// visual row holding the caret (`caret_row`). All three derive from the same
/// greedy width walk `paint_edit`'s layout runs, so the scroll math and the
/// painted rows always agree. `wrap_width` rides along so the caret-blink
/// invalidation can stamp the band it computes. `None` when the window is gone.
#[derive(Debug, Clone, Copy)]
pub(super) struct EditScrollContext {
    pub(super) visible: usize,
    pub(super) total: usize,
    pub(super) caret_row: usize,
    /// The wrap column the context was resolved at (the row-invalidation
    /// band stamp).
    pub(super) wrap_width: i32,
    /// The horizontal scroll range in px (widest line − text area); 0 when
    /// the H scrollbar is hidden (wrap-on or the content fits).
    pub(super) h_overflow: usize,
    /// The horizontal page size in px (the visible text width).
    pub(super) h_page: usize,
    /// Whether each scrollbar is currently shown.
    pub(super) v_scroll_visible: bool,
    pub(super) h_scroll_visible: bool,
    /// The client dimensions (for the gutter hit-test).
    pub(super) client_width: i32,
    pub(super) client_height: i32,
}

/// Resolve an EDIT's [`EditScrollContext`] from its client height, the
/// stored control font's line height (the system default when none is set),
/// and the wrap-aware visual row count.
pub(super) fn edit_scroll_context(state: &mut WinApiState, hwnd: u64) -> Option<EditScrollContext> {
    let (client_height, text, style, width, caret) = {
        let ws = state.window_state();
        let w = ws
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))?;
        let caret = match ws.control_states.get(&crate::handles::Hwnd::from(hwnd)) {
            Some(ControlState::Edit { caret, .. }) => *caret,
            _ => 0,
        };
        (w.height, w.control_text.clone(), w.style, w.width, caret)
    };
    // The font engine is taken out of gdi state so the advance closure can
    // run next to it (the paint path does the same); it is put back
    // unconditionally. Safe under the single shared WinApiState mutex — the
    // take and the put cannot interleave with another handler's.
    // The STORED font (falling back to the system default) drives the line
    // height — the same resolution the paint path uses, so the scroll math
    // and the painted rows agree even after a WM_SETFONT.
    let area = state.with_font_engine(|state, font_engine| {
        let key_and_resolved =
            crate::gdi32::window_font_resolution_or_default(state, hwnd, font_engine);
        match &key_and_resolved {
            Some((key, resolved)) => {
                let line_h = resolved.line_height();
                let advance = &mut |ch: char| font_engine.char_advance(resolved, key, ch);
                edit_text_area(&text, width, client_height, line_h, style, caret, advance)
            }
            // No system font: the 16 px default the EM_* line APIs assume, with
            // a fixed 8 px/char advance for the wrap walk.
            None => edit_text_area(&text, width, client_height, 16, style, caret, &mut |_| {
                8_i32
            }),
        }
    });
    // The H scroll range is non-zero only while the H bar is shown — a wrap-on
    // EDIT (or a line that fits) has nothing to scroll, so WM_HSCROLL stays a
    // no-op there (the pre-deferral behavior).
    let h_overflow = if area.h_scroll_visible {
        usize::try_from(area.max_line_width.saturating_sub(area.wrap_width).max(0)).unwrap_or(0)
    } else {
        0
    };
    Some(EditScrollContext {
        visible: area.visible,
        total: area.total,
        caret_row: area.caret_row,
        wrap_width: area.wrap_width,
        h_overflow,
        h_page: usize::try_from(area.wrap_width.max(0)).unwrap_or(0),
        v_scroll_visible: area.v_scroll_visible,
        h_scroll_visible: area.h_scroll_visible,
        client_width: width,
        client_height,
    })
}
