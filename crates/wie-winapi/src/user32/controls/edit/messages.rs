//! The EDIT message handlers and the EDIT-specific control dispatch: the
//! `(Edit, msg)` arms that shadow the generic control dispatch
//! (`dispatch_edit_message`), the scroll messages, the EM_* message handlers,
//! the focus/caret-blink pair, and the clipboard/undo handlers. Split from the
//! monolithic `edit.rs`; the `pub(super)` items are the cross-file surface
//! imported through `super::messages::…`.

use anyhow::Result;

use crate::guest_memory::read_u16 as read_guest_u16;
use crate::state::{TimerRecord, WindowFlags};
use crate::user32::controls::{ControlState, SEL_EMPTY, SEL_MULTICHAR, SEL_MULTILINE, SEL_TEXT};
use crate::user32::controls::{WinMsg, control_state, deliver_command, invalidate};
use crate::user32::{
    DLGC_WANTCHARS, EN_CHANGE, EN_HSCROLL, EN_VSCROLL, VK_DELETE, VK_DOWN, VK_END, VK_HOME,
    VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_UP, WinApiState, find_window, find_window_mut,
    high_word, low_i32, low_word, make_command_wparam, read_guest_ansi_lossy,
    read_guest_utf16_lossy, write_guest_ansi_c_string, write_guest_i32, write_guest_u32,
    write_guest_utf16_c_string,
};

use super::keyboard::{
    ctrl_is_down, edit_delete_at_caret, edit_get_limit, edit_get_line, edit_get_selection,
    edit_line_count, edit_line_from_char, edit_line_index, edit_line_length, edit_move_caret,
    edit_set_limit, edit_set_selection,
};
use super::math::{clamp_scroll_offset, edit_scroll_context, line_from_char};
use super::mouse::{edit_mouse_dblclk, edit_mouse_down, edit_mouse_move, edit_scroll_caret};
use super::mutation::{
    EditMutation, byte_index_of_char, edit_char, normalized_selection, replace_crosses_lines,
    replace_range,
};
use super::paint::{edit_invalidate_caret, edit_invalidate_full, edit_invalidate_mutation};
use super::state::{
    CARET_BLINK_MS, CARET_TIMER_ID, H_LINE_STEP, MAX_GUEST_TEXT, edit_state_for_window,
    edit_state_mut, edit_text,
};

/// `WM_VSCROLL` / `WM_HSCROLL` scroll-bar request codes (winuser.h) — the
/// wParam LOW word. THUMBTRACK/POSITION carry the thumb position in the high
/// word.
pub(super) const SB_LINEUP: u16 = 0;
pub(super) const SB_LINEDOWN: u16 = 1;
pub(super) const SB_PAGEUP: u16 = 2;
pub(super) const SB_PAGEDOWN: u16 = 3;
pub(super) const SB_THUMBPOSITION: u16 = 4;
pub(super) const SB_THUMBTRACK: u16 = 5;
pub(super) const SB_TOP: u16 = 6;
pub(super) const SB_BOTTOM: u16 = 7;
pub(super) const SB_ENDSCROLL: u16 = 8;

/// The horizontal scroll-bar codes — the SAME values winuser.h aliases for
/// the H scrollbar (LINELEFT == LINEUP, and so on).
pub(super) const SB_LINELEFT: u16 = 0;
pub(super) const SB_LINERIGHT: u16 = 1;
pub(super) const SB_PAGELEFT: u16 = 2;
pub(super) const SB_PAGERIGHT: u16 = 3;
pub(super) const SB_LEFT: u16 = 6;
pub(super) const SB_RIGHT: u16 = 7;

/// The EDIT-specific control dispatch: the `(Edit, msg)` arms that must SHADOW
/// the generic control dispatch — an Edit's `WM_LBUTTONDOWN` places the caret,
/// it does NOT fall through to the generic press arm, and the EM_* messages
/// have no generic arm at all. The generic dispatch calls this first for an
/// EDIT window; `Ok(None)` means the message is not an edit message
/// (`WM_PAINT`, `WM_GETTEXT`, `WM_SETTEXT`, `WM_SETFONT`, `WM_COMMAND`,
/// `WM_LBUTTONUP`, a non-caret `WM_TIMER`, a non-navigation `WM_KEYDOWN`,
/// …) and the generic arms run unchanged. Every handled arm returns
/// `Ok(Some(..))` — never `Ok(None)` — so the fall-through signal is
/// unambiguous.
pub(crate) fn dispatch_edit_message(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    message: u32,
    word_parameter: u64,
    long_parameter: u64,
) -> Result<Option<u64>> {
    let unicode = find_window(state, hwnd).is_some_and(|w| w.unicode);
    match WinMsg::from(message) {
        // A left-click on an EDIT places the caret at the click and begins a
        // drag selection: the click becomes the anchor, the edit captures the
        // mouse so dragging off the control still extends the selection, and
        // focus moves to the edit.
        WinMsg::WM_LBUTTONDOWN => {
            let x = i32::from(low_word(long_parameter));
            let y = i32::from(high_word(long_parameter));
            edit_mouse_down(state, hwnd, x, y);
            // A mouse click focuses the EDIT without a WM_SETFOCUS: the blink
            // phase is reset to ON and the blink timer re-armed exactly like
            // the focus path, so a click on an edit whose phase was left OFF
            // (or whose timer a kill-focus disarmed) cannot leave the caret
            // invisible until the next key focus.
            edit_focus_gained(state, hwnd);
            if let Some(window) = find_window_mut(state, hwnd) {
                window.flags.insert(WindowFlags::PRESSED);
                window.flags.insert(WindowFlags::FOCUSED);
            }
            state.window_state().focus_window_handle = crate::handles::Hwnd::from(hwnd);
            state.window_state().capture_window_handle = crate::handles::Hwnd::from(hwnd);
            invalidate(state, hwnd);
            Ok(Some(0))
        }
        // A second press within the double-click time/slop — the host
        // synthesizes WM_LBUTTONDBLCLK from it, matching Windows — selects
        // the whitespace-delimited word under the click and starts a drag
        // from it, like the single-click press above.
        WinMsg::WM_LBUTTONDBLCLK => {
            let x = i32::from(low_word(long_parameter));
            let y = i32::from(high_word(long_parameter));
            edit_mouse_dblclk(state, hwnd, x, y);
            if let Some(window) = find_window_mut(state, hwnd) {
                window.flags.insert(WindowFlags::PRESSED);
                window.flags.insert(WindowFlags::FOCUSED);
            }
            state.window_state().focus_window_handle = crate::handles::Hwnd::from(hwnd);
            state.window_state().capture_window_handle = crate::handles::Hwnd::from(hwnd);
            invalidate(state, hwnd);
            Ok(Some(0))
        }
        // While the edit holds the mouse capture, a move extends the drag
        // selection from the click anchor to the current position (the caret
        // tracks the pointer). Hover moves without capture are no-ops.
        WinMsg::WM_MOUSEMOVE => {
            let x = i32::from(low_word(long_parameter));
            let y = i32::from(high_word(long_parameter));
            if edit_mouse_move(state, hwnd, x, y) {
                invalidate(state, hwnd);
            }
            Ok(Some(0))
        }
        // EDIT focus: show the caret (blink phase reset to on) and arm the
        // internal ~530 ms blink timer — the same SetTimer mechanism a real
        // Windows EDIT runs internally.
        WinMsg::WM_SETFOCUS => {
            edit_focus_gained(state, hwnd);
            Ok(Some(0))
        }
        WinMsg::WM_KILLFOCUS => {
            edit_focus_lost(state, hwnd);
            Ok(Some(0))
        }
        // The EDIT's internal caret-blink timer: flip the caret phase and
        // repaint (the caret bar is drawn only in the on phase). The repaint
        // is narrowed to the caret's row — the blink only toggles a 1 px ×
        // line-height bar, so the whole EDIT need not redraw. A non-caret
        // WM_TIMER is not an edit message: it falls through to the generic
        // arms (which return the neutral zero).
        WinMsg::WM_TIMER => {
            if word_parameter == CARET_TIMER_ID {
                if edit_caret_tick(state, hwnd) {
                    edit_invalidate_caret(state, hwnd);
                }
                Ok(Some(0))
            } else {
                Ok(None)
            }
        }
        WinMsg::WM_GETDLGCODE => Ok(Some(DLGC_WANTCHARS)),
        WinMsg::WM_CHAR => {
            let changed = edit_char(state, hwnd, word_parameter);
            if changed {
                // Typing past the last visible row auto-scrolls the caret
                // into view — the same minimal scroll EM_SCROLLCARET applies.
                edit_scroll_caret(state, hwnd);
                invalidate(state, hwnd);
                return edit_notify_change(state, hwnd);
            }
            Ok(Some(0))
        }
        // Caret navigation keys on a focused EDIT (Shift extends the
        // selection; Ctrl+Home/End are document-wide in a multiline EDIT).
        // VK_DELETE has no WM_CHAR, so it is handled below.
        WinMsg::WM_KEYDOWN => {
            let vk = word_parameter & 0xFF;
            if matches!(
                vk,
                VK_LEFT | VK_RIGHT | VK_HOME | VK_END | VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT
            ) {
                if edit_move_caret(state, hwnd, vk) {
                    // Any caret move keeps the caret in view (the same
                    // minimal scroll EM_SCROLLCARET applies).
                    edit_scroll_caret(state, hwnd);
                    invalidate(state, hwnd);
                    // A caret move refreshes the parent's status-bar Ln/Col:
                    // vertical moves deliver EN_VSCROLL, horizontal moves
                    // EN_HSCROLL (Ctrl+Home/End are document-wide vertical
                    // jumps). The notification fires only when the caret
                    // actually moved — a key at the document edge is a no-op
                    // and stays silent.
                    let notify = match vk {
                        VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT => EN_VSCROLL,
                        VK_HOME | VK_END if ctrl_is_down(state) => EN_VSCROLL,
                        _ => EN_HSCROLL,
                    };
                    return edit_notify_scroll(state, hwnd, notify);
                }
                Ok(Some(0))
            } else if vk == VK_DELETE {
                let changed = edit_delete_at_caret(state, hwnd);
                if changed {
                    invalidate(state, hwnd);
                    return edit_notify_change(state, hwnd);
                }
                Ok(Some(0))
            } else {
                // A non-navigation, non-delete key is not an edit message.
                Ok(None)
            }
        }
        // EM_SETSEL: wParam = start, lParam = end (character positions); a
        // negative argument means "end of text", so (0, -1) selects all.
        WinMsg::EM_SETSEL => {
            let start = low_i32(word_parameter, "EM_SETSEL start")?;
            let end = low_i32(long_parameter, "EM_SETSEL end")?;
            edit_set_selection(state, hwnd, start, end);
            invalidate(state, hwnd);
            Ok(Some(1)) // TRUE
        }
        // EM_GETSEL: optional output pointers (start, end) + packed return
        // MAKELONG(start, end) — low word start, high word end.
        WinMsg::EM_GETSEL => {
            let (start, end) = edit_get_selection(state, hwnd);
            if word_parameter != 0 {
                write_guest_u32(engine, word_parameter, u32::try_from(start).unwrap_or(0))?;
            }
            if long_parameter != 0 {
                write_guest_u32(engine, long_parameter, u32::try_from(end).unwrap_or(0))?;
            }
            let start_lo = u32::try_from(start).unwrap_or(0) & 0xFFFF;
            let end_hi = (u32::try_from(end).unwrap_or(0) & 0xFFFF) << 16;
            Ok(Some(u64::from(start_lo | end_hi)))
        }
        // EM_LIMITTEXT: wParam = max chars the user can type (0 = none).
        WinMsg::EM_LIMITTEXT => {
            edit_set_limit(state, hwnd, word_parameter);
            Ok(Some(1)) // TRUE
        }
        WinMsg::EM_GETLIMITTEXT => Ok(Some(edit_get_limit(state, hwnd))),
        // EM_GETLINECOUNT: the '\n'-separated line count (1 for empty).
        WinMsg::EM_GETLINECOUNT => Ok(Some(edit_line_count(state, hwnd))),
        // EM_LINEFROMCHAR: the line containing the char index (-1 = the
        // caret's line). wParam is a 32-bit index; the low-word decode treats
        // 0xFFFF_FFFF as -1 like the other control messages.
        WinMsg::EM_LINEFROMCHAR => {
            let index = low_i32(word_parameter, "EM_LINEFROMCHAR index")?;
            Ok(Some(edit_line_from_char(state, hwnd, index)))
        }
        // EM_LINEINDEX: char index of the line's first char (-1 if the line
        // is out of range).
        WinMsg::EM_LINEINDEX => {
            let line = low_i32(word_parameter, "EM_LINEINDEX line")?;
            Ok(Some(edit_line_index(state, hwnd, line)))
        }
        // EM_LINELENGTH: chars in the line holding the char index, excluding
        // its EOL (-1 = the caret's line, minus its selection).
        WinMsg::EM_LINELENGTH => {
            let index = low_i32(word_parameter, "EM_LINELENGTH index")?;
            Ok(Some(edit_line_length(state, hwnd, index)))
        }
        // EM_GETLINE: wParam = line, lParam = buffer whose first WORD is the
        // capacity (including the NUL). Copies the line without its EOL;
        // returns the char count (0 for an out-of-range line).
        WinMsg::EM_GETLINE => {
            let line = low_i32(word_parameter, "EM_GETLINE line")?;
            let copied = edit_get_line(engine, unicode, state, hwnd, line, long_parameter)?;
            Ok(Some(copied))
        }
        // EM_REPLACESEL: replace the selection with the string at lParam; a
        // real change delivers EN_CHANGE like a keystroke.
        WinMsg::EM_REPLACESEL => {
            let changed = edit_replace_selection(engine, unicode, state, hwnd, long_parameter)?;
            if changed {
                invalidate(state, hwnd);
                return edit_notify_change(state, hwnd);
            }
            Ok(Some(0))
        }
        // EM_SCROLLCARET: bring the caret's row into view by the smallest
        // scroll (viewport-aware minimal-scroll).
        WinMsg::EM_SCROLLCARET => {
            if edit_scroll_caret(state, hwnd) {
                invalidate(state, hwnd);
            }
            Ok(Some(1)) // TRUE
        }
        // WM_VSCROLL: the wParam low word is the SB_* scroll code, the high
        // word the thumb position (SB_THUMBTRACK/POSITION). The offset is
        // clamped to the wrap-aware viewport and the control is invalidated
        // so the paint re-renders from the new top row. A real scroll
        // delivers EN_VSCROLL to the parent.
        WinMsg::WM_VSCROLL => {
            let code = low_word(word_parameter);
            let thumb = high_word(word_parameter);
            if edit_scroll_vertical(state, hwnd, code, thumb) {
                invalidate(state, hwnd);
                return edit_notify_scroll(state, hwnd, EN_VSCROLL);
            }
            Ok(Some(0))
        }
        // WM_HSCROLL: the wParam low word is the SB_* scroll code, the high
        // word the thumb position (SB_THUMBTRACK/POSITION). Moves the
        // horizontal scroll offset of a wrap-off EDIT (WS_HSCROLL) — a
        // wrap-on EDIT has no horizontal scrollbar, so the codes are no-ops
        // there. EN_HSCROLL is delivered to the parent either way.
        WinMsg::WM_HSCROLL => {
            let code = low_word(word_parameter);
            let thumb = high_word(word_parameter);
            if edit_scroll_horizontal(state, hwnd, code, thumb) {
                invalidate(state, hwnd);
            }
            edit_notify_scroll(state, hwnd, EN_HSCROLL)
        }
        // WM_MOUSEWHEEL: the signed delta in the wParam high word scrolls the
        // multiline EDIT (3 lines per notch). The gui layer routes the wheel
        // to the FOCUS window, matching Windows.
        WinMsg::WM_MOUSEWHEEL => {
            if edit_mouse_wheel(state, hwnd, word_parameter) {
                invalidate(state, hwnd);
            }
            Ok(Some(0))
        }
        WinMsg::EM_GETMODIFY => Ok(Some(edit_get_modify(state, hwnd))),
        WinMsg::EM_SETMODIFY => {
            edit_set_modify(state, hwnd, word_parameter);
            Ok(Some(1))
        }
        // EM_GETHANDLE: a LocalAlloc'd guest copy of the text (a fresh
        // buffer per call; the guest owns it and LocalFree's it).
        WinMsg::EM_GETHANDLE => {
            let handle = edit_get_handle(engine, unicode, state, hwnd)?;
            Ok(Some(handle))
        }
        // EM_SETHANDLE: adopt the guest buffer as the text (the caller
        // relinquishes ownership; caret/selection reset).
        WinMsg::EM_SETHANDLE => {
            let ok = edit_set_handle(engine, unicode, state, hwnd, long_parameter)?;
            Ok(Some(u64::from(ok)))
        }
        // EM_SETTABSTOPS: wParam = stop count, lParam = u16 array in dialog
        // units (0 = default stops).
        WinMsg::EM_SETTABSTOPS => {
            let ok = edit_set_tab_stops(engine, state, hwnd, word_parameter, long_parameter)?;
            Ok(Some(u64::from(ok)))
        }
        // EM_POSFROMCHAR: wParam = char index, lParam = POINT* (client
        // coords). Basic answer — the stored-font metrics resolve the row y.
        WinMsg::EM_POSFROMCHAR => {
            let index = low_i32(word_parameter, "EM_POSFROMCHAR index")?;
            let ok = edit_pos_from_char(engine, state, hwnd, index, long_parameter)?;
            Ok(Some(u64::from(ok)))
        }
        // EM_SELECTIONTYPE: the SEL_* bitmask of the current selection.
        WinMsg::EM_SELECTIONTYPE => Ok(Some(edit_selection_type(state, hwnd))),
        WinMsg::EM_GETFIRSTVISIBLELINE => Ok(Some(edit_first_visible_line(state, hwnd))),
        // EM_CANUNDO — TRUE when the single-level undo buffer holds a
        // snapshot (an edit since the last undo/empty).
        WinMsg::EM_CANUNDO => Ok(Some(u64::from(edit_can_undo(state, hwnd)))),
        // EM_UNDO / WM_UNDO (the Edit menu's Undo command sends WM_UNDO to
        // the focused edit): restore the last mutation and clear the buffer.
        // A real restore delivers EN_CHANGE like any text change.
        WinMsg::EM_UNDO | WinMsg::WM_UNDO => {
            if edit_undo(state, hwnd) {
                invalidate(state, hwnd);
                return edit_notify_change(state, hwnd);
            }
            Ok(Some(0))
        }
        // EM_EMPTYUNDOBUFFER: discard any pending undo snapshot.
        WinMsg::EM_EMPTYUNDOBUFFER => {
            edit_empty_undo_buffer(state, hwnd);
            Ok(Some(0))
        }
        // WM_COPY: store the selected text on the host clipboard (no text
        // change, so no EN_CHANGE).
        WinMsg::WM_COPY => {
            edit_copy(state, hwnd);
            Ok(Some(0))
        }
        // WM_CUT: copy the selection to the clipboard, then delete it (the
        // delete delivers EN_CHANGE like any text change).
        WinMsg::WM_CUT => {
            if edit_cut(state, hwnd) {
                invalidate(state, hwnd);
                return edit_notify_change(state, hwnd);
            }
            Ok(Some(0))
        }
        // WM_PASTE: insert the clipboard text at the caret, replacing the
        // selection; an empty clipboard is a no-op.
        WinMsg::WM_PASTE => {
            if edit_paste(state, hwnd) {
                invalidate(state, hwnd);
                return edit_notify_change(state, hwnd);
            }
            Ok(Some(0))
        }
        // WM_CLEAR: delete the selection WITHOUT copying it — the clipboard
        // is emptied, so IsClipboardFormatAvailable goes false.
        WinMsg::WM_CLEAR => {
            if edit_clear(state, hwnd) {
                invalidate(state, hwnd);
                return edit_notify_change(state, hwnd);
            }
            Ok(Some(0))
        }
        // Everything else is not an edit message: the generic control
        // dispatch owns it (WM_PAINT, WM_GETTEXT, WM_SETTEXT, WM_SETFONT,
        // WM_COMMAND, WM_LBUTTONUP, ...).
        _ => Ok(None),
    }
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
        let crossed = replace_crosses_lines(text, start, end, replacement);
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
            replacement,
            *limit,
        );
        (changed, start, crossed)
    };
    if changed {
        edit_invalidate_mutation(state, hwnd, edit_start, crossed_lines);
    }
    changed
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
    let restored = {
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
    };
    if restored {
        // The restore can rewrite any line — a full repaint is the safe band.
        edit_invalidate_full(state, hwnd);
    }
    restored
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
    let moved = *first_visible_line != old;
    if moved {
        // A scroll reflows the viewport: any pending row band is stale.
        edit_invalidate_full(state, hwnd);
    }
    moved
}

/// EDIT: WM_HSCROLL — apply one horizontal scroll-bar request on a wrap-off
/// EDIT (WS_HSCROLL). `code` is the SB_* code in the wParam low word; `thumb`
/// is the high-word thumb position used by SB_THUMBTRACK/POSITION. Line
/// scrolls step one character cell (8 px); pages are the visible text width.
/// A wrap-on EDIT has no horizontal overflow, so every code clamps to 0.
/// Returns whether the offset moved.
pub(super) fn edit_scroll_horizontal(
    state: &mut WinApiState,
    hwnd: u64,
    code: u16,
    thumb: u16,
) -> bool {
    let Some(context) = edit_scroll_context(state, hwnd) else {
        return false;
    };
    let ControlState::Edit {
        first_visible_column,
        ..
    } = edit_state_for_window(state, hwnd)
    else {
        return false;
    };
    let old = *first_visible_column;
    let target = match code {
        SB_LINELEFT => old.saturating_sub(H_LINE_STEP),
        SB_LINERIGHT => old.saturating_add(H_LINE_STEP),
        SB_PAGELEFT => old.saturating_sub(context.h_page),
        SB_PAGERIGHT => old.saturating_add(context.h_page),
        SB_LEFT => 0,
        SB_RIGHT => context.h_overflow,
        SB_THUMBPOSITION | SB_THUMBTRACK => usize::from(thumb),
        _ => old, // unknown codes: no-op
    };
    *first_visible_column = target.min(context.h_overflow);
    let moved = *first_visible_column != old;
    if moved {
        // A scroll reflows the viewport: any pending row band is stale.
        edit_invalidate_full(state, hwnd);
    }
    moved
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
    let moved = *first_visible_line != old;
    if moved {
        // A scroll reflows the viewport: any pending row band is stale.
        edit_invalidate_full(state, hwnd);
    }
    moved
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
pub(crate) fn edit_invalidate_text_buffer(state: &mut WinApiState, hwnd: u64) {
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
    // A whole-text adoption rewrites every row — a full repaint is the safe
    // band (and the adoption marks the window for the next paint cycle).
    edit_invalidate_full(state, hwnd);
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
        let line_h = state.with_font_engine(|state, font_engine| {
            crate::gdi32::window_font_resolution(state, hwnd, font_engine)
                .map_or(0, |(_key, resolved)| resolved.line_height())
        });
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
pub(crate) fn edit_notify_scroll(
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
