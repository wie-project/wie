use super::{
    CREATE_ALWAYS, CREATE_NEW, Context, DUPLICATE_CLOSE_SOURCE, ERROR_ACCESS_DENIED,
    ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND,
    ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DRIVE, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
    ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, ERROR_READ_FAULT, FAKE_DISK_CLUSTERS, FAKE_DISK_GIB,
    FAKE_STDERR_HANDLE, FAKE_STDIN_HANDLE, FAKE_STDOUT_HANDLE, FILE_ATTRIBUTE_ARCHIVE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_BEGIN, FILE_CURRENT, FILE_END, FILE_TYPE_CHAR, FILE_TYPE_DISK,
    FILE_TYPE_UNKNOWN, FIXED_SYSTEM_FILETIME, FindHandle, HandlerContext, INVALID_FILE_ATTRIBUTES,
    INVALID_HANDLE_VALUE, INVALID_SET_FILE_POINTER, LOGICAL_DRIVE_TCHARS, OPEN_ALWAYS,
    OPEN_EXISTING, OpenGuestFile, Path, Result, TRUNCATE_EXISTING, WinApiHandlerResult,
    WinApiState, checked_address, get_user_profile_dir_impl, is_main_module_path, low_u32,
    read_ansi_string_from_cpu, read_guest_utf16_lossy, read_stack_u64, read_u16, read_u64,
    read_wide_string_from_cpu, refill_stdin_from_host, ret_bool_true, ret_u64, write_guest_u16,
    write_guest_u32, write_guest_u64, write_guest_utf16_units,
};
use crate::guest_layout::{
    ByHandleFileInformation, FIND_DATA_A_ALT_NAME_OFFSET, FIND_DATA_FILE_NAME_OFFSET,
    FIND_DATA_W_ALT_NAME_OFFSET, FindDataHeader,
};
use crate::guest_memory::with_typed_write;

pub use dir::*;
pub use open::*;
pub use path::*;
pub use rw::*;
pub use time::*;
pub use vol::*;
pub use watch::*;

mod dir;
mod open;
mod path;
mod rw;
mod time;
mod vol;
mod watch;

/// Handles `KERNEL32.dll!GetFileType`.
pub fn handle_get_file_type(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for GetFileType")?;

    let return_value = match handle {
        FAKE_STDIN_HANDLE | FAKE_STDOUT_HANDLE | FAKE_STDERR_HANDLE => FILE_TYPE_CHAR,
        _ if is_open_file_handle(state, handle) => FILE_TYPE_DISK,
        _ => FILE_TYPE_UNKNOWN,
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GetFileAttributesA`.
pub fn handle_get_file_attributes_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_va = engine
        .read_rcx()
        .context("failed to read RCX for GetFileAttributesA")?;

    let path = read_ansi_string_from_cpu(engine, path_va, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = resolve_full_windows_path(&cwd, &path);
    let return_value = file_attributes_for_path(state, &full_path);
    if return_value == INVALID_FILE_ATTRIBUTES {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
    } else {
        state.process.last_error = 0;
    }

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GetFileAttributesW`.
pub fn handle_get_file_attributes_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_va = engine
        .read_rcx()
        .context("failed to read RCX for GetFileAttributesW")?;

    let path = read_wide_string_from_cpu(engine, path_va, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = resolve_full_windows_path(&cwd, &path);
    let return_value = file_attributes_for_path(state, &full_path);
    if return_value == INVALID_FILE_ATTRIBUTES {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
    } else {
        state.process.last_error = 0;
    }

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!FindFirstFileW`.
pub fn handle_find_first_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let pattern_va = engine
        .read_rcx()
        .context("failed to read RCX for FindFirstFileW")?;

    let find_data_va = engine
        .read_rdx()
        .context("failed to read RDX for FindFirstFileW")?;

    let pattern = read_wide_string_from_cpu(engine, pattern_va, 1024)?;
    let return_value = finish_find_first(engine, state, &pattern, find_data_va, true)?;

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!FindFirstFileA`.
pub fn handle_find_first_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let pattern_va = engine
        .read_rcx()
        .context("failed to read RCX for FindFirstFileA")?;

    let find_data_va = engine
        .read_rdx()
        .context("failed to read RDX for FindFirstFileA")?;

    let pattern = read_ansi_string_from_cpu(engine, pattern_va, 1024)?;
    let return_value = finish_find_first(engine, state, &pattern, find_data_va, false)?;

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!FindNextFileW`.
pub fn handle_find_next_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let find_handle = engine
        .read_rcx()
        .context("failed to read RCX for FindNextFileW")?;

    let find_data_va = engine
        .read_rdx()
        .context("failed to read RDX for FindNextFileW")?;

    let return_value = finish_find_next(engine, state, find_handle, find_data_va, true)?;

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!FindNextFileA`.
pub fn handle_find_next_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let find_handle = engine
        .read_rcx()
        .context("failed to read RCX for FindNextFileA")?;

    let find_data_va = engine
        .read_rdx()
        .context("failed to read RDX for FindNextFileA")?;

    let return_value = finish_find_next(engine, state, find_handle, find_data_va, false)?;

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!FindClose`.
pub fn handle_find_close(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let find_handle = engine
        .read_rcx()
        .context("failed to read RCX for FindClose")?;

    state
        .file_io
        .find_handles
        .retain(|handle| handle.handle != find_handle);

    ctx.finish(1)
}
/// Handles `KERNEL32.dll!CreateFileW`.
pub fn handle_create_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let file_name_va = engine
        .read_rcx()
        .context("failed to read RCX for CreateFileW")?;

    let desired_access = engine
        .read_rdx()
        .context("failed to read RDX for CreateFileW")?;

    let _share_mode = engine
        .read_r8()
        .context("failed to read R8 for CreateFileW")?;

    let _security_attributes = engine
        .read_r9()
        .context("failed to read R9 for CreateFileW")?;

    let file_name = if file_name_va == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, file_name_va, 32_768)
            .context("failed to read CreateFileW file name")?
    };

    // 5th arg (CreationDisposition) lives at [RSP+0x28] at Win64 API entry.
    let creation_disposition =
        read_create_file_stack_u32(engine, 0x28).map_or(OPEN_EXISTING, u64::from);

    let return_value = finish_create_file(
        engine,
        state,
        &file_name,
        desired_access,
        creation_disposition,
        "CreateFileW",
    );

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!CreateFileA`.
pub fn handle_create_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let file_name_va = engine
        .read_rcx()
        .context("failed to read RCX for CreateFileA")?;

    let desired_access = engine
        .read_rdx()
        .context("failed to read RDX for CreateFileA")?;

    let _share_mode = engine
        .read_r8()
        .context("failed to read R8 for CreateFileA")?;

    let _security_attributes = engine
        .read_r9()
        .context("failed to read R9 for CreateFileA")?;

    let file_name = if file_name_va == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, file_name_va, 32_768)
            .context("failed to read CreateFileA file name")?
    };

    let creation_disposition =
        read_create_file_stack_u32(engine, 0x28).map_or(OPEN_EXISTING, u64::from);

    let return_value = finish_create_file(
        engine,
        state,
        &file_name,
        desired_access,
        creation_disposition,
        "CreateFileA",
    );

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!CloseHandle`.
pub fn handle_close_handle(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for CloseHandle")?;

    let return_value = if handle == 0 || handle == INVALID_HANDLE_VALUE {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0
    } else if is_open_file_handle(state, handle) {
        // Flush written bytes to virtual store and/or bottle host path.
        if let Some(open_file) = find_open_file(state, handle) {
            let path = open_file.path.clone();
            sync_open_bytes_to_virtual(state, &path, handle);
        }
        persist_open_file_to_host(state, handle);
        // Best-effort teardown: failure to sync is not fatal.
        let _ = crate::guest_io_host::unregister_open_file(engine, state, handle).ok();
        state.file_io.open_files.remove(&handle);
        // Drop any directory-watch anchor for this handle (stops the watcher).
        if let Some(watch) = state.kernel.sync.watch_handles.remove(&handle) {
            watch.deactivate();
        }
        // Drop the cached streaming `File` (if any) so the host fd is released.
        state.file_io.cached_streams.remove(&handle);
        state.process.last_error = 0;
        1
    } else if state
        .kernel
        .sync
        .objects
        .remove(&crate::KernelHandle::from(handle))
        .is_some()
    {
        // Thread / event kernel handles (object may still be live via Arc).
        state.process.last_error = 0;
        1
    } else {
        // Console / module / other fake kernel objects: accept and no-op so
        // CRT and UI stubs that close non-file handles keep working.
        state.process.last_error = 0;
        1
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GetFileInformationByHandle`.
pub fn handle_get_file_information_by_handle(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for GetFileInformationByHandle")?;

    let info_va = engine
        .read_rdx()
        .context("failed to read RDX for GetFileInformationByHandle")?;

    let open_file = find_open_file(state, handle);
    let success = open_file.is_some() && info_va != 0;

    if let Some(open_file) = open_file.filter(|_| info_va != 0) {
        let file_size = open_file.size();
        // One shared-lock borrow instead of ten per-field writes. The view
        // starts zeroed, which covers the (empty) struct tail; every field
        // the old handler wrote is set explicitly below.
        with_typed_write::<ByHandleFileInformation, _, _>(engine, info_va, |info| {
            info.dw_file_attributes = u32::try_from(FILE_ATTRIBUTE_ARCHIVE).unwrap_or(0x20);
            let ft_low = u32::try_from(FIXED_SYSTEM_FILETIME & 0xffff_ffff).unwrap_or(0);
            let ft_high = u32::try_from(FIXED_SYSTEM_FILETIME >> 32).unwrap_or(0);
            info.ft_creation_time_low = ft_low;
            info.ft_creation_time_high = ft_high;
            info.ft_last_access_time_low = ft_low;
            info.ft_last_access_time_high = ft_high;
            info.ft_last_write_time_low = ft_low;
            info.ft_last_write_time_high = ft_high;
            info.dw_volume_serial_number = 0x1234_abcd;
            info.n_file_size_high =
                u32::try_from(file_size >> 32).context("open file size high does not fit u32")?;
            info.n_file_size_low = u32::try_from(file_size & 0xffff_ffff)
                .context("open file size low does not fit u32")?;
            info.n_number_of_links = 1;
            info.n_file_index_high = 0;
            info.n_file_index_low = 1;
            Ok(())
        })
        .context("failed to write BY_HANDLE_FILE_INFORMATION")?;

        state.process.last_error = 0;
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GetFileSizeEx`.
pub fn handle_get_file_size_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let size_va = engine.read_rdx()?;
    let return_value = if let Some(open_file) = find_open_file(state, handle) {
        if size_va != 0 {
            write_guest_u64(engine, size_va, open_file.size())?;
        }
        state.process.last_error = 0;
        1
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0
    };
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!SetFilePointerEx`.
pub fn handle_set_file_pointer_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    // Win64: DistanceToMove is LARGE_INTEGER by value in RDX (signed 64-bit).
    let distance_raw = engine.read_rdx()?;
    let distance = i64::from_le_bytes(distance_raw.to_le_bytes());
    // SetFilePointerEx(hFile, liDistanceToMove, lpNewFilePointer, dwMoveMethod):
    // R8 = lpNewFilePointer (output pointer), R9 = dwMoveMethod.
    let new_pos_va = engine.read_r8()?;
    let move_method = engine.read_r9()?;

    let valid_method =
        move_method == FILE_BEGIN || move_method == FILE_CURRENT || move_method == FILE_END;
    let return_value = if !is_open_file_handle(state, handle) {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0
    } else if !valid_method {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        0
    } else {
        let open_file = find_open_file_mut(state, handle)
            .context("open file vanished during SetFilePointerEx")?;
        let file_size = open_file.size();
        let base = if move_method == FILE_BEGIN {
            0_i64
        } else if move_method == FILE_CURRENT {
            i64::try_from(open_file.cursor).unwrap_or(0)
        } else {
            i64::try_from(file_size).unwrap_or(0)
        };
        let new_position = base.saturating_add(distance);
        if new_position < 0 {
            state.process.last_error = ERROR_INVALID_PARAMETER;
            0
        } else {
            let new_cursor = u64::try_from(new_position).unwrap_or(0);
            open_file.cursor = new_cursor;
            if new_pos_va != 0 {
                write_guest_u64(engine, new_pos_va, new_cursor)?;
            }
            state.process.last_error = 0;
            1
        }
    };
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!SetEndOfFile`.
pub fn handle_set_end_of_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let return_value = if is_open_file_handle(state, handle) {
        let (streaming, host, cursor, path) = {
            let f = find_open_file(state, handle).context("open file vanished")?;
            (f.streaming, f.host_path.clone(), f.cursor, f.path.clone())
        };
        if streaming {
            if let Some(host) = host {
                drop(crate::vfs::host_set_len(&host, cursor));
            }
        } else {
            if let Some(f) = find_open_file_mut(state, handle) {
                let len = usize::try_from(cursor).unwrap_or(0);
                f.bytes.resize(len, 0);
            }
            sync_open_bytes_to_virtual(state, &path, handle);
            persist_open_file_to_host(state, handle);
        }
        state.process.last_error = 0;
        1
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0
    };
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!FlushFileBuffers`.
pub(crate) fn file_attributes_for_path(state: &WinApiState, path: &str) -> u64 {
    let normalized = path.trim();
    if normalized.is_empty() {
        return INVALID_FILE_ATTRIBUTES;
    }

    let mounts_ref: Vec<(String, std::path::PathBuf)> = state
        .file_io
        .host_file_mounts
        .iter()
        .map(|m| (m.guest_path.clone(), m.host_path.clone()))
        .collect();
    let virtuals_ref: Vec<(String, usize)> = state
        .file_io
        .virtual_files
        .iter()
        .map(|v| (v.guest_path.clone(), v.bytes.len()))
        .collect();
    let ctx = crate::vfs::ResolveCtx {
        volumes: &state.file_io.volumes,
        main_module_path: &state.process.main_module_path,
        main_module_file_name: &state.process.main_module_file_name,
        host_file_mounts: &mounts_ref,
        virtual_files: &virtuals_ref,
        synthetic_dirs: crate::vfs::DEFAULT_SYNTHETIC_DIRS,
    };

    let st = crate::vfs::stat_path(&ctx, normalized);
    match st.kind {
        crate::vfs::PathKind::NotFound => INVALID_FILE_ATTRIBUTES,
        crate::vfs::PathKind::Directory => u64::from(st.attributes),
        crate::vfs::PathKind::File => {
            if ctx.path_is_main_module(normalized) {
                FILE_ATTRIBUTE_ARCHIVE
            } else {
                u64::from(st.attributes).max(FILE_ATTRIBUTE_ARCHIVE)
            }
        }
    }
}

/// Collect dir entries for a Find pattern (dir + mask).
pub(crate) fn collect_find_entries(
    state: &WinApiState,
    full_pattern: &str,
) -> Vec<crate::vfs::DirEntry> {
    let mounts_ref: Vec<(String, std::path::PathBuf)> = state
        .file_io
        .host_file_mounts
        .iter()
        .map(|m| (m.guest_path.clone(), m.host_path.clone()))
        .collect();
    let virtuals_ref: Vec<(String, usize)> = state
        .file_io
        .virtual_files
        .iter()
        .map(|v| (v.guest_path.clone(), v.bytes.len()))
        .collect();
    let ctx = crate::vfs::ResolveCtx {
        volumes: &state.file_io.volumes,
        main_module_path: &state.process.main_module_path,
        main_module_file_name: &state.process.main_module_file_name,
        host_file_mounts: &mounts_ref,
        virtual_files: &virtuals_ref,
        synthetic_dirs: crate::vfs::DEFAULT_SYNTHETIC_DIRS,
    };
    let (dir, mask) = crate::vfs::split_find_pattern(full_pattern);
    crate::vfs::list_dir_filtered(&ctx, &dir, &mask)
}

/// Write shared `WIN32_FIND_DATA{A,W}` header fields (not the name).
///
/// Layout (minwinbase.h) — **not** `BY_HANDLE_FILE_INFORMATION`:
/// ```text
/// 0  dwFileAttributes
/// 4  ftCreationTime / 12 ftLastAccessTime / 20 ftLastWriteTime
/// 28 nFileSizeHigh / 32 nFileSizeLow
/// 36 dwReserved0 / 40 dwReserved1
/// 44 cFileName[MAX_PATH]
/// ```
pub(crate) fn write_find_data_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    find_data_va: u64,
    attributes: u32,
    file_size: u64,
) -> Result<()> {
    if find_data_va == 0 {
        return Ok(());
    }

    // The 44-byte header goes through one typed write instead of a hand-built
    // byte array. The view starts zeroed, which covers dwReserved0/1 — the
    // old code zeroed them explicitly in the header array. `FILETIME` is a
    // (low, high) `DWORD` pair, so the fixed time splits like the old
    // `to_le_bytes` header write.
    with_typed_write::<FindDataHeader, _, _>(engine, find_data_va, |header| {
        header.dw_file_attributes = attributes;
        let ft_low = u32::try_from(FIXED_SYSTEM_FILETIME & 0xffff_ffff).unwrap_or(0);
        let ft_high = u32::try_from(FIXED_SYSTEM_FILETIME >> 32).unwrap_or(0);
        header.ft_creation_time_low = ft_low;
        header.ft_creation_time_high = ft_high;
        header.ft_last_access_time_low = ft_low;
        header.ft_last_access_time_high = ft_high;
        header.ft_last_write_time_low = ft_low;
        header.ft_last_write_time_high = ft_high;
        header.n_file_size_high = u32::try_from(file_size >> 32).unwrap_or(0);
        header.n_file_size_low = u32::try_from(file_size & 0xffff_ffff).unwrap_or(0);
        // dwReserved0 / dwReserved1 stay zero.
        Ok(())
    })
    .context("failed to write WIN32_FIND_DATA header")
}

pub(crate) fn write_find_data_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    find_data_va: u64,
    file_name: &str,
    attributes: u32,
    file_size: u64,
) -> Result<()> {
    write_find_data_common(engine, find_data_va, attributes, file_size)?;

    if find_data_va == 0 {
        return Ok(());
    }

    // cFileName is at offset 44 (after dwReserved1); cAlternateFileName[14]
    // starts at 44 + MAX_PATH*2 = 564.
    let file_name_address = checked_address(
        find_data_va,
        FIND_DATA_FILE_NAME_OFFSET,
        "WIN32_FIND_DATAW.cFileName",
    );
    let alt_name_address = checked_address(
        find_data_va,
        FIND_DATA_W_ALT_NAME_OFFSET,
        "WIN32_FIND_DATAW.cAlternateFileName",
    );

    let mut bytes = Vec::new();
    for unit in file_name.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());

    engine
        .mem_write(file_name_address, &bytes)
        .context("failed to write WIN32_FIND_DATAW.cFileName")?;
    write_guest_u16(engine, alt_name_address, 0)?;

    Ok(())
}

pub(crate) fn write_find_data_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    find_data_va: u64,
    file_name: &str,
    attributes: u32,
    file_size: u64,
) -> Result<()> {
    write_find_data_common(engine, find_data_va, attributes, file_size)?;

    if find_data_va == 0 {
        return Ok(());
    }

    // Same header as W; cFileName is CHAR[MAX_PATH] at offset 44.
    let file_name_address = checked_address(
        find_data_va,
        FIND_DATA_FILE_NAME_OFFSET,
        "WIN32_FIND_DATAA.cFileName",
    );
    let alt_name_address = checked_address(
        find_data_va,
        FIND_DATA_A_ALT_NAME_OFFSET,
        "WIN32_FIND_DATAA.cAlternateFileName",
    );

    let mut bytes = crate::vfs::encode_acp(file_name);
    bytes.push(0);

    engine
        .mem_write(file_name_address, &bytes)
        .context("failed to write WIN32_FIND_DATAA.cFileName")?;
    engine
        .mem_write(alt_name_address, &[0_u8])
        .context("failed to write WIN32_FIND_DATAA.cAlternateFileName")?;

    Ok(())
}

pub(crate) fn finish_find_first(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    pattern: &str,
    find_data_va: u64,
    unicode: bool,
) -> Result<u64> {
    if pattern.trim().is_empty() {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        return Ok(INVALID_HANDLE_VALUE);
    }

    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_pattern = resolve_full_windows_path(&cwd, pattern);
    let entries_vec = collect_find_entries(state, &full_pattern);
    if entries_vec.is_empty() {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        return Ok(INVALID_HANDLE_VALUE);
    }

    let mut entries: std::collections::VecDeque<_> = entries_vec.into();
    let first = entries
        .pop_front()
        .context("find entries went empty after non-empty check")?;
    if unicode {
        write_find_data_w(
            engine,
            find_data_va,
            &first.name,
            first.attributes,
            first.size,
        )?;
    } else {
        write_find_data_a(
            engine,
            find_data_va,
            &first.name,
            first.attributes,
            first.size,
        )?;
    }

    let handle = state.file_io.next_find_handle.as_u64();
    state.file_io.next_find_handle = crate::FindFileHandle::from(
        state
            .file_io
            .next_find_handle
            .as_u64()
            .checked_add(1)
            .context("find handle overflow")?,
    );

    state.file_io.find_handles.push(FindHandle {
        handle,
        pattern: full_pattern,
        remaining: entries,
    });
    state.process.last_error = 0;
    Ok(handle)
}

pub(crate) fn finish_find_next(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    find_handle: u64,
    find_data_va: u64,
    unicode: bool,
) -> Result<u64> {
    let Some(slot) = state
        .file_io
        .find_handles
        .iter_mut()
        .find(|h| h.handle == find_handle)
    else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        return Ok(0);
    };

    let Some(next) = slot.remaining.pop_front() else {
        state.process.last_error = ERROR_NO_MORE_FILES;
        return Ok(0);
    };
    if unicode {
        write_find_data_w(engine, find_data_va, &next.name, next.attributes, next.size)?;
    } else {
        write_find_data_a(engine, find_data_va, &next.name, next.attributes, next.size)?;
    }
    state.process.last_error = 0;
    Ok(1)
}

pub(crate) fn finish_create_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    file_name: &str,
    desired_access: u64,
    creation_disposition: u64,
    api_name: &str,
) -> u64 {
    // Directories are not regular files. Opening one with FILE_LIST_DIRECTORY
    // (0x1 — the FILE_READ_DATA value aliased for directory handles) succeeds
    // as a ReadDirectoryChangesW anchor; anything else falls through to the
    // regular open, which cannot load a directory's bytes.
    if (desired_access & FILE_LIST_DIRECTORY) != 0
        && let Some(handle) = open_directory_for_watch(state, file_name)
    {
        state.process.last_error = 0;
        tracing::info!(
            path = %file_name,
            desired_access,
            creation_disposition,
            handle,
            "{api_name} (directory watch anchor)"
        );
        return handle;
    }

    let return_value =
        match open_or_create_guest_path(state, file_name, desired_access, creation_disposition) {
            Ok(OpenFileOutcome::Handle(handle)) => {
                state.process.last_error = 0;
                handle
            }
            Ok(OpenFileOutcome::HandleCreated(handle)) => {
                // OPEN_ALWAYS / CREATE_ALWAYS created a new file (docs: GetLastError may be 0).
                state.process.last_error = 0;
                handle
            }
            Ok(OpenFileOutcome::HandleExists(handle)) => {
                // Microsoft Learn: CREATE_ALWAYS / OPEN_ALWAYS set ERROR_ALREADY_EXISTS
                // when the named file already existed.
                if creation_disposition == CREATE_ALWAYS || creation_disposition == OPEN_ALWAYS {
                    state.process.last_error = ERROR_ALREADY_EXISTS;
                } else {
                    state.process.last_error = 0;
                }
                handle
            }
            Err(win_error) => {
                // Not-found on an OPEN_EXISTING probe is a routine existence
                // check (e.g. a game scanning for its data files) — keep it
                // out of the error channel; anything else is a real failure.
                if win_error == ERROR_FILE_NOT_FOUND
                    && creation_disposition == OPEN_EXISTING
                {
                    tracing::debug!(
                        path = %file_name,
                        desired_access,
                        win_error,
                        "{api_name} probe not found"
                    );
                } else {
                    tracing::error!(
                        path = %file_name,
                        desired_access,
                        creation_disposition,
                        win_error,
                        "{api_name} open failed"
                    );
                }
                state.process.last_error = win_error;
                INVALID_HANDLE_VALUE
            }
        };

    if return_value != INVALID_HANDLE_VALUE {
        tracing::info!(
            path = %file_name,
            desired_access,
            creation_disposition,
            handle = return_value,
            "{api_name}"
        );
        // Best-effort teardown: failure to register the mirror is not fatal —
        // the guest I/O accelerator simply falls back to the host path.
        let _ = crate::guest_io_host::register_open_file(engine, state, return_value).ok();
    }

    return_value
}

/// `FILE_LIST_DIRECTORY` — the `CreateFileW` access right that real Windows
/// requires to read directory change notifications. Aliases `FILE_READ_DATA`
/// (0x1) for directory handles.
const FILE_LIST_DIRECTORY: u64 = 0x1;

/// Open an existing mapped directory as a `ReadDirectoryChangesW` anchor.
///
/// Real Windows returns a directory handle for `CreateFileW(path,
/// FILE_LIST_DIRECTORY, ...)`. We have no host handle for directories, so the
/// "handle" is an [`OpenGuestFile`] with empty bytes whose `host_path` is the
/// directory; `ReadDirectoryChangesW` resolves handle → host path. A watch
/// object is pre-created (watcher not started) so the read can find it
/// without touching the kernel-object table.
///
/// Returns `None` when the path is not an existing mapped directory — the
/// caller falls through to the regular file-open path, which reports the
/// normal error.
fn open_directory_for_watch(state: &mut WinApiState, file_name: &str) -> Option<u64> {
    if file_name.is_empty() {
        return None;
    }
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = resolve_full_windows_path(&cwd, file_name);
    let host_path = crate::kernel32::file_io::watch::resolve_watch_host_path(state, &full_path)?;

    let handle = allocate_open_file_ex(
        state,
        &full_path,
        Vec::new(),
        Some(host_path.clone()),
        false,
    )
    .ok()?;
    state.kernel.sync.watch_handles.insert(
        handle,
        std::sync::Arc::new(crate::sync_obj::DirectoryWatchObject::new(
            &full_path, host_path,
        )),
    );
    Some(handle)
}

/// Stat a guest path using the VFS, building the resolve context from state.
pub(crate) fn stat_guest_path(state: &WinApiState, full_path: &str) -> crate::vfs::PathStat {
    let mounts_ref: Vec<(String, std::path::PathBuf)> = state
        .file_io
        .host_file_mounts
        .iter()
        .map(|m| (m.guest_path.clone(), m.host_path.clone()))
        .collect();
    let virtuals_ref: Vec<(String, usize)> = state
        .file_io
        .virtual_files
        .iter()
        .map(|v| (v.guest_path.clone(), v.bytes.len()))
        .collect();
    let ctx = crate::vfs::ResolveCtx {
        volumes: &state.file_io.volumes,
        main_module_path: &state.process.main_module_path,
        main_module_file_name: &state.process.main_module_file_name,
        host_file_mounts: &mounts_ref,
        virtual_files: &virtuals_ref,
        synthetic_dirs: crate::vfs::DEFAULT_SYNTHETIC_DIRS,
    };
    crate::vfs::stat_path(&ctx, full_path)
}

pub(crate) fn temp_name_id_u32(id: u64) -> u32 {
    u32::try_from(id & 0xffff_ffff).unwrap_or(0)
}

pub(crate) fn finish_create_file_create_only(state: &mut WinApiState, guest_path: &str) {
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, guest_path);
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    if let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full) {
        drop(crate::vfs::create_host_file(&map.host));
    } else {
        ensure_virtual_file(state, &full);
    }
}

pub(crate) fn write_fixed_dir_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    dir: &str,
) -> Result<WinApiHandlerResult> {
    let buffer_len = engine.read_rcx()?;
    let buffer_va = engine.read_rdx()?;
    let units: Vec<u16> = dir.encode_utf16().collect();
    let required = u64::try_from(units.len().saturating_add(1)).unwrap_or(0);
    let return_value = if buffer_va == 0 || buffer_len < required {
        required
    } else {
        let mut t = units;
        t.push(0);
        write_guest_utf16_units(engine, buffer_va, &t)?;
        u64::try_from(t.len().saturating_sub(1)).unwrap_or(0)
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

pub(crate) fn write_fixed_dir_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    dir: &str,
) -> Result<WinApiHandlerResult> {
    let buffer_len = engine.read_rcx()?;
    let buffer_va = engine.read_rdx()?;
    let bytes = crate::vfs::encode_acp(dir);
    let required = u64::try_from(bytes.len().saturating_add(1)).unwrap_or(0);
    let return_value = if buffer_va == 0 || buffer_len < required {
        required
    } else {
        let mut out = bytes;
        out.push(0);
        engine.mem_write(buffer_va, &out)?;
        u64::try_from(out.len().saturating_sub(1)).unwrap_or(0)
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!FindFirstFileExW` — `FindFirstFileW` semantics with
/// the extended argument list (info level / search op / filter / flags are
/// accepted; the basic info level writes the same `WIN32_FIND_DATAW`).
pub fn handle_find_first_file_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let pattern_va = engine
        .read_rcx()
        .context("failed to read RCX for FindFirstFileExW")?;
    let _info_level = engine.read_rdx()?;
    let find_data_va = engine
        .read_r8()
        .context("failed to read R8 for FindFirstFileExW")?;
    let _search_op = engine.read_r9()?;
    let pattern = read_wide_string_from_cpu(engine, pattern_va, 1024)?;
    let return_value = finish_find_first(engine, state, &pattern, find_data_va, true)?;
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!PeekNamedPipe`.
///
/// No named-pipe handles exist under WIE; pipes created by `CreatePipe` are
/// anonymous byte pipes. Return FALSE with `ERROR_INVALID_HANDLE` for the
/// probe (the boot paths that call it guard on the failure).
pub fn handle_peek_named_pipe(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _handle = ctx.engine.read_rcx()?;
    ctx.state.process.last_error = ERROR_INVALID_HANDLE;
    ctx.finish(0)
}
/// Handles `KERNEL32.dll!SetNamedPipeHandleState` — accepted no-op.
pub fn handle_set_named_pipe_handle_state(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _handle = ctx.engine.read_rcx()?;
    let _mode = ctx.engine.read_rdx()?;
    let _max_collect = ctx.engine.read_r8()?;
    let _collect_data_timeout = ctx.engine.read_r9()?;
    ctx.finish(1)
}
/// Handles `KERNEL32.dll!GetOverlappedResult`.
///
/// No overlapped I/O is in flight under WIE: writes 0 bytes transferred and
/// returns TRUE for a valid file handle, FALSE with `ERROR_INVALID_HANDLE`
/// otherwise. `bWait` is accepted and ignored.
pub fn handle_get_overlapped_result(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for GetOverlappedResult")?;
    let _overlapped = engine.read_rdx()?;
    let transferred_va = engine.read_r8()?;
    let _wait = engine.read_r9()?;
    if transferred_va != 0 {
        crate::guest_memory::write_u32(engine, transferred_va, 0)?;
    }
    if state.file_io.open_files.contains_key(&handle) {
        state.process.last_error = 0;
        return ctx.finish(1);
    }
    state.process.last_error = ERROR_INVALID_HANDLE;
    ctx.finish(0)
}
