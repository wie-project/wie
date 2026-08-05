//! The EDIT mouse paths: click-to-caret placement, drag selection, the
//! word-selecting double-click, the scrollbar-gutter presses/thumb drags, and
//! `EM_SCROLLCARET` (the caret-into-view scroll the click handlers hand off
//! to). Split from the monolithic `edit.rs`; the `pub(super)` items are the
//! cross-file surface imported through `super::mouse::…`.

use crate::gdi32::FontKey;
use crate::user32::WinApiState;
use crate::user32::controls::{ControlState, HitTestLayout, ScrollDrag, control_state};

use super::math::{clamp_scroll_offset, edit_scroll_context, edit_text_area, layout_visible_lines};
use super::messages::{
    SB_PAGEDOWN, SB_PAGELEFT, SB_PAGERIGHT, SB_PAGEUP, edit_scroll_horizontal, edit_scroll_vertical,
};
use super::paint::{edit_invalidate_full, edit_invalidate_span, scrollbar_thumb};
use super::state::{
    ES_ALIGN_MASK, SCROLLBAR_WIDTH, edit_state_for_window, edit_state_mut, edit_wrap_from_style,
};

/// The char index whose glyph cell contains the client point (x, y) — the
/// inverse of the advance-summed caret x `paint_edit` draws.
///
/// `layout.wrap_width` is the client width minus the 2 px side margins (the
/// same column `layout_visible_lines` lays out against); the row for y is
/// picked from that same visual-row layout, so a click in wrapped text lands
/// on the glyph the paint shows there. A click left of the text clamps to the
/// row start, past the last glyph to the row end; a single-line EDIT (wrap
/// off, one row at y=0) picks its only row for any y.
#[must_use]
pub(crate) fn edit_char_index_at_point<F>(
    text: &str,
    x: i32,
    y: i32,
    layout: &HitTestLayout,
    advance: &mut F,
) -> usize
where
    F: FnMut(char) -> i32,
{
    let rows = layout_visible_lines(
        text,
        layout.wrap_width,
        layout.line_height,
        layout.first_visible,
        layout.wrap,
        layout.alignment,
        advance,
    );
    // The last row at or above the click (rows are y-rebased to 0 at the
    // first visible row); a click above the first row falls back to it.
    let Some(row) = rows
        .iter()
        .rev()
        .find(|r| r.y <= y)
        .or_else(|| rows.first())
    else {
        return 0;
    };
    // The paint starts text at the 2 px left margin plus the row's alignment
    // offset (`tx = offset + 2`, then `x = tx + row.x`).
    let row_x = x.saturating_sub(2).saturating_sub(row.x);
    if row_x <= 0 {
        return row.char_start;
    }
    let mut acc = 0_i32;
    let mut local = 0_usize;
    for ch in row.text.chars() {
        let w = advance(ch);
        let cell_right = acc.saturating_add(w);
        if row_x < cell_right {
            // The click is in this glyph's cell; the half-advance boundary
            // decides whether the caret lands before or after the char.
            let before = row_x < acc.saturating_add(w.saturating_div(2));
            return row.char_start.saturating_add(if before {
                local
            } else {
                local.saturating_add(1)
            });
        }
        acc = cell_right;
        local = local.saturating_add(1);
    }
    row.char_end
}

/// The char index at the client point (x, y) for an EDIT window, resolving
/// the stored font exactly like the paint path (`None` when the window is
/// gone). The wrap flag and wrap column derive from the window's style and
/// width the same way `paint_edit` derives them, so the caret lands on the
/// glyph that is drawn at the click.
fn edit_char_at_point(state: &mut WinApiState, hwnd: u64, x: i32, y: i32) -> Option<usize> {
    let (text, style, width, height, first_visible_line, first_visible_column, caret) = {
        let ws = state.window_state();
        let w = ws
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))?;
        let (first_visible_line, first_visible_column, caret) =
            match ws.control_states.get(&crate::handles::Hwnd::from(hwnd)) {
                Some(ControlState::Edit {
                    first_visible_line,
                    first_visible_column,
                    caret,
                    ..
                }) => (*first_visible_line, *first_visible_column, *caret),
                _ => (0, 0, 0),
            };
        (
            w.control_text.clone(),
            w.style,
            w.width,
            w.height,
            first_visible_line,
            first_visible_column,
            caret,
        )
    };
    let wrap = edit_wrap_from_style(style);
    let alignment = style & ES_ALIGN_MASK;
    // The font engine is taken out of gdi state so the advance closure can
    // run next to it (the paint path does the same); it is put back
    // unconditionally. Safe under the single shared WinApiState mutex.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = FontKey::default();
    // The STORED font (falling back to the system default) drives the line
    // height and the per-glyph advances — the same resolution the paint path
    // uses, so the click-to-caret mapping and the drawn glyphs agree.
    let key_and_resolved = match crate::gdi32::window_font_resolution(state, hwnd, &mut font_engine)
    {
        Some(key_and_resolved) => Some(key_and_resolved),
        None => font_engine
            .resolve(&default_key, 16)
            .map(|resolved| (default_key, resolved)),
    };
    let result = match &key_and_resolved {
        Some((key, resolved)) => {
            let line_h = resolved.line_height();
            let advance = &mut |ch: char| font_engine.char_advance(resolved, key, ch);
            let area = edit_text_area(&text, width, height, line_h, style, caret, advance);
            // A wrap-off EDIT scrolled right draws its rows shifted left by
            // the offset; add it back so the click maps to the drawn glyph.
            let shift = if area.h_scroll_visible {
                i32::try_from(first_visible_column).unwrap_or(0)
            } else {
                0
            };
            edit_char_index_at_point(
                &text,
                x.saturating_add(shift),
                y,
                &HitTestLayout {
                    wrap_width: area.wrap_width,
                    line_height: line_h,
                    first_visible: first_visible_line,
                    wrap,
                    alignment,
                },
                advance,
            )
        }
        // No system font: the 16 px default the EM_* line APIs assume, with
        // a fixed 8 px/char advance for the hit test.
        None => {
            let area = edit_text_area(&text, width, height, 16, style, caret, &mut |_| 8_i32);
            edit_char_index_at_point(
                &text,
                x,
                y,
                &HitTestLayout {
                    wrap_width: area.wrap_width,
                    line_height: 16,
                    first_visible: first_visible_line,
                    wrap,
                    alignment,
                },
                &mut |_| 8_i32,
            )
        }
    };
    state.gdi_state().font_engine = font_engine;
    Some(result)
}

/// EDIT: WM_LBUTTONDOWN — place the caret at the click and collapse the
/// selection; the click becomes the drag anchor (the selection edge later
/// mouse moves extend from). The dispatch arm sets the mouse capture.
///
/// A press in a scrollbar gutter is consumed by the scrollbar instead: a
/// track click pages toward the pointer, a press on the thumb arms a drag
/// (the follow-up moves drive the offset through `edit_scrollbar_drag`).
pub(super) fn edit_mouse_down(state: &mut WinApiState, hwnd: u64, x: i32, y: i32) {
    if edit_scrollbar_press(state, hwnd, x, y) {
        return;
    }
    let Some(index) = edit_char_at_point(state, hwnd, x, y) else {
        return;
    };
    let span = {
        let ws = state.window_state();
        let style = ws
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .map_or(0, |w| w.style);
        let ControlState::Edit {
            caret,
            sel_start,
            sel_end,
            goal_column,
            ..
        } = edit_state_mut(&mut ws.control_states, hwnd, style)
        else {
            return;
        };
        // The click moves the caret (and collapses the selection): the rows
        // holding the old and new caret/selection spans must repaint.
        let lo = (*sel_start).min(*sel_end).min(*caret).min(index);
        let hi = (*sel_start).max(*sel_end).max(*caret).max(index);
        *caret = index;
        *sel_start = index;
        *sel_end = index;
        // A click is horizontal movement: the vertical-movement goal column
        // is stale (the same clearing horizontal keys apply).
        *goal_column = None;
        (lo, hi)
    };
    edit_invalidate_span(state, hwnd, span.0, span.1.saturating_add(1));
    // Task 2.4 handoff: a click in the partial strip below the last full
    // visible row (or any off-viewport hit) must bring the caret row into
    // view. The dispatch arm invalidates unconditionally after the handler
    // runs, so the repaint is covered whether or not the viewport moved.
    edit_scroll_caret(state, hwnd);
}

/// Resolve a press in the vertical/horizontal scrollbar gutter of an EDIT.
/// A track click pages toward the pointer; a press on the thumb arms a thumb
/// drag (stored on the control state; `edit_mouse_move` follows it while the
/// edit holds the capture). Returns whether the press was consumed.
fn edit_scrollbar_press(state: &mut WinApiState, hwnd: u64, x: i32, y: i32) -> bool {
    let Some(context) = edit_scroll_context(state, hwnd) else {
        return false;
    };
    // The vertical gutter: the right SCROLLBAR_WIDTH column.
    if context.v_scroll_visible && x >= context.client_width.saturating_sub(SCROLLBAR_WIDTH) {
        let (thumb, thumb_pos) = scrollbar_thumb(
            context.client_height,
            first_visible_line_of(state, hwnd),
            context.total.saturating_sub(context.visible),
            context.visible,
        );
        if y < thumb_pos {
            edit_scroll_vertical(state, hwnd, SB_PAGEUP, 0);
        } else if y >= thumb_pos.saturating_add(thumb) {
            edit_scroll_vertical(state, hwnd, SB_PAGEDOWN, 0);
        } else {
            set_scrollbar_drag(state, hwnd, true, y.saturating_sub(thumb_pos));
        }
        return true;
    }
    // The horizontal gutter: the bottom SCROLLBAR_WIDTH strip of a wrap-off
    // EDIT with an overflowing line.
    if context.h_scroll_visible && y >= context.client_height.saturating_sub(SCROLLBAR_WIDTH) {
        let (thumb, thumb_pos) = scrollbar_thumb(
            context.client_width,
            first_visible_column_of(state, hwnd),
            context.h_overflow,
            context.h_page,
        );
        if x < thumb_pos {
            edit_scroll_horizontal(state, hwnd, SB_PAGELEFT, 0);
        } else if x >= thumb_pos.saturating_add(thumb) {
            edit_scroll_horizontal(state, hwnd, SB_PAGERIGHT, 0);
        } else {
            set_scrollbar_drag(state, hwnd, false, x.saturating_sub(thumb_pos));
        }
        return true;
    }
    false
}

/// The current vertical scroll offset of `hwnd` (0 when the state is absent).
#[must_use]
fn first_visible_line_of(state: &WinApiState, hwnd: u64) -> usize {
    match control_state(state, hwnd) {
        Some(ControlState::Edit {
            first_visible_line, ..
        }) => *first_visible_line,
        _ => 0,
    }
}

/// The current horizontal scroll offset (px) of `hwnd`.
#[must_use]
fn first_visible_column_of(state: &WinApiState, hwnd: u64) -> usize {
    match control_state(state, hwnd) {
        Some(ControlState::Edit {
            first_visible_column,
            ..
        }) => *first_visible_column,
        _ => 0,
    }
}

/// Arm a scrollbar thumb drag with the given axis and grab offset.
fn set_scrollbar_drag(state: &mut WinApiState, hwnd: u64, vertical: bool, grab_offset: i32) {
    if let ControlState::Edit { scrollbar_drag, .. } = edit_state_for_window(state, hwnd) {
        *scrollbar_drag = Some(ScrollDrag {
            vertical,
            grab_offset,
        });
    }
}

/// EDIT: WM_MOUSEMOVE while the edit holds the capture — extend the drag
/// selection from the anchor (the click position) to the current position.
///
/// The anchor is the selection edge the caret is not at, since the caret
/// tracks the pointer (the same anchor model `edit_move_caret` uses for
/// Shift-arrows); crossing the anchor flips the selection edge. Hover moves
/// without capture are no-ops. An in-flight scrollbar thumb drag takes
/// precedence (it scrolls instead of selecting). Returns whether the
/// selection or scroll offset changed.
pub(super) fn edit_mouse_move(state: &mut WinApiState, hwnd: u64, x: i32, y: i32) -> bool {
    if state.window_state().capture_window_handle != crate::handles::Hwnd::from(hwnd) {
        return false;
    }
    if let Some(drag) = scrollbar_drag_of(state, hwnd) {
        return edit_scrollbar_drag(state, hwnd, drag, x, y);
    }
    let Some(index) = edit_char_at_point(state, hwnd, x, y) else {
        return false;
    };
    let mut dirty_span: Option<(usize, usize)> = None;
    let changed = {
        let ws = state.window_state();
        let style = ws
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .map_or(0, |w| w.style);
        let ControlState::Edit {
            caret,
            sel_start,
            sel_end,
            goal_column,
            ..
        } = edit_state_mut(&mut ws.control_states, hwnd, style)
        else {
            return false;
        };
        if index == *caret {
            false
        } else {
            let (old_sel_start, old_sel_end) = (*sel_start, *sel_end);
            let old_caret = *caret;
            let anchor = if *caret == *sel_start {
                *sel_end
            } else {
                *sel_start
            };
            *sel_start = anchor.min(index);
            *sel_end = anchor.max(index);
            *caret = index;
            // Horizontal movement drops the vertical-movement goal column like
            // any horizontal key.
            *goal_column = None;
            // The drag extended the selection: the old and new spans' rows
            // must repaint.
            let lo = old_sel_start
                .min(old_sel_end)
                .min(old_caret)
                .min(index)
                .min(*sel_start)
                .min(*sel_end);
            let hi = old_sel_start
                .max(old_sel_end)
                .max(old_caret)
                .max(index)
                .max(*sel_start)
                .max(*sel_end);
            dirty_span = Some((lo, hi));
            true
        }
    };
    if let Some((lo, hi)) = dirty_span {
        edit_invalidate_span(state, hwnd, lo, hi.saturating_add(1));
    }
    changed
}

/// The in-flight thumb drag of `hwnd`, if any.
fn scrollbar_drag_of(state: &WinApiState, hwnd: u64) -> Option<ScrollDrag> {
    match control_state(state, hwnd) {
        Some(ControlState::Edit { scrollbar_drag, .. }) => *scrollbar_drag,
        _ => None,
    }
}

/// Follow an armed scrollbar thumb drag: map the pointer (minus the grab
/// offset) onto the scrollbar travel, then back to the scroll offset.
/// Returns whether the offset changed.
fn edit_scrollbar_drag(
    state: &mut WinApiState,
    hwnd: u64,
    drag: ScrollDrag,
    x: i32,
    y: i32,
) -> bool {
    let Some(context) = edit_scroll_context(state, hwnd) else {
        return false;
    };
    let (track, span, visible, pointer) = if drag.vertical {
        (
            context.client_height,
            context.total.saturating_sub(context.visible),
            context.visible,
            y,
        )
    } else {
        (context.client_width, context.h_overflow, context.h_page, x)
    };
    let (thumb, _) = scrollbar_thumb(track, 0, span, visible);
    let travel = track.saturating_sub(thumb);
    let within = pointer.saturating_sub(drag.grab_offset).clamp(0, travel);
    // Map the pointer back onto the scroll range (travel → span).
    let offset = i64::from(within)
        .saturating_mul(i64::try_from(span).unwrap_or(0))
        .saturating_div(i64::from(travel.max(1)))
        .max(0);
    let offset = usize::try_from(offset).unwrap_or(0);
    if drag.vertical {
        let ControlState::Edit {
            first_visible_line, ..
        } = edit_state_for_window(state, hwnd)
        else {
            return false;
        };
        let old = *first_visible_line;
        *first_visible_line = offset.min(span);
        let moved = *first_visible_line != old;
        if moved {
            // A scroll reflows the viewport: any pending row band is stale.
            edit_invalidate_full(state, hwnd);
        }
        moved
    } else {
        let ControlState::Edit {
            first_visible_column,
            ..
        } = edit_state_for_window(state, hwnd)
        else {
            return false;
        };
        let old = *first_visible_column;
        *first_visible_column = offset.min(span);
        let moved = *first_visible_column != old;
        if moved {
            // A scroll reflows the viewport: any pending row band is stale.
            edit_invalidate_full(state, hwnd);
        }
        moved
    }
}

/// EDIT: WM_LBUTTONUP — end the drag session: release the mouse capture and
/// clear any in-flight scrollbar thumb drag (the selection stays as-is;
/// Windows finalizes the drag on release).
pub(crate) fn edit_mouse_up(state: &mut WinApiState, hwnd: u64) {
    if state.window_state().capture_window_handle == crate::handles::Hwnd::from(hwnd) {
        state.window_state().capture_window_handle = crate::handles::Hwnd::NULL;
    }
    if let ControlState::Edit { scrollbar_drag, .. } = edit_state_for_window(state, hwnd) {
        *scrollbar_drag = None;
    }
}

/// The whitespace-delimited word `[start, end)` containing `char_index`; a
/// click on whitespace (or past the end of the text) yields an empty selection
/// at the index. `char::is_whitespace` splits words, matching the Win32
/// `IsCharAlphaNumeric`-style word breaking closely enough for an EDIT.
#[must_use]
fn word_bounds(text: &str, char_index: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let index = char_index.min(len);
    if index >= len || chars.get(index).is_none_or(|c| c.is_whitespace()) {
        return (index, index);
    }
    let mut start = index;
    while start > 0
        && !chars
            .get(start.saturating_sub(1))
            .is_some_and(|c| c.is_whitespace())
    {
        start = start.saturating_sub(1);
    }
    let mut end = index;
    while end < len && !chars.get(end).is_some_and(|c| c.is_whitespace()) {
        end = end.saturating_add(1);
    }
    (start, end)
}

/// EDIT: WM_LBUTTONDBLCLK — select the whitespace-delimited word under the
/// click (empty when the click lands on whitespace); the click becomes the
/// drag anchor so a subsequent drag extends from the word. The dispatch arm
/// sets the capture and focus like a single-click press.
pub(super) fn edit_mouse_dblclk(state: &mut WinApiState, hwnd: u64, x: i32, y: i32) {
    let Some(index) = edit_char_at_point(state, hwnd, x, y) else {
        return;
    };
    let span = {
        let ws = state.window_state();
        let Some(window) = ws
            .windows
            .iter_mut()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        else {
            return;
        };
        let style = window.style;
        let len = window.control_text.chars().count();
        let (word_start, word_end) = word_bounds(&window.control_text, index);
        let ControlState::Edit {
            caret,
            sel_start,
            sel_end,
            goal_column,
            ..
        } = edit_state_mut(&mut ws.control_states, hwnd, style)
        else {
            return;
        };
        // The word selection replaces the old selection: both spans' rows
        // must repaint.
        let lo = (*sel_start)
            .min(*sel_end)
            .min(*caret)
            .min(word_start)
            .min(word_end);
        let hi = (*sel_start)
            .max(*sel_end)
            .max(*caret)
            .max(word_start)
            .max(word_end);
        *caret = word_end.min(len);
        *sel_start = word_start.min(len);
        *sel_end = word_end.min(len);
        // The caret moved horizontally (to the word end); drop any remembered
        // vertical-movement goal column.
        *goal_column = None;
        (lo, hi)
    };
    edit_invalidate_span(state, hwnd, span.0, span.1.saturating_add(1));
    // Same Task 2.4 handoff as a single click: a double-click on a row below
    // the last full visible row must scroll the selected word into view (the
    // dispatch arm's unconditional invalidate covers the repaint).
    edit_scroll_caret(state, hwnd);
}

/// EDIT: EM_SCROLLCARET — bring the caret's visual row into the viewport by
/// the SMALLEST scroll: a caret below the bottom edge advances
/// `first_visible_line` just enough to show it on the LAST visible row; a
/// caret above the top edge jumps the viewport up to it; a caret already
/// visible leaves the offset untouched. Windows scrolls minimally — it does
/// NOT snap the caret line to the top (the pre-Task-2.4 behavior). Returns
/// whether the offset moved.
pub(super) fn edit_scroll_caret(state: &mut WinApiState, hwnd: u64) -> bool {
    let Some(context) = edit_scroll_context(state, hwnd) else {
        return false;
    };
    let ControlState::Edit {
        first_visible_line, ..
    } = edit_state_for_window(state, hwnd)
    else {
        return false;
    };
    let old = *first_visible_line;
    if context.caret_row < old {
        // Above the viewport: reveal the caret at the top edge.
        *first_visible_line = context.caret_row;
    } else if context.caret_row >= old.saturating_add(context.visible) {
        // Below the viewport: pull it up to the bottom edge only.
        *first_visible_line = clamp_scroll_offset(
            context
                .caret_row
                .saturating_sub(context.visible)
                .saturating_add(1),
            context.total,
            context.visible,
        );
    }
    let moved = *first_visible_line != old;
    if moved {
        // A scroll reflows the viewport: any pending row band is stale.
        edit_invalidate_full(state, hwnd);
    }
    moved
}
