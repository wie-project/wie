//! Common dialog stubs (`comdlg32.dll`) for open/save file simulation.

use crate::guest_memory::{
    checked_field_address, read_i32 as read_guest_i32, read_u32 as read_guest_u32,
    read_u64 as read_guest_u64, write_u16 as write_guest_u16, write_u32 as write_guest_u32,
};
use crate::guest_string::{
    read_ansi_lossy, read_utf16_lossy, write_ansi_c_string, write_utf16_c_string,
};
use crate::handles::Hwnd;
use crate::state::{
    FileDialogFilter, FileDialogRequest, FileDialogSession, FindDialogSession, FontDialogSession,
    PendingNativeFileDialog,
};
use crate::user32::controls::{ControlClassKind, ControlState};
use crate::user32::{
    BS_DEFPUSHBUTTON, CommandPayload, CreateWindowRequest, GuestCallbackRequest, IDCANCEL, IDOK,
    QueuedWindowMessage, WS_CHILD, WS_CLIPCHILDREN, WS_TABSTOP, WS_VISIBLE, WinApiControlSignal,
    WindowClassIdentifier, create_window_record, deliver_focus_change, find_window,
    find_window_mut, is_known_window, window_client_size,
};
use crate::vfs::VolumeConfig;
use crate::{
    FileDialogPolicy, FontDialogPolicy, HandlerContext, OuterReturn, WinApiHandlerResult,
    WinApiState,
};
use anyhow::{Context, Result};

/// `OPENFILENAME` field offsets on Win64 (8-byte pointer alignment).
const OFN_LPSTR_FILE: u64 = 48;
const OFN_NMAX_FILE: u64 = 56;
const OFN_LPSTR_FILE_TITLE: u64 = 64;
const OFN_NMAX_FILE_TITLE: u64 = 72;
const OFN_FLAGS: u64 = 96;
const OFN_NFILE_OFFSET: u64 = 100;
const OFN_NFILE_EXTENSION: u64 = 102;
/// `OPENFILENAME.hwndOwner` — the dialog's owner window.
const OFN_HWND_OWNER: u64 = 8;
/// `OPENFILENAME.lpstrFilter` — the double-NUL-terminated `name\0pattern\0` pairs.
const OFN_LPSTR_FILTER: u64 = 24;
/// `OPENFILENAME.lpstrInitialDir` — the directory the dialog lists.
const OFN_LPSTR_INITIAL_DIR: u64 = 80;
/// `OPENFILENAME.lpstrDefExt` — appended when the typed name has no extension.
const OFN_LPSTR_DEF_EXT: u64 = 104;

/// No extended common-dialog error.
const CDERR_NONE: u32 = 0;

/// Control ids inside the file dialog (must differ from `IDOK`/`IDCANCEL`,
/// which the dialog-proc stub treats as close).
const FILE_DLG_EDIT_ID: u64 = 1000;
const FILE_DLG_LIST_ID: u64 = 1001;

/// File-dialog window size (pixels, classic 8×16 base units at 96 DPI).
const FILE_DLG_CX: i32 = 360;
const FILE_DLG_CY: i32 = 200;

// Winuser.h style bits the user32 module does not name.
const WS_BORDER: u32 = 0x0080_0000;
const ES_AUTOHSCROLL: u32 = 0x0080;

/// `FINDREPLACE` field offsets on Win64 (commdlg.h). The structure is a
/// UNICODE structure regardless of the A/W suffix of the creating API.
const FR_HWND_OWNER: u64 = 8;
const FR_FLAGS: u64 = 24;
const FR_LPSTR_FIND_WHAT: u64 = 32;
const FR_LPSTR_REPLACE_WITH: u64 = 40;
const FR_W_FIND_WHAT_LEN: u64 = 48;
const FR_W_REPLACE_WITH_LEN: u64 = 52;

/// `FINDREPLACE.Flags` bits (commdlg.h). `FR_DOWN` (0x1) is deliberately not
/// named: the dialog preserves the owner's down/up choice untouched.
const FR_WHOLEWORD: u32 = 0x0002;
const FR_MATCHCASE: u32 = 0x0004;
const FR_FINDNEXT: u32 = 0x0008;
const FR_REPLACE: u32 = 0x0010;
const FR_REPLACEALL: u32 = 0x0020;
const FR_DIALOGTERM: u32 = 0x0040;

/// Control ids inside the find/replace dialog.
///
/// Cancel reuses IDCANCEL's numeric value (2) so IsDialogMessage's Escape
/// handling (`DialogKeyAction::Command(IDCANCEL)`) closes the dialog without
/// a special case.
const FIND_DLG_EDIT_ID: u16 = 1200;
const FIND_DLG_REPLACE_EDIT_ID: u16 = 1201;
const FIND_DLG_MATCH_CASE_ID: u16 = 1202;
const FIND_DLG_WHOLE_WORD_ID: u16 = 1203;
const FIND_DLG_FIND_NEXT_ID: u16 = 1204;
const FIND_DLG_REPLACE_ID: u16 = 1205;
const FIND_DLG_REPLACE_ALL_ID: u16 = 1206;
const FIND_DLG_CANCEL_ID: u16 = 2;

/// Find-dialog window size (pixels). Replace mode is taller (an extra row).
const FIND_DLG_CX: i32 = 340;
const FIND_DLG_CY: i32 = 150;
const FIND_DLG_CY_REPLACE: i32 = 190;

/// Find-dialog layout (pixels, classic 8×16 base units at 96 DPI).
const FIND_DLG_LABEL_X: i32 = 8;
const FIND_DLG_FIELD_X: i32 = 92;
const FIND_DLG_FIELD_W: i32 = 232;
const FIND_DLG_EDIT_H: i32 = 22;
const FIND_DLG_BTN_X: i32 = 244;
const FIND_DLG_BTN_W: i32 = 88;
const FIND_DLG_BTN_H: i32 = 26;

/// The registered-message name `FindTextW`/`ReplaceTextW` report through.
const FINDMSGSTRING_NAME: &str = "findmsgstring";
/// One past the last id `RegisterWindowMessageA/W` may return (0xFFFF); the
/// mirror of `user32::misc::REGISTERED_MESSAGE_LIMIT` for the host-side
/// fallback registration in `findmsgstring_id`.
const REGISTERED_MESSAGE_LIMIT: u32 = 0x1_0000;

// ── ChooseFontW (commdlg.h / wingdi.h field offsets, Win64) ───────────────

/// `CHOOSEFONTW.hwndOwner` — the dialog's owner window.
const CF_HWND_OWNER: u64 = 0x08;
/// `CHOOSEFONTW.lpLogFont` — pointer to the `LOGFONTW` written back.
const CF_LP_LOG_FONT: u64 = 0x18;
/// `CHOOSEFONTW.iPointSize` — returned size in tenths of points.
const CF_IPOINT_SIZE: u64 = 0x20;
/// `CHOOSEFONTW.Flags` — `CF_*` bits (commdlg.h).
const CF_FLAGS: u64 = 0x24;
/// `CHOOSEFONTW.rgbColors` — returned text color.
const CF_RGB_COLORS: u64 = 0x28;

/// `LOGFONTW.lfHeight` (negative = character height in px).
const LF_HEIGHT: u64 = 0x00;
/// `LOGFONTW.lfItalic` .. `lfCharSet` — one byte each, packed into a u32:
/// italic (bit 0), underline (byte 1), strikeout (byte 2), charset (byte 3).
/// `lfWeight` (offset 0x10) and the other untouched fields are preserved by
/// the write-back (it only overwrites `lfHeight`, this word, and `lfFaceName`).
const LF_ITALIC_UNDERLINE_STRIKE_CHARSET: u64 = 0x14;
/// `LOGFONTW.lfFaceName` — `WCHAR[32]` (`LF_FACESIZE`).
const LF_FACE_NAME: u64 = 0x1C;

/// `CF_SCREENFONTS` (commdlg.h) — the dialog serves screen fonts.
const CF_SCREEN_FONTS: u32 = 0x1;

/// Control ids inside the font dialog (must differ from `IDOK`/`IDCANCEL`).
///
/// The two effects ids are the sentinel results the shared dialog-proc stub
/// (`encode_file_dialog_proc`) passes to `EndDialog`; the `EndDialog` handler
/// turns them into checkbox toggles instead of closing the dialog. They are
/// `pub` because the wie-runtime stub encoder embeds them in machine code.
pub const FONT_DLG_STRIKEOUT_ID: u64 = 1302;
pub const FONT_DLG_UNDERLINE_ID: u64 = 1303;
const FONT_DLG_FAMILY_LIST_ID: u64 = 1300;
const FONT_DLG_SIZE_LIST_ID: u64 = 1301;

/// Font-dialog window size (pixels).
///
/// The height leaves the Strikeout/Underline effects row (y=224, 20 px tall)
/// fully INSIDE the dialog with a bottom margin — the pre-fix 230 clipped
/// the buttons' bottom 14 px off the dialog (the reported "Strikeout and
/// Underline are outside the dialog" bug). 260 matches the classic Windows
/// font dialog's ~320×260 proportions.
const FONT_DLG_CX: i32 = 340;
const FONT_DLG_CY: i32 = 260;

/// Handles `comdlg32.dll!GetOpenFileNameA`.
pub fn handle_get_open_file_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_file_name(ctx, false, "GetOpenFileNameA")
}

/// Handles `comdlg32.dll!GetOpenFileNameW`.
pub fn handle_get_open_file_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_file_name(ctx, true, "GetOpenFileNameW")
}

/// Handles `comdlg32.dll!GetSaveFileNameA`.
pub fn handle_get_save_file_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_file_name(ctx, false, "GetSaveFileNameA")
}

/// Handles `comdlg32.dll!GetSaveFileNameW`.
pub fn handle_get_save_file_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_file_name(ctx, true, "GetSaveFileNameW")
}

/// Handles `comdlg32.dll!CommDlgExtendedError`.
pub fn handle_comm_dlg_extended_error(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = u64::from(state.window_state().comm_dlg_extended_error);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CommDlgExtendedError")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `comdlg32.dll!PrintDlgW` — simulated user-cancel.
///
/// Real printing is out of scope (the L6 plan): the dialog returns FALSE
/// exactly like a user who cancels. RNotepad's `DIALOG_FilePrint` treats a
/// FALSE return as "user canceled" and returns cleanly without touching
/// `hDC`, so this is a safe no-op. `hDevMode`/`hDevNames` are left untouched
/// (the `PRINTDLG` struct is not written).
pub fn handle_print_dlg_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    state_comm_dlg_none(&mut *ctx.state);
    tracing::info!(target: "wiegui", "PrintDlgW: printing is not emulated; cancelling");
    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from PrintDlgW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}

/// Handles `comdlg32.dll!PageSetupDlgW` — simulated user-cancel.
///
/// Like [`handle_print_dlg_w`], real page setup is out of scope: return FALSE
/// (canceled). RNotepad's `DIALOG_FilePageSetup` ignores the return value and
/// only copies `hDevMode`/`hDevNames` back out of the struct (both unchanged
/// here), so the call is a clean no-op.
pub fn handle_page_setup_dlg_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    state_comm_dlg_none(&mut *ctx.state);
    tracing::info!(target: "wiegui", "PageSetupDlgW: page setup is not emulated; cancelling");
    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from PageSetupDlgW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}

/// Clear the common-dialog extended error (a canceled dialog is not an error).
fn state_comm_dlg_none(state: &mut WinApiState) {
    state.window_state().comm_dlg_extended_error = CDERR_NONE;
}

/// Handles `comdlg32.dll!GetFileTitleA`.
pub fn handle_get_file_title_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_file_title(ctx, false, "GetFileTitleA")
}

/// Handles `comdlg32.dll!GetFileTitleW`.
pub fn handle_get_file_title_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_file_title(ctx, true, "GetFileTitleW")
}

/// GetFileTitle's documented return for an invalid file name / title buffer.
const GET_FILE_TITLE_ERR_INVALID: u64 = 1;

/// Upper bound for the path scan; real paths never approach this, and the
/// page-safe readers stop at the NUL far earlier.
const GET_FILE_TITLE_MAX_PATH: usize = 0x8000;

/// Handles `comdlg32.dll!GetFileTitleA/W` — copies the basename of a path
/// (everything after the last `\` or `/`) into a fixed-size guest buffer.
///
/// Return contract (MSDN): 0 = success, 1 = invalid file name, negative =
/// buffer too small (the absolute value is the required size in characters
/// including the terminating NUL).
fn handle_get_file_title(
    ctx: &mut HandlerContext<'_>,
    unicode: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let file_ptr = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let title_ptr = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let cch_raw = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    // cchTitle is a WORD (u16); clamp oversized guest values defensively.
    let cch_title = usize::from(u16::try_from(cch_raw).unwrap_or(u16::MAX));

    let return_value = if title_ptr == 0 {
        // Real GetFileTitle treats a NULL title buffer as an invalid parameter.
        tracing::warn!(api = api_name, "GetFileTitle called with NULL title buffer");
        GET_FILE_TITLE_ERR_INVALID
    } else {
        let path = if unicode {
            read_utf16_lossy(engine, file_ptr, GET_FILE_TITLE_MAX_PATH)
                .with_context(|| format!("failed to read {api_name} path"))?
        } else {
            read_ansi_lossy(engine, file_ptr, GET_FILE_TITLE_MAX_PATH)
                .with_context(|| format!("failed to read {api_name} path"))?
        };

        // Basename: everything after the last `\` or `/` (the unwrap_or
        // guards the empty-string case: no separator leaves file_start at 0,
        // and `get(0..)` on an empty string is None).
        let file_start = path
            .rfind(['\\', '/'])
            .map_or(0, |index| index.saturating_add(1));
        let basename = path.get(file_start..).unwrap_or("");

        if basename.is_empty() {
            // No basename: a genuinely empty path or a path ending in a
            // separator. Real GetFileTitle only treats the trailing-separator
            // form as an invalid file name; an empty path succeeds with an
            // empty title. The buffer is NUL-terminated either way.
            if unicode {
                write_utf16_c_string(engine, title_ptr, cch_title, "")?;
            } else {
                write_ansi_c_string(engine, title_ptr, cch_title, "")?;
            }
            if path.is_empty() {
                0
            } else {
                GET_FILE_TITLE_ERR_INVALID
            }
        } else {
            write_get_file_title(engine, title_ptr, cch_title, basename, unicode)?
        }
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Copies `basename` into the guest title buffer, returning the GetFileTitle
/// result code (0 on success, the negated required size on truncation).
///
/// The capacity accounting matches the character width: UTF-16 units for the
/// W variant, bytes for the A variant (A buffers are byte-counted).
fn write_get_file_title(
    engine: &mut dyn wie_cpu::CpuEngine,
    title_ptr: u64,
    cch_title: usize,
    basename: &str,
    unicode: bool,
) -> Result<u64> {
    let content_chars = if unicode {
        basename.encode_utf16().count()
    } else {
        basename.len()
    };
    let required = content_chars
        .checked_add(1)
        .context("GetFileTitle required size overflow")?;

    if cch_title == 0 || content_chars >= cch_title {
        // Buffer too small: write cch_title-1 characters plus NUL (the c-string
        // writers enforce the cap), and return -(required incl. NUL) as the
        // documented negative int. The write functions never overflow the
        // buffer even when the char-aligned prefix is longer than the cap.
        let truncated: String = basename.chars().take(cch_title.saturating_sub(1)).collect();
        if unicode {
            write_utf16_c_string(engine, title_ptr, cch_title, &truncated)?;
        } else {
            write_ansi_c_string(engine, title_ptr, cch_title, &truncated)?;
        }
        // Two's-complement of the required size in the low 32 bits (EAX) so
        // the guest sees the negative int return.
        let required_u32 =
            u32::try_from(required).context("GetFileTitle required size exceeds u32")?;
        Ok(u64::from(0_u32.wrapping_sub(required_u32)))
    } else {
        if unicode {
            write_utf16_c_string(engine, title_ptr, cch_title, basename)?;
        } else {
            write_ansi_c_string(engine, title_ptr, cch_title, basename)?;
        }
        Ok(0)
    }
}

/// Handles `comdlg32.dll!ChooseColorA` (simulates accept with black color).
pub fn handle_choose_color_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let choose_color_ptr = engine
        .read_rcx()
        .context("failed to read RCX for ChooseColorA")?;

    state.window_state().comm_dlg_extended_error = CDERR_NONE;

    // CHOOSECOLORA has rgbResult at offset 0x10 (after lStructSize + hwndOwner + hInstance).
    if choose_color_ptr != 0 {
        // Write default RGB color (black) into rgbResult field.
        let rgb_field = choose_color_ptr.wrapping_add(0x10);
        drop(crate::guest_memory::write_u32(
            engine, rgb_field, 0x00_00_00,
        ));
    }

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from ChooseColorA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// Handles `comdlg32.dll!ChooseFontW`.
///
/// Under [`FontDialogPolicy::Cancel`] (the default for headless runs) returns
/// FALSE like a user canceling. Under [`FontDialogPolicy::Interactive`] builds
/// the host font dialog and runs its in-guest modal loop (see
/// [`open_host_font_dialog`]).
pub fn handle_choose_font_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let cf_ptr = engine
        .read_rcx()
        .context("failed to read RCX for ChooseFontW")?;

    if cf_ptr == 0 {
        state_comm_dlg_none(state);
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from ChooseFontW")?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    // Clone the policy to avoid borrowing window_state() across the match.
    let policy = state.window_state().font_dialog_policy.clone();
    match policy {
        FontDialogPolicy::Cancel => {
            state_comm_dlg_none(state);
            tracing::debug!("ChooseFontW cancelled by policy");
            let return_address = engine
                .return_from_win64_api(0)
                .context("failed to return from ChooseFontW")?;
            Ok(WinApiHandlerResult {
                return_address,
                return_value: 0,
            })
        }
        FontDialogPolicy::Interactive => open_host_font_dialog(ctx, cf_ptr),
    }
}

fn handle_get_file_name(
    ctx: &mut HandlerContext<'_>,
    unicode: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let ofn_ptr = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    if ofn_ptr == 0 {
        state.window_state().comm_dlg_extended_error = CDERR_NONE;
        let return_address = engine
            .return_from_win64_api(0)
            .with_context(|| format!("failed to return from {api_name}"))?;

        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let file_buffer_ptr = read_guest_u64(
        engine,
        checked_field_address(ofn_ptr, OFN_LPSTR_FILE, "OPENFILENAME.lpstrFile"),
    )
    .with_context(|| format!("failed to read lpstrFile for {api_name}"))?;

    let max_file = read_guest_u32(
        engine,
        checked_field_address(ofn_ptr, OFN_NMAX_FILE, "OPENFILENAME.nMaxFile"),
    )
    .with_context(|| format!("failed to read nMaxFile for {api_name}"))?;

    let file_title_ptr = read_guest_u64(
        engine,
        checked_field_address(ofn_ptr, OFN_LPSTR_FILE_TITLE, "OPENFILENAME.lpstrFileTitle"),
    )
    .with_context(|| format!("failed to read lpstrFileTitle for {api_name}"))?;

    let max_file_title = read_guest_u32(
        engine,
        checked_field_address(ofn_ptr, OFN_NMAX_FILE_TITLE, "OPENFILENAME.nMaxFileTitle"),
    )
    .with_context(|| format!("failed to read nMaxFileTitle for {api_name}"))?;

    // Clone the policy to avoid borrowing state.window_state() across
    // mutable accesses inside the match arms.
    let policy = state.window_state().file_dialog_policy.clone();
    let buffer = OfnBuffer {
        ofn_ptr,
        file_buffer_ptr,
        max_file,
        file_title_ptr,
        max_file_title,
    };
    let return_value = match policy {
        FileDialogPolicy::Cancel => {
            state.window_state().comm_dlg_extended_error = CDERR_NONE;
            tracing::debug!(api = api_name, "file dialog cancelled by policy");
            0
        }

        FileDialogPolicy::Accept { path } => {
            if buffer.file_buffer_ptr == 0 || buffer.max_file == 0 {
                state.window_state().comm_dlg_extended_error = CDERR_NONE;
                tracing::warn!(
                    api = api_name,
                    "file dialog accept policy but lpstrFile/nMaxFile invalid"
                );
                0
            } else {
                write_selected_path(
                    engine,
                    &SelectedPathWrite {
                        ofn_ptr: buffer.ofn_ptr,
                        file_buffer_ptr: buffer.file_buffer_ptr,
                        max_file: buffer.max_file,
                        file_title_ptr: buffer.file_title_ptr,
                        max_file_title: buffer.max_file_title,
                        path: &path,
                        unicode,
                    },
                )
                .with_context(|| format!("failed to write selected path for {api_name}"))?;

                state.window_state().comm_dlg_extended_error = CDERR_NONE;
                state.window_state().last_file_dialog_path = Some(path.clone());

                tracing::info!(api = api_name, %path, unicode, "file dialog accepted");
                1
            }
        }

        // No scripted decision: show the host file dialog. When the GUI
        // presenter registered a native file-dialog bridge (macOS panel via
        // rfd), use it — the guest thread blocks inside the bridge until the
        // user picks, exactly like the MessageBox bridge. Without a bridge
        // (headless/trace sessions), build the in-app "FileDialog" window and
        // run its modal message loop in-guest (the DialogBoxParam pattern).
        FileDialogPolicy::Interactive => {
            let bridge_registered = state
                .try_window_state()
                .is_some_and(|window_state| window_state.file_dialog_bridge.is_some());
            if bridge_registered {
                return open_host_file_dialog_via_bridge(ctx, api_name, unicode, &buffer);
            }
            return open_host_file_dialog(ctx, api_name, unicode, &buffer);
        }
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// The `OPENFILENAME` buffer fields both the policy and the interactive flows
/// write through (the chosen path lands in `lpstrFile`, `lpstrFileTitle`).
struct OfnBuffer {
    ofn_ptr: u64,
    file_buffer_ptr: u64,
    max_file: u32,
    file_title_ptr: u64,
    max_file_title: u32,
}

pub(crate) struct SelectedPathWrite<'a> {
    ofn_ptr: u64,
    file_buffer_ptr: u64,
    max_file: u32,
    file_title_ptr: u64,
    max_file_title: u32,
    path: &'a str,
    unicode: bool,
}

/// Reads an `OPENFILENAME` string field (UTF-16 for the W variant, ANSI for A).
fn read_ofn_string(
    engine: &mut dyn wie_cpu::CpuEngine,
    ptr: u64,
    unicode: bool,
    api_name: &str,
) -> Result<String> {
    if unicode {
        read_utf16_lossy(engine, ptr, 1024)
            .with_context(|| format!("failed to read {api_name} string"))
    } else {
        read_ansi_lossy(engine, ptr, 1024)
            .with_context(|| format!("failed to read {api_name} string"))
    }
}

/// The guest's current directory as a Windows path (falls back to `C:\`).
fn current_guest_directory(state: &WinApiState) -> String {
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide)
        .trim_end_matches('\0')
        .to_owned();
    if cwd.is_empty() {
        r"C:\".to_owned()
    } else {
        cwd
    }
}

/// The directory part of a Windows path (empty when there is no separator).
fn directory_of(path: &str) -> &str {
    path.rfind(['\\', '/'])
        .map_or("", |index| path.get(..index).unwrap_or(""))
}

/// The basename of a Windows path (the whole path when there is no separator).
fn basename_of(path: &str) -> &str {
    path.rfind(['\\', '/']).map_or(path, |index| {
        path.get(index.saturating_add(1)..).unwrap_or(path)
    })
}

/// Resolve the file dialog's owner: the `OPENFILENAME.hwndOwner` when it names
/// a known window, else the active window, else the first window.
fn resolve_dialog_owner(state: &mut WinApiState, owner_raw: u64) -> u64 {
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

/// Directory entry names (files and directories) of `guest_dir`, resolved
/// through the host volume mapping and confined to a guest volume.
///
/// `..` may ascend within a volume (`C:\App\..` lists the bottle root) but
/// never above its root (`C:\..`), and only C: (bottle) / D: (bridge, when
/// configured) are listable. Entries whose host path does not map back into
/// a guest volume (e.g. a symlink pointing outside the bottle) are hidden.
/// Empty when the directory is unmapped, escapes the volumes, or unreadable.
fn list_directory(state: &WinApiState, guest_dir: &str) -> Vec<String> {
    let volumes = &state.file_io.volumes;
    let Some(confined) = crate::vfs::confine_guest_path(volumes, guest_dir) else {
        return Vec::new();
    };
    let Some(map) = crate::vfs::guest_path_to_host(volumes, &confined) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&map.host) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        // A directory entry that maps to no guest path is not selectable
        // (the guest filesystem cannot see it), so it is not listed.
        .filter(|entry| crate::vfs::host_path_to_guest(volumes, &entry.path()).is_some())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    // Cap the listing so a huge directory cannot starve the paint cycle.
    names.truncate(1024);
    names
}

/// The dialog's starting directory, always confined to a guest volume.
///
/// Resolution order: `lpstrInitialDir` when it is guest-visible, else the
/// `lpstrFile` directory, else the guest cwd, else the bottle root (`C:\`).
/// Any candidate that is not inside a guest volume (an unmapped drive, a
/// host path, `..` above a volume root) falls through, so the dialog never
/// opens browsing a directory the guest filesystem cannot see.
fn resolve_initial_dir(
    volumes: &VolumeConfig,
    guest_cwd: &str,
    caller_dir: &str,
    file_dir: &str,
) -> String {
    for candidate in [caller_dir, file_dir] {
        if !candidate.is_empty() && crate::vfs::confine_guest_path(volumes, candidate).is_some() {
            return candidate.to_owned();
        }
    }
    if crate::vfs::confine_guest_path(volumes, guest_cwd).is_some() {
        return guest_cwd.to_owned();
    }
    r"C:\".to_owned()
}

/// Whether `path` is an absolute Windows path (drive letter or UNC separator).
fn is_absolute_windows_path(path: &str) -> bool {
    path.starts_with("\\\\")
        || path.starts_with("//")
        || path.get(..2).is_some_and(|prefix| {
            prefix.ends_with(':')
                && prefix
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
        })
}

/// Appends `def_ext` (without its dot) when the path's basename has no dot and
/// the path does not end in a separator (a directory selection).
fn apply_default_extension(path: &str, def_ext: Option<&str>) -> String {
    let Some(def_ext) = def_ext else {
        return path.to_owned();
    };
    let def_ext = def_ext.trim_matches('.');
    if def_ext.is_empty() || path.ends_with(['\\', '/']) {
        return path.to_owned();
    }
    let basename = path.rsplit(['\\', '/']).next().unwrap_or(path);
    if basename.contains('.') {
        return path.to_owned();
    }
    format!("{path}.{def_ext}")
}

/// Builds the guest path the dialog returns: an absolute typed path is used
/// as-is; a bare name joins the dialog's directory; `lpstrDefExt` appends an
/// extension when the final name has no dot. Empty input → empty (cancel).
fn finalize_guest_path(session: &FileDialogSession, edit_text: &str) -> String {
    let typed = edit_text.trim();
    if typed.is_empty() {
        return String::new();
    }
    let joined = if session.initial_dir.is_empty() || is_absolute_windows_path(typed) {
        typed.to_owned()
    } else {
        format!(
            "{}\\{typed}",
            session.initial_dir.trim_end_matches(['\\', '/'])
        )
    };
    apply_default_extension(&joined, session.default_extension.as_deref())
}

/// `EndDialog` write-back for a closing file dialog: writes the chosen path
/// into the `OPENFILENAME` buffer (OK) and clears the session. Returns the
/// effective result the modal loop returns — 0 when the dialog was cancelled
/// or the edit held no path.
pub(crate) fn complete_file_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    dialog_hwnd: u64,
    result: u64,
) -> Result<u64> {
    let Some(session) = state.window_state().file_dialog.clone() else {
        return Ok(result);
    };
    if session.dialog_hwnd != dialog_hwnd {
        return Ok(result);
    }
    let effective = if result == 0 {
        0
    } else {
        let edit_text = find_window(state, session.edit_hwnd)
            .map_or_else(String::new, |window| window.control_text.clone());
        let path = finalize_guest_path(&session, &edit_text);
        // Confinement: the dialog only accepts paths inside a guest volume
        // (the C: bottle or the optional D: bridge) — the LISTBOX never
        // lists out-of-volume entries, so an Accept must not return one
        // either. A typed path that escapes the volumes — `..` above a
        // volume root, an unmapped drive, any host path — is refused like a
        // cancel (FALSE, buffer untouched): the guest filesystem cannot see
        // it, so accepting it would hand back a file that does not exist.
        // `..` within a volume is collapsed and the canonical path written
        // back, matching what the guest can actually open.
        let accepted = if path.is_empty() {
            None
        } else {
            crate::vfs::confine_guest_path(&state.file_io.volumes, &path)
        };
        match accepted {
            None => {
                tracing::info!(%path, "file dialog refused out-of-bottle selection");
                0
            }
            Some(path) => {
                write_selected_path(
                    engine,
                    &SelectedPathWrite {
                        ofn_ptr: session.ofn_ptr,
                        file_buffer_ptr: session.file_buffer_ptr,
                        max_file: session.max_file,
                        file_title_ptr: session.file_title_ptr,
                        max_file_title: session.max_file_title,
                        path: &path,
                        unicode: session.unicode,
                    },
                )
                .context("failed to write selected file-dialog path")?;
                state.window_state().last_file_dialog_path = Some(path.clone());
                tracing::info!(%path, unicode = session.unicode, "file dialog accepted");
                1
            }
        }
    };
    state.window_state().file_dialog = None;
    Ok(effective)
}

/// `FileDialogPolicy::Interactive`: build the host file dialog (a
/// "FileDialog"-class window with EDIT / LISTBOX / OK / Cancel controls) and
/// ask the runtime to run its modal message loop in-guest.
///
/// The dialog window carries the planted file-dialog proc stub as its
/// `dialog_proc`, so `WM_COMMAND(IDOK/IDCANCEL)` / `WM_CLOSE` bridge into the
/// stub, which calls `EndDialog`; the extended `EndDialog` handler performs the
/// `OPENFILENAME` write-back (see [`complete_file_dialog`]) and posts the
/// `WM_QUIT` the loop exits on. The runtime resumes the guest with the modal
/// loop's return — `GetOpenFileName`'s TRUE/FALSE.
fn open_host_file_dialog(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
    buffer: &OfnBuffer,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let OfnBuffer {
        ofn_ptr,
        file_buffer_ptr,
        max_file,
        file_title_ptr,
        max_file_title,
    } = *buffer;

    // Without the planted loop/proc bodies (headless/trace sessions) or with a
    // file dialog already open, fall back to Cancel: a guest must never hang.
    let loop_va = state.window_state().file_dialog_loop_va;
    let proc_va = state.window_state().file_dialog_proc_va;
    if loop_va == 0 || proc_va == 0 || state.window_state().file_dialog.is_some() {
        state.window_state().comm_dlg_extended_error = CDERR_NONE;
        tracing::warn!(
            api = api_name,
            "interactive file dialog unavailable; cancelling"
        );
        let return_address = engine
            .return_from_win64_api(0)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let owner_raw = read_guest_u64(
        engine,
        checked_field_address(ofn_ptr, OFN_HWND_OWNER, "OPENFILENAME.hwndOwner"),
    )
    .with_context(|| format!("failed to read hwndOwner for {api_name}"))?;
    let initial_dir_ptr = read_guest_u64(
        engine,
        checked_field_address(
            ofn_ptr,
            OFN_LPSTR_INITIAL_DIR,
            "OPENFILENAME.lpstrInitialDir",
        ),
    )
    .with_context(|| format!("failed to read lpstrInitialDir for {api_name}"))?;
    let def_ext_ptr = read_guest_u64(
        engine,
        checked_field_address(ofn_ptr, OFN_LPSTR_DEF_EXT, "OPENFILENAME.lpstrDefExt"),
    )
    .with_context(|| format!("failed to read lpstrDefExt for {api_name}"))?;

    let initial_file = read_ofn_string(engine, file_buffer_ptr, unicode, api_name)?;
    let caller_initial_dir = if initial_dir_ptr != 0 {
        read_ofn_string(engine, initial_dir_ptr, unicode, api_name)?
    } else {
        String::new()
    };
    // Confined resolution: lpstrInitialDir wins when it is guest-visible,
    // then the lpstrFile directory, then the guest cwd, else the bottle root.
    let initial_dir = resolve_initial_dir(
        &state.file_io.volumes,
        &current_guest_directory(state),
        &caller_initial_dir,
        directory_of(&initial_file),
    );
    let default_extension = if def_ext_ptr != 0 {
        Some(read_ofn_string(engine, def_ext_ptr, unicode, api_name)?)
    } else {
        None
    };

    let is_save = api_name.contains("Save");
    let parent_handle = resolve_dialog_owner(state, owner_raw);
    let (owner_w, owner_h) = window_client_size(state, parent_handle);
    let (dialog_x, dialog_y) =
        if parent_handle != 0 && owner_w >= FILE_DLG_CX && owner_h >= FILE_DLG_CY {
            (
                owner_w.saturating_sub(FILE_DLG_CX).saturating_div(2),
                owner_h.saturating_sub(FILE_DLG_CY).saturating_div(2),
            )
        } else {
            (0, 0)
        };

    // The dialog window: parented to the owner (composites into its surface)
    // and carrying the file-dialog proc stub as its dialog proc.
    let (dialog_hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name("FileDialog".to_owned()),
            title: if is_save {
                "Save As".to_owned()
            } else {
                "Open".to_owned()
            },
            style: WS_VISIBLE | WS_CLIPCHILDREN,
            extended_style: 0,
            parent_handle,
            menu_handle: 0,
            instance_handle: 0,
            x: dialog_x,
            y: dialog_y,
            width: FILE_DLG_CX,
            height: FILE_DLG_CY,
        },
        unicode,
    )?;
    if dialog_hwnd == 0 {
        state.window_state().comm_dlg_extended_error = CDERR_NONE;
        let return_address = engine
            .return_from_win64_api(0)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    if let Some(window) = find_window_mut(state, dialog_hwnd) {
        window.dialog_proc = proc_va;
        window.dialog_unicode = unicode;
        window.client_rect = (0, 0, FILE_DLG_CX, FILE_DLG_CY);
    }

    // Single-line EDIT for the typed path, seeded with the lpstrFile basename.
    let edit_text = basename_of(&initial_file).to_owned();
    let (edit_hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Atom(0x0081),
            title: edit_text,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
            extended_style: 0,
            parent_handle: dialog_hwnd,
            menu_handle: FILE_DLG_EDIT_ID,
            instance_handle: 0,
            x: 8,
            y: 8,
            width: FILE_DLG_CX.saturating_sub(16),
            height: 22,
        },
        unicode,
    )?;

    // LISTBOX of the directory entries.
    let list_width = FILE_DLG_CX.saturating_sub(16);
    let (list_hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Atom(0x0083),
            title: String::new(),
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            extended_style: 0,
            parent_handle: dialog_hwnd,
            menu_handle: FILE_DLG_LIST_ID,
            instance_handle: 0,
            x: 8,
            y: 36,
            width: list_width,
            height: FILE_DLG_CY.saturating_sub(84),
        },
        unicode,
    )?;

    // OK (the dialog's Enter default) and Cancel.
    let (ok_hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Atom(0x0080),
            title: "OK".to_owned(),
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON,
            extended_style: 0,
            parent_handle: dialog_hwnd,
            menu_handle: IDOK,
            instance_handle: 0,
            x: FILE_DLG_CX.saturating_sub(176),
            y: FILE_DLG_CY.saturating_sub(40),
            width: 80,
            height: 28,
        },
        unicode,
    )?;
    let (cancel_hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Atom(0x0080),
            title: "Cancel".to_owned(),
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            extended_style: 0,
            parent_handle: dialog_hwnd,
            menu_handle: IDCANCEL,
            instance_handle: 0,
            x: FILE_DLG_CX.saturating_sub(88),
            y: FILE_DLG_CY.saturating_sub(40),
            width: 80,
            height: 28,
        },
        unicode,
    )?;

    for hwnd in [edit_hwnd, list_hwnd, ok_hwnd, cancel_hwnd] {
        if let Some(window) = find_window_mut(state, hwnd) {
            window.client_rect = (0, 0, window.width, window.height);
        }
    }
    // The OK button is the dialog's Enter default (BS_DEFPUSHBUTTON).
    state
        .window_state()
        .control_states
        .entry(Hwnd::from(ok_hwnd))
        .or_insert_with(|| ControlClassKind::Button.new_state())
        .set_default_push(true);
    // Populate the LISTBOX with the current directory's entries.
    let listing = list_directory(state, &initial_dir);
    state
        .window_state()
        .control_states
        .entry(Hwnd::from(list_hwnd))
        .or_insert_with(|| ControlClassKind::ListBox.new_state());
    if let Some(ControlState::ListBox { items, .. }) = state
        .window_state()
        .control_states
        .get_mut(&Hwnd::from(list_hwnd))
    {
        *items = listing;
    }

    // The dialog is modal: an empty GetMessage must yield, not synthesize the
    // regression-mode WM_QUIT, and the dialog takes activation.
    {
        let mut queue = state.lock_message_queue();
        queue.dialog_depth = queue.dialog_depth.saturating_add(1);
    }
    state.window_state().active_window_handle = Hwnd::from(dialog_hwnd);

    // Record the session for the EndDialog write-back.
    state.window_state().file_dialog = Some(FileDialogSession {
        dialog_hwnd,
        edit_hwnd,
        ofn_ptr,
        file_buffer_ptr,
        max_file,
        file_title_ptr,
        max_file_title,
        unicode,
        initial_dir,
        default_extension,
    });

    // Initial keyboard focus: the path EDIT (host-side WM_SETFOCUS).
    state.window_state().focus_window_handle = Hwnd::from(edit_hwnd);
    let _unused = deliver_focus_change(state, engine, 0, edit_hwnd, OuterReturn::Fixed(edit_hwnd))?;

    // Mark the whole subtree invalidated so the first empty GetMessage paints
    // the dialog face + controls.
    for hwnd in [dialog_hwnd, edit_hwnd, list_hwnd, ok_hwnd, cancel_hwnd] {
        if let Some(window) = find_window_mut(state, hwnd) {
            window.invalidated = true;
        }
    }

    tracing::info!(
        target: "wiegui",
        api = api_name,
        hwnd = dialog_hwnd,
        parent = parent_handle,
        unicode,
        "file dialog opened"
    );

    // Run the planted modal loop in-guest; its return value (the dialog result
    // slot: 1 on OK, 0 on cancel) becomes the GetOpenFileName return.
    Err(WinApiControlSignal::GuestCallbackRequested {
        request: GuestCallbackRequest {
            callback_address: loop_va,
            window_handle: dialog_hwnd,
            message: 0,
            word_parameter: 0,
            long_parameter: 0,
            unicode: false,
            outer_return: OuterReturn::Passthrough,
        },
    }
    .into())
}

/// `FileDialogPolicy::Interactive` with a host bridge registered: show the
/// native panel (macOS NSOpenPanel/NSSavePanel via rfd, behind the
/// `GuestHandle::set_file_dialog_bridge` seam) and return its pick.
///
/// The handler runs in TWO entries, split around the bridge:
///
/// - **First entry** (state lock held): read the guest's `OPENFILENAME`,
///   record everything the write-back needs in
///   [`PendingNativeFileDialog`], and return
///   [`WinApiControlSignal::FileDialogBridgeRequested`]. The runtime then
///   DROPS the shared state lock and runs the bridge on the guest thread —
///   the native panel blocks the main thread for the whole session, and the
///   winit event loop needs the SAME lock to service frame/user events while
///   the panel is up, so holding it across the bridge would deadlock into the
///   macOS beachball.
/// - **Re-entry** (the engine re-executes the fake API after the bridge
///   returns): take the pending record, write its pick back into the guest
///   `OPENFILENAME` buffer, and return the dialog result.
///
/// The picked HOST path is confined to a guest volume at accept: a pick the
/// guest filesystem cannot see (the user browsed outside the bottle via the
/// panel's sidebar) cancels like a user pressing Cancel — FALSE, `lpstrFile`
/// untouched, with a `tracing::warn`.
fn open_host_file_dialog_via_bridge(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    unicode: bool,
    buffer: &OfnBuffer,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    // Re-entry: the runtime recorded the bridge's pick; write it back.
    if let Some(pending) = state.window_state().pending_native_file_dialog.take() {
        return finish_native_file_dialog(engine, state, api_name, pending);
    }

    let initial_file = read_ofn_string(engine, buffer.file_buffer_ptr, unicode, api_name)?;
    let filter_ptr = read_guest_u64(
        engine,
        checked_field_address(buffer.ofn_ptr, OFN_LPSTR_FILTER, "OPENFILENAME.lpstrFilter"),
    )
    .with_context(|| format!("failed to read lpstrFilter for {api_name}"))?;

    // The native panel starts at the BOTTLE ROOT (`{root}/drive_c`) — the
    // user asked for the bottle root, not the guest cwd (the process
    // identity pins it to `C:\App`). The in-app emulated dialog keeps its
    // own precedence; only the bridge path changes. Without a bottle root,
    // fall back to the confined resolution (the same precedence the in-app
    // dialog uses).
    let initial_host_dir = state
        .file_io
        .volumes
        .bottle_root
        .as_ref()
        .map(|root| root.join("drive_c"))
        .or_else(|| {
            let initial_dir_ptr = read_guest_u64(
                engine,
                checked_field_address(
                    buffer.ofn_ptr,
                    OFN_LPSTR_INITIAL_DIR,
                    "OPENFILENAME.lpstrInitialDir",
                ),
            )
            .unwrap_or(0);
            let caller_initial_dir = if initial_dir_ptr != 0 {
                read_ofn_string(engine, initial_dir_ptr, unicode, api_name).unwrap_or_default()
            } else {
                String::new()
            };
            let initial_dir = resolve_initial_dir(
                &state.file_io.volumes,
                &current_guest_directory(state),
                &caller_initial_dir,
                directory_of(&initial_file),
            );
            crate::vfs::guest_path_to_host(&state.file_io.volumes, &initial_dir)
                .map(|mapping| mapping.host)
        });
    let default_file_name = {
        let basename = basename_of(&initial_file);
        (!basename.is_empty()).then(|| basename.to_owned())
    };
    let is_save = api_name.contains("Save");
    let filters = if filter_ptr != 0 {
        let components = read_ofn_filter_components(engine, filter_ptr, unicode)?;
        parse_ofn_filter(&components)
    } else {
        Vec::new()
    };

    let request = FileDialogRequest {
        initial_host_dir,
        default_file_name,
        is_save,
        filters,
    };

    // Record the write-back metadata and hand the request to the runtime: it
    // drops the shared state lock, runs the bridge on this guest thread, and
    // the engine's re-execution of the fake API re-enters this handler (see
    // `PendingNativeFileDialog`).
    state.window_state().pending_native_file_dialog = Some(PendingNativeFileDialog {
        ofn_ptr: buffer.ofn_ptr,
        file_buffer_ptr: buffer.file_buffer_ptr,
        max_file: buffer.max_file,
        file_title_ptr: buffer.file_title_ptr,
        max_file_title: buffer.max_file_title,
        unicode,
        pick: None,
    });

    Err(WinApiControlSignal::FileDialogBridgeRequested { request }.into())
}

/// Write the native panel's pick back into the guest `OPENFILENAME` buffer.
///
/// Runs on the handler's re-entry (after the runtime ran the bridge WITHOUT
/// the shared state lock). `None` pick = the user cancelled (or the bridge
/// vanished mid-call — a racing teardown must not hang the guest); an
/// out-of-bottle pick is refused like a cancel — FALSE, `lpstrFile` untouched.
fn finish_native_file_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    api_name: &str,
    pending: PendingNativeFileDialog,
) -> Result<WinApiHandlerResult> {
    let Some(pick) = pending.pick else {
        state.window_state().comm_dlg_extended_error = CDERR_NONE;
        tracing::info!(api = api_name, "native file dialog cancelled");
        return file_dialog_return(engine, api_name, 0);
    };

    // Confinement at accept: the picked HOST path must map into a guest volume
    // (the C: bottle or the optional D: bridge). `host_path_to_guest` IS the
    // confinement — it returns None for anything outside both volumes, so an
    // out-of-bottle pick is refused like a cancel (FALSE, buffer untouched).
    let Some(guest_path) = crate::vfs::host_path_to_guest(&state.file_io.volumes, &pick.host_path)
    else {
        state.window_state().comm_dlg_extended_error = CDERR_NONE;
        tracing::warn!(
            api = api_name,
            host = %pick.host_path.display(),
            "native file dialog pick outside the bottle; cancelling"
        );
        return file_dialog_return(engine, api_name, 0);
    };

    if pending.file_buffer_ptr == 0 || pending.max_file == 0 {
        state.window_state().comm_dlg_extended_error = CDERR_NONE;
        tracing::warn!(
            api = api_name,
            "file dialog bridge accepted but lpstrFile/nMaxFile invalid"
        );
        return file_dialog_return(engine, api_name, 0);
    }

    // The shared write-back (`write_selected_path` — the same machinery the
    // policy + EndDialog flows use): copy the guest path into `lpstrFile` (+
    // `lpstrFileTitle`) and store `nFileOffset` / `nFileExtension`.
    write_selected_path(
        engine,
        &SelectedPathWrite {
            ofn_ptr: pending.ofn_ptr,
            file_buffer_ptr: pending.file_buffer_ptr,
            max_file: pending.max_file,
            file_title_ptr: pending.file_title_ptr,
            max_file_title: pending.max_file_title,
            path: &guest_path,
            unicode: pending.unicode,
        },
    )
    .with_context(|| format!("failed to write selected path for {api_name}"))?;

    state.window_state().comm_dlg_extended_error = CDERR_NONE;
    state.window_state().last_file_dialog_path = Some(guest_path.clone());
    tracing::info!(api = api_name, %guest_path, unicode = pending.unicode, "native file dialog accepted");
    file_dialog_return(engine, api_name, 1)
}

/// Build a `WinApiHandlerResult` that returns `value` from the API.
fn file_dialog_return(
    engine: &mut dyn wie_cpu::CpuEngine,
    api_name: &str,
    value: u64,
) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .with_context(|| format!("failed to return from {api_name}"))?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// Cap for a guest `lpstrFilter` multi-string scan (in characters/units).
/// Real filters are a few dozen characters; this bounds a hostile guest.
const OFN_FILTER_MAX_CHARS: usize = 256;

/// Read the payload of a double-NUL-terminated guest multi-string — the
/// `lpstrFilter` shape (`name\0pattern\0…\0\0`) — as the NUL-separated
/// components before the final double NUL. `wide` reads UTF-16 units (W
/// variants), else bytes (A variants).
///
/// Page-safe like the `guest_string` readers: each bulk read stops at the
/// 4 KiB page boundary so an unmapped tail page cannot fail a valid prefix.
fn read_ofn_filter_components(
    engine: &mut dyn wie_cpu::CpuEngine,
    ptr: u64,
    wide: bool,
) -> Result<Vec<String>> {
    if ptr == 0 {
        return Ok(Vec::new());
    }
    // Accumulate raw units/bytes, stopping at TWO consecutive NULs.
    let mut payload: Vec<u8> = Vec::with_capacity(64);
    let mut scratch = [0_u8; 4096];
    let mut cursor = ptr;
    let mut remaining = OFN_FILTER_MAX_CHARS;
    let mut end = false;
    // Carried across page reads so a terminator straddling a 4 KiB boundary
    // (a NUL as the last unit of one page, a NUL as the first of the next)
    // is still detected.
    let mut prev_zero = false;
    while remaining > 0 && !end {
        let byte_budget = if wide {
            // Keep the budget even so a UTF-16 unit is never split.
            remaining.saturating_mul(2).min(scratch.len() & !1)
        } else {
            remaining.min(scratch.len())
        };
        if byte_budget == 0 {
            break;
        }
        // Stay inside the current 4 KiB page (`mem_read` needs a valid range).
        let page_end = (cursor | 4095).wrapping_add(1);
        let in_page = page_end
            .saturating_sub(cursor)
            .min(u64::try_from(byte_budget).unwrap_or(u64::MAX));
        let mut take = usize::try_from(in_page)
            .unwrap_or(byte_budget)
            .min(byte_budget);
        if wide {
            take &= !1;
        }
        if take == 0 {
            break;
        }
        let slice = scratch.get_mut(..take).context("lpstrFilter read slice")?;
        engine
            .mem_read(cursor, slice)
            .context("failed to read lpstrFilter")?;

        if wide {
            for pair in slice.as_chunks::<2>().0 {
                let lo = *pair.first().unwrap_or(&0);
                let hi = *pair.get(1).unwrap_or(&0);
                let unit = u16::from_le_bytes([lo, hi]);
                if unit == 0 {
                    if prev_zero {
                        // Two consecutive NUL units = the multi-string end.
                        end = true;
                        break;
                    }
                    prev_zero = true;
                    // Keep the single-NUL component separator in the payload.
                    payload.extend_from_slice(&[0, 0]);
                } else {
                    prev_zero = false;
                    payload.extend_from_slice(pair);
                }
            }
        } else {
            for &byte in slice.iter() {
                if byte == 0 {
                    if prev_zero {
                        end = true;
                        break;
                    }
                    prev_zero = true;
                    // Keep the single-NUL component separator in the payload.
                    payload.push(0);
                } else {
                    prev_zero = false;
                    payload.push(byte);
                }
            }
        }
        let consumed = if wide { take / 2 } else { take };
        remaining = remaining.saturating_sub(consumed);
        cursor = cursor.wrapping_add(u64::try_from(take).unwrap_or(0));
    }

    if wide {
        let units: Vec<u16> = payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let lo = *pair.first().unwrap_or(&0);
                let hi = *pair.get(1).unwrap_or(&0);
                u16::from_le_bytes([lo, hi])
            })
            .collect();
        Ok(String::from_utf16_lossy(&units)
            .split('\0')
            .filter(|component| !component.is_empty())
            .map(str::to_owned)
            .collect())
    } else {
        Ok(crate::vfs::decode_ansi_utf8_first(&payload)
            .split('\0')
            .filter(|component| !component.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

/// Parse `lpstrFilter` components into native filters (best-effort).
///
/// A filter group survives only when every pattern in it is a simple `*.ext`
/// glob (`*.*` catch-alls, wildcard names, and empty names are dropped) — a
/// wrong native filter grays out every file on macOS, which is worse than
/// showing all files. Semicolon-separated simple globs (`*.rs;*.toml`) stay
/// one group. Returns an empty list when nothing survives (the panel shows
/// everything).
fn parse_ofn_filter(components: &[String]) -> Vec<FileDialogFilter> {
    let mut filters = Vec::new();
    // Components arrive as `name, patterns` pairs.
    for pair in components.chunks(2) {
        let [name, patterns] = pair else {
            // A trailing odd component (the multi-string ended mid-pair).
            break;
        };
        let patterns: Vec<String> = patterns
            .split(';')
            .map(str::trim)
            .filter(|pattern| is_simple_filter_glob(pattern))
            .map(str::to_owned)
            .collect();
        if name.is_empty() || patterns.is_empty() {
            continue;
        }
        filters.push(FileDialogFilter {
            name: name.clone(),
            patterns,
        });
    }
    filters
}

/// Whether `pattern` is a plain `*.ext` glob (`*.txt`): exactly one leading
/// `*.` and a non-empty extension of word characters. Anything else — `*.*`,
/// `*.tar.gz`, bare names, wildcards in the extension — is not a safe native
/// filter and is dropped.
fn is_simple_filter_glob(pattern: &str) -> bool {
    let Some(extension) = pattern.strip_prefix("*.") else {
        return false;
    };
    !extension.is_empty() && extension.chars().all(|c| c.is_ascii_alphanumeric())
}

/// `GetOpenFileName`/`GetSaveFileName` shared write-back (policy + interactive
/// flows): copies the selected path into `lpstrFile` (+ `lpstrFileTitle`) and
/// stores `nFileOffset` / `nFileExtension` in the `OPENFILENAME` struct.
pub(crate) fn write_selected_path(
    engine: &mut dyn wie_cpu::CpuEngine,
    request: &SelectedPathWrite<'_>,
) -> Result<()> {
    let max_file_chars =
        usize::try_from(request.max_file).context("OPENFILENAME.nMaxFile does not fit usize")?;

    if request.unicode {
        write_utf16_c_string(
            engine,
            request.file_buffer_ptr,
            max_file_chars,
            request.path,
        )
        .context("failed to write Unicode lpstrFile")?;
    } else {
        write_ansi_c_string(
            engine,
            request.file_buffer_ptr,
            max_file_chars,
            request.path,
        )
        .context("failed to write ANSI lpstrFile")?;
    }

    let (file_name, file_offset, extension_offset) = split_path_components(request.path);

    if request.file_title_ptr != 0 && request.max_file_title != 0 {
        let max_title_chars = usize::try_from(request.max_file_title)
            .context("OPENFILENAME.nMaxFileTitle does not fit usize")?;

        if request.unicode {
            write_utf16_c_string(engine, request.file_title_ptr, max_title_chars, file_name)
                .context("failed to write Unicode lpstrFileTitle")?;
        } else {
            write_ansi_c_string(engine, request.file_title_ptr, max_title_chars, file_name)
                .context("failed to write ANSI lpstrFileTitle")?;
        }
    }

    write_guest_u16(
        engine,
        checked_field_address(
            request.ofn_ptr,
            OFN_NFILE_OFFSET,
            "OPENFILENAME.nFileOffset",
        ),
        file_offset,
    )?;

    write_guest_u16(
        engine,
        checked_field_address(
            request.ofn_ptr,
            OFN_NFILE_EXTENSION,
            "OPENFILENAME.nFileExtension",
        ),
        extension_offset,
    )?;

    // Leave Flags as the guest provided them; only offsets/title/path are updated.
    let _flags = read_guest_u32(
        engine,
        checked_field_address(request.ofn_ptr, OFN_FLAGS, "OPENFILENAME.Flags"),
    )?;

    Ok(())
}

/// Returns `(file_name, nFileOffset, nFileExtension)` for an OPENFILENAME result.
fn split_path_components(path: &str) -> (&str, u16, u16) {
    let separator = path.rfind(['\\', '/']);
    let file_start = separator.map_or(0, |index| index.saturating_add(1));
    let file_name = path.get(file_start..).unwrap_or(path);

    let extension_start_in_file = file_name.rfind('.').map_or(file_name.len(), |index| {
        // nFileExtension points at the character after the dot when present.
        index.saturating_add(1)
    });

    let file_offset = u16::try_from(file_start).unwrap_or(u16::MAX);
    let extension_offset =
        u16::try_from(file_start.saturating_add(extension_start_in_file)).unwrap_or(u16::MAX);

    (file_name, file_offset, extension_offset)
}

// ── FindTextW / ReplaceTextW modeless dialogs ─────────────────────────────
//
// The guest (RNotepad) implements ALL find logic guest-side: it handles the
// registered `FINDMSGSTRING` message, reads the `FINDREPLACE` struct (the
// message's lParam), and drives its EDIT via EM_* messages. The host only
// supplies the modeless dialog and the message: `FindTextW`/`ReplaceTextW`
// build a "FindDialog"-class window from the existing EDIT/STATIC/BUTTON
// controls, and the dialog's buttons are handled host-side — they write the
// user's choices back into the GUEST `FINDREPLACE` struct and post
// `FINDMSGSTRING` to the owner window (the same queue the guest's main
// GetMessage loop drains, so a plain push suffices — no wake needed).

/// Handles `comdlg32.dll!FindTextW`.
pub fn handle_find_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_find_replace_text(ctx, "FindTextW", false)
}

/// Handles `comdlg32.dll!ReplaceTextW`.
pub fn handle_replace_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_find_replace_text(ctx, "ReplaceTextW", true)
}

/// Shared `FindTextW`/`ReplaceTextW` implementation: reads the guest's
/// `FINDREPLACE` struct, builds the modeless host dialog (seeding the edit
/// lines from `lpstrFindWhat` / `lpstrReplaceWith`), and returns the dialog's
/// HWND. The dialog is modeless — the guest's main loop keeps pumping, so no
/// in-guest modal loop is planted (unlike `GetOpenFileName`).
fn handle_find_replace_text(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    replace_mode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let fr_ptr = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    if fr_ptr == 0 {
        // FindTextW(NULL) fails like GetOpenFileName(NULL): no dialog.
        let return_address = engine
            .return_from_win64_api(0)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let owner_raw = read_guest_u64(
        engine,
        checked_field_address(fr_ptr, FR_HWND_OWNER, "FINDREPLACE.hwndOwner"),
    )
    .with_context(|| format!("failed to read hwndOwner for {api_name}"))?;
    let find_what_ptr = read_guest_u64(
        engine,
        checked_field_address(fr_ptr, FR_LPSTR_FIND_WHAT, "FINDREPLACE.lpstrFindWhat"),
    )
    .with_context(|| format!("failed to read lpstrFindWhat for {api_name}"))?;
    let find_what_len = read_guest_u32(
        engine,
        checked_field_address(fr_ptr, FR_W_FIND_WHAT_LEN, "FINDREPLACE.wFindWhatLen"),
    )
    .with_context(|| format!("failed to read wFindWhatLen for {api_name}"))?;
    let replace_with_ptr = if replace_mode {
        read_guest_u64(
            engine,
            checked_field_address(
                fr_ptr,
                FR_LPSTR_REPLACE_WITH,
                "FINDREPLACE.lpstrReplaceWith",
            ),
        )
        .with_context(|| format!("failed to read lpstrReplaceWith for {api_name}"))?
    } else {
        0
    };
    let replace_with_len = if replace_mode {
        read_guest_u32(
            engine,
            checked_field_address(fr_ptr, FR_W_REPLACE_WITH_LEN, "FINDREPLACE.wReplaceWithLen"),
        )
        .with_context(|| format!("failed to read wReplaceWithLen for {api_name}"))?
    } else {
        0
    };
    let flags = read_guest_u32(
        engine,
        checked_field_address(fr_ptr, FR_FLAGS, "FINDREPLACE.Flags"),
    )
    .with_context(|| format!("failed to read Flags for {api_name}"))?;

    // The dialog's EDIT lines are seeded from the guest's buffers (RNotepad
    // keeps its search/replace text there across Find menu opens).
    let seed_find = if find_what_ptr != 0 {
        read_utf16_lossy(engine, find_what_ptr, 1024)
            .with_context(|| format!("failed to read {api_name} find text"))?
    } else {
        String::new()
    };
    let seed_replace = if replace_with_ptr != 0 {
        read_utf16_lossy(engine, replace_with_ptr, 1024)
            .with_context(|| format!("failed to read {api_name} replace text"))?
    } else {
        String::new()
    };

    let cy = if replace_mode {
        FIND_DLG_CY_REPLACE
    } else {
        FIND_DLG_CY
    };
    let parent_handle = resolve_dialog_owner(state, owner_raw);
    let (owner_w, owner_h) = window_client_size(state, parent_handle);
    let (dialog_x, dialog_y) = if parent_handle != 0 && owner_w >= FIND_DLG_CX && owner_h >= cy {
        (
            owner_w.saturating_sub(FIND_DLG_CX).saturating_div(2),
            owner_h.saturating_sub(cy).saturating_div(2),
        )
    } else {
        (0, 0)
    };

    let (dialog_hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name("FindDialog".to_owned()),
            title: if replace_mode {
                "Replace".to_owned()
            } else {
                "Find".to_owned()
            },
            style: WS_VISIBLE | WS_CLIPCHILDREN,
            extended_style: 0,
            parent_handle,
            menu_handle: 0,
            instance_handle: 0,
            x: dialog_x,
            y: dialog_y,
            width: FIND_DLG_CX,
            height: cy,
        },
        true,
    )?;
    if dialog_hwnd == 0 {
        let return_address = engine
            .return_from_win64_api(0)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    // Row 0: "Find what:" label + search EDIT.
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0082), // STATIC
        "Find what:".to_owned(),
        0,
        0,
        FIND_DLG_LABEL_X,
        8,
        78,
        20,
    )?;
    let find_edit_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0081), // EDIT
        seed_find,
        WS_BORDER | ES_AUTOHSCROLL,
        u64::from(FIND_DLG_EDIT_ID),
        FIND_DLG_FIELD_X,
        8,
        FIND_DLG_FIELD_W,
        FIND_DLG_EDIT_H,
    )?;

    // Replace mode adds a "Replace with:" row between the edit and the
    // checkboxes; the buttons get two extra slots.
    let replace_edit_hwnd = if replace_mode {
        create_find_control(
            state,
            dialog_hwnd,
            WindowClassIdentifier::Atom(0x0082), // STATIC
            "Replace with:".to_owned(),
            0,
            0,
            FIND_DLG_LABEL_X,
            36,
            78,
            20,
        )?;
        create_find_control(
            state,
            dialog_hwnd,
            WindowClassIdentifier::Atom(0x0081), // EDIT
            seed_replace,
            WS_BORDER | ES_AUTOHSCROLL,
            u64::from(FIND_DLG_REPLACE_EDIT_ID),
            FIND_DLG_FIELD_X,
            36,
            FIND_DLG_FIELD_W,
            FIND_DLG_EDIT_H,
        )?
    } else {
        0
    };

    // Checkboxes render as push buttons whose caption shows the state; the
    // checked state itself lives on the session and is mirrored into
    // `FINDREPLACE.Flags` when the dialog submits (the host Button control
    // has no BS_AUTOCHECKBOX state).
    let match_case_checked = flags & FR_MATCHCASE != 0;
    let whole_word_checked = flags & FR_WHOLEWORD != 0;
    let checkbox_y = if replace_mode { 64 } else { 40 };
    let match_case_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        if match_case_checked {
            "[x] Match case".to_owned()
        } else {
            "[ ] Match case".to_owned()
        },
        0,
        u64::from(FIND_DLG_MATCH_CASE_ID),
        16,
        checkbox_y,
        100,
        20,
    )?;
    let whole_word_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        if whole_word_checked {
            "[x] Match whole word".to_owned()
        } else {
            "[ ] Match whole word".to_owned()
        },
        0,
        u64::from(FIND_DLG_WHOLE_WORD_ID),
        120,
        checkbox_y,
        140,
        20,
    )?;

    // The command buttons: Find Next is the Enter default (BS_DEFPUSHBUTTON).
    let (find_next_y, cancel_y) = if replace_mode { (8, 98) } else { (8, 40) };
    let find_next_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        "Find Next".to_owned(),
        BS_DEFPUSHBUTTON,
        u64::from(FIND_DLG_FIND_NEXT_ID),
        FIND_DLG_BTN_X,
        find_next_y,
        FIND_DLG_BTN_W,
        FIND_DLG_BTN_H,
    )?;
    let replace_hwnd = if replace_mode {
        Some(create_find_control(
            state,
            dialog_hwnd,
            WindowClassIdentifier::Atom(0x0080), // BUTTON
            "Replace".to_owned(),
            0,
            u64::from(FIND_DLG_REPLACE_ID),
            FIND_DLG_BTN_X,
            38,
            FIND_DLG_BTN_W,
            FIND_DLG_BTN_H,
        )?)
    } else {
        None
    };
    let replace_all_hwnd = if replace_mode {
        Some(create_find_control(
            state,
            dialog_hwnd,
            WindowClassIdentifier::Atom(0x0080), // BUTTON
            "Replace All".to_owned(),
            0,
            u64::from(FIND_DLG_REPLACE_ALL_ID),
            FIND_DLG_BTN_X,
            68,
            FIND_DLG_BTN_W,
            FIND_DLG_BTN_H,
        )?)
    } else {
        None
    };
    let cancel_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        "Cancel".to_owned(),
        0,
        u64::from(FIND_DLG_CANCEL_ID),
        FIND_DLG_BTN_X,
        cancel_y,
        FIND_DLG_BTN_W,
        FIND_DLG_BTN_H,
    )?;

    // The Find Next button is the dialog's Enter default.
    state
        .window_state()
        .control_states
        .entry(Hwnd::from(find_next_hwnd))
        .or_insert_with(|| ControlClassKind::Button.new_state())
        .set_default_push(true);

    // Record the session for the button write-back. The dialog is modeless,
    // so the message-queue dialog_depth is NOT touched: an empty GetMessage
    // keeps yielding normally while the guest pumps its main loop.
    state.window_state().find_dialogs.push(FindDialogSession {
        dialog_hwnd,
        fr_ptr,
        owner_hwnd: parent_handle,
        find_what_ptr,
        find_what_len,
        replace_with_ptr,
        replace_with_len,
        find_edit_hwnd,
        replace_edit_hwnd,
        match_case_hwnd,
        whole_word_hwnd,
        match_case_checked,
        whole_word_checked,
        replace_mode,
    });

    // The dialog takes activation and the search EDIT takes keyboard focus,
    // so the first keystrokes land in the find field.
    state.window_state().active_window_handle = Hwnd::from(dialog_hwnd);
    state.window_state().focus_window_handle = Hwnd::from(find_edit_hwnd);
    let _unused = deliver_focus_change(
        state,
        engine,
        0,
        find_edit_hwnd,
        OuterReturn::Fixed(find_edit_hwnd),
    )?;

    // Mark the whole subtree invalidated so the first empty GetMessage
    // synthesizes the WM_PAINTs (dialog face + every control).
    let mut subtree = vec![
        dialog_hwnd,
        find_edit_hwnd,
        match_case_hwnd,
        whole_word_hwnd,
        find_next_hwnd,
        cancel_hwnd,
    ];
    if replace_edit_hwnd != 0 {
        subtree.push(replace_edit_hwnd);
    }
    if let Some(hwnd) = replace_hwnd {
        subtree.push(hwnd);
    }
    if let Some(hwnd) = replace_all_hwnd {
        subtree.push(hwnd);
    }
    for hwnd in subtree {
        if let Some(window) = find_window_mut(state, hwnd) {
            window.invalidated = true;
        }
    }

    tracing::info!(
        target: "wiegui",
        api = api_name,
        hwnd = dialog_hwnd,
        parent = parent_handle,
        replace_mode,
        "find dialog opened"
    );

    let return_address = engine
        .return_from_win64_api(dialog_hwnd)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: dialog_hwnd,
    })
}

/// Create one control inside the find dialog and set its client rect.
///
/// `rect` is `(x, y, width, height)` in dialog-client pixels.
#[allow(clippy::too_many_arguments)] // 8 finder args + state; a rect struct would hide the layout
fn create_find_control(
    state: &mut WinApiState,
    dialog_hwnd: u64,
    class_identifier: WindowClassIdentifier,
    title: String,
    style_bits: u32,
    id: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<u64> {
    let (hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier,
            title,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | style_bits,
            extended_style: 0,
            parent_handle: dialog_hwnd,
            menu_handle: id,
            instance_handle: 0,
            x,
            y,
            width,
            height,
        },
        true,
    )?;
    if let Some(window) = find_window_mut(state, hwnd) {
        window.client_rect = (0, 0, width, height);
    }
    Ok(hwnd)
}

/// Whether `hwnd` is one of the host-owned Find/Replace dialogs.
///
/// The user32 control/message paths consult this to route the dialog's
/// messages host-side instead of bridging to a guest proc (the find dialog
/// has no `dialog_proc`).
#[must_use]
pub(crate) fn is_find_dialog_window(state: &WinApiState, hwnd: u64) -> bool {
    state.try_window_state().is_some_and(|ws| {
        ws.find_dialogs
            .iter()
            .any(|session| session.dialog_hwnd == hwnd)
    })
}

/// Handle a `WM_COMMAND` raised by one of the find dialog's buttons.
///
/// Called from the user32 control dispatch (`deliver_command`) when the
/// command bubbles up to a find-dialog window — and from IsDialogMessage for
/// Enter/Escape. Buttons either toggle a checkbox or submit the dialog:
/// write the edit texts + checkbox state back into the guest `FINDREPLACE`
/// struct and post `FINDMSGSTRING` to the owner. Cancel additionally tears
/// the dialog down (FR_DIALOGTERM first, so the owner cleans up its state).
pub(crate) fn handle_find_dialog_command(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    dialog_hwnd: u64,
    word_parameter: u64,
    child_hwnd: u64,
) -> Result<Option<u64>> {
    let Some(index) = state
        .window_state()
        .find_dialogs
        .iter()
        .position(|session| session.dialog_hwnd == dialog_hwnd)
    else {
        return Ok(Some(0));
    };
    let command = CommandPayload::decode(word_parameter, child_hwnd);

    match command.id {
        FIND_DLG_MATCH_CASE_ID => {
            toggle_find_checkbox(state, index, "Match case")?;
        }
        FIND_DLG_WHOLE_WORD_ID => {
            toggle_find_checkbox(state, index, "Match whole word")?;
        }
        FIND_DLG_FIND_NEXT_ID => submit_find_dialog(engine, state, index, FR_FINDNEXT)?,
        FIND_DLG_REPLACE_ID => submit_find_dialog(engine, state, index, FR_REPLACE)?,
        FIND_DLG_REPLACE_ALL_ID => submit_find_dialog(engine, state, index, FR_REPLACEALL)?,
        FIND_DLG_CANCEL_ID => close_find_dialog(engine, state, index)?,
        _ => {
            tracing::debug!(
                target: "wiegui",
                id = command.id,
                hwnd = dialog_hwnd,
                "find dialog: unhandled command"
            );
        }
    }
    Ok(Some(0))
}

/// Toggle one of the find dialog's checkbox buttons and repaint it (the
/// caption carries the state: `[x]` / `[ ]`).
fn toggle_find_checkbox(state: &mut WinApiState, index: usize, label: &str) -> Result<()> {
    let (checked, checkbox_hwnd, title) = {
        let session = state
            .window_state()
            .find_dialogs
            .get_mut(index)
            .context("find-dialog session vanished")?;
        let checked = if label == "Match case" {
            session.match_case_checked = !session.match_case_checked;
            session.match_case_checked
        } else {
            session.whole_word_checked = !session.whole_word_checked;
            session.whole_word_checked
        };
        let checkbox_hwnd = if label == "Match case" {
            session.match_case_hwnd
        } else {
            session.whole_word_hwnd
        };
        let title = format!("[{}] {label}", if checked { "x" } else { " " });
        (checked, checkbox_hwnd, title)
    };
    tracing::debug!(target: "wiegui", checked, "find dialog checkbox toggled");
    if let Some(window) = find_window_mut(state, checkbox_hwnd) {
        window.control_text = title;
        window.invalidated = true;
    }
    Ok(())
}

/// Submit a Find/Replace/Replace-All action: write the edit texts and the
/// checkbox state into the guest `FINDREPLACE` struct, then post
/// `FINDMSGSTRING` to the owner (lParam = the struct's VA).
fn submit_find_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    index: usize,
    action_flag: u32,
) -> Result<()> {
    let session = state
        .window_state()
        .find_dialogs
        .get(index)
        .cloned()
        .context("find-dialog session vanished")?;

    let find_text = find_window(state, session.find_edit_hwnd)
        .map_or_else(String::new, |window| window.control_text.clone());
    let replace_text = if session.replace_edit_hwnd != 0 {
        find_window(state, session.replace_edit_hwnd)
            .map_or_else(String::new, |window| window.control_text.clone())
    } else {
        String::new()
    };

    // Preserve the owner's bits (FR_DOWN, FR_HIDEWHOLEWORD, ...) and fold the
    // checkbox state + the action into the action-bit group.
    let current_flags = read_guest_u32(
        engine,
        checked_field_address(session.fr_ptr, FR_FLAGS, "FINDREPLACE.Flags"),
    )
    .context("failed to read FINDREPLACE.Flags on submit")?;
    let mut new_flags = current_flags
        & !(FR_FINDNEXT | FR_REPLACE | FR_REPLACEALL | FR_DIALOGTERM | FR_MATCHCASE | FR_WHOLEWORD);
    new_flags |= action_flag;
    if session.match_case_checked {
        new_flags |= FR_MATCHCASE;
    }
    if session.whole_word_checked {
        new_flags |= FR_WHOLEWORD;
    }

    let find_len = usize::try_from(session.find_what_len)
        .context("FINDREPLACE.wFindWhatLen does not fit usize")?;
    write_utf16_c_string(engine, session.find_what_ptr, find_len, &find_text)
        .context("failed to write FINDREPLACE.lpstrFindWhat")?;
    if session.replace_edit_hwnd != 0 {
        let replace_len = usize::try_from(session.replace_with_len)
            .context("FINDREPLACE.wReplaceWithLen does not fit usize")?;
        write_utf16_c_string(engine, session.replace_with_ptr, replace_len, &replace_text)
            .context("failed to write FINDREPLACE.lpstrReplaceWith")?;
    }
    write_guest_u32(
        engine,
        checked_field_address(session.fr_ptr, FR_FLAGS, "FINDREPLACE.Flags"),
        new_flags,
    )
    .context("failed to write FINDREPLACE.Flags on submit")?;

    post_find_msgstring(state, &session);
    Ok(())
}

/// Close the find dialog: post `FINDMSGSTRING` with `FR_DIALOGTERM` so the
/// owner clears its find state, then tear the dialog subtree down.
fn close_find_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    index: usize,
) -> Result<()> {
    let session = state
        .window_state()
        .find_dialogs
        .get(index)
        .cloned()
        .context("find-dialog session vanished")?;

    let current_flags = read_guest_u32(
        engine,
        checked_field_address(session.fr_ptr, FR_FLAGS, "FINDREPLACE.Flags"),
    )
    .context("failed to read FINDREPLACE.Flags on close")?;
    write_guest_u32(
        engine,
        checked_field_address(session.fr_ptr, FR_FLAGS, "FINDREPLACE.Flags"),
        (current_flags & !(FR_FINDNEXT | FR_REPLACE | FR_REPLACEALL | FR_DIALOGTERM))
            | FR_DIALOGTERM,
    )
    .context("failed to write FINDREPLACE.Flags on close")?;

    post_find_msgstring(state, &session);
    destroy_find_dialog(state, &session);
    Ok(())
}

/// Queue `FINDMSGSTRING` to the dialog's owner with lParam = the guest
/// `FINDREPLACE` VA. The owner's GetMessage loop is already pumping (modeless
/// dialog), so a plain queue push delivers it on the next drain.
fn post_find_msgstring(state: &mut WinApiState, session: &FindDialogSession) {
    let message_id = findmsgstring_id(state);
    let mut queue = state.lock_message_queue();
    let time = queue.next_message_time;
    queue.messages.push(QueuedWindowMessage {
        window_handle: Hwnd::from(session.owner_hwnd),
        message: message_id,
        word_parameter: 0,
        long_parameter: session.fr_ptr,
        time,
        point_x: 0,
        point_y: 0,
    });
    queue.next_message_time = time.saturating_add(1);
    tracing::debug!(
        target: "wiegui",
        message_id,
        owner = session.owner_hwnd,
        fr_ptr = session.fr_ptr,
        "FINDMSGSTRING posted"
    );
}

/// Resolve the id `RegisterWindowMessageW("FINDMSGSTRING")` returns — or, if
/// the guest never registered it, allocate one now (registered names are
/// session-stable, so a later guest registration returns the same id).
fn findmsgstring_id(state: &mut WinApiState) -> u32 {
    if let Some(id) = state
        .window_state()
        .registered_messages
        .get(FINDMSGSTRING_NAME)
        .copied()
    {
        return id;
    }
    if state.window_state().next_registered_message >= REGISTERED_MESSAGE_LIMIT {
        return 0;
    }
    let id = state.window_state().next_registered_message;
    state.window_state().next_registered_message = state
        .window_state()
        .next_registered_message
        .saturating_add(1);
    state
        .window_state()
        .registered_messages
        .insert(FINDMSGSTRING_NAME.to_owned(), id);
    id
}

/// Remove the find dialog, its controls, and its session state.
fn destroy_find_dialog(state: &mut WinApiState, session: &FindDialogSession) {
    let dialog_handle = Hwnd::from(session.dialog_hwnd);
    // Snapshot (handle, parent) so the subtree walk does not re-borrow
    // `state` while the window list is being iterated.
    let pairs: Vec<(Hwnd, Hwnd)> = state
        .window_state()
        .windows
        .iter()
        .map(|window| (window.handle, window.parent_handle))
        .collect();
    let subtree: Vec<Hwnd> = pairs
        .iter()
        .filter_map(|(handle, _)| {
            if window_handle_in_subtree(&pairs, *handle, dialog_handle) {
                Some(*handle)
            } else {
                None
            }
        })
        .collect();
    state
        .window_state()
        .windows
        .retain(|window| !subtree.contains(&window.handle));
    state
        .window_state()
        .control_states
        .retain(|hwnd, _| !subtree.contains(hwnd));
    if subtree.contains(&state.window_state().focus_window_handle) {
        state.window_state().focus_window_handle = Hwnd::NULL;
    }
    if subtree.contains(&state.window_state().capture_window_handle) {
        state.window_state().capture_window_handle = Hwnd::NULL;
    }
    if subtree.contains(&state.window_state().active_window_handle) {
        state.window_state().active_window_handle = Hwnd::NULL;
    }
    state
        .window_state()
        .find_dialogs
        .retain(|candidate| candidate.dialog_hwnd != session.dialog_hwnd);
    tracing::info!(
        target: "wiegui",
        hwnd = session.dialog_hwnd,
        "find dialog closed"
    );
}

// ── ChooseFontW interactive dialog ─────────────────────────────────────────

/// Whether `hwnd` is the in-flight modal font dialog.
///
/// The `EndDialog` handler routes on this: the font dialog's write-back
/// (`complete_font_dialog`) handles the shared proc stub's sentinel results
/// (effects toggles that keep the dialog open) in addition to OK/Cancel.
#[must_use]
pub(crate) fn is_font_dialog_window(state: &WinApiState, hwnd: u64) -> bool {
    state.try_window_state().is_some_and(|ws| {
        ws.font_dialog
            .as_ref()
            .is_some_and(|session| session.dialog_hwnd == hwnd)
    })
}

/// Windows `MulDiv(a, b, c)` (wingdi.h): `(a * b + c / 2) / c`, signed
/// rounding toward zero on the half. Used for the point ↔ pixel height
/// conversion (the same formula RNotepad's `HeightFromPointSize` uses).
fn mul_div(a: i64, b: i64, c: i64) -> i64 {
    let product = a * b;
    if product >= 0 {
        (product + c / 2) / c
    } else {
        (product - c / 2) / c
    }
}

/// Point size (tenths of points) for the initial `LOGFONTW.lfHeight`.
///
/// `lfHeight < 0` is a character height in pixels; at 96 DPI one point is
/// 4/3 px, so `tenths = MulDiv(720, |height|, 96)`. `0` (the engine's default
/// 16 px) maps to 12 points. Rounded to a whole point so the size LISTBOX
/// (integer points 8..72) can seed its selection.
fn point_size_tenths_from_lf_height(lf_height: i32) -> i32 {
    let px = if lf_height == 0 {
        16
    } else {
        lf_height.unsigned_abs().max(1)
    };
    let tenths = mul_div(720, i64::from(px), 96).max(1);
    // Round to the nearest whole point (the list only offers integers).
    i32::try_from((tenths + 5) / 10 * 10)
        .unwrap_or(120)
        .clamp(80, 720)
}

/// `LOGFONTW.lfHeight` for a point size in tenths: `-MulDiv(t, 96, 720)`.
fn lf_height_from_point_size_tenths(point_tenths: i32) -> i32 {
    let negative = -mul_div(i64::from(point_tenths), 96, 720);
    i32::try_from(negative).unwrap_or(-16)
}

/// Family names for the dialog's LISTBOX, from the host font database.
fn dialog_family_names() -> Vec<String> {
    let mut names = crate::gdi32::system_family_names();
    // Keep the list predictable even on a font-less host.
    if names.is_empty() {
        names.push("System".to_owned());
    }
    names
}

/// Point-size items (whole points 8..72) for the dialog's size LISTBOX.
fn dialog_point_sizes() -> Vec<String> {
    (8..=72).map(|point| point.to_string()).collect()
}

/// `FontDialogPolicy::Interactive`: build the host font dialog (a
/// "FontDialog"-class window with a family LISTBOX, a size LISTBOX, the
/// Strikeout/Underline effects buttons, and OK/Cancel) and ask the runtime to
/// run the file dialog's in-guest modal loop.
///
/// The dialog window carries the planted file-dialog proc stub as its
/// `dialog_proc`, so `WM_COMMAND(IDOK/IDCANCEL)` / `WM_CLOSE` bridge into the
/// stub, which calls `EndDialog`. The extended `EndDialog` handler performs
/// the `CHOOSEFONTW`/`LOGFONTW` write-back (see [`complete_font_dialog`]) and
/// posts the `WM_QUIT` the loop exits on. The effects buttons close through
/// the same stub with sentinel results, which the handler turns into toggles
/// (the dialog stays open).
fn open_host_font_dialog(ctx: &mut HandlerContext<'_>, cf_ptr: u64) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    let log_font_ptr = read_guest_u64(
        engine,
        checked_field_address(cf_ptr, CF_LP_LOG_FONT, "CHOOSEFONTW.lpLogFont"),
    )
    .context("failed to read lpLogFont for ChooseFontW")?;
    let initial_point_tenths = read_guest_u32(
        engine,
        checked_field_address(cf_ptr, CF_IPOINT_SIZE, "CHOOSEFONTW.iPointSize"),
    )
    .context("failed to read iPointSize for ChooseFontW")?;
    let flags = read_guest_u32(
        engine,
        checked_field_address(cf_ptr, CF_FLAGS, "CHOOSEFONTW.Flags"),
    )
    .context("failed to read Flags for ChooseFontW")?;
    let rgb_colors = read_guest_u32(
        engine,
        checked_field_address(cf_ptr, CF_RGB_COLORS, "CHOOSEFONTW.rgbColors"),
    )
    .context("failed to read rgbColors for ChooseFontW")?;

    // Without the planted loop/proc bodies (headless/trace sessions) or with a
    // font dialog already open, fall back to Cancel: a guest must never hang.
    let loop_va = state.window_state().file_dialog_loop_va;
    let proc_va = state.window_state().file_dialog_proc_va;
    if log_font_ptr == 0
        || loop_va == 0
        || proc_va == 0
        || state.window_state().font_dialog.is_some()
    {
        state_comm_dlg_none(state);
        tracing::warn!("ChooseFontW interactive dialog unavailable; cancelling");
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from ChooseFontW")?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    // Seed the dialog from the guest's initial LOGFONTW (CF_INITTOLOGFONTSTRUCT
    // semantics — RNotepad initializes `lf` from its stored font). The guest
    // stack frame holding `lf` stays live for the whole modal loop, so the
    // write-back re-reads it rather than caching the bytes here.
    let initial_face = read_utf16_lossy(
        engine,
        checked_field_address(log_font_ptr, LF_FACE_NAME, "LOGFONTW.lfFaceName"),
        32,
    )
    .context("failed to read ChooseFontW lfFaceName")?;
    let lf_height = read_guest_i32(
        engine,
        checked_field_address(log_font_ptr, LF_HEIGHT, "LOGFONTW.lfHeight"),
    )
    .context("failed to read ChooseFontW lfHeight")?;
    let effects_word = read_guest_u32(
        engine,
        checked_field_address(
            log_font_ptr,
            LF_ITALIC_UNDERLINE_STRIKE_CHARSET,
            "LOGFONTW effects",
        ),
    )
    .context("failed to read ChooseFontW underline/strikeout")?;

    let family_names = dialog_family_names();
    let point_sizes = dialog_point_sizes();
    let seed_family_index = family_names
        .iter()
        .position(|name| name.eq_ignore_ascii_case(&initial_face))
        .unwrap_or(0);
    let seed_size_index = if initial_point_tenths != 0 {
        let point = i32::try_from(initial_point_tenths / 10).unwrap_or(10);
        point.clamp(8, 72).saturating_sub(8).clamp(
            0,
            i32::try_from(point_sizes.len().saturating_sub(1)).unwrap_or(0),
        )
    } else {
        // lfHeight 0 → the engine's 16 px default → 12 points.
        let point = point_size_tenths_from_lf_height(lf_height) / 10;
        point.saturating_sub(8).clamp(0, 64)
    };
    let seed_family = family_names
        .get(seed_family_index)
        .cloned()
        .unwrap_or_default();
    let seed_point_size = point_sizes
        .get(usize::try_from(seed_size_index).unwrap_or(0))
        .and_then(|text| text.parse::<i32>().ok())
        .unwrap_or(10)
        .saturating_mul(10);

    let owner_raw = read_guest_u64(
        engine,
        checked_field_address(cf_ptr, CF_HWND_OWNER, "CHOOSEFONTW.hwndOwner"),
    )
    .context("failed to read hwndOwner for ChooseFontW")?;
    let parent_handle = resolve_dialog_owner(state, owner_raw);
    let (owner_w, owner_h) = window_client_size(state, parent_handle);
    let (dialog_x, dialog_y) =
        if parent_handle != 0 && owner_w >= FONT_DLG_CX && owner_h >= FONT_DLG_CY {
            (
                owner_w.saturating_sub(FONT_DLG_CX).saturating_div(2),
                owner_h.saturating_sub(FONT_DLG_CY).saturating_div(2),
            )
        } else {
            (0, 0)
        };

    let (dialog_hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name("FontDialog".to_owned()),
            title: "Font".to_owned(),
            style: WS_VISIBLE | WS_CLIPCHILDREN,
            extended_style: 0,
            parent_handle,
            menu_handle: 0,
            instance_handle: 0,
            x: dialog_x,
            y: dialog_y,
            width: FONT_DLG_CX,
            height: FONT_DLG_CY,
        },
        false,
    )?;
    if dialog_hwnd == 0 {
        state_comm_dlg_none(state);
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from ChooseFontW")?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    if let Some(window) = find_window_mut(state, dialog_hwnd) {
        window.dialog_proc = proc_va;
        window.dialog_unicode = false;
        window.client_rect = (0, 0, FONT_DLG_CX, FONT_DLG_CY);
    }

    // Row 0: "Font:" label + family LISTBOX (from the host font database).
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0082), // STATIC
        "Font:".to_owned(),
        0,
        0,
        8,
        8,
        60,
        18,
    )?;
    let family_list_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0083), // LISTBOX
        String::new(),
        0,
        FONT_DLG_FAMILY_LIST_ID,
        72,
        8,
        168,
        140,
    )?;
    // Row 1: "Size:" label + size LISTBOX (whole points 8..72).
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0082), // STATIC
        "Size:".to_owned(),
        0,
        0,
        8,
        156,
        60,
        18,
    )?;
    let size_list_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0083), // LISTBOX
        String::new(),
        0,
        FONT_DLG_SIZE_LIST_ID,
        72,
        156,
        60,
        60,
    )?;

    // Effects checkboxes render as push buttons whose caption shows the state
    // (the find-dialog precedent); the checked state lives on the session and
    // is mirrored into the LOGFONTW on OK. The shared dialog-proc stub maps
    // these ids to sentinel EndDialog results the handler turns into toggles.
    let strikeout_checked = effects_word >> 16 & 0xFF != 0;
    let underline_checked = effects_word >> 8 & 0xFF != 0;
    let strikeout_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        if strikeout_checked {
            "[x] Strikeout".to_owned()
        } else {
            "[ ] Strikeout".to_owned()
        },
        0,
        FONT_DLG_STRIKEOUT_ID,
        16,
        224,
        100,
        20,
    )?;
    let underline_hwnd = create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        if underline_checked {
            "[x] Underline".to_owned()
        } else {
            "[ ] Underline".to_owned()
        },
        0,
        FONT_DLG_UNDERLINE_ID,
        120,
        224,
        100,
        20,
    )?;

    // OK (the dialog's Enter default) and Cancel.
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        "OK".to_owned(),
        BS_DEFPUSHBUTTON,
        IDOK,
        FONT_DLG_CX.saturating_sub(176),
        200,
        80,
        24,
    )?;
    create_find_control(
        state,
        dialog_hwnd,
        WindowClassIdentifier::Atom(0x0080), // BUTTON
        "Cancel".to_owned(),
        0,
        IDCANCEL,
        FONT_DLG_CX.saturating_sub(88),
        200,
        80,
        24,
    )?;

    // Seed the LISTBOX items + initial selections.
    state
        .window_state()
        .control_states
        .entry(Hwnd::from(family_list_hwnd))
        .or_insert_with(|| ControlClassKind::ListBox.new_state());
    state
        .window_state()
        .control_states
        .entry(Hwnd::from(size_list_hwnd))
        .or_insert_with(|| ControlClassKind::ListBox.new_state());
    if let ControlState::ListBox {
        items, sel_index, ..
    } = state
        .window_state()
        .control_states
        .get_mut(&Hwnd::from(family_list_hwnd))
        .context("family listbox state")?
    {
        *items = family_names;
        *sel_index = i32::try_from(seed_family_index).unwrap_or(0);
    }
    if let ControlState::ListBox {
        items, sel_index, ..
    } = state
        .window_state()
        .control_states
        .get_mut(&Hwnd::from(size_list_hwnd))
        .context("size listbox state")?
    {
        *items = point_sizes;
        *sel_index = seed_size_index;
    }
    // Bring the seeded selections into view: the guest's current family may
    // sit far down the host database, and the size seed at 12 pt is near the
    // top — the viewport opens showing the selected rows (Windows behavior).
    let _scrolled_family =
        crate::user32::controls::listbox_scroll_selection_into_view(state, family_list_hwnd);
    let _scrolled_size =
        crate::user32::controls::listbox_scroll_selection_into_view(state, size_list_hwnd);

    // The dialog is modal: an empty GetMessage must yield, and the dialog
    // takes activation.
    {
        let mut queue = state.lock_message_queue();
        queue.dialog_depth = queue.dialog_depth.saturating_add(1);
    }
    state.window_state().active_window_handle = Hwnd::from(dialog_hwnd);

    state.window_state().font_dialog = Some(FontDialogSession {
        dialog_hwnd,
        cf_ptr,
        log_font_ptr,
        rgb_colors,
        flags,
        family_list_hwnd,
        size_list_hwnd,
        strikeout_hwnd,
        underline_hwnd,
        strikeout_checked,
        underline_checked,
        selected_family: seed_family,
        selected_point_size: seed_point_size,
    });

    // Initial keyboard focus: the family LISTBOX.
    state.window_state().focus_window_handle = Hwnd::from(family_list_hwnd);
    let _unused = deliver_focus_change(
        state,
        engine,
        0,
        family_list_hwnd,
        OuterReturn::Fixed(family_list_hwnd),
    )?;

    // Mark the whole subtree invalidated so the first empty GetMessage paints
    // the dialog face + controls.
    for hwnd in [
        dialog_hwnd,
        family_list_hwnd,
        size_list_hwnd,
        strikeout_hwnd,
        underline_hwnd,
    ] {
        if let Some(window) = find_window_mut(state, hwnd) {
            window.invalidated = true;
        }
    }

    tracing::info!(
        target: "wiegui",
        hwnd = dialog_hwnd,
        parent = parent_handle,
        "font dialog opened"
    );

    // Run the planted modal loop in-guest; its return value (the dialog
    // result slot: 1 on OK, 0 on cancel) becomes the ChooseFont return.
    Err(WinApiControlSignal::GuestCallbackRequested {
        request: GuestCallbackRequest {
            callback_address: loop_va,
            window_handle: dialog_hwnd,
            message: 0,
            word_parameter: 0,
            long_parameter: 0,
            unicode: false,
            outer_return: OuterReturn::Passthrough,
        },
    }
    .into())
}

/// `EndDialog` write-back for a closing font dialog.
///
/// Three result shapes (the shared dialog-proc stub passes the control id):
/// - `FONT_DLG_STRIKEOUT_ID` / `FONT_DLG_UNDERLINE_ID` — an effects toggle:
///   flip the session's checkbox state + caption, repaint, and return `None`
///   so the modal loop keeps running (the dialog stays open).
/// - `1` (OK) — write the selection into the guest `LOGFONTW` (via
///   `lpLogFont`) and the `CHOOSEFONTW` fields, return `Some(1)`.
/// - `0` (Cancel / WM_CLOSE) — no write-back, return `Some(0)`.
///
/// The `LOGFONTW` is re-read from guest memory at write-back (the guest's
/// stack frame holds it live across the modal loop), so untouched fields —
/// `lfWeight`, `lfCharSet`, `lfItalic`, `lfPitchAndFamily`, … — are preserved
/// exactly and only the dialog-owned fields (`lfHeight`, `lfUnderline`,
/// `lfStrikeOut`, `lfFaceName`) change.
pub(crate) fn complete_font_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    dialog_hwnd: u64,
    result: u64,
) -> Result<Option<u64>> {
    let Some(session) = state.window_state().font_dialog.clone() else {
        return Ok(Some(result));
    };
    if session.dialog_hwnd != dialog_hwnd {
        return Ok(Some(result));
    }

    // Effects toggle: flip the session checkbox state + caption, keep open.
    if result == FONT_DLG_STRIKEOUT_ID || result == FONT_DLG_UNDERLINE_ID {
        let (checked, checkbox_hwnd, label) = {
            let font_dialog = state
                .window_state()
                .font_dialog
                .as_mut()
                .context("font-dialog session vanished")?;
            if result == FONT_DLG_STRIKEOUT_ID {
                font_dialog.strikeout_checked = !session.strikeout_checked;
                (
                    font_dialog.strikeout_checked,
                    session.strikeout_hwnd,
                    "Strikeout",
                )
            } else {
                font_dialog.underline_checked = !session.underline_checked;
                (
                    font_dialog.underline_checked,
                    session.underline_hwnd,
                    "Underline",
                )
            }
        };
        tracing::debug!(target: "wiegui", label, checked, "font dialog effects toggled");
        if let Some(window) = find_window_mut(state, checkbox_hwnd) {
            window.control_text = format!("[{}] {label}", if checked { "x" } else { " " });
            window.invalidated = true;
        }
        return Ok(None);
    }

    // Read the current selection from the LISTBOX controls.
    let (family, point_size_tenths) = {
        let window_state = state.window_state();
        let family = window_state
            .control_states
            .get(&Hwnd::from(session.family_list_hwnd))
            .and_then(|control_state| match control_state {
                ControlState::ListBox {
                    items, sel_index, ..
                } => {
                    let index = usize::try_from(*sel_index).ok()?;
                    items.get(index).cloned()
                }
                _ => None,
            })
            .unwrap_or_else(|| session.selected_family.clone());
        let point = window_state
            .control_states
            .get(&Hwnd::from(session.size_list_hwnd))
            .and_then(|control_state| match control_state {
                ControlState::ListBox {
                    items, sel_index, ..
                } => {
                    let index = usize::try_from(*sel_index).ok()?;
                    items.get(index).and_then(|text| text.parse::<i32>().ok())
                }
                _ => None,
            })
            .unwrap_or(session.selected_point_size / 10);
        (family, point.saturating_mul(10).max(1))
    };
    let strikeout_checked = state
        .window_state()
        .font_dialog
        .as_ref()
        .map_or(session.strikeout_checked, |s| s.strikeout_checked);
    let underline_checked = state
        .window_state()
        .font_dialog
        .as_ref()
        .map_or(session.underline_checked, |s| s.underline_checked);

    if result == 0 {
        state.window_state().font_dialog = None;
        return Ok(Some(0));
    }

    // OK: write the selection into the guest LOGFONTW, preserving every field
    // the dialog does not own (weight, charset, italic, precision, …).
    let effects_word = read_guest_u32(
        engine,
        checked_field_address(
            session.log_font_ptr,
            LF_ITALIC_UNDERLINE_STRIKE_CHARSET,
            "LOGFONTW effects",
        ),
    )
    .context("failed to read LOGFONTW effects on font-dialog accept")?;
    let charset_byte = effects_word >> 24 & 0xFF;
    let italic_byte = effects_word & 0xFF;
    let lf_height = lf_height_from_point_size_tenths(point_size_tenths);
    // lfHeight is negative (character height); write its i32 bit pattern.
    let lf_height_bits = u32::from_le_bytes(lf_height.to_le_bytes());
    write_guest_u32(
        engine,
        checked_field_address(session.log_font_ptr, LF_HEIGHT, "LOGFONTW.lfHeight"),
        lf_height_bits,
    )
    .context("failed to write LOGFONTW.lfHeight")?;
    let new_effects = italic_byte
        | u32::from(underline_checked) << 8
        | u32::from(strikeout_checked) << 16
        | charset_byte << 24;
    write_guest_u32(
        engine,
        checked_field_address(
            session.log_font_ptr,
            LF_ITALIC_UNDERLINE_STRIKE_CHARSET,
            "LOGFONTW effects",
        ),
        new_effects,
    )
    .context("failed to write LOGFONTW underline/strikeout")?;
    write_utf16_c_string(
        engine,
        checked_field_address(session.log_font_ptr, LF_FACE_NAME, "LOGFONTW.lfFaceName"),
        32,
        &family,
    )
    .context("failed to write LOGFONTW.lfFaceName")?;

    // CHOOSEFONTW write-back: iPointSize (tenths), Flags, rgbColors.
    write_guest_u32(
        engine,
        checked_field_address(session.cf_ptr, CF_IPOINT_SIZE, "CHOOSEFONTW.iPointSize"),
        u32::try_from(point_size_tenths).context("iPointSize does not fit u32")?,
    )
    .context("failed to write CHOOSEFONTW.iPointSize")?;
    write_guest_u32(
        engine,
        checked_field_address(session.cf_ptr, CF_FLAGS, "CHOOSEFONTW.Flags"),
        session.flags | CF_SCREEN_FONTS,
    )
    .context("failed to write CHOOSEFONTW.Flags")?;
    write_guest_u32(
        engine,
        checked_field_address(session.cf_ptr, CF_RGB_COLORS, "CHOOSEFONTW.rgbColors"),
        session.rgb_colors,
    )
    .context("failed to write CHOOSEFONTW.rgbColors")?;

    state.window_state().font_dialog = None;
    tracing::info!(
        target: "wiegui",
        %family,
        point_tenths = point_size_tenths,
        strikeout_checked,
        underline_checked,
        "font dialog accepted"
    );
    Ok(Some(1))
}

/// Whether `handle` is `root` or a descendant of `root` (parent-chain walk
/// over a `(handle, parent)` snapshot).
fn window_handle_in_subtree(pairs: &[(Hwnd, Hwnd)], handle: Hwnd, root: Hwnd) -> bool {
    if handle == root {
        return true;
    }
    let mut current = handle;
    loop {
        let Some(&(_, parent)) = pairs.iter().find(|(candidate, _)| *candidate == current) else {
            return false;
        };
        if parent == root {
            return true;
        }
        if parent == Hwnd::NULL || parent == current {
            return false;
        }
        current = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_default_extension, basename_of, complete_file_dialog, dialog_family_names,
        directory_of, finalize_guest_path, handle_choose_font_w, handle_find_dialog_command,
        handle_find_text_w, handle_get_open_file_name_w, handle_get_save_file_name_w,
        handle_page_setup_dlg_w, handle_print_dlg_w, handle_replace_text_w,
        is_absolute_windows_path, is_find_dialog_window, is_simple_filter_glob, list_directory,
        parse_ofn_filter, resolve_initial_dir, split_path_components,
    };
    use crate::guest_heap::GuestHeap;
    use crate::handles::Hwnd;
    use crate::present::MessageQueue;
    use crate::state::{
        FileDialogSession, FileIoState, HeapState, ProcessState, WinApiEnvironment,
    };
    use crate::sync_obj::SyncState;
    use crate::thread::ThreadState;
    use crate::user32::controls::ControlState;
    use crate::user32::dialog::handle_end_dialog;
    use crate::user32::{
        BN_CLICKED, CreateWindowRequest, IDOK, WS_VISIBLE, WinApiControlSignal,
        WindowClassIdentifier, create_window_record, find_window, find_window_mut,
        make_command_wparam,
    };
    use crate::vfs::VolumeConfig;
    use crate::{
        DEFAULT_ENVIRONMENT, DllStateMap, FileDialogBridge, FileDialogPick, FileDialogPolicy,
        FontDialogPolicy, HandlerContext, KernelState, ModuleState, WinApiHandlerResult,
        WinApiState,
    };
    use ahash::HashMap;
    use ahash::HashMapExt;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use wie_cpu::{CpuEngine, IcedCpu};

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    const STACK_TOP: u64 = 0x100_FF00;

    /// Minimal engine for handler tests: guest pages + a return address.
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, wie_cpu::RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, STACK_SIZE, wie_cpu::RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        cpu.write_rsp(STACK_TOP).ok();
    }

    fn test_state() -> WinApiState {
        WinApiState {
            heap_state: HeapState {
                heap: GuestHeap::new(0x2000, 0x10000),
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: crate::FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: crate::GuestStdinMode::InjectOnly,
                ucrt_files: HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: crate::RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: DEFAULT_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: crate::ModuleHandle::from(
                    crate::dll_loader::REAL_MODULE_HANDLE_BASE,
                ),
            },
        }
    }

    fn test_environment() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 0,
        }
    }

    /// Write an `OPENFILENAME` (Win64) into guest memory at `ofn_ptr` with
    /// `lpstrFile` → `file_buf` and optional `lpstrDefExt` → `def_ext_ptr`.
    fn write_ofn(
        engine: &mut IcedCpu,
        ofn_ptr: u64,
        file_buf: u64,
        max_file: u32,
        def_ext_ptr: u64,
    ) {
        engine.mem_write(ofn_ptr, &0x58_u32.to_le_bytes()).ok(); // lStructSize
        engine.mem_write(ofn_ptr + 8, &0_u64.to_le_bytes()).ok(); // hwndOwner
        engine.mem_write(ofn_ptr + 48, &file_buf.to_le_bytes()).ok(); // lpstrFile
        engine.mem_write(ofn_ptr + 56, &max_file.to_le_bytes()).ok(); // nMaxFile
        engine.mem_write(ofn_ptr + 64, &0_u64.to_le_bytes()).ok(); // lpstrFileTitle
        engine.mem_write(ofn_ptr + 72, &0_u32.to_le_bytes()).ok(); // nMaxFileTitle
        engine.mem_write(ofn_ptr + 80, &0_u64.to_le_bytes()).ok(); // lpstrInitialDir (0 → guest cwd fallback)
        engine
            .mem_write(ofn_ptr + 104, &def_ext_ptr.to_le_bytes())
            .ok(); // lpstrDefExt
    }

    fn read_guest_utf16(engine: &mut IcedCpu, ptr: u64, max_units: usize) -> String {
        let mut units = Vec::new();
        for i in 0..max_units {
            let mut buf = [0_u8; 2];
            let ok = engine.mem_read(ptr + (i as u64) * 2, &mut buf).ok();
            let unit = u16::from_le_bytes(buf);
            if unit == 0 || ok.is_none() {
                break;
            }
            units.push(unit);
        }
        String::from_utf16_lossy(&units)
    }

    /// NUL-terminated UTF-16LE bytes for a guest string literal.
    fn utf16_bytes(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    // ── Pure helpers ──────────────────────────────────────────────────────

    #[test]
    fn split_windows_path() {
        let (name, file_off, ext_off) = split_path_components(r"C:\Games\level.smc");
        assert_eq!(name, "level.smc");
        assert_eq!(file_off, 9);
        assert_eq!(ext_off, 15);
    }

    #[test]
    fn split_path_without_extension() {
        let (name, file_off, ext_off) = split_path_components(r"C:\Games\level");
        assert_eq!(name, "level");
        assert_eq!(file_off, 9);
        assert_eq!(ext_off, 14);
    }

    #[test]
    fn apply_default_extension_appends_when_name_has_no_dot() {
        assert_eq!(
            apply_default_extension(r"C:\work\notes", Some("txt")),
            r"C:\work\notes.txt"
        );
        // The def-ext often arrives with its dot; the helper trims it.
        assert_eq!(
            apply_default_extension(r"C:\work\notes", Some(".txt")),
            r"C:\work\notes.txt"
        );
    }

    #[test]
    fn apply_default_extension_skips_when_dot_present_or_directory() {
        assert_eq!(
            apply_default_extension(r"C:\work\notes.txt", Some("txt")),
            r"C:\work\notes.txt"
        );
        assert_eq!(
            apply_default_extension(r"C:\work\dir\", Some("txt")),
            r"C:\work\dir\",
            "a directory selection never gains an extension"
        );
        assert_eq!(
            apply_default_extension(r"C:\work\notes", None),
            r"C:\work\notes"
        );
        assert_eq!(
            apply_default_extension(r"C:\work\notes", Some("")),
            r"C:\work\notes"
        );
    }

    #[test]
    fn is_absolute_windows_path_detects_drive_and_unc() {
        assert!(is_absolute_windows_path(r"C:\foo"));
        assert!(is_absolute_windows_path(r"c:/foo"));
        assert!(is_absolute_windows_path(r"\\server\share"));
        assert!(!is_absolute_windows_path("notes.txt"));
        assert!(!is_absolute_windows_path(r"sub\dir\notes.txt"));
    }

    #[test]
    fn directory_and_basename_split() {
        assert_eq!(directory_of(r"C:\foo\bar.txt"), r"C:\foo");
        assert_eq!(basename_of(r"C:\foo\bar.txt"), "bar.txt");
        assert_eq!(directory_of("plain.txt"), "");
        assert_eq!(basename_of("plain.txt"), "plain.txt");
    }

    fn session_with(initial_dir: &str, def_ext: Option<&str>) -> FileDialogSession {
        FileDialogSession {
            dialog_hwnd: 0,
            edit_hwnd: 0,
            ofn_ptr: 0,
            file_buffer_ptr: 0,
            max_file: 0,
            file_title_ptr: 0,
            max_file_title: 0,
            unicode: true,
            initial_dir: initial_dir.to_owned(),
            default_extension: def_ext.map(str::to_owned),
        }
    }

    #[test]
    fn finalize_guest_path_joins_bare_name_with_directory() {
        let session = session_with(r"C:\work", Some("txt"));
        assert_eq!(finalize_guest_path(&session, "notes"), r"C:\work\notes.txt");
        assert_eq!(
            finalize_guest_path(&session, "notes.txt"),
            r"C:\work\notes.txt"
        );
        assert_eq!(
            finalize_guest_path(&session, "  "),
            "",
            "blank edit cancels"
        );
    }

    #[test]
    fn finalize_guest_path_keeps_absolute_typed_path() {
        let session = session_with(r"C:\work", Some("txt"));
        assert_eq!(
            finalize_guest_path(&session, r"D:\elsewhere\report.md"),
            r"D:\elsewhere\report.md"
        );
        // Trailing-separator directory selection keeps the extension rule off.
        assert_eq!(finalize_guest_path(&session, r"C:\work\"), r"C:\work\");
    }

    // ── Policy seam (dispatch-level) ──────────────────────────────────────

    /// Drive the W handler with a scripted policy; returns the result value.
    fn dispatch_open(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        policy: FileDialogPolicy,
    ) -> anyhow::Result<WinApiHandlerResult> {
        state.window_state().file_dialog_policy = policy;
        write_regs(engine, 0x5000, 0, 0, 0);
        handle_get_open_file_name_w(&mut HandlerContext::new(engine, test_environment(), state))
    }

    #[test]
    fn accept_policy_writes_utf16_path_into_buffer() {
        let mut engine = test_engine();
        let mut state = test_state();
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("*.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        let result = dispatch_open(
            &mut engine,
            &mut state,
            FileDialogPolicy::Accept {
                path: r"C:\work\notes.txt".to_owned(),
            },
        )
        .expect("accept must succeed");
        assert_eq!(result.return_value, 1);
        assert_eq!(
            read_guest_utf16(&mut engine, file_buf, 64),
            r"C:\work\notes.txt"
        );
        // nFileOffset points at the basename, nFileExtension after the dot.
        let mut off = [0_u8; 2];
        engine.mem_read(0x5000 + 100, &mut off).ok();
        assert_eq!(u16::from_le_bytes(off), 8);
        engine.mem_read(0x5000 + 102, &mut off).ok();
        assert_eq!(u16::from_le_bytes(off), 14);
        assert_eq!(
            state.window_state().last_file_dialog_path.as_deref(),
            Some(r"C:\work\notes.txt")
        );
    }

    #[test]
    fn interactive_policy_without_loop_machinery_falls_back_to_cancel() {
        let mut engine = test_engine();
        let mut state = test_state();
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);
        // file_dialog_loop_va / proc_va stay 0 (headless session default).

        let result = dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
            .expect("fallback must succeed");
        assert_eq!(result.return_value, 0, "no host dialog → cancel");
        assert_eq!(state.window_state().comm_dlg_extended_error, 0);
        assert!(state.window_state().file_dialog.is_none());
    }

    #[test]
    fn interactive_policy_builds_dialog_and_requests_modal_loop() {
        let mut engine = test_engine();
        let mut state = test_state();
        let loop_va = 0x7000_0040_B000;
        let proc_va = 0x7000_0040_B100;
        state.window_state().file_dialog_loop_va = loop_va;
        state.window_state().file_dialog_proc_va = proc_va;
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        let error = dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
            .expect_err("interactive must request the modal loop");
        let signal = error
            .downcast_ref::<WinApiControlSignal>()
            .expect("must be a control signal");
        let WinApiControlSignal::GuestCallbackRequested { request } = signal else {
            panic!("expected GuestCallbackRequested, got {signal:?}");
        };
        assert_eq!(request.callback_address, loop_va, "loop body VA");

        let dialog_hwnd = state
            .window_state()
            .file_dialog
            .as_ref()
            .expect("session recorded")
            .dialog_hwnd;
        assert_eq!(request.window_handle, dialog_hwnd);

        // The dialog + 4 controls exist; the dialog carries the proc stub.
        let windows = &state.window_state().windows;
        assert_eq!(windows.len(), 5, "dialog + EDIT + LISTBOX + OK + Cancel");
        let dialog = find_window(&mut state, dialog_hwnd).expect("dialog window");
        assert_eq!(
            dialog.dialog_proc, proc_va,
            "dialog proc = file-dialog stub"
        );
        assert_ne!(dialog.width, 0);
        // Modal: depth up, dialog active, edit focused.
        assert_eq!(state.lock_message_queue().dialog_depth, 1);
        assert_eq!(
            state.window_state().active_window_handle.as_u64(),
            dialog_hwnd
        );
        // The path EDIT is seeded with the lpstrFile basename.
        let edit_hwnd = state.window_state().file_dialog.as_ref().unwrap().edit_hwnd;
        assert_eq!(state.window_state().focus_window_handle.as_u64(), edit_hwnd);
        let edit = find_window(&mut state, edit_hwnd).expect("edit window");
        assert_eq!(edit.control_text, "notes.txt");
    }

    #[test]
    fn end_dialog_writes_chosen_path_for_file_dialog() {
        let mut engine = test_engine();
        let mut state = test_state();
        // A bottle must be configured or the confinement would refuse the
        // accept: `C:\new-note.txt` has to land inside the guest C: volume.
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        let loop_va = 0x7000_0040_B000;
        let proc_va = 0x7000_0040_B100;
        state.window_state().file_dialog_loop_va = loop_va;
        state.window_state().file_dialog_proc_va = proc_va;
        state.window_state().dialog_result_va = 0x4000;
        engine.mem_write(0x4000, &0_u32.to_le_bytes()).ok();
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        let def_ext = 0x6100;
        engine.mem_write(def_ext, &utf16_bytes("txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, def_ext);

        // Build the dialog exactly like the interactive handler does.
        dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
            .expect_err("interactive must request the modal loop");
        let dialog_hwnd = state
            .window_state()
            .file_dialog
            .as_ref()
            .expect("session recorded")
            .dialog_hwnd;

        // The user typed a new file name (def-ext "txt" is appended).
        let edit_hwnd = state.window_state().file_dialog.as_ref().unwrap().edit_hwnd;
        if let Some(window) = find_window_mut(&mut state, edit_hwnd) {
            window.control_text = "new-note".to_owned();
        }

        // OK: EndDialog(1) → the path lands in the OPENFILENAME buffer.
        write_regs(&mut engine, dialog_hwnd, IDOK, 0, 0);
        let result = handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        assert_eq!(result.return_value, 1);
        assert_eq!(
            read_guest_utf16(&mut engine, file_buf, 64),
            r"C:\new-note.txt",
            "bare name joins the dialog directory (guest cwd = C:\\) and gains .txt"
        );
        assert_eq!(
            state.window_state().last_file_dialog_path.as_deref(),
            Some(r"C:\new-note.txt")
        );
        // The modal loop's result slot holds TRUE.
        let mut slot = [0_u8; 4];
        engine.mem_read(0x4000, &mut slot).ok();
        assert_eq!(u32::from_le_bytes(slot), 1);
        // The session is cleared and the dialog subtree torn down.
        assert!(state.window_state().file_dialog.is_none());
        assert!(
            !state
                .window_state()
                .windows
                .iter()
                .any(|w| w.handle == Hwnd::from(dialog_hwnd))
        );
    }

    #[test]
    fn end_dialog_cancel_clears_session_without_writing() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
        state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
        state.window_state().dialog_result_va = 0x4000;
        engine.mem_write(0x4000, &0_u32.to_le_bytes()).ok();
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
            .expect_err("interactive must request the modal loop");
        let dialog_hwnd = state
            .window_state()
            .file_dialog
            .as_ref()
            .expect("session recorded")
            .dialog_hwnd;

        write_regs(&mut engine, dialog_hwnd, 0, 0, 0); // EndDialog(0) = cancel
        let result = handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        assert_eq!(result.return_value, 1);
        // The lpstrFile buffer keeps its original content.
        assert_eq!(read_guest_utf16(&mut engine, file_buf, 64), "notes.txt");
        let mut slot = [0_u8; 4];
        engine.mem_read(0x4000, &mut slot).ok();
        assert_eq!(u32::from_le_bytes(slot), 0, "cancel → FALSE");
        assert!(state.window_state().file_dialog.is_none());
    }

    /// `complete_file_dialog` is a no-op for dialogs it does not own.
    #[test]
    fn complete_file_dialog_ignores_unknown_dialog() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.window_state().file_dialog = Some(session_with(r"C:\work", None));
        let result = complete_file_dialog(&mut engine, &mut state, 0x1234, 1).expect("no-op");
        assert_eq!(result, 1);
        assert!(
            state.window_state().file_dialog.is_some(),
            "session untouched"
        );
    }

    // ── Bottle confinement (directory + selection) ────────────────────────

    /// A temporary bottle with a small drive_c layout for listing tests.
    fn temp_bottle(tag: &str) -> (PathBuf, VolumeConfig) {
        let root = std::env::temp_dir().join(format!("wie-ofn-{tag}-{}", std::process::id()));
        let drive_c = root.join("drive_c");
        std::fs::create_dir_all(drive_c.join("App")).expect("create drive_c/App");
        std::fs::create_dir_all(drive_c.join("Windows")).expect("create drive_c/Windows");
        std::fs::write(drive_c.join("root.txt"), b"x").expect("write root file");
        std::fs::write(drive_c.join("App").join("app.txt"), b"x").expect("write app file");
        let volumes = VolumeConfig {
            bottle_root: Some(root.clone()),
            drive_d_root: None,
        };
        (root, volumes)
    }

    #[test]
    fn resolve_initial_dir_prefers_confined_caller_dir() {
        let volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        // lpstrInitialDir confined to the bottle wins over everything.
        assert_eq!(
            resolve_initial_dir(&volumes, r"C:\App", r"C:\work", r"C:\other"),
            r"C:\work"
        );
        // Out-of-bottle lpstrInitialDir (unmapped D:, host path) falls
        // through to the lpstrFile directory.
        assert_eq!(
            resolve_initial_dir(&volumes, r"C:\App", r"D:\x", r"C:\work"),
            r"C:\work"
        );
        assert_eq!(
            resolve_initial_dir(&volumes, r"C:\App", "/Users/me/x", r"C:\work"),
            r"C:\work"
        );
        // ...then to the guest cwd when it is bottle-mapped.
        assert_eq!(resolve_initial_dir(&volumes, r"C:\App", "", ""), r"C:\App");
        // Nothing guest-visible → the bottle root.
        assert_eq!(
            resolve_initial_dir(&volumes, r"D:\cwd", r"D:\caller", r"D:\file"),
            r"C:\"
        );
        // No bottle configured → the fallback root (listing will be empty).
        assert_eq!(
            resolve_initial_dir(&VolumeConfig::default(), r"C:\App", "", ""),
            r"C:\"
        );
    }

    #[test]
    fn list_directory_confines_to_bottle_and_blocks_ascent() {
        let (root, volumes) = temp_bottle("confine-list");
        let mut state = test_state();
        state.file_io.volumes = volumes;

        // An in-bottle directory lists its own entries, no parent link.
        let app = list_directory(&state, r"C:\App");
        assert!(app.contains(&"app.txt".to_owned()));
        assert!(!app.contains(&"..".to_owned()));

        // `..` ascends within the volume: C:\App\.. → the bottle root.
        let root_listing = list_directory(&state, r"C:\App\..");
        assert!(root_listing.contains(&"App".to_owned()));
        assert!(root_listing.contains(&"root.txt".to_owned()));

        // Ascent above the bottle root is blocked (empty listing).
        assert!(list_directory(&state, r"C:\App\..\..").is_empty());
        assert!(list_directory(&state, r"C:\..").is_empty());

        // Unmapped drives and host paths are not listable at all.
        assert!(list_directory(&state, r"E:\anything").is_empty());
        assert!(list_directory(&state, r"D:\x").is_empty());
        assert!(list_directory(&state, "/Users/me/x").is_empty());

        let _unused = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn list_directory_hides_symlink_escape_entries() {
        let (root, volumes) = temp_bottle("confine-symlink");
        let outside = root.join("outside-secret.txt");
        std::fs::write(&outside, b"secret").expect("write outside file");
        std::os::unix::fs::symlink(&outside, root.join("drive_c").join("leak.txt"))
            .expect("create escape symlink");
        let mut state = test_state();
        state.file_io.volumes = volumes;

        // The symlink's host path resolves outside the bottle, so it maps to
        // no guest path and must not appear in the listing.
        let listing = list_directory(&state, r"C:\");
        assert!(!listing.iter().any(|name| name == "leak.txt"));

        let _unused = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn end_dialog_rejects_out_of_bottle_path() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
        state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
        state.window_state().dialog_result_va = 0x4000;
        engine.mem_write(0x4000, &0_u32.to_le_bytes()).ok();
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        // Each escape gets a fresh dialog (EndDialog tears the subtree down).
        // `/Users/me/x.txt` is deliberately absent: Windows path rules read a
        // leading `/` as rooted-on-current-drive, so it resolves to the
        // confined `C:\Users\me\x.txt` — not an escape.
        for escape in [
            r"C:\..\..\etc\passwd",
            r"E:\elsewhere.txt",
            r"..\..\..\etc\passwd",
            r"\\server\share\x",
        ] {
            dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
                .expect_err("interactive must request the modal loop");
            let dialog_hwnd = state
                .window_state()
                .file_dialog
                .as_ref()
                .expect("session recorded")
                .dialog_hwnd;
            let edit_hwnd = state.window_state().file_dialog.as_ref().unwrap().edit_hwnd;
            if let Some(window) = find_window_mut(&mut state, edit_hwnd) {
                window.control_text = escape.to_owned();
            }
            write_regs(&mut engine, dialog_hwnd, IDOK, 0, 0);
            let result = handle_end_dialog(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            ))
            .expect("EndDialog succeeds");
            // Refused like a cancel: the modal-loop result slot holds FALSE,
            // the buffer is untouched, and the session is cleared so a later
            // dialog can open.
            assert_eq!(result.return_value, 1, "EndDialog itself succeeds");
            assert_eq!(
                read_guest_u32_at(&mut engine, 0x4000),
                0,
                "escape {escape} must be refused (FALSE result)"
            );
            assert_eq!(read_guest_utf16(&mut engine, file_buf, 64), "notes.txt");
            assert!(state.window_state().file_dialog.is_none());
            assert!(state.window_state().last_file_dialog_path.is_none());
        }
    }

    #[test]
    fn end_dialog_collapses_dotdot_within_bottle() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
        state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
        state.window_state().dialog_result_va = 0x4000;
        engine.mem_write(0x4000, &0_u32.to_le_bytes()).ok();
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        dispatch_open(&mut engine, &mut state, FileDialogPolicy::Interactive)
            .expect_err("interactive must request the modal loop");
        let dialog_hwnd = state
            .window_state()
            .file_dialog
            .as_ref()
            .expect("session recorded")
            .dialog_hwnd;
        let edit_hwnd = state.window_state().file_dialog.as_ref().unwrap().edit_hwnd;

        // `..` inside the volume is collapsed to the canonical guest path.
        if let Some(window) = find_window_mut(&mut state, edit_hwnd) {
            window.control_text = r"C:\App\..\readme.txt".to_owned();
        }
        write_regs(&mut engine, dialog_hwnd, IDOK, 0, 0);
        let result = handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        assert_eq!(result.return_value, 1);
        assert_eq!(
            read_guest_utf16(&mut engine, file_buf, 64),
            r"C:\readme.txt",
            "within-bottle `..` collapses and the canonical path is written back"
        );
        assert!(state.window_state().file_dialog.is_none());
    }

    // ── Native file-dialog bridge (macOS panels via rfd) ──────────────────

    /// Drive the W handler with a scripted native bridge (Interactive policy).
    ///
    /// The real flow is two entries around the bridge: the handler's first
    /// entry builds the request and returns `FileDialogBridgeRequested`; the
    /// runtime runs the bridge WITHOUT the shared lock and records the pick;
    /// the engine's re-execution of the fake API re-enters the handler, which
    /// writes the pick back. This helper simulates exactly that (the runtime
    /// is not involved in unit tests).
    fn dispatch_open_with_bridge(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        bridge: FileDialogBridge,
    ) -> anyhow::Result<WinApiHandlerResult> {
        state.window_state().file_dialog_policy = FileDialogPolicy::Interactive;
        state.window_state().file_dialog_bridge = Some(bridge);
        write_regs(engine, 0x5000, 0, 0, 0);
        let first = handle_get_open_file_name_w(&mut HandlerContext::new(
            engine,
            test_environment(),
            state,
        ))
        .expect_err("the first entry parks the guest for the native panel");
        let signal = first
            .downcast_ref::<WinApiControlSignal>()
            .expect("a control signal");
        let WinApiControlSignal::FileDialogBridgeRequested { request } = signal else {
            panic!("expected a file-dialog bridge request");
        };
        // What the runtime does between the two entries: take the bridge out,
        // run it (no shared lock), restore it, record the pick.
        let bridge = state
            .window_state()
            .file_dialog_bridge
            .take()
            .expect("bridge registered");
        let picked = bridge(request);
        state.window_state().file_dialog_bridge = Some(bridge);
        state
            .window_state()
            .pending_native_file_dialog
            .as_mut()
            .expect("pending session recorded")
            .pick = picked;
        // Re-entry: the handler writes the pick back.
        handle_get_open_file_name_w(&mut HandlerContext::new(engine, test_environment(), state))
    }

    /// A scripted bridge standing in for the native panel: the pick is a host
    /// path inside the bottle, so the write-back must succeed and return TRUE.
    #[test]
    fn bridge_pick_inside_bottle_writes_guest_path_and_returns_true() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        let bridge: FileDialogBridge = Box::new(|request| {
            // The native panel starts at the BOTTLE ROOT mapped into the
            // bottle (`{root}/drive_c`), not the guest cwd.
            assert_eq!(
                request.initial_host_dir.as_deref(),
                Some(std::path::Path::new("/tmp/bottle/drive_c")),
                "initial dir = the bottle root mapped into the bottle"
            );
            assert_eq!(request.default_file_name.as_deref(), Some("notes.txt"));
            assert!(!request.is_save, "GetOpenFileName is an open panel");
            Some(FileDialogPick {
                host_path: PathBuf::from("/tmp/bottle/drive_c/App/notes.txt"),
            })
        });

        let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
            .expect("bridge accept must succeed");
        assert_eq!(result.return_value, 1, "an in-bottle pick → TRUE");
        assert_eq!(
            read_guest_utf16(&mut engine, file_buf, 64),
            r"C:\App\notes.txt",
            "the host pick maps back to the guest path in lpstrFile"
        );
        assert_eq!(
            state.window_state().last_file_dialog_path.as_deref(),
            Some(r"C:\App\notes.txt")
        );
        assert!(
            state.window_state().file_dialog.is_none(),
            "the bridge path builds no in-app dialog session"
        );
    }

    /// The picked host path lands OUTSIDE both guest volumes (the user
    /// browsed away via the panel's sidebar): the confinement at accept must
    /// refuse it like a cancel — FALSE, `lpstrFile` untouched.
    #[test]
    fn bridge_pick_outside_bottle_cancels_without_touching_buffer() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        let bridge: FileDialogBridge = Box::new(|_| {
            Some(FileDialogPick {
                host_path: PathBuf::from("/etc/passwd"),
            })
        });
        let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
            .expect("bridge accept must succeed");
        assert_eq!(result.return_value, 0, "an out-of-bottle pick → FALSE");
        assert_eq!(
            read_guest_utf16(&mut engine, file_buf, 64),
            "notes.txt",
            "lpstrFile stays untouched"
        );
        assert!(state.window_state().last_file_dialog_path.is_none());
        assert_eq!(state.window_state().comm_dlg_extended_error, 0);
    }

    /// The bridge returning `None` is the user pressing Cancel in the native
    /// panel: FALSE, no write-back.
    #[test]
    fn bridge_cancel_returns_false_without_touching_buffer() {
        let mut engine = test_engine();
        let mut state = test_state();
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        let bridge: FileDialogBridge = Box::new(|_| None);
        let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
            .expect("bridge cancel must succeed");
        assert_eq!(result.return_value, 0, "cancel → FALSE");
        assert_eq!(read_guest_utf16(&mut engine, file_buf, 64), "notes.txt");
        assert!(state.window_state().last_file_dialog_path.is_none());
    }

    /// The request must carry the save flag and the best-effort filter parse:
    /// the simple "*.txt" group survives, the "*.*" catch-all is dropped.
    #[test]
    fn bridge_save_receives_save_flag_and_parsed_filter() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("report.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);
        // lpstrFilter: "Text Documents\0*.txt\0All Files\0*.*\0\0".
        let filter_buf = 0x6200;
        engine
            .mem_write(
                filter_buf,
                &utf16_bytes("Text Documents\0*.txt\0All Files\0*.*\0\0"),
            )
            .ok();
        engine
            .mem_write(0x5000 + 24, &filter_buf.to_le_bytes())
            .ok();

        let bridge: FileDialogBridge = Box::new(|request| {
            assert!(request.is_save, "GetSaveFileName is a save panel");
            assert_eq!(request.default_file_name.as_deref(), Some("report.txt"));
            assert_eq!(request.filters.len(), 1, "the *.* catch-all is dropped");
            assert_eq!(request.filters[0].name, "Text Documents");
            assert_eq!(request.filters[0].patterns, vec!["*.txt".to_owned()]);
            Some(FileDialogPick {
                host_path: PathBuf::from("/tmp/bottle/drive_c/report.txt"),
            })
        });

        state.window_state().file_dialog_policy = FileDialogPolicy::Interactive;
        state.window_state().file_dialog_bridge = Some(bridge);
        write_regs(&mut engine, 0x5000, 0, 0, 0);
        // Entry 1: build the request; entry 2 (after the bridge ran) writes
        // the pick back — the same two-entry flow `dispatch_open_with_bridge`
        // drives, here for the Save handler.
        let first = handle_get_save_file_name_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect_err("the first entry parks the guest for the native panel");
        let signal = first
            .downcast_ref::<WinApiControlSignal>()
            .expect("a control signal");
        let WinApiControlSignal::FileDialogBridgeRequested { request } = signal else {
            panic!("expected a file-dialog bridge request");
        };
        let bridge = state
            .window_state()
            .file_dialog_bridge
            .take()
            .expect("bridge registered");
        let picked = bridge(request);
        state.window_state().file_dialog_bridge = Some(bridge);
        state
            .window_state()
            .pending_native_file_dialog
            .as_mut()
            .expect("pending session recorded")
            .pick = picked;
        let result = handle_get_save_file_name_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("bridge save must succeed");
        assert_eq!(result.return_value, 1);
        assert_eq!(
            read_guest_utf16(&mut engine, file_buf, 64),
            r"C:\report.txt",
            "the save pick maps back into the bottle"
        );
    }

    /// A drive-D bridge pick maps to a `D:\…` guest path (the D: volume is
    /// guest-visible when the bridge root is configured).
    #[test]
    fn bridge_pick_in_drive_d_maps_to_guest_d_path() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: Some(PathBuf::from("/Users/me/data")),
        };
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("a.7z")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        let bridge: FileDialogBridge = Box::new(|_| {
            Some(FileDialogPick {
                host_path: PathBuf::from("/Users/me/data/archive/a.7z"),
            })
        });
        let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
            .expect("bridge accept must succeed");
        assert_eq!(result.return_value, 1);
        assert_eq!(
            read_guest_utf16(&mut engine, file_buf, 64),
            r"D:\archive\a.7z"
        );
    }

    /// The native panel opens at the BOTTLE ROOT even when the guest cwd is
    /// `C:\App` (the process identity hardcodes it) — the user asked for the
    /// bottle root, not the guest's working directory.
    #[test]
    fn bridge_initial_dir_is_the_bottle_root_not_the_guest_cwd() {
        let mut engine = test_engine();
        let mut state = test_state();
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        // The real session seeds the guest cwd to C:\App; the in-app dialog
        // would resolve to {root}/drive_c/App, but the bridge must NOT.
        state.file_io.current_directory_wide = "C:\\App\0".encode_utf16().collect();
        let file_buf = 0x6000;
        engine.mem_write(file_buf, &utf16_bytes("notes.txt")).ok();
        write_ofn(&mut engine, 0x5000, file_buf, 260, 0);

        let bridge: FileDialogBridge = Box::new(|request| {
            assert_eq!(
                request.initial_host_dir.as_deref(),
                Some(std::path::Path::new("/tmp/bottle/drive_c")),
                "initial dir = the bottle root, not C:\\App's directory"
            );
            assert_ne!(
                request.initial_host_dir.as_deref(),
                Some(std::path::Path::new("/tmp/bottle/drive_c/App")),
                "the guest cwd must not leak into the native panel"
            );
            Some(FileDialogPick {
                host_path: PathBuf::from("/tmp/bottle/drive_c/notes.txt"),
            })
        });

        let result = dispatch_open_with_bridge(&mut engine, &mut state, bridge)
            .expect("bridge accept must succeed");
        assert_eq!(result.return_value, 1);
        assert_eq!(
            read_guest_utf16(&mut engine, file_buf, 64),
            r"C:\notes.txt",
            "a pick at the bottle root maps back to C:\\"
        );
    }

    /// `parse_ofn_filter` keeps only simple `*.ext` glob groups; complex or
    /// catch-all patterns drop the group (a wrong native filter grays out
    /// every file on macOS, which is worse than showing all files).
    #[test]
    fn parse_ofn_filter_keeps_simple_globs_only() {
        // A `*.*` catch-all pair is dropped; the simple pair survives.
        let filters = parse_ofn_filter(&[
            "Text Documents".to_owned(),
            "*.txt".to_owned(),
            "All Files".to_owned(),
            "*.*".to_owned(),
        ]);
        assert_eq!(filters.len(), 1, "All Files (*.*) is dropped");
        assert_eq!(filters[0].name, "Text Documents");
        assert_eq!(filters[0].patterns, vec!["*.txt".to_owned()]);

        // Semicolon-separated simple globs survive as one filter group.
        let filters = parse_ofn_filter(&["Code".to_owned(), "*.rs;*.toml".to_owned()]);
        assert_eq!(filters.len(), 1);
        assert_eq!(
            filters[0].patterns,
            vec!["*.rs".to_owned(), "*.toml".to_owned()]
        );

        // Complex patterns drop the whole group (nothing to show → no filter).
        assert!(parse_ofn_filter(&["Any".to_owned(), "*".to_owned()]).is_empty());
        assert!(parse_ofn_filter(&["All".to_owned(), "*.*".to_owned()]).is_empty());
        assert!(parse_ofn_filter(&["Multi".to_owned(), "*.tar.gz".to_owned()]).is_empty());
        assert!(parse_ofn_filter(&["Bare".to_owned(), "readme.txt".to_owned()]).is_empty());
        assert!(parse_ofn_filter(&["Empty".to_owned(), String::new()]).is_empty());
        // No filter at all → empty.
        assert!(parse_ofn_filter(&[]).is_empty());
    }

    #[test]
    fn simple_filter_glob_rejects_catchalls_and_complex_patterns() {
        assert!(is_simple_filter_glob("*.txt"));
        assert!(is_simple_filter_glob("*.TXT"));
        assert!(!is_simple_filter_glob("*.*"));
        assert!(!is_simple_filter_glob("*"));
        assert!(!is_simple_filter_glob("*.tar.gz"));
        assert!(!is_simple_filter_glob("*.doc;*.txt"));
        assert!(!is_simple_filter_glob("readme.txt"));
        assert!(!is_simple_filter_glob(""));
    }

    // ── ChooseFontW (L6) ──────────────────────────────────────────────────

    /// Write a `LOGFONTW` into guest memory at `ptr`.
    fn write_logfont(engine: &mut IcedCpu, ptr: u64, face: &str, charset: u8) {
        engine.mem_write(ptr, &0_i32.to_le_bytes()).ok(); // lfHeight (0 → 12 pt seed)
        engine.mem_write(ptr + 0x10, &400_i32.to_le_bytes()).ok(); // lfWeight = FW_NORMAL
        engine
            .mem_write(ptr + 0x14, &(u32::from(charset) << 24).to_le_bytes())
            .ok(); // lfItalic/Underline/StrikeOut = 0, lfCharSet = charset
        engine.mem_write(ptr + 0x1C, &utf16_bytes(face)).ok(); // lfFaceName
    }

    /// Write a `CHOOSEFONTW` into guest memory at `cf_ptr` (Win64 layout).
    fn write_choosefont(engine: &mut IcedCpu, cf_ptr: u64, logfont_ptr: u64, flags: u32, rgb: u32) {
        engine.mem_write(cf_ptr, &0x60_u32.to_le_bytes()).ok(); // lStructSize
        engine.mem_write(cf_ptr + 8, &0_u64.to_le_bytes()).ok(); // hwndOwner
        engine
            .mem_write(cf_ptr + 0x18, &logfont_ptr.to_le_bytes())
            .ok(); // lpLogFont
        engine.mem_write(cf_ptr + 0x20, &0_u32.to_le_bytes()).ok(); // iPointSize
        engine.mem_write(cf_ptr + 0x24, &flags.to_le_bytes()).ok(); // Flags
        engine.mem_write(cf_ptr + 0x28, &rgb.to_le_bytes()).ok(); // rgbColors
    }

    /// Drive `ChooseFontW` with a scripted policy; returns the result value.
    fn dispatch_choose_font(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        policy: FontDialogPolicy,
    ) -> anyhow::Result<WinApiHandlerResult> {
        state.window_state().font_dialog_policy = policy;
        write_regs(engine, 0x5000, 0, 0, 0);
        handle_choose_font_w(&mut HandlerContext::new(engine, test_environment(), state))
    }

    /// A font-dialog test scaffold: `CHOOSEFONTW` at 0x5000, `LOGFONTW` at
    /// 0x6000, loop/proc stubs wired, Interactive policy.
    fn font_dialog_scaffold(engine: &mut IcedCpu, state: &mut WinApiState, face: &str, flags: u32) {
        state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
        state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
        write_logfont(engine, 0x6000, face, 1); // DEFAULT_CHARSET
        write_choosefont(engine, 0x5000, 0x6000, flags, 0x00_30_50);
        dispatch_choose_font(engine, state, FontDialogPolicy::Interactive)
            .expect_err("interactive must request the modal loop");
    }

    #[test]
    fn choose_font_cancel_policy_returns_false() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_logfont(&mut engine, 0x6000, "Arial", 1);
        write_choosefont(&mut engine, 0x5000, 0x6000, 0x1 | 0x40 | 0x100, 0);

        let result = dispatch_choose_font(&mut engine, &mut state, FontDialogPolicy::Cancel)
            .expect("cancel must succeed");
        assert_eq!(result.return_value, 0);
        assert!(state.window_state().font_dialog.is_none());
        // The LOGFONTW is untouched by a cancel.
        assert_eq!(read_guest_utf16(&mut engine, 0x601C, 32), "Arial");
    }

    #[test]
    fn choose_font_without_loop_machinery_falls_back_to_cancel() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_logfont(&mut engine, 0x6000, "Arial", 1);
        write_choosefont(&mut engine, 0x5000, 0x6000, 0, 0);

        let result = dispatch_choose_font(&mut engine, &mut state, FontDialogPolicy::Interactive)
            .expect("fallback must succeed");
        assert_eq!(result.return_value, 0, "no host dialog → cancel");
        assert!(state.window_state().font_dialog.is_none());
    }

    #[test]
    fn choose_font_interactive_builds_dialog_and_requests_loop() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);

        let dialog_hwnd = state
            .window_state()
            .font_dialog
            .as_ref()
            .expect("session recorded")
            .dialog_hwnd;
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();

        // The dialog + its controls exist; the dialog carries the proc stub.
        // dialog + "Font:" label + family list + "Size:" label + size list +
        // Strikeout + Underline + OK + Cancel = 9 windows.
        let windows = &state.window_state().windows;
        assert_eq!(windows.len(), 9, "font dialog + 8 controls");
        let dialog = find_window(&mut state, dialog_hwnd).expect("dialog window");
        assert_eq!(
            dialog.dialog_proc, 0x7000_0040_B100,
            "dialog proc = file-dialog stub"
        );
        // Modal: depth up, dialog active, family list focused.
        assert_eq!(state.lock_message_queue().dialog_depth, 1);
        assert_eq!(
            state.window_state().active_window_handle.as_u64(),
            dialog_hwnd
        );
        assert_eq!(
            state.window_state().focus_window_handle.as_u64(),
            session.family_list_hwnd
        );
        // The family list carries the host database families with a selection.
        let ControlState::ListBox {
            items, sel_index, ..
        } = state
            .window_state()
            .control_states
            .get(&Hwnd::from(session.family_list_hwnd))
            .expect("family listbox state")
        else {
            panic!("family list is a listbox");
        };
        assert!(!items.is_empty(), "family list seeded from the host db");
        assert_eq!(*sel_index, 0, "default selection is the first family");
        // The size list offers whole points 8..72 with the 12 pt seed.
        let ControlState::ListBox {
            items: sizes,
            sel_index: size_sel,
            ..
        } = state
            .window_state()
            .control_states
            .get(&Hwnd::from(session.size_list_hwnd))
            .expect("size listbox state")
        else {
            panic!("size list is a listbox");
        };
        assert_eq!(sizes.len(), 65, "8..72 inclusive");
        assert_eq!(*size_sel, 4, "lfHeight 0 → 12 pt seed");
    }

    /// Paint the whole font-dialog subtree exactly like the pump's repaint
    /// cycle: the dialog face first (WM_PAINT → paint_dialog), then each
    /// visible control's WM_PAINT via the control dispatch.
    fn paint_font_dialog_subtree(engine: &mut IcedCpu, state: &mut WinApiState, dialog_hwnd: u64) {
        crate::user32::dialog::paint_dialog(state, dialog_hwnd);
        let children: Vec<u64> = state
            .window_state()
            .windows
            .iter()
            .filter(|w| w.parent_handle == Hwnd::from(dialog_hwnd))
            .map(|w| w.handle.as_u64())
            .collect();
        for hwnd in children {
            crate::user32::dispatch_control_proc(
                engine,
                state,
                hwnd,
                crate::user32::WinMsg::WM_PAINT.as_u32(),
                0,
                0,
            )
            .expect("control WM_PAINT");
        }
    }

    #[test]
    fn font_dialog_survives_control_click_in_owner_frame() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);

        // Open the font dialog parented to the owner (hwndOwner → owner).
        state.window_state().file_dialog_loop_va = 0x7000_0040_B000;
        state.window_state().file_dialog_proc_va = 0x7000_0040_B100;
        write_logfont(&mut engine, 0x6000, "", 1);
        write_choosefont(&mut engine, 0x5000, 0x6000, 0x1 | 0x40, 0x00_30_50);
        engine.mem_write(0x5008, &owner_hwnd.to_le_bytes()).ok();
        dispatch_choose_font(&mut engine, &mut state, FontDialogPolicy::Interactive)
            .expect_err("interactive must request the modal loop");
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();
        let dialog_hwnd = session.dialog_hwnd;

        // Open cycle: paint the face + every control, then drain.
        paint_font_dialog_subtree(&mut engine, &mut state, dialog_hwnd);
        state.present().drain_pending_publishes();

        let owner = Hwnd::from(owner_hwnd);
        let sample = |state: &mut WinApiState| -> Option<u32> {
            let frame = state.present().published.get(&owner)?.clone();
            let idx = 190_usize * frame.width as usize + 235_usize;
            frame.pixels.get(idx).copied()
        };
        // The dialog face (BTNFACE) must be present in the owner frame.
        assert_eq!(
            sample(&mut state),
            Some(0x00F0_F0F0),
            "dialog face must appear in the owner frame after the open cycle"
        );

        // Click the family listbox (dialog-relative (72,8,168,140); click at
        // child-relative (20,40) → item row 2).
        let lparam = u64::from(40_u32 << 16 | 20_u32);
        let click = crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            session.family_list_hwnd,
            crate::user32::WinMsg::WM_LBUTTONDOWN.as_u32(),
            1,
            lparam,
        );
        assert!(
            click.is_err(),
            "the selection change notifies the dialog proc"
        );

        // The click invalidated the listbox: repaint it (the next cycle).
        crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            session.family_list_hwnd,
            crate::user32::WinMsg::WM_PAINT.as_u32(),
            0,
            0,
        )
        .expect("listbox WM_PAINT");
        state.present().drain_pending_publishes();

        assert_eq!(
            sample(&mut state),
            Some(0x00F0_F0F0),
            "dialog face must SURVIVE a click on a listbox control"
        );
        // The listbox area itself is white (COLOR_WINDOW) and the clicked row
        // is highlighted (COLOR_HIGHLIGHT) somewhere in the listbox rect
        // (302,193)-(470,333) in the owner — the selection followed the click.
        let frame = state
            .present()
            .published
            .get(&owner)
            .expect("owner frame after click")
            .clone();
        let highlight = (193..333).fold(0_u32, |acc, y| {
            acc + (302..470).fold(0_u32, |acc, x| {
                let idx = y as usize * frame.width as usize + x as usize;
                acc + u32::from(frame.pixels.get(idx).copied() == Some(0x0000_78D7))
            })
        });
        assert!(
            highlight > 100,
            "the clicked listbox row must be highlighted (COLOR_HIGHLIGHT); found {highlight} px"
        );
    }

    #[test]
    fn font_dialog_controls_fit_inside_the_dialog_bounds() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();
        let dialog = find_window(&mut state, session.dialog_hwnd).expect("dialog window");
        let (dialog_w, dialog_h) = (dialog.width, dialog.height);
        assert_eq!(
            (dialog_w, dialog_h),
            (340, 260),
            "font dialog is ~320x260-ish"
        );

        let children: Vec<(i32, i32, i32, i32, String)> = state
            .window_state()
            .windows
            .iter()
            .filter(|w| w.parent_handle == Hwnd::from(session.dialog_hwnd))
            .map(|w| (w.x, w.y, w.width, w.height, w.class_name.clone()))
            .collect();
        assert_eq!(children.len(), 8, "label + list + label + list + 4 buttons");
        for (x, y, w, h, class) in children {
            assert!(x >= 0 && y >= 0, "{class} sits at a negative position");
            assert!(
                x.saturating_add(w) <= dialog_w,
                "{class} overflows the dialog's right edge (x={x} w={w} dialog_w={dialog_w})"
            );
            assert!(
                y.saturating_add(h) <= dialog_h,
                "{class} overflows the dialog's bottom edge (y={y} h={h} \
                 dialog_h={dialog_h}) — the Strikeout/Underline buttons were \
                 outside the dialog before the height fix"
            );
        }
    }

    #[test]
    fn font_dialog_family_list_scrolls_via_wheel_and_arrows() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();

        // The family list holds the whole host database — far more rows than
        // the 140 px tall listbox can show, so the wheel must scroll it.
        let family = session.family_list_hwnd;
        let list_state = |state: &mut WinApiState| {
            let ControlState::ListBox {
                items,
                sel_index,
                first_visible,
            } = state
                .window_state()
                .control_states
                .get(&Hwnd::from(family))
                .expect("family listbox state")
            else {
                panic!("family list is a listbox");
            };
            (items.len(), *sel_index, *first_visible)
        };
        let (count, sel, first) = list_state(&mut state);
        assert!(count > 10, "the host db must overflow the listbox viewport");
        assert_eq!(sel, 0, "default selection is the first family");
        // The seeded selection (index 0) is already visible — no initial scroll.
        assert_eq!(first, 0);

        // Wheel down over the listbox: the viewport scrolls 3 rows.
        let wheel_down = u64::from(u16::MAX - 119) << 16; // delta = -120
        crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            family,
            crate::user32::WinMsg::WM_MOUSEWHEEL.as_u32(),
            wheel_down,
            0,
        )
        .expect("wheel scrolls the family list");
        let (_, _, first) = list_state(&mut state);
        assert_eq!(first, 3, "one wheel notch scrolls 3 rows");

        // Arrow keys move the selection and keep it visible (focused).
        let keydown = crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            family,
            crate::user32::WinMsg::WM_KEYDOWN.as_u32(),
            0x28, // VK_DOWN
            0,
        );
        assert!(
            keydown.is_err(),
            "a selection change notifies the dialog proc"
        );
        let (_, sel, first) = list_state(&mut state);
        assert_eq!(sel, 1, "VK_DOWN moves the selection one row");
        assert!(
            sel >= i32::try_from(first).unwrap_or(0),
            "the selection stays visible after the key move"
        );
    }

    #[test]
    fn end_dialog_font_writes_logfont_and_choosefont_back() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();

        // The seeded family is whatever the host db lists first; the write-back
        // must mirror exactly that (plus the seeded 12 pt size).
        let ControlState::ListBox {
            items, sel_index, ..
        } = state
            .window_state()
            .control_states
            .get(&Hwnd::from(session.family_list_hwnd))
            .expect("family listbox state")
        else {
            panic!("family list is a listbox");
        };
        let family = items
            .get(usize::try_from(*sel_index).unwrap_or(0))
            .unwrap()
            .clone();

        // OK: EndDialog(1) → the selection lands in the LOGFONTW + CHOOSEFONTW.
        write_regs(&mut engine, session.dialog_hwnd, IDOK, 0, 0);
        let result = handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        assert_eq!(result.return_value, 1);

        // LOGFONTW: lfHeight = -MulDiv(120, 96, 720) = -16; face written;
        // underline/strikeout/charset preserved (0/0/1); weight untouched.
        let mut height = [0_u8; 4];
        engine.mem_read(0x6000, &mut height).ok();
        assert_eq!(i32::from_le_bytes(height), -16, "12 pt at 96 DPI → -16 px");
        assert_eq!(read_guest_utf16(&mut engine, 0x601C, 32), family);
        let mut effects = [0_u8; 4];
        engine.mem_read(0x6014, &mut effects).ok();
        assert_eq!(
            u32::from_le_bytes(effects),
            1 << 24,
            "charset preserved, effects off"
        );
        let mut weight = [0_u8; 4];
        engine.mem_read(0x6010, &mut weight).ok();
        assert_eq!(i32::from_le_bytes(weight), 400, "lfWeight preserved");

        // CHOOSEFONTW: iPointSize in tenths, Flags OR CF_SCREENFONTS, rgbColors.
        assert_eq!(
            read_guest_u32_at(&mut engine, 0x5020),
            120,
            "12 pt in tenths"
        );
        assert_eq!(
            read_guest_u32_at(&mut engine, 0x5024),
            0x1 | 0x40 | 0x1,
            "guest flags + CF_SCREENFONTS"
        );
        assert_eq!(
            read_guest_u32_at(&mut engine, 0x5028),
            0x00_30_50,
            "rgbColors preserved"
        );
        assert!(
            state.window_state().font_dialog.is_none(),
            "session cleared"
        );
    }

    #[test]
    fn end_dialog_font_effects_toggle_keeps_dialog_open() {
        let mut engine = test_engine();
        let mut state = test_state();
        font_dialog_scaffold(&mut engine, &mut state, "", 0x1 | 0x40);
        let session = state.window_state().font_dialog.as_ref().unwrap().clone();

        // EndDialog(strikeout sentinel) → toggle, no close.
        write_regs(
            &mut engine,
            session.dialog_hwnd,
            super::FONT_DLG_STRIKEOUT_ID,
            0,
            0,
        );
        let result = handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        assert_eq!(result.return_value, 1, "EndDialog itself succeeds");
        assert!(
            state.window_state().font_dialog.is_some(),
            "toggle must not close the dialog"
        );
        assert!(
            state
                .window_state()
                .font_dialog
                .as_ref()
                .unwrap()
                .strikeout_checked
        );
        assert!(
            !state
                .window_state()
                .font_dialog
                .as_ref()
                .unwrap()
                .underline_checked
        );
        let strikeout = find_window(&mut state, session.strikeout_hwnd).expect("strikeout button");
        assert_eq!(strikeout.control_text, "[x] Strikeout");

        // The underline toggle flips its own checkbox.
        write_regs(
            &mut engine,
            session.dialog_hwnd,
            super::FONT_DLG_UNDERLINE_ID,
            0,
            0,
        );
        handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        assert!(
            state
                .window_state()
                .font_dialog
                .as_ref()
                .unwrap()
                .underline_checked
        );

        // OK now closes and writes the toggled effects into the LOGFONTW.
        write_regs(&mut engine, session.dialog_hwnd, IDOK, 0, 0);
        handle_end_dialog(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("EndDialog succeeds");
        let mut effects = [0_u8; 4];
        engine.mem_read(0x6014, &mut effects).ok();
        assert_eq!(
            u32::from_le_bytes(effects),
            (1 << 8) | (1 << 16) | (1 << 24),
            "underline + strikeout + charset written on OK"
        );
        assert!(state.window_state().font_dialog.is_none());
    }

    #[test]
    fn print_dlg_and_page_setup_dlg_return_false() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_regs(&mut engine, 0x5000, 0, 0, 0);

        let print_result = handle_print_dlg_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("PrintDlgW succeeds");
        assert_eq!(
            print_result.return_value, 0,
            "PrintDlgW simulates user-cancel"
        );

        let setup_result = handle_page_setup_dlg_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("PageSetupDlgW succeeds");
        assert_eq!(
            setup_result.return_value, 0,
            "PageSetupDlgW simulates user-cancel"
        );
    }

    #[test]
    fn font_dialog_family_enumeration_has_system_families() {
        let families = dialog_family_names();
        assert!(
            !families.is_empty(),
            "host fontdb must name at least one family"
        );
        assert!(families.windows(2).all(|pair| pair[0] <= pair[1]), "sorted");
    }

    // ── FindTextW / ReplaceTextW (Task 4.2) ───────────────────────────────

    const FR_TEST_DOWN: u32 = 0x0001;
    const FR_TEST_MATCHCASE: u32 = 0x0004;
    const FR_TEST_FINDNEXT: u32 = 0x0008;
    const FR_TEST_REPLACE: u32 = 0x0010;
    const FR_TEST_REPLACEALL: u32 = 0x0020;
    const FR_TEST_DIALOGTERM: u32 = 0x0040;

    /// Write a `FINDREPLACE` (Win64) into guest memory at `fr_ptr`, with the
    /// find/replace string buffers at the fixed test addresses 0x6000/0x6100.
    fn write_findreplace(engine: &mut IcedCpu, fr_ptr: u64, owner: u64, flags: u32) {
        engine.mem_write(fr_ptr, &0x58_u32.to_le_bytes()).ok(); // lStructSize
        engine.mem_write(fr_ptr + 8, &owner.to_le_bytes()).ok(); // hwndOwner
        engine.mem_write(fr_ptr + 16, &0_u64.to_le_bytes()).ok(); // hInstance
        engine.mem_write(fr_ptr + 24, &flags.to_le_bytes()).ok(); // Flags
        engine
            .mem_write(fr_ptr + 32, &0x6000_u64.to_le_bytes())
            .ok(); // lpstrFindWhat
        engine
            .mem_write(fr_ptr + 40, &0x6100_u64.to_le_bytes())
            .ok(); // lpstrReplaceWith
        engine.mem_write(fr_ptr + 48, &64_u32.to_le_bytes()).ok(); // wFindWhatLen
        engine.mem_write(fr_ptr + 52, &64_u32.to_le_bytes()).ok(); // wReplaceWithLen
    }

    fn read_guest_u32_at(engine: &mut IcedCpu, address: u64) -> u32 {
        let mut bytes = [0_u8; 4];
        engine.mem_read(address, &mut bytes).ok();
        u32::from_le_bytes(bytes)
    }

    /// Create a plain top-level window for the dialog to be owned by.
    fn create_owner_window(state: &mut WinApiState) -> u64 {
        let (hwnd, _, _) = create_window_record(
            state,
            CreateWindowRequest {
                class_identifier: WindowClassIdentifier::Name("Owner".to_owned()),
                title: "Owner".to_owned(),
                style: WS_VISIBLE,
                extended_style: 0,
                parent_handle: 0,
                menu_handle: 0,
                instance_handle: 0,
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
            false,
        )
        .expect("owner window created");
        hwnd
    }

    /// Open a find (or replace) dialog the way the guest would: `FindTextW`
    /// / `ReplaceTextW` with a seeded `FINDREPLACE`; returns the dialog hwnd.
    fn open_find_dialog(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        owner_hwnd: u64,
        replace: bool,
    ) -> u64 {
        let find_buf = 0x6000;
        let replace_buf = 0x6100;
        engine.mem_write(find_buf, &utf16_bytes("needle")).ok();
        engine.mem_write(replace_buf, &utf16_bytes("haystack")).ok();
        write_findreplace(engine, 0x5000, owner_hwnd, FR_TEST_DOWN | FR_TEST_MATCHCASE);
        write_regs(engine, 0x5000, 0, 0, 0);
        let result = if replace {
            handle_replace_text_w(&mut HandlerContext::new(engine, test_environment(), state))
        } else {
            handle_find_text_w(&mut HandlerContext::new(engine, test_environment(), state))
        }
        .expect("find dialog opens");
        result.return_value
    }

    /// The queued FINDMSGSTRING, if one was posted.
    fn queued_find_msg(state: &mut WinApiState) -> Option<crate::state::QueuedWindowMessage> {
        let findmsg_id = state
            .window_state()
            .registered_messages
            .get("findmsgstring")
            .copied();
        let queue = state.lock_message_queue();
        findmsg_id.and_then(|id| {
            queue
                .messages
                .iter()
                .find(|message| message.message == id)
                .cloned()
        })
    }

    #[test]
    fn find_text_w_builds_modeless_dialog() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);

        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        assert_ne!(dialog_hwnd, 0, "FindTextW returns the dialog HWND");

        let session = state
            .window_state()
            .find_dialogs
            .first()
            .expect("session recorded")
            .clone();
        assert_eq!(session.dialog_hwnd, dialog_hwnd);
        assert_eq!(session.owner_hwnd, owner_hwnd);
        assert_eq!(session.fr_ptr, 0x5000);
        assert!(!session.replace_mode);
        assert_eq!(session.find_what_ptr, 0x6000);
        assert_eq!(session.find_what_len, 64);

        // The find EDIT is seeded from lpstrFindWhat and takes the focus.
        assert_eq!(
            session.find_edit_hwnd,
            state.window_state().focus_window_handle.as_u64()
        );
        let edit = find_window(&mut state, session.find_edit_hwnd).expect("find edit");
        assert_eq!(edit.control_text, "needle");
        // The checkbox state mirrors the guest's initial Flags (FR_MATCHCASE).
        assert!(session.match_case_checked);
        assert!(!session.whole_word_checked);

        // Modeless: the message-queue dialog depth is untouched and the
        // dialog + its controls exist as ordinary windows.
        assert_eq!(state.lock_message_queue().dialog_depth, 0);
        assert!(
            find_window(&mut state, dialog_hwnd).is_some(),
            "dialog window exists"
        );
        let controls = state
            .window_state()
            .windows
            .iter()
            .filter(|w| w.parent_handle == Hwnd::from(dialog_hwnd))
            .count();
        assert_eq!(controls, 6, "label + edit + 2 checkboxes + 2 buttons");
    }

    #[test]
    fn find_text_w_null_returns_zero() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_regs(&mut engine, 0, 0, 0, 0);
        let result = handle_find_text_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("NULL FindTextW returns zero");
        assert_eq!(result.return_value, 0);
        assert!(state.window_state().find_dialogs.is_empty());
    }

    #[test]
    fn find_next_writes_struct_and_posts_findmsgstring() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        // The guest registered FINDMSGSTRING at startup (like RNotepad does).
        state
            .window_state()
            .registered_messages
            .insert("findmsgstring".to_owned(), 0xC100);

        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let session = state.window_state().find_dialogs.first().unwrap().clone();
        let find_next_hwnd = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_FIND_NEXT_ID))
            .expect("Find Next button")
            .handle
            .as_u64();

        // The user typed a new search string into the find EDIT.
        if let Some(window) = find_window_mut(&mut state, session.find_edit_hwnd) {
            window.control_text = "needle2".to_owned();
        }

        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_FIND_NEXT_ID), BN_CLICKED),
            find_next_hwnd,
        )
        .expect("Find Next handled");

        // The new text lands in the guest lpstrFindWhat buffer.
        assert_eq!(read_guest_utf16(&mut engine, 0x6000, 32), "needle2");
        // Flags: FR_FINDNEXT set, FR_MATCHCASE preserved from the checkbox,
        // FR_DOWN preserved from the guest.
        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_eq!(
            flags & (FR_TEST_FINDNEXT | FR_TEST_MATCHCASE | FR_TEST_DOWN),
            FR_TEST_FINDNEXT | FR_TEST_MATCHCASE | FR_TEST_DOWN,
            "action + checkbox + preserved bits"
        );
        assert_eq!(flags & FR_TEST_REPLACE, 0);
        // FINDMSGSTRING posted to the owner with lParam = the FINDREPLACE VA.
        let posted = queued_find_msg(&mut state).expect("FINDMSGSTRING posted");
        assert_eq!(posted.window_handle.as_u64(), owner_hwnd);
        assert_eq!(posted.message, 0xC100);
        assert_eq!(posted.long_parameter, 0x5000);
        assert_eq!(posted.word_parameter, 0);
        // The dialog stays open (modeless).
        assert_eq!(state.window_state().find_dialogs.len(), 1);
    }

    #[test]
    fn checkbox_toggle_updates_state_and_caption() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let session = state.window_state().find_dialogs.first().unwrap().clone();
        assert!(session.match_case_checked);

        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_MATCH_CASE_ID), BN_CLICKED),
            session.match_case_hwnd,
        )
        .expect("checkbox toggle handled");

        let session = state.window_state().find_dialogs.first().unwrap().clone();
        assert!(!session.match_case_checked);
        let checkbox = find_window(&mut state, session.match_case_hwnd).expect("checkbox");
        assert_eq!(checkbox.control_text, "[ ] Match case");
        // No FINDMSGSTRING is posted for a checkbox toggle.
        assert!(state.lock_message_queue().messages.is_empty());
    }

    #[test]
    fn cancel_posts_dialogterm_and_destroys_dialog() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let window_count = state.window_state().windows.len();
        let cancel_hwnd = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_CANCEL_ID))
            .expect("Cancel button")
            .handle
            .as_u64();

        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_CANCEL_ID), BN_CLICKED),
            cancel_hwnd,
        )
        .expect("Cancel handled");

        // FR_DIALOGTERM posted (the fallback registration allocates the id).
        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_ne!(flags & FR_TEST_DIALOGTERM, 0);
        let posted = queued_find_msg(&mut state).expect("FINDMSGSTRING posted");
        assert_eq!(posted.window_handle.as_u64(), owner_hwnd);
        assert_eq!(posted.long_parameter, 0x5000);
        // The dialog + its 6 controls are gone; only the owner remains.
        assert_eq!(state.window_state().windows.len(), window_count - 7);
        assert!(state.window_state().find_dialogs.is_empty());
        assert!(
            state
                .window_state()
                .windows
                .iter()
                .all(|w| w.handle.as_u64() != dialog_hwnd)
        );
    }

    #[test]
    fn replace_text_w_builds_replace_dialog_and_submits() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        state
            .window_state()
            .registered_messages
            .insert("findmsgstring".to_owned(), 0xC100);

        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, true);
        let session = state.window_state().find_dialogs.first().unwrap().clone();
        assert!(session.replace_mode);
        assert_ne!(session.replace_edit_hwnd, 0);
        let replace_edit =
            find_window(&mut state, session.replace_edit_hwnd).expect("replace edit");
        assert_eq!(replace_edit.control_text, "haystack");

        // The user typed into both edits, then pressed Replace.
        if let Some(window) = find_window_mut(&mut state, session.find_edit_hwnd) {
            window.control_text = "a".to_owned();
        }
        if let Some(window) = find_window_mut(&mut state, session.replace_edit_hwnd) {
            window.control_text = "b".to_owned();
        }
        let replace_button = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_REPLACE_ID))
            .expect("Replace button")
            .handle
            .as_u64();
        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_REPLACE_ID), BN_CLICKED),
            replace_button,
        )
        .expect("Replace handled");

        assert_eq!(read_guest_utf16(&mut engine, 0x6000, 32), "a");
        assert_eq!(read_guest_utf16(&mut engine, 0x6100, 32), "b");
        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_ne!(flags & FR_TEST_REPLACE, 0);
        assert_eq!(flags & FR_TEST_REPLACEALL, 0);
        let posted = queued_find_msg(&mut state).expect("FINDMSGSTRING posted");
        assert_eq!(posted.long_parameter, 0x5000);
        assert_eq!(state.window_state().find_dialogs.len(), 1);
    }

    #[test]
    fn replace_all_sets_replaceall_flag() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        state
            .window_state()
            .registered_messages
            .insert("findmsgstring".to_owned(), 0xC100);
        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, true);
        let replace_all_button = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_REPLACE_ALL_ID))
            .expect("Replace All button")
            .handle
            .as_u64();

        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_REPLACE_ALL_ID), BN_CLICKED),
            replace_all_button,
        )
        .expect("Replace All handled");

        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_ne!(flags & FR_TEST_REPLACEALL, 0);
        assert_eq!(flags & FR_TEST_REPLACE, 0);
        assert_eq!(state.window_state().find_dialogs.len(), 1);
    }

    #[test]
    fn find_next_button_click_flows_through_control_dispatch() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        state
            .window_state()
            .registered_messages
            .insert("findmsgstring".to_owned(), 0xC100);
        open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let find_next_hwnd = state
            .window_state()
            .windows
            .iter()
            .find(|w| w.menu_handle == u64::from(super::FIND_DLG_FIND_NEXT_ID))
            .expect("Find Next button")
            .handle
            .as_u64();

        // A real mouse click: the guest's DispatchMessage routes the press
        // and release to the Button control, whose WM_LBUTTONUP delivers
        // BN_CLICKED to the find dialog (host-side, via deliver_button_command).
        crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            find_next_hwnd,
            crate::user32::WinMsg::WM_LBUTTONDOWN.as_u32(),
            0,
            0,
        )
        .expect("button press");
        crate::user32::dispatch_control_proc(
            &mut engine,
            &mut state,
            find_next_hwnd,
            crate::user32::WinMsg::WM_LBUTTONUP.as_u32(),
            0,
            0,
        )
        .expect("button release");

        // The click submitted the dialog: FINDMSGSTRING to the owner with
        // FR_FINDNEXT set, and the dialog stays open (modeless).
        let posted = queued_find_msg(&mut state).expect("FINDMSGSTRING posted via click");
        assert_eq!(posted.window_handle.as_u64(), owner_hwnd);
        assert_eq!(posted.long_parameter, 0x5000);
        let flags = read_guest_u32_at(&mut engine, 0x5000 + 24);
        assert_ne!(flags & FR_TEST_FINDNEXT, 0);
        assert_eq!(state.window_state().find_dialogs.len(), 1);
    }

    #[test]
    fn find_and_replace_dialogs_coexist() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        let find_dialog = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        let replace_dialog = open_find_dialog(&mut engine, &mut state, owner_hwnd, true);

        assert_ne!(find_dialog, replace_dialog);
        assert_eq!(state.window_state().find_dialogs.len(), 2);
        // Each dialog is independently detectable.
        assert!(is_find_dialog_window(&state, find_dialog));
        assert!(is_find_dialog_window(&state, replace_dialog));
        assert!(!is_find_dialog_window(&state, owner_hwnd));
    }

    #[test]
    fn is_find_dialog_window_false_when_closed() {
        let mut engine = test_engine();
        let mut state = test_state();
        let owner_hwnd = create_owner_window(&mut state);
        let dialog_hwnd = open_find_dialog(&mut engine, &mut state, owner_hwnd, false);
        assert!(is_find_dialog_window(&state, dialog_hwnd));
        // After the dialog is destroyed the session is gone.
        let session = state.window_state().find_dialogs.first().unwrap().clone();
        handle_find_dialog_command(
            &mut engine,
            &mut state,
            dialog_hwnd,
            make_command_wparam(u64::from(super::FIND_DLG_CANCEL_ID), BN_CLICKED),
            session.match_case_hwnd,
        )
        .expect("cancel");
        assert!(!is_find_dialog_window(&state, dialog_hwnd));
    }
}
