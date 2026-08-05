//! The EDIT control, split into its natural seams (the `edit.rs` monolith
//! was 3,500 lines — this module's submodules each stay far under the
//! 1,500-line cap):
//!
//! - [`state`] — constants and the shared seeded-state accessors
//! - [`math`] — the line/layout/scroll geometry
//! - [`mutation`] — `WM_CHAR`/`VK_BACK` insertion and the `replace_range` core
//! - [`keyboard`] — caret movement, `VK_DELETE`, the line-metric EM_* handlers
//! - [`messages`] — the EDIT-specific dispatch (`dispatch_edit_message`) and
//!   the EM_* / scroll / focus / clipboard handlers
//! - [`mouse`] — click-to-caret, drag selection, scrollbar-gutter presses
//! - [`paint`] — `paint_edit`, the row-level invalidation band, scrollbars
//!
//! The re-exports below are the module's PUBLIC API surface — exactly what
//! `controls` (and, through `controls`, `button.rs` and `state/tests.rs`)
//! references. Everything else stays private to `edit`.

mod keyboard;
mod math;
mod messages;
mod mouse;
mod mutation;
mod paint;
mod state;

// `UndoSnapshot` is the type of the public `ControlState::Edit::undo_snapshot`
// field, so it must be reachable at the same visibility as the enum.
pub use mutation::UndoSnapshot;

// The EDIT-specific dispatch arms: `controls::dispatch` calls this for an
// EDIT window before its generic arms run.
pub(super) use messages::dispatch_edit_message;
// The SetWindowText / WM_SETTEXT handlers in `controls` clear the cached
// EM_GETHANDLE buffer and the undo buffer outside the control dispatch.
pub(super) use messages::edit_invalidate_text_buffer;
pub(super) use messages::edit_notify_scroll;
// The generic WM_LBUTTONUP arm in `controls` ends an EDIT's drag session.
pub(super) use mouse::edit_mouse_up;
// The generic WM_SETFONT / WM_SETTEXT arms in `controls` reset an EDIT's
// pending row band to full.
pub(super) use paint::edit_reset_invalid_rows;
// `controls::button` reads the EDIT's dirty band and paints it through the
// same `paint_edit` the control dispatch's WM_PAINT arm uses.
pub(super) use paint::{edit_dirty_band, paint_edit};
// The no-create undo-buffer clear is called from the SetWindowText handlers
// in `user32::window` (they write control text outside the control dispatch).
pub(crate) use messages::edit_clear_undo_buffer;

// Re-exported for the host unit tests in `state/tests.rs` (through `controls`);
// test-only so the lib build has no unused re-export.
#[cfg(test)]
pub(crate) use math::{
    VisibleSegment, clamp_scroll_offset, edit_text_area, layout_visible_lines, visible_line_count,
    visual_rows,
};
#[cfg(test)]
pub(crate) use mouse::edit_char_index_at_point;
#[cfg(test)]
pub(crate) use state::scrollbar_visible;
