//! Common dialog stubs (`comdlg32.dll`): open/save file simulation, print /
//! page-setup, choose color/font, and find/replace modeless dialogs.
//!
//! The module is split by dialog family:
//! - [`file`] — `GetOpenFileName` / `GetSaveFileName` / `GetFileTitle`
//! - [`print`] — `PrintDlgW` / `PageSetupDlgW`
//! - [`color`] — `ChooseColorA`
//! - [`font`] — `ChooseFontW`
//! - [`find`] — `FindTextW` / `ReplaceTextW`

pub mod color;
pub mod file;
pub mod find;
pub mod font;
pub mod print;

#[cfg(test)]
mod file_tests;
#[cfg(test)]
pub(crate) mod test_support;

use crate::WinApiState;
use crate::user32::is_known_window;

/// No extended common-dialog error.
pub(crate) const CDERR_NONE: u32 = 0;

// Winuser.h style bits the user32 module does not name.
pub(crate) const WS_BORDER: u32 = 0x0080_0000;
pub(crate) const ES_AUTOHSCROLL: u32 = 0x0080;

/// Clear the common-dialog extended error (a canceled dialog is not an error).
pub(crate) fn state_comm_dlg_none(state: &mut WinApiState) {
    state.window_state().comm_dlg_extended_error = CDERR_NONE;
}

/// Resolve a dialog's owner: `hwndOwner` when it names a known window, else
/// the active window, else the first window.
pub(crate) fn resolve_dialog_owner(state: &mut WinApiState, owner_raw: u64) -> u64 {
    if owner_raw != 0 && is_known_window(state, owner_raw) {
        return owner_raw;
    }
    let active = state.window_state().active_window_handle.as_u64();
    if active != 0 {
        return active;
    }
    state
        .window_state()
        .windows
        .first()
        .map_or(0, |window| window.handle.as_u64())
}

// The `comdlg32::…` paths the dispatch table, the runtime stub encoder, and
// the user32 dialog/control paths resolve.
pub use color::handle_choose_color_a;
pub use file::{
    handle_comm_dlg_extended_error, handle_get_file_title_a, handle_get_file_title_w,
    handle_get_open_file_name_a, handle_get_open_file_name_w, handle_get_save_file_name_a,
    handle_get_save_file_name_w,
};
pub use find::{handle_find_text_w, handle_replace_text_w};
pub use font::{FONT_DLG_STRIKEOUT_ID, FONT_DLG_UNDERLINE_ID, handle_choose_font_w};
pub use print::{handle_page_setup_dlg_w, handle_print_dlg_w};

// pub(crate) re-exports: the user32 dialog/control paths consult these.
pub(crate) use file::complete_file_dialog;
pub(crate) use find::{handle_find_dialog_command, is_find_dialog_window};
pub(crate) use font::{complete_font_dialog, is_font_dialog_window};
