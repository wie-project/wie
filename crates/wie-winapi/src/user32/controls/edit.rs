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
use crate::state::{TimerRecord, WindowFlags};
use crate::user32::{
    EN_CHANGE, VK_CONTROL, VK_DOWN, VK_END, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT,
    VK_SHIFT, VK_UP, WinApiState, find_window, find_window_mut, make_command_wparam,
    read_guest_ansi_lossy, read_guest_utf16_lossy, write_guest_ansi_c_string, write_guest_i32,
    write_guest_utf16_c_string,
};

/// Cap for guest buffer reads (EM_SETHANDLE / EM_REPLACESEL adoption).
const MAX_GUEST_TEXT: usize = 1 << 20;

/// The EDIT's internal caret-blink timer id — the id a real Windows EDIT
/// control uses for its caret timer, so a guest SetTimer on the same
/// window/id replaces it exactly like Windows (the timer is a plain record in
/// the thread's timer list).
pub(super) const CARET_TIMER_ID: u64 = 1;

/// Caret blink half-period in ms — `SPI_GETCARETTIMEOUT`'s 530 ms default
/// (the spec's ~530 ms).
pub(super) const CARET_BLINK_MS: u32 = 530;

/// `WS_HSCROLL` — a multiline EDIT with a horizontal scrollbar does NOT word
/// wrap (notepad toggles wrap by dropping the horizontal scroll style).
const WS_HSCROLL: u32 = 0x0010_0000;

/// `WM_VSCROLL` / `WM_HSCROLL` scroll-bar request codes (winuser.h) — the
/// wParam LOW word. THUMBTRACK/POSITION carry the thumb position in the high
/// word.
const SB_LINEUP: u16 = 0;
const SB_LINEDOWN: u16 = 1;
const SB_PAGEUP: u16 = 2;
const SB_PAGEDOWN: u16 = 3;
const SB_THUMBPOSITION: u16 = 4;
const SB_THUMBTRACK: u16 = 5;
const SB_TOP: u16 = 6;
const SB_BOTTOM: u16 = 7;
const SB_ENDSCROLL: u16 = 8;

/// The ES_LEFT/CENTER/RIGHT alignment bits (the low 2 style bits).
const ES_ALIGN_MASK: u32 = 0x0003;

/// The EDIT control's state, seeded on demand. Only reachable from the
/// `(Edit, _)` dispatch arms, so the seed kind is always `Edit`. Takes the
/// `control_states` field (not the whole `WindowState`) so callers can hold a
/// `window` borrow from `ws.windows` at the same time (disjoint fields). The
/// seed captures the window's creation style into `style_bits` — and RE-captures
/// it on every touch, because a window's creation style never changes, so the
/// refresh is a no-op for a correct seed while healing a state that was first
/// seeded with style 0 by a different seeder (`control_state_mut`'s
/// `kind.new_state()` runs before this path saw the window record).
fn edit_state_mut(
    control_states: &mut ahash::HashMap<crate::handles::Hwnd, ControlState>,
    hwnd: u64,
    style: u32,
) -> &mut ControlState {
    let state = control_states
        .entry(crate::handles::Hwnd::from(hwnd))
        .or_insert_with(|| ControlClassKind::new_edit_state(style));
    if let ControlState::Edit { style_bits, .. } = state {
        *style_bits = style;
    }
    state
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
        goal_column,
        limit,
        style_bits,
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
                    goal_column,
                    modified,
                    handle_buffer,
                    undo_snapshot,
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
                        goal_column,
                        modified,
                        handle_buffer,
                        undo_snapshot,
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
                    goal_column,
                    modified,
                    handle_buffer,
                    undo_snapshot,
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
/// EM_GETHANDLE buffer and the undo snapshot. Bundled so the shared mutation
/// helper stays under the `too_many_arguments` lint bar.
struct EditMutation<'a> {
    caret: &'a mut usize,
    sel_start: &'a mut usize,
    sel_end: &'a mut usize,
    goal_column: &'a mut Option<usize>,
    modified: &'a mut bool,
    handle_buffer: &'a mut u64,
    undo_snapshot: &'a mut Option<UndoSnapshot>,
}

/// A single-level undo snapshot: the full text plus the caret and selection
/// captured BEFORE a mutation (Task 2.6). `EM_UNDO`/`WM_UNDO` restore it and
/// clear the buffer — Windows undo is single-level, there is no redo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoSnapshot {
    text: String,
    caret: usize,
    sel_start: usize,
    sel_end: usize,
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
        // unchanged, so neither the modify flag, the cached GETHANDLE buffer,
        // nor the undo snapshot move.
        return false;
    }
    // A real mutation replaces the single-level undo snapshot with the
    // pre-mutation state — EM_UNDO restores text + caret + selection.
    *edits.undo_snapshot = Some(UndoSnapshot {
        text: text.clone(),
        caret: *edits.caret,
        sel_start: *edits.sel_start,
        sel_end: *edits.sel_end,
    });
    text.replace_range(start_byte..end_byte, &replacement);
    *edits.caret = start.saturating_add(replacement.chars().count());
    *edits.sel_start = *edits.caret;
    *edits.sel_end = *edits.caret;
    // Any text mutation also drops the vertical-movement goal column — the
    // caret moved horizontally, so the remembered column is stale.
    *edits.goal_column = None;
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
        true
    })();
    state.gdi_state().font_engine = font_engine;
    moved
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
        return replace_range(
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
            goal_column,
            modified,
            handle_buffer,
            undo_snapshot,
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
    Ok(edit_replace_selection_with(state, hwnd, &replacement))
}

/// Replace the selection with a host-side string — the shared core of
/// `EM_REPLACESEL` and `WM_PASTE` (both splice through the same
/// [`replace_range`] path, capturing an undo snapshot). Returns whether the
/// text changed.
fn edit_replace_selection_with(state: &mut WinApiState, hwnd: u64, replacement: &str) -> bool {
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
    replace_range(
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
        replacement,
        *limit,
    )
}

/// EDIT: EM_CANUNDO — whether the single-level undo buffer holds a snapshot
/// (typing, pasting, cutting, or clearing since the last undo/empty).
#[must_use]
pub(super) fn edit_can_undo(state: &WinApiState, hwnd: u64) -> bool {
    matches!(
        control_state(state, hwnd),
        Some(ControlState::Edit {
            undo_snapshot: Some(_),
            ..
        })
    )
}

/// EDIT: EM_UNDO / WM_UNDO — restore the single-level undo snapshot (the
/// text, caret, and selection captured before the last mutation) and clear
/// the buffer. Returns whether an undo happened (FALSE when the buffer is
/// empty — there is no redo).
pub(super) fn edit_undo(state: &mut WinApiState, hwnd: u64) -> bool {
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
        modified,
        handle_buffer,
        undo_snapshot,
        ..
    } = edit_state_mut(&mut ws.control_states, hwnd, style)
    else {
        return false;
    };
    let Some(snapshot) = undo_snapshot.take() else {
        return false;
    };
    // Restore the pre-mutation text, caret, and selection. The take above
    // cleared the buffer — single-level undo, so a second EM_UNDO is a no-op.
    window.control_text = snapshot.text;
    *caret = snapshot.caret;
    *sel_start = snapshot.sel_start;
    *sel_end = snapshot.sel_end;
    // Undo is a text mutation: it dirties the modify flag, invalidates the
    // cached EM_GETHANDLE buffer, and drops the vertical-movement goal column
    // (the caret moved horizontally).
    *modified = true;
    *handle_buffer = 0;
    *goal_column = None;
    true
}

/// EDIT: EM_EMPTYUNDOBUFFER — discard any pending undo snapshot.
pub(super) fn edit_empty_undo_buffer(state: &mut WinApiState, hwnd: u64) {
    if let ControlState::Edit { undo_snapshot, .. } = edit_state_for_window(state, hwnd) {
        *undo_snapshot = None;
    }
}

/// EDIT: discard any pending undo snapshot WITHOUT seeding a control state —
/// the no-create variant of [`edit_empty_undo_buffer`], for text replacements
/// that bypass the control dispatch (SetWindowText, WM_SETTEXT on any control
/// kind). Real Windows clears an EDIT's undo buffer when the program sets the
/// text, so WM_UNDO never reverts past program-set text. No-op when no Edit
/// state exists yet (a fresh control has no snapshot to drop).
pub(crate) fn edit_clear_undo_buffer(state: &mut WinApiState, hwnd: u64) {
    let ws = state.window_state();
    if let Some(ControlState::Edit { undo_snapshot, .. }) =
        ws.control_states.get_mut(&crate::handles::Hwnd::from(hwnd))
    {
        *undo_snapshot = None;
    }
}

/// The selected text of an EDIT as a host `String` (empty when nothing is
/// selected or the window is gone).
#[must_use]
fn selected_text(state: &WinApiState, hwnd: u64) -> String {
    let (sel_start, sel_end) = edit_get_selection(state, hwnd);
    edit_text(state, hwnd).map_or_else(String::new, |text| {
        let start = byte_index_of_char(text, sel_start);
        let end = byte_index_of_char(text, sel_end);
        text.get(start..end).unwrap_or("").to_owned()
    })
}

/// EDIT: WM_COPY — store the selected text on the host clipboard. No-op with
/// an empty selection (the clipboard is untouched, matching Windows' edit
/// control). Returns whether anything was copied.
pub(super) fn edit_copy(state: &mut WinApiState, hwnd: u64) -> bool {
    let selected = selected_text(state, hwnd);
    if selected.is_empty() {
        return false;
    }
    state.clipboard().set_text(selected);
    true
}

/// EDIT: WM_CUT — copy the selection to the clipboard, then delete it (the
/// deletion captures an undo snapshot like any mutation). No-op with an empty
/// selection.
pub(super) fn edit_cut(state: &mut WinApiState, hwnd: u64) -> bool {
    if !edit_copy(state, hwnd) {
        return false;
    }
    edit_delete_at_caret(state, hwnd)
}

/// EDIT: WM_PASTE — insert the clipboard text at the caret, replacing the
/// selection (honoring the limit). An empty clipboard is a no-op.
pub(super) fn edit_paste(state: &mut WinApiState, hwnd: u64) -> bool {
    let Some(clipboard_text) = state.clipboard().text().map(str::to_owned) else {
        return false;
    };
    if clipboard_text.is_empty() {
        return false;
    }
    edit_replace_selection_with(state, hwnd, &clipboard_text)
}

/// EDIT: WM_CLEAR — delete the selection WITHOUT writing it to the clipboard.
///
/// Like Windows' edit control, the clipboard is EMPTIED (the deleted text is
/// never placed on it — that is what distinguishes CLEAR from CUT), so
/// `IsClipboardFormatAvailable(CF_TEXT)` goes false. No-op with an empty
/// selection (the clipboard is untouched then).
pub(super) fn edit_clear(state: &mut WinApiState, hwnd: u64) -> bool {
    let (sel_start, sel_end) = edit_get_selection(state, hwnd);
    if sel_start == sel_end {
        return false;
    }
    state.clipboard().clear();
    edit_delete_at_caret(state, hwnd)
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
fn visual_rows<F>(
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

/// The wrap-aware vertical scroll context of a multiline EDIT: how many visual
/// rows fit in the client (`visible`), the full row count (`total`), and the
/// visual row holding the caret (`caret_row`). All three derive from the same
/// greedy width walk `paint_edit`'s layout runs, so the scroll math and the
/// painted rows always agree. `None` when the window is gone.
struct EditScrollContext {
    visible: usize,
    total: usize,
    caret_row: usize,
}

/// Resolve an EDIT's [`EditScrollContext`] from its client height, the
/// stored control font's line height (the system default when none is set),
/// and the wrap-aware visual row count.
fn edit_scroll_context(state: &mut WinApiState, hwnd: u64) -> Option<EditScrollContext> {
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
    let multiline = style & ES_MULTILINE != 0;
    // Wrap is on when the multiline EDIT has no horizontal scrollbar — the
    // same condition paint_edit uses, so row counts and painted rows agree.
    let wrap = multiline && style & WS_HSCROLL == 0;
    let wrap_width = width.saturating_sub(4);
    // The font engine is taken out of gdi state so the advance closure can
    // run next to it (the paint path does the same); it is put back
    // unconditionally. Safe under the single shared WinApiState mutex — the
    // take and the put cannot interleave with another handler's.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = FontKey::default();
    // The STORED font (falling back to the system default) drives the line
    // height — the same resolution the paint path uses, so the scroll math
    // and the painted rows agree even after a WM_SETFONT.
    let key_and_resolved = match crate::gdi32::window_font_resolution(state, hwnd, &mut font_engine)
    {
        Some(key_and_resolved) => Some(key_and_resolved),
        None => font_engine
            .resolve(&default_key, 16)
            .map(|resolved| (default_key, resolved)),
    };
    let (line_h, rows) = match &key_and_resolved {
        Some((key, resolved)) => {
            let line_h = resolved.line_height();
            let advance = &mut |ch: char| font_engine.char_advance(resolved, key, ch);
            (line_h, visual_rows(&text, wrap_width, wrap, caret, advance))
        }
        // No system font: the 16 px default the EM_* line APIs assume, with
        // a fixed 8 px/char advance for the wrap walk.
        None => (
            16,
            visual_rows(&text, wrap_width, wrap, caret, &mut |_| 8_i32),
        ),
    };
    state.gdi_state().font_engine = font_engine;
    Some(EditScrollContext {
        visible: visible_line_count(client_height, line_h),
        total: rows.0,
        caret_row: rows.1,
    })
}

/// EDIT: WM_VSCROLL — apply one vertical scroll-bar request. `code` is the
/// SB_* code in the wParam low word; `thumb` is the high-word thumb position
/// used by SB_THUMBTRACK/SB_THUMBPOSITION. A page is the number of visible
/// rows (the same amount PgUp/PgDn move the caret). Returns whether the offset
/// moved.
pub(super) fn edit_scroll_vertical(
    state: &mut WinApiState,
    hwnd: u64,
    code: u16,
    thumb: u16,
) -> bool {
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
    let target = match code {
        SB_LINEUP => old.saturating_sub(1),
        SB_LINEDOWN => old.saturating_add(1),
        SB_PAGEUP => old.saturating_sub(context.visible),
        SB_PAGEDOWN => old.saturating_add(context.visible),
        SB_THUMBPOSITION | SB_THUMBTRACK => usize::from(thumb),
        SB_TOP => 0,
        SB_BOTTOM => context.total.saturating_sub(context.visible),
        SB_ENDSCROLL => old, // end of a scroll-bar interaction: no-op
        _ => old,            // unknown codes: no-op
    };
    *first_visible_line = clamp_scroll_offset(target, context.total, context.visible);
    *first_visible_line != old
}

/// EDIT: WM_MOUSEWHEEL — scroll the multiline EDIT vertically. `wparam`'s
/// high word is the signed wheel delta (a wheel notch = 120 delta units); one
/// notch scrolls 3 lines — the Windows default (`SPI_GETWHEELSCROLLLINES`) —
/// and smaller trackpad deltas scroll proportionally. A positive delta (wheel
/// away from the user) scrolls UP. Returns whether the offset moved.
pub(super) fn edit_mouse_wheel(state: &mut WinApiState, hwnd: u64, wparam: u64) -> bool {
    let hi = u16::try_from((wparam >> 16) & 0xFFFF).unwrap_or(0);
    let delta = i32::from(i16::from_le_bytes(hi.to_le_bytes()));
    // Truncating division drops partial notches: a 60-unit trackpad flick is
    // a no-op while a 120-unit notch scrolls a full 3 lines.
    let lines = i64::from(delta.saturating_div(120).saturating_mul(3));
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
    // Positive delta = wheel away from the user = scroll UP (earlier rows);
    // negative = toward the user = scroll DOWN (later rows).
    let target = if lines > 0 {
        old.saturating_sub(usize::try_from(lines).unwrap_or(0))
    } else {
        old.saturating_add(usize::try_from(lines.saturating_neg()).unwrap_or(0))
    };
    *first_visible_line = clamp_scroll_offset(target, context.total, context.visible);
    *first_visible_line != old
}

// ── Task 2.5: mouse caret placement, drag selection, double-click ──────────

/// The char index whose glyph cell contains the client point (x, y) — the
/// inverse of the advance-summed caret x `paint_edit` draws.
///
/// `wrap_width` is the client width minus the 2 px side margins (the same
/// column `layout_visible_lines` lays out against); the row for y is picked
/// from that same visual-row layout, so a click in wrapped text lands on the
/// glyph the paint shows there. A click left of the text clamps to the row
/// start, past the last glyph to the row end; a single-line EDIT (wrap off,
/// one row at y=0) picks its only row for any y.
#[must_use]
pub(crate) fn edit_char_index_at_point<F>(
    text: &str,
    x: i32,
    y: i32,
    wrap_width: i32,
    line_height: i32,
    first_visible: usize,
    wrap: bool,
    alignment: u32,
    advance: &mut F,
) -> usize
where
    F: FnMut(char) -> i32,
{
    let rows = layout_visible_lines(
        text,
        wrap_width,
        line_height,
        first_visible,
        wrap,
        alignment,
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
    let (text, style, width, first_visible_line) = {
        let ws = state.window_state();
        let w = ws
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))?;
        let first_visible_line = match ws.control_states.get(&crate::handles::Hwnd::from(hwnd)) {
            Some(ControlState::Edit {
                first_visible_line, ..
            }) => *first_visible_line,
            _ => 0,
        };
        (w.control_text.clone(), w.style, w.width, first_visible_line)
    };
    let multiline = style & ES_MULTILINE != 0;
    let wrap = multiline && style & WS_HSCROLL == 0;
    let wrap_width = width.saturating_sub(4);
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
            edit_char_index_at_point(
                &text,
                x,
                y,
                wrap_width,
                line_h,
                first_visible_line,
                wrap,
                alignment,
                advance,
            )
        }
        // No system font: the 16 px default the EM_* line APIs assume, with
        // a fixed 8 px/char advance for the hit test.
        None => edit_char_index_at_point(
            &text,
            x,
            y,
            wrap_width,
            16,
            first_visible_line,
            wrap,
            alignment,
            &mut |_| 8_i32,
        ),
    };
    state.gdi_state().font_engine = font_engine;
    Some(result)
}

/// EDIT: WM_LBUTTONDOWN — place the caret at the click and collapse the
/// selection; the click becomes the drag anchor (the selection edge later
/// mouse moves extend from). The dispatch arm sets the mouse capture.
pub(super) fn edit_mouse_down(state: &mut WinApiState, hwnd: u64, x: i32, y: i32) {
    let Some(index) = edit_char_at_point(state, hwnd, x, y) else {
        return;
    };
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
    *caret = index;
    *sel_start = index;
    *sel_end = index;
    // A click is horizontal movement: the vertical-movement goal column is
    // stale (the same clearing horizontal keys apply).
    *goal_column = None;
    // Task 2.4 handoff: a click in the partial strip below the last full
    // visible row (or any off-viewport hit) must bring the caret row into
    // view. The dispatch arm invalidates unconditionally after the handler
    // runs, so the repaint is covered whether or not the viewport moved.
    edit_scroll_caret(state, hwnd);
}

/// EDIT: WM_MOUSEMOVE while the edit holds the capture — extend the drag
/// selection from the anchor (the click position) to the current position.
///
/// The anchor is the selection edge the caret is not at, since the caret
/// tracks the pointer (the same anchor model `edit_move_caret` uses for
/// Shift-arrows); crossing the anchor flips the selection edge. Hover moves
/// without capture are no-ops. Returns whether the selection changed.
pub(super) fn edit_mouse_move(state: &mut WinApiState, hwnd: u64, x: i32, y: i32) -> bool {
    if state.window_state().capture_window_handle != crate::handles::Hwnd::from(hwnd) {
        return false;
    }
    let Some(index) = edit_char_at_point(state, hwnd, x, y) else {
        return false;
    };
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
        return false;
    }
    let anchor = if *caret == *sel_start {
        *sel_end
    } else {
        *sel_start
    };
    *sel_start = anchor.min(index);
    *sel_end = anchor.max(index);
    *caret = index;
    // Horizontal movement drops the vertical-movement goal column like any
    // horizontal key.
    *goal_column = None;
    true
}

/// EDIT: WM_LBUTTONUP — end the drag session: release the mouse capture (the
/// selection stays as-is; Windows finalizes the drag on release).
pub(super) fn edit_mouse_up(state: &mut WinApiState, hwnd: u64) {
    if state.window_state().capture_window_handle == crate::handles::Hwnd::from(hwnd) {
        state.window_state().capture_window_handle = crate::handles::Hwnd::NULL;
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
    *caret = word_end.min(len);
    *sel_start = word_start.min(len);
    *sel_end = word_end.min(len);
    // The caret moved horizontally (to the word end); drop any remembered
    // vertical-movement goal column.
    *goal_column = None;
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
    *first_visible_line != old
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
        goal_column,
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
    *goal_column = None;
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
/// The y uses the SAME stored control font the paint path resolves (falling
/// back to the system default), so EM_POSFROMCHAR and the rendered rows
/// agree; per-glyph x lands with Task 2.5's hit-test.
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
        // The STORED font (falling back to the system default) drives the
        // line height — the same resolution the paint path uses, so
        // EM_POSFROMCHAR and the rendered rows agree. The engine is taken
        // out of gdi state so the resolution helper can run next to `state`;
        // it is put back unconditionally. Safe under the single shared
        // WinApiState mutex — the take and the put cannot interleave with
        // another handler's.
        let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
        let line_h = crate::gdi32::window_font_resolution(state, hwnd, &mut font_engine)
            .map_or(0, |(_key, resolved)| resolved.line_height());
        state.gdi_state().font_engine = font_engine;
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

/// Send an `EN_*` scroll notification (`EN_VSCROLL` / `EN_HSCROLL`) as
/// WM_COMMAND(MAKEWPARAM(id, notify)) to the parent — the EDIT scrolled, so
/// notepad re-reads the caret position into its status bar.
pub(super) fn edit_notify_scroll(
    state: &mut WinApiState,
    hwnd: u64,
    notify: u64,
) -> Result<Option<u64>> {
    let id = find_window(state, hwnd).map_or(0, |w| w.menu_handle);
    let command_wparam = make_command_wparam(id, notify);
    deliver_command(state, hwnd, command_wparam)
}

/// EDIT: WM_SETFOCUS — show the caret (blink phase reset to on) and arm the
/// internal blink timer. The timer is a real `TimerRecord` on the edit's
/// window: the message pump synthesizes the guest-visible `WM_TIMER` while
/// the thread idles in `GetMessage`, exactly like the SetTimer a Windows EDIT
/// control runs internally.
pub(super) fn edit_focus_gained(state: &mut WinApiState, hwnd: u64) {
    if let Some(window) = find_window_mut(state, hwnd) {
        window.flags.insert(WindowFlags::FOCUSED);
    }
    if let ControlState::Edit { caret_on, .. } = edit_state_for_window(state, hwnd) {
        *caret_on = true;
    }
    let window_handle = crate::handles::Hwnd::from(hwnd);
    let timers = &mut state.window_state().timers;
    if !timers
        .iter()
        .any(|timer| timer.window_handle == window_handle && timer.timer_id == CARET_TIMER_ID)
    {
        timers.push(TimerRecord {
            window_handle,
            timer_id: CARET_TIMER_ID,
            interval_ms: CARET_BLINK_MS,
            callback_address: 0,
            next_fire: crate::user32::misc::timer_deadline(CARET_BLINK_MS),
        });
    }
}

/// EDIT: WM_KILLFOCUS — hide the caret and disarm the blink timer. Paint
/// already skips the caret while unfocused; the phase resets on the next
/// focus so the caret returns solid.
pub(super) fn edit_focus_lost(state: &mut WinApiState, hwnd: u64) {
    if let Some(window) = find_window_mut(state, hwnd) {
        window.flags.remove(WindowFlags::FOCUSED);
    }
    let window_handle = crate::handles::Hwnd::from(hwnd);
    state
        .window_state()
        .timers
        .retain(|timer| timer.window_handle != window_handle || timer.timer_id != CARET_TIMER_ID);
}

/// EDIT: one caret-blink `WM_TIMER` tick — flip the caret phase. Returns
/// whether the phase changed (always true while the state exists), so the
/// caller repaints exactly on real ticks.
pub(super) fn edit_caret_tick(state: &mut WinApiState, hwnd: u64) -> bool {
    if let ControlState::Edit { caret_on, .. } = edit_state_for_window(state, hwnd) {
        *caret_on = !*caret_on;
        return true;
    }
    false
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
    let (sel_start, sel_end, caret, style_bits, first_visible_line, caret_on) =
        match control_state(state, info.dc_window.as_u64()) {
            Some(ControlState::Edit {
                caret,
                sel_start,
                sel_end,
                style_bits,
                first_visible_line,
                caret_on,
                ..
            }) => (
                (*sel_start).min(*sel_end),
                (*sel_start).max(*sel_end),
                *caret,
                *style_bits,
                *first_visible_line,
                *caret_on,
            ),
            _ => (0, 0, 0, 0, 0, true),
        };
    let (sel_start, sel_end, caret) = (sel_start.min(len), sel_end.min(len), caret.min(len));
    let line_h = resolved.line_height();
    // The multiline/wrap decisions read the LIVE creation style from the
    // window record, not `style_bits`: the read-only `control_state` accessor
    // never refreshes `style_bits`, whose lazy seed starts at 0 — so the very
    // FIRST paint (WM_PAINT right after creation, before any keyboard/input
    // message ran a mutating accessor) would otherwise render a multiline
    // EDIT as single-line: one row, vertically centered. The caret/selection
    // fields and the ES_ALIGN_MASK bits are correctly maintained (messages
    // update them) and stay on the control state.
    let edit_style = state
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
        // caret lands at the END of the row before the break. It draws only
        // in the blink ON phase (the focus timer toggles `caret_on`).
        if focused && caret_on && !caret_drawn && caret >= row.char_start && caret <= row.char_end {
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
