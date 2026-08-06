//! The EDIT keyboard paths: caret movement (arrow/Home/End/PgUp/PgDn,
//! Shift-extension, VK_DELETE) and the caret/line-metric EM_* handlers
//! (`EM_SETSEL`/`EM_GETSEL`/`EM_LIMITTEXT`/`EM_GETLINE`/…), plus the guest
//! Shift/Ctrl state queries. Split from the monolithic `edit.rs`; the
//! `pub(super)` items are the cross-file surface imported through
//! `super::keyboard::…`.

use anyhow::Result;

use crate::gdi32::FontKey;
use crate::guest_memory::read_u16 as read_guest_u16;
use crate::user32::controls::{ControlState, ES_MULTILINE, control_state};
use crate::user32::{
    VK_CONTROL, VK_DOWN, VK_END, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_SHIFT, VK_UP,
    WinApiState, write_guest_ansi_c_string, write_guest_utf16_c_string,
};

use super::math::{line_char_len, line_from_char, line_index_of};
use super::mutation::{EditMutation, normalized_selection, replace_crosses_lines, replace_range};
use super::paint::{edit_invalidate_mutation, edit_invalidate_span};
use super::state::{edit_state_for_window, edit_state_mut, edit_text};

/// EDIT: arrow/Home/End/PgUp/PgDn caret movement. Shift extends the selection
/// (the anchor stays at the edge the caret moved away from); without Shift the
/// selection collapses. Vertical keys are multiline-only: they move to the
/// same column on the adjacent line (or a page for PgUp/PgDn), clamping to the
/// target line's length while remembering the goal column. Home/End are
/// line-aware in a multiline EDIT (Ctrl makes them document-wide). Returns
/// whether the caret/selection moved.
pub(super) fn edit_move_caret(state: &mut WinApiState, hwnd: u64, vk: u64) -> bool {
    let extend = shift_is_down(state);
    let ctrl = ctrl_is_down(state);
    // The font engine is taken out of gdi state so the stored-font resolution
    // can run next to `state` (the same take/put the paint path uses); it is
    // put back on every path below. Safe under the single shared WinApiState
    // mutex — the take and the put cannot interleave with another handler's.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = FontKey::default();
    let key_and_resolved = match crate::gdi32::window_font_resolution(state, hwnd, &mut font_engine)
    {
        Some(key_and_resolved) => Some(key_and_resolved),
        None => font_engine
            .resolve(&default_key, 16)
            .map(|resolved| (default_key, resolved)),
    };
    let mut dirty_span: Option<(usize, usize)> = None;
    // The character position the caret LEFT — its row is invalidated again
    // below, independent of the dirty span's row mapping and of `caret_on`.
    let mut moved_from: Option<usize> = None;
    let moved = (|| {
        let ws = state.window_state();
        let Some(window) = ws
            .windows
            .iter_mut()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        else {
            return false;
        };
        let style = window.style;
        // The PgUp/PgDn page size: the client height divided by the STORED
        // font's line height (the same resolution the paint path uses), so a
        // page matches the painted rows once a WM_SETFONT changed the font.
        // A degenerate (zero) line height keeps a 1-line page.
        let line_h = key_and_resolved
            .as_ref()
            .map_or(16, |(_key, resolved)| resolved.line_height())
            .max(1);
        let page_lines = usize::try_from(window.height)
            .unwrap_or(0)
            .saturating_div(usize::try_from(line_h).unwrap_or(1));
        let ControlState::Edit {
            caret,
            sel_start,
            sel_end,
            style_bits,
            goal_column,
            ..
        } = edit_state_mut(&mut ws.control_states, hwnd, style)
        else {
            return false;
        };
        let text = &window.control_text;
        let len = text.chars().count();
        let old_caret = (*caret).min(len);
        moved_from = Some(old_caret);
        let (old_sel_start, old_sel_end) = (*sel_start, *sel_end);
        let multiline = *style_bits & ES_MULTILINE != 0;
        let vertical = multiline && matches!(vk, VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT);
        if vertical {
            // The goal column is the column of the first press of a vertical run;
            // later presses keep it so the caret returns once a longer line shows
            // up (clamping only ever applies to the SHORT lines in between).
            let line_start = line_index_of(text, line_from_char(text, old_caret)).unwrap_or(0);
            if goal_column.is_none() {
                *goal_column = Some(old_caret.saturating_sub(line_start));
            }
        } else if matches!(vk, VK_LEFT | VK_RIGHT | VK_HOME | VK_END) {
            *goal_column = None;
        }
        let new_caret = caret_navigation_target(
            text,
            old_caret,
            vk,
            ctrl,
            multiline,
            *goal_column,
            page_lines.max(1),
        );
        if new_caret == old_caret && *sel_start == *sel_end {
            return false;
        }
        if extend {
            // The anchor is the selection edge the caret is not at (or the old
            // caret when the selection was empty).
            let anchor = if *sel_start == *sel_end {
                old_caret
            } else if old_caret == (*sel_start).min(*sel_end) {
                (*sel_start).max(*sel_end)
            } else {
                (*sel_start).min(*sel_end)
            };
            let (lo, hi) = (anchor.min(new_caret), anchor.max(new_caret));
            *sel_start = lo;
            *sel_end = hi;
            *caret = new_caret;
        } else {
            *caret = new_caret;
            *sel_start = new_caret;
            *sel_end = new_caret;
        }
        // The caret bar moved from the old position to the new one, and the
        // selection highlight changed: both spans' rows must repaint.
        let lo = old_sel_start
            .min(old_sel_end)
            .min(old_caret)
            .min(new_caret)
            .min(*sel_start)
            .min(*sel_end);
        let hi = old_sel_start
            .max(old_sel_end)
            .max(old_caret)
            .max(new_caret)
            .max(*sel_start)
            .max(*sel_end);
        dirty_span = Some((lo, hi));
        true
    })();
    state.gdi_state().font_engine = font_engine;
    if !moved {
        return false;
    }
    if let Some((lo, hi)) = dirty_span {
        edit_invalidate_span(state, hwnd, lo, hi.saturating_add(1));
    }
    // A caret-only span at the position the caret LEFT: the row where the
    // caret bar was last drawn is repainted (erased) even when the move's
    // character-span row mapping does not reach it — the stale-bar
    // guarantee the span band cannot always give (a caret-only move on a
    // line boundary can map the span to the adjacent line, and a caret
    // move while `caret_on` is false must still clear the old bar once the
    // blink phase returns). A redundant repaint of an already-clean row is
    // harmless.
    if let Some(old_caret) = moved_from {
        edit_invalidate_span(state, hwnd, old_caret, old_caret.saturating_add(1));
    }
    true
}

/// The caret target for one navigation keypress: vertical moves step to the
/// adjacent line (or `page_lines` for PgUp/PgDn) at the goal column clamped to
/// the target line's length; Home/End are line-aware in a multiline EDIT and
/// document-wide with Ctrl held (and always document-wide on a single line).
#[must_use]
fn caret_navigation_target(
    text: &str,
    old_caret: usize,
    vk: u64,
    ctrl: bool,
    multiline: bool,
    goal_column: Option<usize>,
    page_lines: usize,
) -> usize {
    let len = text.chars().count();
    match vk {
        VK_LEFT => old_caret.saturating_sub(1),
        VK_RIGHT => old_caret.saturating_add(1).min(len),
        VK_HOME if ctrl => 0,
        VK_END if ctrl => len,
        VK_HOME | VK_END if !multiline => {
            if vk == VK_HOME {
                0
            } else {
                len
            }
        }
        VK_HOME => line_index_of(text, line_from_char(text, old_caret)).unwrap_or(0),
        VK_END => {
            let line = line_from_char(text, old_caret);
            let start = line_index_of(text, line).unwrap_or(0);
            start.saturating_add(line_char_len(text, line).unwrap_or(0))
        }
        VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT => {
            let line = line_from_char(text, old_caret);
            let line_start = line_index_of(text, line).unwrap_or(0);
            let column = old_caret.saturating_sub(line_start);
            let goal = goal_column.unwrap_or(column);
            // The last line is the one before the final `\n` (or the only
            // line), so the split count is the valid line range end.
            let last_line = text.split('\n').count().saturating_sub(1);
            let target_line = match vk {
                VK_UP => line.saturating_sub(1),
                VK_DOWN => line.saturating_add(1).min(last_line),
                VK_PRIOR => line.saturating_sub(page_lines),
                _ => line.saturating_add(page_lines).min(last_line),
            };
            let target_len = line_char_len(text, target_line).unwrap_or(0);
            line_index_of(text, target_line)
                .unwrap_or(old_caret)
                .saturating_add(goal.min(target_len))
        }
        _ => old_caret,
    }
}

/// EDIT: VK_DELETE — delete the selection, or the character at the caret.
/// Returns whether the text changed.
pub(super) fn edit_delete_at_caret(state: &mut WinApiState, hwnd: u64) -> bool {
    // Phase 1: mutate the text (the window/control-state borrows end here).
    let (changed, edit_start, crossed_lines) = {
        let ws = state.window_state();
        let Some(window) = ws
            .windows
            .iter_mut()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        else {
            return false;
        };
        let style = window.style;
        let ControlState::Edit {
            caret,
            sel_start,
            sel_end,
            goal_column,
            limit,
            modified,
            handle_buffer,
            undo_snapshot,
            ..
        } = edit_state_mut(&mut ws.control_states, hwnd, style)
        else {
            return false;
        };
        let text = &mut window.control_text;
        let len = text.chars().count();
        let (start, end) = normalized_selection(*sel_start, *sel_end, len);
        if start != end {
            let crossed = replace_crosses_lines(text, start, end, "");
            let changed = replace_range(
                text,
                EditMutation {
                    caret,
                    sel_start,
                    sel_end,
                    goal_column,
                    modified,
                    handle_buffer,
                    undo_snapshot,
                },
                start,
                end,
                "",
                *limit,
            );
            (changed, start, crossed)
        } else {
            let caret_pos = *caret;
            if caret_pos >= len {
                (false, 0, false)
            } else {
                let crossed =
                    replace_crosses_lines(text, caret_pos, caret_pos.saturating_add(1), "");
                let changed = replace_range(
                    text,
                    EditMutation {
                        caret,
                        sel_start,
                        sel_end,
                        goal_column,
                        modified,
                        handle_buffer,
                        undo_snapshot,
                    },
                    caret_pos,
                    caret_pos.saturating_add(1),
                    "",
                    *limit,
                );
                (changed, caret_pos, crossed)
            }
        }
    };
    if changed {
        edit_invalidate_mutation(state, hwnd, edit_start, crossed_lines);
    }
    changed
}

/// EDIT: EM_SETSEL — set the selection. A negative argument means "end of
/// text", so `(0, -1)` selects everything; the caret lands at the end edge.
pub(crate) fn edit_set_selection(state: &mut WinApiState, hwnd: u64, start: i32, end: i32) {
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
        let ControlState::Edit {
            caret,
            sel_start,
            sel_end,
            ..
        } = edit_state_mut(&mut ws.control_states, hwnd, style)
        else {
            return;
        };
        let len = window.control_text.chars().count();
        let start_us = if start < 0 {
            len
        } else {
            usize::try_from(start).unwrap_or(len).min(len)
        };
        let end_us = if end < 0 {
            len
        } else {
            usize::try_from(end).unwrap_or(len).min(len)
        };
        // The highlight moves from the old selection/caret to the new one:
        // both spans' rows must repaint.
        let lo = (*sel_start)
            .min(*sel_end)
            .min(*caret)
            .min(start_us)
            .min(end_us);
        let hi = (*sel_start)
            .max(*sel_end)
            .max(*caret)
            .max(start_us)
            .max(end_us);
        *sel_start = start_us.min(end_us);
        *sel_end = start_us.max(end_us);
        *caret = *sel_end;
        (lo, hi)
    };
    edit_invalidate_span(state, hwnd, span.0, span.1.saturating_add(1));
}

/// EDIT: EM_GETSEL — the current (start, end) character range, normalized.
#[must_use]
pub(super) fn edit_get_selection(state: &WinApiState, hwnd: u64) -> (usize, usize) {
    let sel = match control_state(state, hwnd) {
        Some(ControlState::Edit {
            sel_start, sel_end, ..
        }) => ((*sel_start).min(*sel_end), (*sel_start).max(*sel_end)),
        _ => (0, 0),
    };
    tracing::debug!(
        target: "wie_winapi",
        hwnd = format_args!("{hwnd:#x}"),
        start = sel.0,
        end = sel.1,
        "EM_GETSEL"
    );
    sel
}

/// EDIT: EM_LIMITTEXT — the typing cap in characters (0 = unlimited).
pub(super) fn edit_set_limit(state: &mut WinApiState, hwnd: u64, limit: u64) {
    if let ControlState::Edit { limit: cap, .. } = edit_state_for_window(state, hwnd) {
        *cap = usize::try_from(limit).unwrap_or(usize::MAX);
    }
}

/// EDIT: EM_GETLIMITTEXT — the stored typing cap (0 when never set).
#[must_use]
pub(super) fn edit_get_limit(state: &WinApiState, hwnd: u64) -> u64 {
    match control_state(state, hwnd) {
        Some(ControlState::Edit { limit, .. }) => u64::try_from(*limit).unwrap_or(u64::MAX),
        _ => 0,
    }
}

/// EDIT: EM_GETLINECOUNT — the `\n`-separated line count (1 for empty text).
#[must_use]
pub(super) fn edit_line_count(state: &WinApiState, hwnd: u64) -> u64 {
    let count = edit_text(state, hwnd).map_or(0, |text| text.split('\n').count());
    u64::try_from(count).unwrap_or(0)
}

/// EDIT: EM_LINEFROMCHAR — the line holding `char_index`; -1 asks for the
/// caret's line.
#[must_use]
pub(super) fn edit_line_from_char(state: &WinApiState, hwnd: u64, char_index: i32) -> u64 {
    let text = edit_text(state, hwnd).unwrap_or_default();
    let line = if char_index < 0 {
        match control_state(state, hwnd) {
            Some(ControlState::Edit { caret, .. }) => line_from_char(text, *caret),
            _ => 0,
        }
    } else {
        line_from_char(text, usize::try_from(char_index).unwrap_or(usize::MAX))
    };
    tracing::debug!(
        target: "wie_winapi",
        hwnd = format_args!("{hwnd:#x}"),
        char_index,
        line,
        "EM_LINEFROMCHAR"
    );
    u64::try_from(line).unwrap_or(0)
}

/// EDIT: EM_LINEINDEX — the char index of `line`'s first character (-1 when
/// the line is out of range).
#[must_use]
pub(super) fn edit_line_index(state: &WinApiState, hwnd: u64, line: i32) -> u64 {
    let result = if line < 0 {
        u64::MAX // -1
    } else {
        let text = edit_text(state, hwnd).unwrap_or_default();
        match line_index_of(text, usize::try_from(line).unwrap_or(usize::MAX)) {
            Some(index) => u64::try_from(index).unwrap_or(0),
            None => u64::MAX,
        }
    };
    tracing::debug!(
        target: "wie_winapi",
        hwnd = format_args!("{hwnd:#x}"),
        line,
        result = format_args!("{result:#x}"),
        "EM_LINEINDEX"
    );
    result
}

/// EDIT: EM_LINELENGTH — characters in the line holding `wparam` (-1 = the
/// caret's line, minus any selected characters within it).
#[must_use]
pub(super) fn edit_line_length(state: &WinApiState, hwnd: u64, wparam: i32) -> u64 {
    let text = edit_text(state, hwnd).unwrap_or_default();
    let (line, selection) = if wparam < 0 {
        let (caret, sel_start, sel_end) = match control_state(state, hwnd) {
            Some(ControlState::Edit {
                caret,
                sel_start,
                sel_end,
                ..
            }) => (
                *caret,
                (*sel_start).min(*sel_end),
                (*sel_start).max(*sel_end),
            ),
            _ => (0, 0, 0),
        };
        (line_from_char(text, caret), Some((sel_start, sel_end)))
    } else {
        (
            line_from_char(text, usize::try_from(wparam).unwrap_or(usize::MAX)),
            None,
        )
    };
    let Some(line_start) = line_index_of(text, line) else {
        return 0;
    };
    let line_len = line_char_len(text, line).unwrap_or(0);
    let mut length = line_len;
    if let Some((sel_start, sel_end)) = selection {
        // Subtract the selected characters that lie inside the caret's line.
        let line_end = line_start.saturating_add(line_len);
        let overlap_lo = sel_start.max(line_start);
        let overlap_hi = sel_end.min(line_end);
        length = length.saturating_sub(overlap_hi.saturating_sub(overlap_lo));
    }
    u64::try_from(length).unwrap_or(0)
}

/// EDIT: EM_GETLINE — copy `line` (without its EOL) into `buffer`, whose
/// first WORD carries the capacity including the terminator. Returns the char
/// count copied (0 for an out-of-range line or NULL buffer).
pub(super) fn edit_get_line(
    engine: &mut dyn wie_cpu::CpuEngine,
    unicode: bool,
    state: &mut WinApiState,
    hwnd: u64,
    line: i32,
    buffer: u64,
) -> Result<u64> {
    if buffer == 0 || line < 0 {
        return Ok(0);
    }
    let Some(line_text) = edit_text(state, hwnd).and_then(|text| {
        text.split('\n')
            .nth(usize::try_from(line).unwrap_or(usize::MAX))
    }) else {
        return Ok(0);
    };
    let capacity = usize::from(read_guest_u16(engine, buffer)?);
    let copied = if unicode {
        write_guest_utf16_c_string(engine, buffer, capacity, line_text)?
    } else {
        write_guest_ansi_c_string(engine, buffer, capacity, line_text)?
    };
    Ok(u64::try_from(copied).unwrap_or(0))
}
/// Whether the Shift key is held, per the guest keyboard state.
#[must_use]
fn shift_is_down(state: &WinApiState) -> bool {
    state.try_window_state().is_some_and(|ws| {
        ws.keyboard_state
            .get(usize::try_from(VK_SHIFT).unwrap_or(0))
            & 0x80
            != 0
    })
}

/// Whether the Ctrl key is held, per the guest keyboard state.
#[must_use]
pub(super) fn ctrl_is_down(state: &WinApiState) -> bool {
    state.try_window_state().is_some_and(|ws| {
        ws.keyboard_state
            .get(usize::try_from(VK_CONTROL).unwrap_or(0))
            & 0x80
            != 0
    })
}
