//! EDIT-class state machine (WM_CHAR, caret/selection, EM_* messages) and the
//! EDIT paint path (split from `controls.rs`).

use anyhow::Result;

use super::listbox::render_control_text;
use super::paint::fill_rect_clipped;
use super::{
    COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, ControlClassKind, ControlState, control_state,
    deliver_command,
};
use crate::gdi32::ResolvedWindow;
use crate::gdi32::{FontEngine, FontKey, ResolvedFont};
use crate::user32::{
    EN_CHANGE, VK_END, VK_HOME, VK_LEFT, VK_RIGHT, VK_SHIFT, WinApiState, find_window,
    make_command_wparam,
};

/// The EDIT control's state, seeded on demand. Only reachable from the
/// `(Edit, _)` dispatch arms, so the seed kind is always `Edit`. Takes the
/// `control_states` field (not the whole `WindowState`) so callers can hold a
/// `window` borrow from `ws.windows` at the same time (disjoint fields).
fn edit_state_mut(
    control_states: &mut std::collections::HashMap<crate::handles::Hwnd, ControlState>,
    hwnd: u64,
) -> &mut ControlState {
    control_states
        .entry(crate::handles::Hwnd::from(hwnd))
        .or_insert_with(|| ControlClassKind::Edit.new_state())
}

/// EDIT: process one `WM_CHAR` — insert at the caret (replacing an active
/// selection), Backspace deletes before the caret. Returns whether the text
/// changed (callers deliver EN_CHANGE only then).
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
    let ControlState::Edit {
        caret,
        sel_start,
        sel_end,
    } = edit_state_mut(&mut ws.control_states, hwnd)
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
                replace_range(text, caret, sel_start, sel_end, start, end, "");
                return true;
            }
            let caret_pos = *caret;
            if caret_pos == 0 {
                return false;
            }
            replace_range(
                text,
                caret,
                sel_start,
                sel_end,
                caret_pos.saturating_sub(1),
                caret_pos,
                "",
            );
            true
        }
        // Enter/Escape are no-ops for the slice; 0x7F (DEL) is handled by the
        // WM_KEYDOWN VK_DELETE path, never inserted as a character.
        0x0D | 0x1B | 0x7F => false,
        _ if ch >= 0x20 => {
            let Some(c) = char::from_u32(ch) else {
                return false;
            };
            let (start, end) = if start == end {
                (*caret, *caret)
            } else {
                (start, end)
            };
            replace_range(text, caret, sel_start, sel_end, start, end, &c.to_string());
            true
        }
        _ => false,
    }
}

/// Replace the character range `[start, end)` (clamped to the text) with
/// `replacement`; the caret lands after the inserted text and the selection is
/// cleared.
#[allow(clippy::too_many_arguments)]
fn replace_range(
    text: &mut String,
    caret: &mut usize,
    sel_start: &mut usize,
    sel_end: &mut usize,
    start: usize,
    end: usize,
    replacement: &str,
) {
    let len = text.chars().count();
    let start = start.min(len);
    let end = end.min(len).max(start);
    let start_byte = byte_index_of_char(text, start);
    let end_byte = byte_index_of_char(text, end);
    text.replace_range(start_byte..end_byte, replacement);
    *caret = start.saturating_add(replacement.chars().count());
    *sel_start = *caret;
    *sel_end = *caret;
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
    let ControlState::Edit {
        caret,
        sel_start,
        sel_end,
    } = edit_state_mut(&mut ws.control_states, hwnd)
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
    let ControlState::Edit {
        caret,
        sel_start,
        sel_end,
    } = edit_state_mut(&mut ws.control_states, hwnd)
    else {
        return false;
    };
    let text = &mut window.control_text;
    let len = text.chars().count();
    let (start, end) = normalized_selection(*sel_start, *sel_end, len);
    if start != end {
        replace_range(text, caret, sel_start, sel_end, start, end, "");
        return true;
    }
    let caret_pos = *caret;
    if caret_pos >= len {
        return false;
    }
    replace_range(
        text,
        caret,
        sel_start,
        sel_end,
        caret_pos,
        caret_pos.saturating_add(1),
        "",
    );
    true
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
    let ControlState::Edit {
        caret,
        sel_start,
        sel_end,
    } = edit_state_mut(&mut ws.control_states, hwnd)
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
            .0
            .get(usize::try_from(VK_SHIFT).unwrap_or(0))
            .is_some_and(|&key| key & 0x80 != 0)
    })
}

/// EDIT paint: text, selection highlight, and the caret bar.
///
/// Glyphs are proportional, so the caret and selection x positions are the
/// SUMMED advances of the preceding characters (matching the rendered text
/// exactly). The selection is drawn in two passes — the whole line in
/// COLOR_WINDOWTEXT, then the selected run re-rendered in
/// COLOR_HIGHLIGHTTEXT over its COLOR_HIGHLIGHT cells. The caret bar (1 px,
/// full line height) is only drawn while the control has focus.
#[allow(clippy::too_many_arguments)]
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
    let focused = find_window(state, info.dc_window.as_u64()).is_some_and(|w| w.focused);
    let (sel_start, sel_end, caret) = match control_state(state, info.dc_window.as_u64()) {
        Some(ControlState::Edit {
            caret,
            sel_start,
            sel_end,
        }) => (
            (*sel_start).min(*sel_end),
            (*sel_start).max(*sel_end),
            *caret,
        ),
        _ => (0, 0, 0),
    };
    let (sel_start, sel_end, caret) = (sel_start.min(len), sel_end.min(len), caret.min(len));
    let line_h = resolved.line_height();
    let ty = info
        .offset_y
        .saturating_add(height.saturating_sub(line_h).saturating_div(2))
        .max(info.offset_y);
    let right = info.offset_x.saturating_add(width);
    let bottom = info.offset_y.saturating_add(height);
    let clip = Some((info.offset_x, info.offset_y, right, bottom));
    let has_selection = focused && sel_start != sel_end;

    if has_selection {
        // Pass 1: fill the selected cells with COLOR_HIGHLIGHT (behind text).
        let sel_x = tx.saturating_add(font_engine.text_advance(resolved, key, text, sel_start));
        let sel_w = font_engine
            .text_advance(resolved, key, text, sel_end)
            .saturating_sub(font_engine.text_advance(resolved, key, text, sel_start));
        fill_rect_clipped(
            state,
            info,
            width,
            height,
            sel_x,
            ty,
            sel_w,
            line_h,
            COLOR_HIGHLIGHT,
        );
    }
    // Pass 2: the whole line in the normal text color.
    if !text.is_empty() {
        render_control_text(
            state,
            engine,
            info.hwnd,
            info.width,
            info.height,
            tx,
            ty,
            text,
            0,
            clip,
            font_engine,
            resolved,
            key,
        )?;
    }
    if has_selection {
        // Pass 3: re-render the selected run in COLOR_HIGHLIGHTTEXT.
        let selected: String = text
            .chars()
            .skip(sel_start)
            .take(sel_end.saturating_sub(sel_start))
            .collect();
        let sel_x = tx.saturating_add(font_engine.text_advance(resolved, key, text, sel_start));
        render_control_text(
            state,
            engine,
            info.hwnd,
            info.width,
            info.height,
            sel_x,
            ty,
            &selected,
            COLOR_HIGHLIGHTTEXT,
            clip,
            font_engine,
            resolved,
            key,
        )?;
    }
    if focused {
        // Pass 4: the 1 px caret bar at the caret's glyph cell.
        let caret_x = tx.saturating_add(font_engine.text_advance(resolved, key, text, caret));
        fill_rect_clipped(
            state,
            info,
            width,
            height,
            caret_x,
            ty,
            1,
            line_h,
            0x0000_0000,
        );
    }
    Ok(())
}
