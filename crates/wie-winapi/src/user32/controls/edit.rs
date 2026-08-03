//! EDIT-class state machine (WM_CHAR, caret/selection, EM_* messages) and the
//! EDIT paint path (split from `controls.rs`).

use anyhow::Result;

use super::listbox::render_control_text;
use super::paint::fill_rect_clipped;
use super::{
    COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, ControlClassKind, ControlState, ES_MULTILINE, SEL_EMPTY,
    SEL_MULTICHAR, SEL_MULTILINE, SEL_TEXT, control_state, deliver_command,
};
use crate::gdi32::ResolvedWindow;
use crate::gdi32::{FontEngine, FontKey, ResolvedFont};
use crate::guest_memory::read_u16 as read_guest_u16;
use crate::state::WindowFlags;
use crate::user32::{
    EN_CHANGE, VK_END, VK_HOME, VK_LEFT, VK_RIGHT, VK_SHIFT, WinApiState, find_window,
    make_command_wparam, read_guest_ansi_lossy, read_guest_utf16_lossy, write_guest_ansi_c_string,
    write_guest_i32, write_guest_utf16_c_string,
};

/// Cap for guest buffer reads (EM_SETHANDLE / EM_REPLACESEL adoption).
const MAX_GUEST_TEXT: usize = 1 << 20;

/// `WS_HSCROLL` — a multiline EDIT with a horizontal scrollbar does NOT word
/// wrap (notepad toggles wrap by dropping the horizontal scroll style).
const WS_HSCROLL: u32 = 0x0010_0000;

/// The ES_LEFT/CENTER/RIGHT alignment bits (the low 2 style bits).
const ES_ALIGN_MASK: u32 = 0x0003;

/// The EDIT control's state, seeded on demand. Only reachable from the
/// `(Edit, _)` dispatch arms, so the seed kind is always `Edit`. Takes the
/// `control_states` field (not the whole `WindowState`) so callers can hold a
/// `window` borrow from `ws.windows` at the same time (disjoint fields). The
/// seed captures the window's creation style into `style_bits`.
fn edit_state_mut(
    control_states: &mut ahash::HashMap<crate::handles::Hwnd, ControlState>,
    hwnd: u64,
    style: u32,
) -> &mut ControlState {
    control_states
        .entry(crate::handles::Hwnd::from(hwnd))
        .or_insert_with(|| ControlClassKind::new_edit_state(style))
}

/// The EDIT state for `hwnd`, seeded with the window's creation style when
/// first touched. Centralizes the style lookup so the EM_* setters that do not
/// otherwise hold the window record read it exactly once.
fn edit_state_for_window(state: &mut WinApiState, hwnd: u64) -> &mut ControlState {
    let ws = state.window_state();
    let style = ws
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .map_or(0, |w| w.style);
    edit_state_mut(&mut ws.control_states, hwnd, style)
}

/// EDIT: process one `WM_CHAR` — insert at the caret (replacing an active
/// selection), Backspace deletes before the caret. Enter inserts `\n` in a
/// multiline EDIT (`ES_MULTILINE`); the `EM_LIMITTEXT` cap is honored. Returns
/// whether the text changed (callers deliver EN_CHANGE only then).
pub(super) fn edit_char(state: &mut WinApiState, hwnd: u64, char_code: u64) -> bool {
    let ch = u32::try_from(char_code & 0xFFFF).unwrap_or(0);
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
        limit,
        style_bits,
        modified,
        handle_buffer,
        ..
    } = edit_state_mut(&mut ws.control_states, hwnd, style)
    else {
        return false;
    };
    let text = &mut window.control_text;
    let len = text.chars().count();
    let (start, end) = normalized_selection(*sel_start, *sel_end, len);
    match ch {
        0x08 => {
            // VK_BACK: delete the selection, or the character before the caret.
            if start != end {
                return replace_range(
                    text,
                    EditMutation {
                        caret,
                        sel_start,
                        sel_end,
                        modified,
                        handle_buffer,
                    },
                    start,
                    end,
                    "",
                    *limit,
                );
            }
            let caret_pos = *caret;
            if caret_pos == 0 {
                return false;
            }
            replace_range(
                text,
                EditMutation {
                    caret,
                    sel_start,
                    sel_end,
                    modified,
                    handle_buffer,
                },
                caret_pos.saturating_sub(1),
                caret_pos,
                "",
                *limit,
            )
        }
        // Enter inserts a line break only in a multiline EDIT; Escape and
        // 0x7F (DEL) are never inserted as characters (DEL is handled by the
        // WM_KEYDOWN VK_DELETE path).
        0x0D => {
            if *style_bits & ES_MULTILINE != 0 {
                replace_range(
                    text,
                    EditMutation {
                        caret,
                        sel_start,
                        sel_end,
                        modified,
                        handle_buffer,
                    },
                    start,
                    end,
                    "\n",
                    *limit,
                )
            } else {
                false
            }
        }
        0x1B | 0x7F => false,
        _ if ch >= 0x20 => {
            let Some(c) = char::from_u32(ch) else {
                return false;
            };
            let (start, end) = if start == end {
                (*caret, *caret)
            } else {
                (start, end)
            };
            replace_range(
                text,
                EditMutation {
                    caret,
                    sel_start,
                    sel_end,
                    modified,
                    handle_buffer,
                },
                start,
                end,
                &c.to_string(),
                *limit,
            )
        }
        _ => false,
    }
}

/// The mutable caret/selection/modify slice of an EDIT state, plus the cached
/// EM_GETHANDLE buffer. Bundled so the shared mutation helper stays under the
/// `too_many_arguments` lint bar.
struct EditMutation<'a> {
    caret: &'a mut usize,
    sel_start: &'a mut usize,
    sel_end: &'a mut usize,
    modified: &'a mut bool,
    handle_buffer: &'a mut u64,
}

/// Replace the character range `[start, end)` (clamped to the text) with
/// `replacement`; the caret lands after the inserted text and the selection is
/// cleared. The `EM_LIMITTEXT` cap (0 = unlimited) truncates the replacement
/// so the post-edit length never exceeds it — deletions are never capped.
/// Returns whether the text changed (a fully-truncated insertion is a no-op).
fn replace_range(
    text: &mut String,
    edits: EditMutation<'_>,
    start: usize,
    end: usize,
    replacement: &str,
    limit: usize,
) -> bool {
    let len = text.chars().count();
    let start = start.min(len);
    let end = end.min(len).max(start);
    let replacement = if limit > 0 && !replacement.is_empty() {
        let room = limit.saturating_sub(len.saturating_sub(end.saturating_sub(start)));
        let keep = room.min(replacement.chars().count());
        if keep == 0 {
            return false;
        }
        replacement.chars().take(keep).collect::<String>()
    } else {
        replacement.to_owned()
    };
    let start_byte = byte_index_of_char(text, start);
    let end_byte = byte_index_of_char(text, end);
    if start_byte == end_byte && replacement.is_empty() {
        // No-op (e.g. an empty EM_REPLACESEL with no selection): the text is
        // unchanged, so neither the modify flag nor the cached GETHANDLE
        // buffer move.
        return false;
    }
    text.replace_range(start_byte..end_byte, &replacement);
    *edits.caret = start.saturating_add(replacement.chars().count());
    *edits.sel_start = *edits.caret;
    *edits.sel_end = *edits.caret;
    // A real mutation invalidates the cached EM_GETHANDLE buffer — the old
    // handle is the guest's to LocalFree (Windows frees it on the control's
    // next buffer reallocation) — and dirties the modify flag.
    *edits.handle_buffer = 0;
    *edits.modified = true;
    true
}

/// Byte offset of the `char_index`-th character (the end of the string when
/// the index is at or past the last character).
#[must_use]
fn byte_index_of_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// The selection as an ordered, clamped `(start, end)` character range.
#[must_use]
fn normalized_selection(sel_start: usize, sel_end: usize, len: usize) -> (usize, usize) {
    let start = sel_start.min(len);
    let end = sel_end.min(len);
    (start.min(end), start.max(end))
}

// ── Line model — the host `String` is line-split on `\n` (the internal
// separator), and every index below is a CHARACTER index (byte offsets are
// only derived inside `byte_index_of_char` for the actual splice).

/// The character index of the start of `line` (None when out of range).
fn line_index_of(text: &str, line: usize) -> Option<usize> {
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
fn line_from_char(text: &str, char_index: usize) -> usize {
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
fn line_char_len(text: &str, line: usize) -> Option<usize> {
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

/// The control text of `hwnd`, when it is a known window.
fn edit_text(state: &WinApiState, hwnd: u64) -> Option<&str> {
    state.try_window_state().and_then(|ws| {
        ws.windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .map(|w| w.control_text.as_str())
    })
}

/// EDIT: arrow/Home/End caret movement. Shift extends the selection (the
/// anchor stays at the edge the caret moved away from); without Shift the
/// selection collapses. Returns whether the caret/selection moved.
pub(super) fn edit_move_caret(state: &mut WinApiState, hwnd: u64, vk: u64) -> bool {
    let extend = shift_is_down(state);
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
        ..
    } = edit_state_mut(&mut ws.control_states, hwnd, style)
    else {
        return false;
    };
    let len = window.control_text.chars().count();
    let old_caret = (*caret).min(len);
    let new_caret = match vk {
        VK_LEFT => old_caret.saturating_sub(1),
        VK_RIGHT => old_caret.saturating_add(1).min(len),
        VK_HOME => 0,
        VK_END => len,
        _ => old_caret,
    };
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
    true
}

/// EDIT: VK_DELETE — delete the selection, or the character at the caret.
/// Returns whether the text changed.
pub(super) fn edit_delete_at_caret(state: &mut WinApiState, hwnd: u64) -> bool {
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
        limit,
        modified,
        handle_buffer,
        ..
    } = edit_state_mut(&mut ws.control_states, hwnd, style)
    else {
        return false;
    };
    let text = &mut window.control_text;
    let len = text.chars().count();
    let (start, end) = normalized_selection(*sel_start, *sel_end, len);
    if start != end {
        return replace_range(
            text,
            EditMutation {
                caret,
                sel_start,
                sel_end,
                modified,
                handle_buffer,
            },
            start,
            end,
            "",
            *limit,
        );
    }
    let caret_pos = *caret;
    if caret_pos >= len {
        return false;
    }
    replace_range(
        text,
        EditMutation {
            caret,
            sel_start,
            sel_end,
            modified,
            handle_buffer,
        },
        caret_pos,
        caret_pos.saturating_add(1),
        "",
        *limit,
    )
}

/// EDIT: EM_SETSEL — set the selection. A negative argument means "end of
/// text", so `(0, -1)` selects everything; the caret lands at the end edge.
pub(super) fn edit_set_selection(state: &mut WinApiState, hwnd: u64, start: i32, end: i32) {
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
    *sel_start = start_us.min(end_us);
    *sel_end = start_us.max(end_us);
    *caret = *sel_end;
}

/// EDIT: EM_GETSEL — the current (start, end) character range, normalized.
#[must_use]
pub(super) fn edit_get_selection(state: &WinApiState, hwnd: u64) -> (usize, usize) {
    match control_state(state, hwnd) {
        Some(ControlState::Edit {
            sel_start, sel_end, ..
        }) => ((*sel_start).min(*sel_end), (*sel_start).max(*sel_end)),
        _ => (0, 0),
    }
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
    u64::try_from(line).unwrap_or(0)
}

/// EDIT: EM_LINEINDEX — the char index of `line`'s first character (-1 when
/// the line is out of range).
#[must_use]
pub(super) fn edit_line_index(state: &WinApiState, hwnd: u64, line: i32) -> u64 {
    if line < 0 {
        return u64::MAX; // -1
    }
    let text = edit_text(state, hwnd).unwrap_or_default();
    match line_index_of(text, usize::try_from(line).unwrap_or(usize::MAX)) {
        Some(index) => u64::try_from(index).unwrap_or(0),
        None => u64::MAX,
    }
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

/// EDIT: EM_REPLACESEL — replace the selection with the guest string at
/// `text_ptr` (honoring the limit). Returns whether the text changed.
pub(super) fn edit_replace_selection(
    engine: &mut dyn wie_cpu::CpuEngine,
    unicode: bool,
    state: &mut WinApiState,
    hwnd: u64,
    text_ptr: u64,
) -> Result<bool> {
    if text_ptr == 0 {
        return Ok(false);
    }
    let replacement = if unicode {
        read_guest_utf16_lossy(engine, text_ptr, MAX_GUEST_TEXT)?
    } else {
        read_guest_ansi_lossy(engine, text_ptr, MAX_GUEST_TEXT)?
    };
    let ws = state.window_state();
    let Some(window) = ws
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
    else {
        return Ok(false);
    };
    let style = window.style;
    let ControlState::Edit {
        caret,
        sel_start,
        sel_end,
        limit,
        modified,
        handle_buffer,
        ..
    } = edit_state_mut(&mut ws.control_states, hwnd, style)
    else {
        return Ok(false);
    };
    let text = &mut window.control_text;
    let len = text.chars().count();
    let (start, end) = normalized_selection(*sel_start, *sel_end, len);
    Ok(replace_range(
        text,
        EditMutation {
            caret,
            sel_start,
            sel_end,
            modified,
            handle_buffer,
        },
        start,
        end,
        &replacement,
        *limit,
    ))
}

/// EDIT: EM_SCROLLCARET — bring the caret's line into view by making it the
/// first visible line. Viewport-aware clamping lands with Task 2.4's scroll
/// state; today the caret line becomes the top line.
pub(super) fn edit_scroll_caret(state: &mut WinApiState, hwnd: u64) -> bool {
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
        first_visible_line,
        ..
    } = edit_state_mut(&mut ws.control_states, hwnd, style)
    else {
        return false;
    };
    *first_visible_line = line_from_char(&window.control_text, *caret);
    true
}

/// EDIT: EM_GETMODIFY — the modified flag (0 when the state was never seeded).
#[must_use]
pub(super) fn edit_get_modify(state: &WinApiState, hwnd: u64) -> u64 {
    match control_state(state, hwnd) {
        Some(ControlState::Edit { modified, .. }) => u64::from(*modified),
        _ => 0,
    }
}

/// EDIT: EM_SETMODIFY — set/clear the modified flag.
pub(super) fn edit_set_modify(state: &mut WinApiState, hwnd: u64, value: u64) {
    if let ControlState::Edit { modified, .. } = edit_state_for_window(state, hwnd) {
        *modified = value != 0;
    }
}

/// EDIT: invalidate the cached EM_GETHANDLE buffer after a text change that
/// bypassed the `replace_range` paths (WM_SETTEXT). No-op when no Edit state
/// exists yet (a fresh control has no buffer to stale).
pub(super) fn edit_invalidate_text_buffer(state: &mut WinApiState, hwnd: u64) {
    let ws = state.window_state();
    if let Some(ControlState::Edit { handle_buffer, .. }) =
        ws.control_states.get_mut(&crate::handles::Hwnd::from(hwnd))
    {
        *handle_buffer = 0;
    }
}

/// EDIT: EM_GETHANDLE — the cached `LocalAlloc`'d guest copy of the text.
///
/// Windows keeps one buffer handle per control, reused until the text
/// changes; a repeat call without an intervening mutation returns the SAME
/// handle (allocating fresh per call would leak the guest buffers). Any text
/// mutation clears the cache, so the next GETHANDLE allocates a fresh copy and
/// the previous handle is the guest's to `LocalFree`.
pub(super) fn edit_get_handle(
    engine: &mut dyn wie_cpu::CpuEngine,
    unicode: bool,
    state: &mut WinApiState,
    hwnd: u64,
) -> Result<u64> {
    let cached = match edit_state_for_window(state, hwnd) {
        ControlState::Edit { handle_buffer, .. } => *handle_buffer,
        _ => 0,
    };
    if cached != 0 {
        return Ok(cached);
    }
    let text = edit_text(state, hwnd).unwrap_or_default().to_owned();
    let size = if unicode {
        u64::try_from(text.encode_utf16().count())
            .unwrap_or(0)
            .saturating_add(1)
            .saturating_mul(2)
    } else {
        u64::try_from(text.len()).unwrap_or(0).saturating_add(1)
    };
    let va = state.heap_state.heap.alloc_coherent(engine, size);
    if va == 0 {
        return Ok(0);
    }
    if unicode {
        write_guest_utf16_c_string(
            engine,
            va,
            usize::try_from(size.saturating_div(2)).unwrap_or(0),
            &text,
        )?;
    } else {
        write_guest_ansi_c_string(engine, va, usize::try_from(size).unwrap_or(0), &text)?;
    }
    if let ControlState::Edit { handle_buffer, .. } = edit_state_for_window(state, hwnd) {
        *handle_buffer = va;
    }
    Ok(va)
}

/// EDIT: EM_SETHANDLE — adopt the guest buffer (a `LocalAlloc`'d text from
/// `EM_GETHANDLE`) as the control's text; the caller relinquishes ownership.
/// Caret and selection reset to 0.
pub(super) fn edit_set_handle(
    engine: &mut dyn wie_cpu::CpuEngine,
    unicode: bool,
    state: &mut WinApiState,
    hwnd: u64,
    buffer: u64,
) -> Result<bool> {
    if buffer == 0 {
        return Ok(false);
    }
    let text = if unicode {
        read_guest_utf16_lossy(engine, buffer, MAX_GUEST_TEXT)?
    } else {
        read_guest_ansi_lossy(engine, buffer, MAX_GUEST_TEXT)?
    };
    let ws = state.window_state();
    let Some(window) = ws
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
    else {
        return Ok(false);
    };
    let style = window.style;
    let ControlState::Edit {
        caret,
        sel_start,
        sel_end,
        handle_buffer,
        ..
    } = edit_state_mut(&mut ws.control_states, hwnd, style)
    else {
        return Ok(false);
    };
    window.control_text = text;
    *caret = 0;
    *sel_start = 0;
    *sel_end = 0;
    // The adopted buffer becomes the cached EM_GETHANDLE result; any older
    // cached handle is the guest's to LocalFree (Windows frees it on the
    // control's next buffer reallocation).
    *handle_buffer = buffer;
    Ok(true)
}

/// EDIT: EM_SETTABSTOPS — store the tab stops (wParam = count of u16
/// positions in dialog units at lParam; 0 restores the default stops).
pub(super) fn edit_set_tab_stops(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    count: u64,
    ptr: u64,
) -> Result<bool> {
    let ControlState::Edit { tab_stops, .. } = edit_state_for_window(state, hwnd) else {
        return Ok(false);
    };
    tab_stops.clear();
    if count == 0 {
        return Ok(true);
    }
    if ptr == 0 {
        return Ok(false);
    }
    // Sanity-cap the guest-supplied count; each stop is a u16 at +2i.
    for i in 0..count.min(256) {
        let stop = read_guest_u16(engine, ptr.wrapping_add(i.saturating_mul(2)))?;
        tab_stops.push(stop);
    }
    Ok(true)
}

/// EDIT: EM_POSFROMCHAR — TRUE with `POINT(x = 0, y = line × the resolved
/// font's line height)` at `point` for a valid char index, FALSE otherwise.
/// The y uses the SAME 16 px default control font the paint path resolves, so
/// EM_POSFROMCHAR and the rendered rows agree; per-glyph x lands with Task
/// 2.5's hit-test.
pub(super) fn edit_pos_from_char(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    char_index: i32,
    point: u64,
) -> Result<bool> {
    if char_index < 0 {
        return Ok(false);
    }
    let text = edit_text(state, hwnd).unwrap_or_default().to_owned();
    if usize::try_from(char_index).unwrap_or(usize::MAX) >= text.chars().count() {
        return Ok(false);
    }
    if point != 0 {
        write_guest_i32(engine, point, 0)?;
        // Resolve the line height directly on the field — no take/put window.
        // `font_engine` is a plain field under the single shared WinApiState
        // mutex (every API handler, WM_PAINT included, runs while holding
        // it), so the resolve cannot race a concurrent WM_PAINT on another
        // host thread.
        let line_h = state
            .gdi_state()
            .font_engine
            .resolve(&FontKey::default(), 16)
            .map_or(0, |f| f.line_height());
        let line = line_from_char(&text, usize::try_from(char_index).unwrap_or(0));
        let y = i32::try_from(line).unwrap_or(0).saturating_mul(line_h);
        write_guest_i32(engine, point.wrapping_add(4), y)?;
    }
    Ok(true)
}

/// EDIT: EM_SELECTIONTYPE — the `SEL_*` bitmask of the current selection.
#[must_use]
pub(super) fn edit_selection_type(state: &WinApiState, hwnd: u64) -> u64 {
    let (sel_start, sel_end) = edit_get_selection(state, hwnd);
    if sel_start == sel_end {
        return SEL_EMPTY;
    }
    let text = edit_text(state, hwnd).unwrap_or_default();
    let mut bits = SEL_TEXT;
    if sel_end.saturating_sub(sel_start) > 1 {
        bits |= SEL_MULTICHAR;
    }
    if line_from_char(text, sel_start) != line_from_char(text, sel_end.saturating_sub(1)) {
        bits |= SEL_MULTILINE;
    }
    bits
}

/// EDIT: EM_GETFIRSTVISIBLELINE — the first visible line (0 when never seeded).
#[must_use]
pub(super) fn edit_first_visible_line(state: &WinApiState, hwnd: u64) -> u64 {
    match control_state(state, hwnd) {
        Some(ControlState::Edit {
            first_visible_line, ..
        }) => u64::try_from(*first_visible_line).unwrap_or(0),
        _ => 0,
    }
}

/// Send `EN_CHANGE` as WM_COMMAND(MAKEWPARAM(id, EN_CHANGE)) to the parent —
/// the EDIT text changed (same bubble as BN_CLICKED).
pub(super) fn edit_notify_change(state: &mut WinApiState, hwnd: u64) -> Result<Option<u64>> {
    let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
    let command_wparam = make_command_wparam(id, EN_CHANGE);
    deliver_command(state, hwnd, command_wparam)
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

/// EDIT paint: text rows (wrap-aware), selection highlight, and the caret bar.
///
/// Glyphs are proportional, so the caret and selection x positions are the
/// SUMMED advances of the preceding characters (matching the rendered text
/// exactly). Each visual row is drawn in three passes — the whole row in
/// COLOR_WINDOWTEXT, then the selected run re-rendered in COLOR_HIGHLIGHTTEXT
/// over its COLOR_HIGHLIGHT cells — and the caret bar (1 px, full line
/// height) is only drawn while the control has focus.
pub(super) fn paint_edit(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    info: &ResolvedWindow,
    text: &str,
    tx: i32,
    width: i32,
    height: i32,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    let len = text.chars().count();
    let focused = find_window(state, info.dc_window.as_u64())
        .is_some_and(|w| w.flags.contains(WindowFlags::FOCUSED));
    let (sel_start, sel_end, caret, style_bits, first_visible_line) =
        match control_state(state, info.dc_window.as_u64()) {
            Some(ControlState::Edit {
                caret,
                sel_start,
                sel_end,
                style_bits,
                first_visible_line,
                ..
            }) => (
                (*sel_start).min(*sel_end),
                (*sel_start).max(*sel_end),
                *caret,
                *style_bits,
                *first_visible_line,
            ),
            _ => (0, 0, 0, 0, 0),
        };
    let (sel_start, sel_end, caret) = (sel_start.min(len), sel_end.min(len), caret.min(len));
    let line_h = resolved.line_height();
    let multiline = style_bits & ES_MULTILINE != 0;
    // Wrap is on when the multiline EDIT has no horizontal scrollbar (notepad
    // toggles word wrap by dropping the horizontal styles); long lines are
    // horizontally clipped otherwise — the scrollbar itself is Task 2.4.
    let wrap = multiline && style_bits & WS_HSCROLL == 0;
    // Single-line edits keep their vertical centering; multiline rows start
    // at the top of the client rect.
    let base_y = if multiline {
        info.offset_y
    } else {
        info.offset_y
            .saturating_add(height.saturating_sub(line_h).saturating_div(2))
            .max(info.offset_y)
    };
    let right = info.offset_x.saturating_add(width);
    let bottom = info.offset_y.saturating_add(height);
    let clip = Some((info.offset_x, info.offset_y, right, bottom));
    let has_selection = focused && sel_start != sel_end;
    // The wrap column is the client width minus the 2 px side margins (the
    // same 2 px inset the single-line text already uses on the left).
    let rows = layout_visible_lines(
        text,
        width.saturating_sub(4),
        line_h,
        if multiline { first_visible_line } else { 0 },
        wrap,
        style_bits & ES_ALIGN_MASK,
        &mut |ch| font_engine.char_advance(resolved, key, ch),
    );

    let mut caret_drawn = false;
    for row in &rows {
        let y = base_y.saturating_add(row.y);
        if y >= bottom {
            break;
        }
        let x = tx.saturating_add(row.x);
        // Pass 1: fill the selected cells with COLOR_HIGHLIGHT (behind text).
        let (sel_lo, sel_hi) = if has_selection {
            selection_overlap(row, sel_start, sel_end)
        } else {
            (0, 0)
        };
        let sel_x = if sel_lo < sel_hi {
            let lo_x = x.saturating_add(font_engine.text_advance(resolved, key, &row.text, sel_lo));
            let hi_x = x.saturating_add(font_engine.text_advance(resolved, key, &row.text, sel_hi));
            fill_rect_clipped(
                state,
                info,
                width,
                height,
                lo_x,
                y,
                hi_x.saturating_sub(lo_x),
                line_h,
                COLOR_HIGHLIGHT,
            );
            Some((lo_x, sel_lo, sel_hi))
        } else {
            None
        };
        // Pass 2: the whole row in the normal text color.
        if !row.text.is_empty() {
            render_control_text(
                state,
                engine,
                info.hwnd,
                info.width,
                info.height,
                x,
                y,
                &row.text,
                0,
                clip,
                font_engine,
                resolved,
                key,
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
                state,
                engine,
                info.hwnd,
                info.width,
                info.height,
                sel_x,
                y,
                &selected,
                COLOR_HIGHLIGHTTEXT,
                clip,
                font_engine,
                resolved,
                key,
            )?;
        }
        // Pass 4: the 1 px caret bar at the caret's glyph cell. The caret
        // belongs to the first row ending at or past it — a wrap-boundary
        // caret lands at the END of the row before the break.
        if focused && !caret_drawn && caret >= row.char_start && caret <= row.char_end {
            let local = caret.saturating_sub(row.char_start);
            let caret_x =
                x.saturating_add(font_engine.text_advance(resolved, key, &row.text, local));
            fill_rect_clipped(
                state,
                info,
                width,
                height,
                caret_x,
                y,
                1,
                line_h,
                0x0000_0000,
            );
            caret_drawn = true;
        }
    }
    Ok(())
}
