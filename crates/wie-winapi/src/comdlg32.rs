//! Common dialog stubs (`comdlg32.dll`) for open/save file simulation.

use crate::guest_memory::{
    checked_field_address, read_u32 as read_guest_u32, read_u64 as read_guest_u64,
    write_u16 as write_guest_u16, write_u32 as write_guest_u32,
};
use crate::guest_string::{
    read_ansi_lossy, read_utf16_lossy, write_ansi_c_string, write_utf16_c_string,
};
use crate::handles::Hwnd;
use crate::state::{FileDialogSession, FindDialogSession};
use crate::user32::controls::{ControlClassKind, ControlState};
use crate::user32::{
    BS_DEFPUSHBUTTON, CommandPayload, CreateWindowRequest, GuestCallbackRequest, IDCANCEL, IDOK,
    QueuedWindowMessage, WS_CHILD, WS_CLIPCHILDREN, WS_TABSTOP, WS_VISIBLE, WinApiControlSignal,
    WindowClassIdentifier, create_window_record, deliver_focus_change, find_window,
    find_window_mut, is_known_window, window_client_size,
};
use crate::{FileDialogPolicy, HandlerContext, OuterReturn, WinApiHandlerResult, WinApiState};
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

        // No scripted decision: build the host file dialog and run its modal
        // message loop in-guest (the DialogBoxParam pattern), so the guest
        // stays responsive while the user picks a path. Returns a
        // `GuestCallbackRequested` signal to the runtime.
        FileDialogPolicy::Interactive => {
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
/// through the host volume mapping. Empty when the directory cannot be listed.
fn list_directory(state: &WinApiState, guest_dir: &str) -> Vec<String> {
    let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, guest_dir) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&map.host) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    // Cap the listing so a huge directory cannot starve the paint cycle.
    names.truncate(1024);
    names
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
        if path.is_empty() {
            0
        } else {
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
    let initial_dir = if initial_dir_ptr != 0 {
        read_ofn_string(engine, initial_dir_ptr, unicode, api_name)?
    } else {
        let file_dir = directory_of(&initial_file);
        if file_dir.is_empty() {
            current_guest_directory(state)
        } else {
            file_dir.to_owned()
        }
    };
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
    let _ = deliver_focus_change(state, engine, 0, edit_hwnd, OuterReturn::Fixed(edit_hwnd))?;

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
    let _ = deliver_focus_change(
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
        apply_default_extension, basename_of, complete_file_dialog, directory_of,
        finalize_guest_path, handle_find_dialog_command, handle_find_text_w,
        handle_get_open_file_name_w, handle_replace_text_w, is_absolute_windows_path,
        is_find_dialog_window, split_path_components,
    };
    use crate::guest_heap::GuestHeap;
    use crate::handles::Hwnd;
    use crate::present::MessageQueue;
    use crate::state::{
        FileDialogSession, FileIoState, HeapState, ProcessState, WinApiEnvironment,
    };
    use crate::sync_obj::SyncState;
    use crate::thread::ThreadState;
    use crate::user32::dialog::handle_end_dialog;
    use crate::user32::{
        BN_CLICKED, CreateWindowRequest, IDOK, WS_VISIBLE, WinApiControlSignal,
        WindowClassIdentifier, create_window_record, find_window, find_window_mut,
        make_command_wparam,
    };
    use crate::vfs::VolumeConfig;
    use crate::{
        DEFAULT_ENVIRONMENT, DllStateMap, FileDialogPolicy, HandlerContext, KernelState,
        ModuleState, WinApiHandlerResult, WinApiState,
    };
    use ahash::HashMap;
    use ahash::HashMapExt;
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
