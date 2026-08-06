use super::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY, ERROR_FILE_NOT_FOUND,
    ERROR_PATH_NOT_FOUND, HandlerContext, Result, WinApiHandlerResult, WinApiState,
    guest_dir_exists, paths_match_guest, read_ansi_string_from_cpu, read_wide_string_from_cpu,
    resolve_full_windows_path,
};

/// Handles `KERNEL32.dll!CreateDirectoryW`.
pub fn handle_create_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_create_directory(state, &path);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!CreateDirectoryA`.
pub fn handle_create_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_create_directory(state, &path);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!DeleteFileW`.
pub fn handle_delete_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_delete_file(state, &path);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!DeleteFileA`.
pub fn handle_delete_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_delete_file(state, &path);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!RemoveDirectoryW`.
pub fn handle_remove_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_remove_directory(state, &path);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!RemoveDirectoryA`.
pub fn handle_remove_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_remove_directory(state, &path);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!MoveFileW`.
pub fn handle_move_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let from_ptr = engine.read_rcx()?;
    let to_ptr = engine.read_rdx()?;
    let from = if from_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, from_ptr, 32_768)?
    };
    let to = if to_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, to_ptr, 32_768)?
    };
    let return_value = finish_move_file(state, &from, &to);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!MoveFileA`.
pub fn handle_move_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let from_ptr = engine.read_rcx()?;
    let to_ptr = engine.read_rdx()?;
    let from = if from_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, from_ptr, 32_768)?
    };
    let to = if to_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, to_ptr, 32_768)?
    };
    let return_value = finish_move_file(state, &from, &to);
    ctx.finish(return_value)
}
pub(crate) fn finish_create_directory(state: &mut WinApiState, path: &str) -> u64 {
    if path.is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    }
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
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
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
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
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
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
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
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
