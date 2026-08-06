//! The EDIT text-mutation core: `WM_CHAR`/`VK_BACK` insertion, the shared
//! `replace_range` splice, the single-level undo snapshot, and the mutation
//! bookkeeping (`EditMutation`, `replace_crosses_lines`). Split from the
//! monolithic `edit.rs`; the `pub(super)` items are the cross-file surface
//! the keyboard/messages paths import through `super::mutation::…`.

use crate::user32::WinApiState;
use crate::user32::controls::{ControlState, ES_MULTILINE};

use super::paint::edit_invalidate_mutation;
use super::state::edit_state_mut;

/// WM_CHAR codes the EDIT treats as control characters, not printable glyphs
/// (winuser.h VK_* values — an EDIT delivers the Delete key's keystroke as
/// `WM_CHAR` 0x7F, the ASCII DEL, which is distinct from `VK_DELETE` (0x2E)
/// that arrives via `WM_KEYDOWN`).
const VK_BACK: u32 = 0x08;
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
const CHAR_DELETE: u32 = 0x7F;

/// EDIT: process one `WM_CHAR` — insert at the caret (replacing an active
/// selection), Backspace deletes before the caret. Enter inserts `\n` in a
/// multiline EDIT (`ES_MULTILINE`); the `EM_LIMITTEXT` cap is honored. Returns
/// whether the text changed (callers deliver EN_CHANGE only then).
pub(super) fn edit_char(state: &mut WinApiState, hwnd: u64, char_code: u64) -> bool {
    let ch = u32::try_from(char_code & 0xFFFF).unwrap_or(0);
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
            VK_BACK => {
                // VK_BACK: delete the selection, or the character before the caret.
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
                    if caret_pos == 0 {
                        (false, 0, false)
                    } else {
                        let crossed = text.chars().nth(caret_pos.saturating_sub(1)) == Some('\n');
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
                            caret_pos.saturating_sub(1),
                            caret_pos,
                            "",
                            *limit,
                        );
                        (changed, caret_pos.saturating_sub(1), crossed)
                    }
                }
            }
            // Enter inserts a line break only in a multiline EDIT; Escape and
            // CHAR_DELETE are never inserted as characters (Delete is handled
            // by the WM_KEYDOWN VK_DELETE path).
            VK_RETURN => {
                if *style_bits & ES_MULTILINE != 0 {
                    let crossed = replace_crosses_lines(text, start, end, "\n");
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
                        "\n",
                        *limit,
                    );
                    (changed, start, crossed)
                } else {
                    (false, 0, false)
                }
            }
            VK_ESCAPE | CHAR_DELETE => (false, 0, false),
            _ if ch >= VK_SPACE => {
                let Some(c) = char::from_u32(ch) else {
                    return false;
                };
                let (start, end) = if start == end {
                    (*caret, *caret)
                } else {
                    (start, end)
                };
                let crossed = replace_crosses_lines(text, start, end, &c.to_string());
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
                    &c.to_string(),
                    *limit,
                );
                (changed, start, crossed)
            }
            _ => (false, 0, false),
        }
    };
    // Phase 2: mark the rows the mutation dirtied for the next paint — a
    // single-char insert repaints only its line's rows, not the whole EDIT.
    if changed {
        edit_invalidate_mutation(state, hwnd, edit_start, crossed_lines);
    }
    changed
}

/// The mutable caret/selection/modify slice of an EDIT state, plus the cached
/// EM_GETHANDLE buffer and the undo snapshot. Bundled so the shared mutation
/// helper stays under the `too_many_arguments` lint bar.
pub(super) struct EditMutation<'a> {
    pub(super) caret: &'a mut usize,
    pub(super) sel_start: &'a mut usize,
    pub(super) sel_end: &'a mut usize,
    pub(super) goal_column: &'a mut Option<usize>,
    pub(super) modified: &'a mut bool,
    pub(super) handle_buffer: &'a mut u64,
    pub(super) undo_snapshot: &'a mut Option<UndoSnapshot>,
}

/// A single-level undo snapshot: the full text plus the caret and selection
/// captured BEFORE a mutation (Task 2.6). `EM_UNDO`/`WM_UNDO` restore it and
/// clear the buffer — Windows undo is single-level, there is no redo. The
/// fields are `pub(super)`: `messages::edit_undo` reads them, and the struct
/// itself is re-exported `pub` (the `ControlState::Edit::undo_snapshot` field
/// type) through `edit/mod.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoSnapshot {
    pub(super) text: String,
    pub(super) caret: usize,
    pub(super) sel_start: usize,
    pub(super) sel_end: usize,
}

/// Replace the character range `[start, end)` (clamped to the text) with
/// `replacement`; the caret lands after the inserted text and the selection is
/// cleared. The `EM_LIMITTEXT` cap (0 = unlimited) truncates the replacement
/// so the post-edit length never exceeds it — deletions are never capped.
/// Returns whether the text changed (a fully-truncated insertion is a no-op).
/// `pub(super)`: the keyboard `VK_DELETE`/messages `EM_REPLACESEL`/`WM_PASTE`
/// paths splice through the same core.
pub(super) fn replace_range(
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
pub(super) fn byte_index_of_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// The selection as an ordered, clamped `(start, end)` character range.
#[must_use]
pub(super) fn normalized_selection(sel_start: usize, sel_end: usize, len: usize) -> (usize, usize) {
    let start = sel_start.min(len);
    let end = sel_end.min(len);
    (start.min(end), start.max(end))
}

/// Whether replacing the char span [start, end) of `text` with `replacement`
/// crosses a line boundary — the replacement or the removed span contains a
/// `\n`. A line structure change shifts every row below the edit, so the
/// invalidation must cover everything from that line down.
#[must_use]
pub(super) fn replace_crosses_lines(
    text: &str,
    start: usize,
    end: usize,
    replacement: &str,
) -> bool {
    if replacement.contains('\n') {
        return true;
    }
    let start_byte = byte_index_of_char(text, start);
    let end_byte = byte_index_of_char(text, end);
    text.get(start_byte..end_byte)
        .is_some_and(|span| span.contains('\n'))
}
