//! Common dialog stubs (`comdlg32.dll`) for open/save file simulation.

use crate::guest_memory::{
    checked_field_address, read_u32 as read_guest_u32, read_u64 as read_guest_u64,
    write_u16 as write_guest_u16,
};
use crate::guest_string::{
    read_ansi_lossy, read_utf16_lossy, write_ansi_c_string, write_utf16_c_string,
};
use crate::{FileDialogPolicy, HandlerContext, WinApiHandlerResult};
use anyhow::{Context, Result};

/// `OPENFILENAME` field offsets on Win64 (8-byte pointer alignment).
const OFN_LPSTR_FILE: u64 = 48;
const OFN_NMAX_FILE: u64 = 56;
const OFN_LPSTR_FILE_TITLE: u64 = 64;
const OFN_NMAX_FILE_TITLE: u64 = 72;
const OFN_FLAGS: u64 = 96;
const OFN_NFILE_OFFSET: u64 = 100;
const OFN_NFILE_EXTENSION: u64 = 102;

/// No extended common-dialog error.
const CDERR_NONE: u32 = 0;

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
    let return_value = match policy {
        FileDialogPolicy::Cancel => {
            state.window_state().comm_dlg_extended_error = CDERR_NONE;
            tracing::debug!(api = api_name, "file dialog cancelled by policy");
            0
        }

        FileDialogPolicy::Accept { path } => {
            if file_buffer_ptr == 0 || max_file == 0 {
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
                        ofn_ptr,
                        file_buffer_ptr,
                        max_file,
                        file_title_ptr,
                        max_file_title,
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
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

struct SelectedPathWrite<'a> {
    ofn_ptr: u64,
    file_buffer_ptr: u64,
    max_file: u32,
    file_title_ptr: u64,
    max_file_title: u32,
    path: &'a str,
    unicode: bool,
}

fn write_selected_path(
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

#[cfg(test)]
mod tests {
    use super::split_path_components;

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
}
