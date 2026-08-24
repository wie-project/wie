use super::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY, ERROR_FILE_NOT_FOUND,
    ERROR_PATH_NOT_FOUND, HandlerContext, Result, WinApiHandlerResult, WinApiState,
    guest_dir_exists, paths_match_guest, read_ansi_string_from_cpu, read_wide_string_from_cpu,
    resolve_full_windows_path,
};

/// Cap for a guest path-argument read (bytes for the A path, UTF-16 units for
/// the W path) — the Win32 max long-path window.
const PATH_ARG_MAX: usize = 32_768;

/// Shared body of `CreateDirectoryA` / `CreateDirectoryW`.
fn create_directory(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let path_va = ctx.engine.read_rcx()?;
    let path = if wide {
        read_wide_string_from_cpu(ctx.engine, path_va, PATH_ARG_MAX)?
    } else {
        read_ansi_string_from_cpu(ctx.engine, path_va, PATH_ARG_MAX)?
    };
    let return_value = finish_create_directory(&mut *ctx.state, &path);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!CreateDirectoryW`.
pub fn handle_create_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    create_directory(ctx, true)
}
/// Handles `KERNEL32.dll!CreateDirectoryA`.
pub fn handle_create_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    create_directory(ctx, false)
}
/// Shared body of `DeleteFileA` / `DeleteFileW`.
fn delete_file(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let path_va = ctx.engine.read_rcx()?;
    let path = if wide {
        read_wide_string_from_cpu(ctx.engine, path_va, PATH_ARG_MAX)?
    } else {
        read_ansi_string_from_cpu(ctx.engine, path_va, PATH_ARG_MAX)?
    };
    let return_value = finish_delete_file(&mut *ctx.state, &path);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!DeleteFileW`.
pub fn handle_delete_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    delete_file(ctx, true)
}
/// Handles `KERNEL32.dll!DeleteFileA`.
pub fn handle_delete_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    delete_file(ctx, false)
}
/// Shared body of `RemoveDirectoryA` / `RemoveDirectoryW`.
fn remove_directory(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let path_va = ctx.engine.read_rcx()?;
    let path = if wide {
        read_wide_string_from_cpu(ctx.engine, path_va, PATH_ARG_MAX)?
    } else {
        read_ansi_string_from_cpu(ctx.engine, path_va, PATH_ARG_MAX)?
    };
    let return_value = finish_remove_directory(&mut *ctx.state, &path);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!RemoveDirectoryW`.
pub fn handle_remove_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    remove_directory(ctx, true)
}
/// Handles `KERNEL32.dll!RemoveDirectoryA`.
pub fn handle_remove_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    remove_directory(ctx, false)
}
/// Shared body of the `MoveFile*` family (two path arguments).
fn move_file(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let from_va = ctx.engine.read_rcx()?;
    let to_va = ctx.engine.read_rdx()?;
    let from = if wide {
        read_wide_string_from_cpu(ctx.engine, from_va, PATH_ARG_MAX)?
    } else {
        read_ansi_string_from_cpu(ctx.engine, from_va, PATH_ARG_MAX)?
    };
    let to = if wide {
        read_wide_string_from_cpu(ctx.engine, to_va, PATH_ARG_MAX)?
    } else {
        read_ansi_string_from_cpu(ctx.engine, to_va, PATH_ARG_MAX)?
    };
    let return_value = finish_move_file(&mut *ctx.state, &from, &to);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!MoveFileW`.
pub fn handle_move_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    move_file(ctx, true)
}
/// Handles `KERNEL32.dll!MoveFileA`.
pub fn handle_move_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    move_file(ctx, false)
}
pub(crate) fn finish_create_directory(state: &mut WinApiState, path: &str) -> u64 {
    if path.is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    }
    state.file_io.sync_volumes();
    let cwd = state.file_io.cwd_utf8();
    let full = resolve_full_windows_path(&cwd, path);
    if guest_dir_exists(state, &full) {
        state.process.last_error = ERROR_ALREADY_EXISTS;
        return 0;
    }
    let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full) else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    };
    if crate::vfs::mkdir_host(&map.host).is_ok() {
        state.process.last_error = 0;
        1
    } else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        0
    }
}

pub(crate) fn finish_delete_file(state: &mut WinApiState, path: &str) -> u64 {
    if path.is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    }
    state.file_io.sync_volumes();
    let cwd = state.file_io.cwd_utf8();
    let full = resolve_full_windows_path(&cwd, path);
    state
        .file_io
        .virtual_files
        .retain(|v| !paths_match_guest(&full, &v.guest_path));
    if let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full) {
        if crate::vfs::remove_file_host(&map.host).is_ok() {
            state.process.last_error = 0;
            return 1;
        }
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        return 0;
    }
    state.process.last_error = ERROR_FILE_NOT_FOUND;
    0
}

pub(crate) fn finish_remove_directory(state: &mut WinApiState, path: &str) -> u64 {
    if path.is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    }
    state.file_io.sync_volumes();
    let cwd = state.file_io.cwd_utf8();
    let full = resolve_full_windows_path(&cwd, path);
    let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full) else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    };
    match crate::vfs::remove_dir_host(&map.host) {
        Ok(()) => {
            state.process.last_error = 0;
            1
        }
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            state.process.last_error = ERROR_DIR_NOT_EMPTY;
            0
        }
        Err(_) => {
            state.process.last_error = ERROR_PATH_NOT_FOUND;
            0
        }
    }
}

pub(crate) fn finish_move_file(state: &mut WinApiState, from: &str, to: &str) -> u64 {
    if from.is_empty() || to.is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    }
    state.file_io.sync_volumes();
    let cwd = state.file_io.cwd_utf8();
    let full_from = resolve_full_windows_path(&cwd, from);
    let full_to = resolve_full_windows_path(&cwd, to);
    let Some(src) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full_from) else {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        return 0;
    };
    let Some(dst) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full_to) else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    };
    if crate::vfs::rename_host(&src.host, &dst.host).is_ok() {
        state.process.last_error = 0;
        1
    } else {
        state.process.last_error = ERROR_ACCESS_DENIED;
        0
    }
}
/// Handles `KERNEL32.dll!MoveFileExW` — `MoveFileW` semantics; the flags
/// (`MOVEFILE_REPLACE_EXISTING` etc.) are accepted and ignored — plain
/// overwrite is already the behaviour of `finish_move_file`.
pub fn handle_move_file_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    move_file(ctx, true)
}
