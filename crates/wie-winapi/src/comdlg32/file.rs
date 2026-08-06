//! Open/save file dialogs (`GetOpenFileName` / `GetSaveFileName`) and
//! `GetFileTitle`.

use super::{CDERR_NONE, ES_AUTOHSCROLL, WS_BORDER, resolve_dialog_owner};
use crate::guest_layout::OpenFileName;
use crate::guest_memory::{with_typed_read, with_typed_write};
use crate::guest_string::{
    read_ansi_lossy, read_utf16_lossy, write_ansi_c_string, write_utf16_c_string,
};
use crate::handles::Hwnd;
use crate::state::{
    FileDialogFilter, FileDialogRequest, FileDialogSession, PendingNativeFileDialog,
};
use crate::user32::controls::{ControlClassKind, ControlState};
use crate::user32::{
    BS_DEFPUSHBUTTON, CreateWindowRequest, GuestCallbackRequest, IDCANCEL, IDOK, ModalFrame,
    ModalResult, NativePanelKind, WS_CHILD, WS_CLIPCHILDREN, WS_TABSTOP, WS_VISIBLE,
    WinApiControlSignal, WindowClassIdentifier, create_window_record, find_window, find_window_mut,
    finish_native_panel, open_native_panel, window_client_size,
};
use crate::vfs::VolumeConfig;
use crate::{FileDialogPolicy, HandlerContext, OuterReturn, WinApiHandlerResult, WinApiState};
use anyhow::{Context, Result};

// The `OPENFILENAME` layout lives in `crate::guest_layout::OpenFileName`
// (mingw-w64-verified offsets, including the Vista+ reserved tail); the
// handlers read and write the struct through the typed views. The A-variant
// shares the same offsets (only the pointed-to strings are ANSI).

/// `FNERR_INVALIDFILENAME` — the file name in `lpstrFile` is invalid.
///
/// Set when the native file dialog's pick exists but cannot be served: an
/// Open pick whose host file does not exist (a raced/deleted pick — the open
/// panel never offers nonexistent files) or a pick with no file name. The
/// accept must fail like an invalid name rather than look like the user
/// pressed Cancel (which a program like notepad treats as "abort the action,
/// no error" — the silent no-op that made Save/Open appear broken). A
/// well-formed out-of-bottle pick is no longer an error: it registers a
/// pick-mount and returns TRUE.
pub(crate) const FNERR_INVALIDFILENAME: u32 = 0x1003;
/// Control ids inside the file dialog (must differ from `IDOK`/`IDCANCEL`,
/// which the dialog-proc stub treats as close).
const FILE_DLG_EDIT_ID: u64 = 1000;
const FILE_DLG_LIST_ID: u64 = 1001;

/// File-dialog window size (pixels, classic 8×16 base units at 96 DPI).
const FILE_DLG_CX: i32 = 360;
const FILE_DLG_CY: i32 = 200;
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
    let state = &mut *ctx.state;
    let return_value = u64::from(state.window_state().comm_dlg_extended_error);

    ctx.finish(return_value)
}
/// Handles `comdlg32.dll!GetFileTitleA`.
pub fn handle_get_file_title_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_file_title_impl(ctx, false, "GetFileTitleA")
}

/// Handles `comdlg32.dll!GetFileTitleW`.
pub fn handle_get_file_title_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_file_title_impl(ctx, true, "GetFileTitleW")
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
fn handle_get_file_title_impl(
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

    ctx.finish(return_value)
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
        return ctx.finish(0);
    }

    // One typed read for the whole OPENFILENAME (the four buffer fields the
    // policy and interactive flows write through); the layout + offsets live
    // in `crate::guest_layout::OpenFileName`.
    let ofn = with_typed_read::<OpenFileName, _, _>(engine, ofn_ptr, |ofn| Ok(*ofn))
        .with_context(|| format!("failed to read OPENFILENAME for {api_name}"))?;
    let file_buffer_ptr = ofn.lpstr_file;
    let max_file = ofn.n_max_file;
    let file_title_ptr = ofn.lpstr_file_title;
    let max_file_title = ofn.n_max_file_title;

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

    ctx.finish(return_value)
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
    pub(crate) ofn_ptr: u64,
    pub(crate) file_buffer_ptr: u64,
    pub(crate) max_file: u32,
    pub(crate) file_title_ptr: u64,
    pub(crate) max_file_title: u32,
    pub(crate) path: &'a str,
    pub(crate) unicode: bool,
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
pub(crate) fn directory_of(path: &str) -> &str {
    path.rfind(['\\', '/'])
        .map_or("", |index| path.get(..index).unwrap_or(""))
}

/// The basename of a Windows path (the whole path when there is no separator).
pub(crate) fn basename_of(path: &str) -> &str {
    path.rfind(['\\', '/']).map_or(path, |index| {
        path.get(index.saturating_add(1)..).unwrap_or(path)
    })
}
/// Directory entry names (files and directories) of `guest_dir`, resolved
/// through the host volume mapping and confined to a guest volume.
///
/// `..` may ascend within a volume (`C:\App\..` lists the bottle root) but
/// never above its root (`C:\..`), and only C: (bottle) / D: (bridge, when
/// configured) are listable. Entries whose host path does not map back into
/// a guest volume (e.g. a symlink pointing outside the bottle) are hidden.
/// Empty when the directory is unmapped, escapes the volumes, or unreadable.
pub(crate) fn list_directory(state: &WinApiState, guest_dir: &str) -> Vec<String> {
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
pub(crate) fn resolve_initial_dir(
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
pub(crate) fn is_absolute_windows_path(path: &str) -> bool {
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
pub(crate) fn apply_default_extension(path: &str, def_ext: Option<&str>) -> String {
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
pub(crate) fn finalize_guest_path(session: &FileDialogSession, edit_text: &str) -> String {
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
        return ctx.finish(0);
    }

    let ofn = with_typed_read::<OpenFileName, _, _>(engine, ofn_ptr, |ofn| Ok(*ofn))
        .with_context(|| format!("failed to read OPENFILENAME for {api_name}"))?;
    let owner_raw = ofn.hwnd_owner;
    let initial_dir_ptr = ofn.lpstr_initial_dir;
    let def_ext_ptr = ofn.lpstr_def_ext;

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
        return ctx.finish(0);
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

    // The dialog is modal: an empty GetMessage must yield, not synthesize the
    // regression-mode WM_QUIT, and the dialog takes activation. The path EDIT
    // gets the initial keyboard focus (host-side WM_SETFOCUS).
    let (frame, _signal) = ModalFrame::activate(
        state,
        engine,
        dialog_hwnd,
        Some(edit_hwnd),
        &[dialog_hwnd, edit_hwnd, list_hwnd, ok_hwnd, cancel_hwnd],
    )?;
    // Store the frame so EndDialog's shared teardown can finish this session.
    state
        .window_state()
        .modal_frames
        .insert(Hwnd::from(dialog_hwnd), frame);

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
/// The picked HOST path is served at accept. An in-bottle pick maps through
/// the volumes (`C:\…` / `D:\…`). An out-of-bottle pick (the user browsed
/// outside the bottle via the panel's sidebar) registers a pick-mount — the
/// native dialog IS the user's explicit grant — and returns its guest path
/// (`Z:\pick{N}\{name}`) so the guest can open/save the REAL host file in
/// place. Only a genuinely invalid pick (an Open pick whose file no longer
/// exists) cancels with `FNERR_INVALIDFILENAME`.
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
    let ofn = with_typed_read::<OpenFileName, _, _>(engine, buffer.ofn_ptr, |ofn| Ok(*ofn))
        .with_context(|| format!("failed to read OPENFILENAME for {api_name}"))?;
    let filter_ptr = ofn.lpstr_filter;

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
            let initial_dir_ptr = ofn.lpstr_initial_dir;
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
    // `PendingNativeFileDialog`). The panel is a modal session too: open the
    // frame (depth up, activation captured) so the re-entry's
    // `finish_native_panel` restores the owner.
    state.window_state().pending_native_file_dialog = Some(PendingNativeFileDialog {
        ofn_ptr: buffer.ofn_ptr,
        file_buffer_ptr: buffer.file_buffer_ptr,
        max_file: buffer.max_file,
        file_title_ptr: buffer.file_title_ptr,
        max_file_title: buffer.max_file_title,
        unicode,
        pick: None,
        frame: Some(open_native_panel(state, engine, NativePanelKind::File)?),
    });

    Err(WinApiControlSignal::FileDialogBridgeRequested { request }.into())
}

/// Write the native panel's pick back into the guest `OPENFILENAME` buffer.
///
/// Runs on the handler's re-entry (after the runtime ran the bridge WITHOUT
/// the shared state lock). `None` pick = the user cancelled (or the bridge
/// vanished mid-call — a racing teardown must not hang the guest). An
/// out-of-bottle pick registers a pick-mount (see the `vfs::pick_mount`
/// module) and returns its guest path with TRUE; a genuinely invalid pick
/// (an Open pick whose host file does not exist) returns FALSE with
/// `lpstrFile` untouched and sets `CommDlgExtendedError` to
/// `FNERR_INVALIDFILENAME` so the failure is visible to the guest instead of
/// an indistinguishable cancel.
fn finish_native_file_dialog(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    api_name: &str,
    pending: PendingNativeFileDialog,
) -> Result<WinApiHandlerResult> {
    let frame = pending.frame;
    let Some(pick) = pending.pick else {
        state.window_state().comm_dlg_extended_error = CDERR_NONE;
        tracing::info!(api = api_name, "native file dialog cancelled");
        return finish_file_bridge(engine, state, api_name, frame, 0);
    };

    // The consent boundary. An in-bottle pick maps through the volumes (the
    // C: bottle or the optional D: bridge) as before. An OUT-of-bottle pick is
    // the native panel's explicit grant to that one host file — register a
    // pick-mount (`Z:\pick{N}\{name}`) so the guest's later CreateFileW on the
    // returned guest path reads/writes the REAL host file in place (open in
    // place, save in place, created where the user picked). Only a genuinely
    // invalid pick stays a refusal: an Open pick whose host file does not
    // exist (rfd's open panel only offers existing files, so a missing target
    // is a raced/deleted pick) or a pick with no file name (a directory or
    // volume root cannot key a file mount).
    let guest_path = crate::vfs::host_path_to_guest(&state.file_io.volumes, &pick.host_path)
        .or_else(|| {
            let is_save = api_name.contains("Save");
            (is_save || pick.host_path.is_file())
                .then(|| crate::vfs::register_pick_mount(&pick.host_path))
                .flatten()
        });
    let Some(guest_path) = guest_path else {
        // NOT a plain cancel: the pick exists but cannot be served. Surface it
        // as FNERR_INVALIDFILENAME (via CommDlgExtendedError) so a program
        // that depends on the pick (notepad's Save/Open) reports a real
        // failure instead of silently acting as if the user pressed Cancel.
        state.window_state().comm_dlg_extended_error = FNERR_INVALIDFILENAME;
        tracing::warn!(
            api = api_name,
            host = %pick.host_path.display(),
            exists = pick.host_path.is_file(),
            "native file dialog pick cannot be served: outside the bottle and \
             not a mountable file (an Open pick must name an existing file) — \
             refusing with FNERR_INVALIDFILENAME",
        );
        return finish_file_bridge(engine, state, api_name, frame, 0);
    };

    if pending.file_buffer_ptr == 0 || pending.max_file == 0 {
        state.window_state().comm_dlg_extended_error = CDERR_NONE;
        tracing::warn!(
            api = api_name,
            "file dialog bridge accepted but lpstrFile/nMaxFile invalid"
        );
        return finish_file_bridge(engine, state, api_name, frame, 0);
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
    // The post-accept chain's hand-off: the guest path is written back to
    // lpstrFile and this handler returns TRUE — a repro's log ends HERE if the
    // guest never issues the follow-up CreateFileW on the returned path.
    tracing::info!(
        api = api_name,
        %guest_path,
        unicode = pending.unicode,
        ret = 1,
        "native file dialog accepted"
    );
    finish_file_bridge(engine, state, api_name, frame, 1)
}

/// Finish the native bridge's modal frame (opened at the first entry) and
/// return `value` from the file dialog — every native re-entry tail, accept
/// or refuse, closes the frame the same way.
///
/// The per-kind result mapping stays here: the file dialog returns 0 as a
/// cancel and any other value as an `Ok` result; the shared down half
/// ([`finish_native_panel`]) does the frame teardown.
fn finish_file_bridge(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    api_name: &str,
    frame: Option<ModalFrame>,
    value: u64,
) -> Result<WinApiHandlerResult> {
    let result = if value == 0 {
        ModalResult::Cancel
    } else {
        ModalResult::Ok(value)
    };
    if let Some(signal) = finish_native_panel(state, engine, frame, result)? {
        return Err(signal.into());
    }
    file_dialog_return(engine, api_name, value)
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
pub(crate) fn parse_ofn_filter(components: &[String]) -> Vec<FileDialogFilter> {
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
pub(crate) fn is_simple_filter_glob(pattern: &str) -> bool {
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

    // nFileOffset / nFileExtension write-back. OPENFILENAME is an in/out
    // struct: the guest's untouched fields (Flags, lpstrInitialDir,
    // lpTemplateName, the reserved tail, ...) must survive the write. Snapshot
    // the whole struct (it is Copy), edit the two offset fields, write it back
    // — the MENUITEMINFO pattern (two shared-lock borrows).
    let mut ofn = with_typed_read::<OpenFileName, _, _>(engine, request.ofn_ptr, |ofn| Ok(*ofn))
        .context("failed to read OPENFILENAME for the path write-back")?;
    ofn.n_file_offset = file_offset;
    ofn.n_file_extension = extension_offset;
    with_typed_write::<OpenFileName, _, _>(engine, request.ofn_ptr, |ofn_view| {
        *ofn_view = ofn;
        Ok(())
    })
    .context("failed to write OPENFILENAME offsets back")?;

    Ok(())
}

/// Returns `(file_name, nFileOffset, nFileExtension)` for an OPENFILENAME result.
pub(crate) fn split_path_components(path: &str) -> (&str, u16, u16) {
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
