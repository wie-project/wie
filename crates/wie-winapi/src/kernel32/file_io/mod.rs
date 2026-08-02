use super::{
    CREATE_ALWAYS, CREATE_NEW, Context, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS,
    ERROR_DIR_NOT_EMPTY, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
    ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND,
    ERROR_READ_FAULT, FAKE_DISK_CLUSTERS, FAKE_DISK_GIB, FAKE_STDERR_HANDLE, FAKE_STDIN_HANDLE,
    FAKE_STDOUT_HANDLE, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_BEGIN, FILE_CURRENT,
    FILE_END, FILE_TYPE_CHAR, FILE_TYPE_DISK, FILE_TYPE_UNKNOWN, FIXED_SYSTEM_FILETIME, FindHandle,
    HandlerContext, INVALID_FILE_ATTRIBUTES, INVALID_HANDLE_VALUE, INVALID_SET_FILE_POINTER,
    LOGICAL_DRIVE_TCHARS, OPEN_ALWAYS, OPEN_EXISTING, OpenGuestFile, Path, Result,
    TRUNCATE_EXISTING, WinApiHandlerResult, WinApiState, checked_address, checked_field_address,
    get_user_profile_dir_impl, is_main_module_path, low_u32, read_ansi_string_from_cpu,
    read_guest_u16, read_guest_u64, read_guest_utf16_lossy, read_stack_u64,
    read_wide_string_from_cpu, refill_stdin_from_host, ret_bool_true, ret_u64, write_guest_u16,
    write_guest_u32, write_guest_u64, write_guest_utf16_units,
};

pub use dir::*;
pub use open::*;
pub use path::*;
pub use time::*;

mod dir;
mod open;
mod path;
mod time;

pub(crate) fn is_console_output_handle(handle: u64) -> bool {
    matches!(handle, FAKE_STDOUT_HANDLE | FAKE_STDERR_HANDLE)
}
pub(crate) fn write_host_console_handle(handle: u64, bytes: &[u8]) {
    let fd = if handle == FAKE_STDOUT_HANDLE {
        libc::STDOUT_FILENO
    } else {
        libc::STDERR_FILENO
    };
    // Single shared implementation — see `ucrt::write_all_fd` for why this is
    // a raw fd write rather than `std::io`.
    crate::ucrt::write_all_fd(fd, bytes);
}
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

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFileType")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetFileAttributesA`.
pub fn handle_get_file_attributes_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetFileAttributesA")?;

    let path = read_ansi_string_from_cpu(engine, path_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = resolve_full_windows_path(&cwd, &path);
    let return_value = file_attributes_for_path(state, &full_path);
    if return_value == INVALID_FILE_ATTRIBUTES {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
    } else {
        state.process.last_error = 0;
    }

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFileAttributesA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetFileAttributesW`.
pub fn handle_get_file_attributes_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetFileAttributesW")?;

    let path = read_wide_string_from_cpu(engine, path_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = resolve_full_windows_path(&cwd, &path);
    let return_value = file_attributes_for_path(state, &full_path);
    if return_value == INVALID_FILE_ATTRIBUTES {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
    } else {
        state.process.last_error = 0;
    }

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFileAttributesW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!FindFirstFileW`.
pub fn handle_find_first_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let pattern_ptr = engine
        .read_rcx()
        .context("failed to read RCX for FindFirstFileW")?;

    let find_data_ptr = engine
        .read_rdx()
        .context("failed to read RDX for FindFirstFileW")?;

    let pattern = read_wide_string_from_cpu(engine, pattern_ptr, 1024)?;
    let return_value = finish_find_first(engine, state, &pattern, find_data_ptr, true)?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from FindFirstFileW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!FindFirstFileA`.
pub fn handle_find_first_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let pattern_ptr = engine
        .read_rcx()
        .context("failed to read RCX for FindFirstFileA")?;

    let find_data_ptr = engine
        .read_rdx()
        .context("failed to read RDX for FindFirstFileA")?;

    let pattern = read_ansi_string_from_cpu(engine, pattern_ptr, 1024)?;
    let return_value = finish_find_first(engine, state, &pattern, find_data_ptr, false)?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from FindFirstFileA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!FindNextFileW`.
pub fn handle_find_next_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let find_handle = engine
        .read_rcx()
        .context("failed to read RCX for FindNextFileW")?;

    let find_data_ptr = engine
        .read_rdx()
        .context("failed to read RDX for FindNextFileW")?;

    let return_value = finish_find_next(engine, state, find_handle, find_data_ptr, true)?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from FindNextFileW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!FindNextFileA`.
pub fn handle_find_next_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let find_handle = engine
        .read_rcx()
        .context("failed to read RCX for FindNextFileA")?;

    let find_data_ptr = engine
        .read_rdx()
        .context("failed to read RDX for FindNextFileA")?;

    let return_value = finish_find_next(engine, state, find_handle, find_data_ptr, false)?;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from FindNextFileA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from FindClose")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!CreateFileW`.
pub fn handle_create_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let file_name_ptr = engine
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

    let file_name = if file_name_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, file_name_ptr, 32_768)
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

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CreateFileW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!CreateFileA`.
pub fn handle_create_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let file_name_ptr = engine
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

    let file_name = if file_name_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, file_name_ptr, 32_768)
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

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CreateFileA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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
        let _ = crate::guest_io_host::unregister_open_file(engine, state, handle).ok();
        state.file_io.open_files.remove(&handle);
        // Drop the cached streaming `File` (if any) so the host fd is released.
        state.file_io.cached_streams.remove(&handle);
        state.process.last_error = 0;
        1
    } else if state.kernel.sync.objects.remove(&handle).is_some() {
        // Thread / event kernel handles (object may still be live via Arc).
        state.process.last_error = 0;
        1
    } else {
        // Console / module / other fake kernel objects: accept and no-op so
        // CRT and UI stubs that close non-file handles keep working.
        state.process.last_error = 0;
        1
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from CloseHandle")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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

    let info_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetFileInformationByHandle")?;

    let open_file = find_open_file(state, handle);
    let success = open_file.is_some() && info_ptr != 0;

    if let Some(open_file) = open_file.filter(|_| info_ptr != 0) {
        // BY_HANDLE_FILE_INFORMATION:
        // DWORD    dwFileAttributes;     offset 0
        // FILETIME ftCreationTime;       offset 4
        // FILETIME ftLastAccessTime;     offset 12
        // FILETIME ftLastWriteTime;      offset 20
        // DWORD    dwVolumeSerialNumber; offset 28
        // DWORD    nFileSizeHigh;        offset 32
        // DWORD    nFileSizeLow;         offset 36
        // DWORD    nNumberOfLinks;       offset 40
        // DWORD    nFileIndexHigh;       offset 44
        // DWORD    nFileIndexLow;        offset 48

        let attributes_address = checked_field_address(info_ptr, 0, "dwFileAttributes");
        let creation_time_address = checked_field_address(info_ptr, 4, "ftCreationTime");
        let last_access_time_address = checked_field_address(info_ptr, 12, "ftLastAccessTime");
        let last_write_time_address = checked_field_address(info_ptr, 20, "ftLastWriteTime");
        let volume_serial_address = checked_field_address(info_ptr, 28, "dwVolumeSerialNumber");
        let file_size_high_address = checked_field_address(info_ptr, 32, "nFileSizeHigh");
        let file_size_low_address = checked_field_address(info_ptr, 36, "nFileSizeLow");
        let number_of_links_address = checked_field_address(info_ptr, 40, "nNumberOfLinks");
        let file_index_high_address = checked_field_address(info_ptr, 44, "nFileIndexHigh");
        let file_index_low_address = checked_field_address(info_ptr, 48, "nFileIndexLow");

        let file_size = open_file.size();

        write_guest_u32(
            engine,
            attributes_address,
            u32::try_from(FILE_ATTRIBUTE_ARCHIVE).unwrap_or(0x20),
        )?;
        write_guest_u64(engine, creation_time_address, FIXED_SYSTEM_FILETIME)?;
        write_guest_u64(engine, last_access_time_address, FIXED_SYSTEM_FILETIME)?;
        write_guest_u64(engine, last_write_time_address, FIXED_SYSTEM_FILETIME)?;
        write_guest_u32(engine, volume_serial_address, 0x1234_abcd)?;
        let file_size_high =
            u32::try_from(file_size >> 32).context("open file size high does not fit u32")?;

        let file_size_low = u32::try_from(file_size & 0xffff_ffff)
            .context("open file size low does not fit u32")?;

        write_guest_u32(engine, file_size_high_address, file_size_high)?;
        write_guest_u32(engine, file_size_low_address, file_size_low)?;
        write_guest_u32(engine, number_of_links_address, 1)?;
        write_guest_u32(engine, file_index_high_address, 0)?;
        write_guest_u32(engine, file_index_low_address, 1)?;

        state.process.last_error = 0;
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFileInformationByHandle")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!ReadFile`.
pub fn handle_read_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let buffer_ptr = engine.read_rdx()?;
    let bytes_to_read = engine.read_r8()?;
    let bytes_read_ptr = engine.read_r9()?;

    // Microsoft Learn: sets *lpNumberOfBytesRead to zero before any work/error check.
    if bytes_read_ptr != 0 {
        write_guest_u32(engine, bytes_read_ptr, 0)?;
    }

    if buffer_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    // Console stdin: inject buffer, then optional live host line-fill.
    if handle == FAKE_STDIN_HANDLE {
        let requested =
            usize::try_from(bytes_to_read).context("ReadFile byte count does not fit usize")?;

        let mut available = state
            .file_io
            .stdin_bytes
            .len()
            .saturating_sub(state.file_io.stdin_cursor);
        if available == 0 && state.file_io.stdin_mode == crate::GuestStdinMode::LiveHost {
            match refill_stdin_from_host(state) {
                Ok(true) => {
                    available = state
                        .file_io
                        .stdin_bytes
                        .len()
                        .saturating_sub(state.file_io.stdin_cursor);
                }
                Ok(false) => {
                    // Host EOF → success with 0 bytes (already zeroed count).
                    state.process.last_error = 0;
                    let return_address = engine.return_from_win64_api(1)?;
                    return Ok(WinApiHandlerResult {
                        return_address,
                        return_value: 1,
                    });
                }
                Err(()) => {
                    state.process.last_error = ERROR_READ_FAULT;
                    let return_address = engine.return_from_win64_api(0)?;
                    return Ok(WinApiHandlerResult {
                        return_address,
                        return_value: 0,
                    });
                }
            }
        }

        let read_len = requested.min(available);
        if read_len > 0 {
            let end = state
                .file_io
                .stdin_cursor
                .checked_add(read_len)
                .context("ReadFile stdin end overflow")?;
            let data = state
                .file_io
                .stdin_bytes
                .get(state.file_io.stdin_cursor..end)
                .context("ReadFile stdin slice out of range")?;
            engine
                .mem_write(buffer_ptr, data)
                .context("failed to write ReadFile stdin bytes")?;
            state.file_io.stdin_cursor = end;
            if bytes_read_ptr != 0 {
                let read_len_u32 =
                    u32::try_from(read_len).context("ReadFile byte count does not fit u32")?;
                write_guest_u32(engine, bytes_read_ptr, read_len_u32)?;
            }
        }
        // available == 0 && InjectOnly → inject exhausted → EOF (0 bytes, success).
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(1)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 1,
        });
    }

    // Console stdout/stderr are not readable.
    if is_console_output_handle(handle) {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let success = is_open_file_handle(state, handle);

    if success {
        let _ = crate::guest_io_host::sync_host_cursor_from_guest(engine, state, handle).ok();
        let requested =
            usize::try_from(bytes_to_read).context("ReadFile byte count does not fit usize")?;

        let streaming = find_open_file(state, handle).is_some_and(|f| f.streaming);
        if streaming {
            let (host_path, cursor_before, path) = {
                let open_file =
                    find_open_file(state, handle).context("open file vanished during ReadFile")?;
                (
                    open_file.host_path.clone(),
                    open_file.cursor,
                    open_file.path.clone(),
                )
            };
            let Some(host) = host_path else {
                state.process.last_error = ERROR_INVALID_HANDLE;
                let return_address = engine.return_from_win64_api(0)?;
                return Ok(WinApiHandlerResult {
                    return_address,
                    return_value: 0,
                });
            };
            let mut data = vec![0_u8; requested];
            // Cache an open `File` per handle so streaming ReadFile loops don't
            // reopen the host file on every 64 KiB chunk.
            let cached = state
                .file_io
                .cached_streams
                .get(&handle)
                .cloned()
                .or_else(|| {
                    let f = crate::vfs::open_stream_cached(&host).ok()?;
                    state.file_io.cached_streams.insert(handle, f.clone());
                    Some(f)
                });
            let n = if let Some(ref f) = cached {
                crate::vfs::cached_read_at(f, cursor_before, &mut data).unwrap_or(0)
            } else {
                crate::vfs::host_read_at(&host, cursor_before, &mut data).unwrap_or(0)
            };
            data.truncate(n);
            engine
                .mem_write(buffer_ptr, &data)
                .context("failed to write ReadFile stream bytes")?;
            if let Some(open_file) = find_open_file_mut(state, handle) {
                open_file.cursor = cursor_before.saturating_add(u64::try_from(n).unwrap_or(0));
            }
            if bytes_read_ptr != 0 {
                write_guest_u32(engine, bytes_read_ptr, u32::try_from(n).unwrap_or(0))?;
            }
            if is_main_module_path(state, &path) {
                state.file_io.executable_file_cursor =
                    cursor_before.saturating_add(u64::try_from(n).unwrap_or(0));
            }
            state.process.last_error = 0;
        } else {
            // Phase 1: advance cursor and capture slice bounds without cloning the path/body.
            let (start, end, cursor_after, is_exe) = {
                let (cursor_usize, end, cursor_after, path_for_exe) = {
                    let open_file = find_open_file_mut(state, handle)
                        .context("open file vanished during ReadFile")?;

                    let cursor_before = open_file.cursor;
                    let cursor_usize =
                        usize::try_from(cursor_before).context("file cursor does not fit usize")?;
                    let available = open_file.bytes.len().saturating_sub(cursor_usize);
                    let read_len = requested.min(available);
                    let end = cursor_usize
                        .checked_add(read_len)
                        .context("ReadFile end offset overflow")?;
                    let read_len_u64 =
                        u64::try_from(read_len).context("ReadFile byte count does not fit u64")?;
                    open_file.cursor = cursor_before
                        .checked_add(read_len_u64)
                        .context("ReadFile cursor overflow")?;
                    (cursor_usize, end, open_file.cursor, open_file.path.clone())
                };
                let is_exe = is_main_module_path(state, &path_for_exe);
                (cursor_usize, end, cursor_after, is_exe)
            };

            // Phase 2: immutable borrow for zero-copy mem_write of the file slice.
            {
                let open_file = find_open_file(state, handle)
                    .context("open file vanished during ReadFile write")?;
                let data = open_file
                    .bytes
                    .get(start..end)
                    .context("ReadFile slice out of range")?;
                engine
                    .mem_write(buffer_ptr, data)
                    .context("failed to write ReadFile bytes")?;

                let read_len_u32 =
                    u32::try_from(data.len()).context("ReadFile byte count does not fit u32")?;
                if bytes_read_ptr != 0 {
                    write_guest_u32(engine, bytes_read_ptr, read_len_u32)?;
                }
            }

            if is_exe {
                state.file_io.executable_file_cursor = cursor_after;
            }

            state.process.last_error = 0;
            let _ = crate::guest_io_host::sync_slot_from_host(engine, state, handle).ok();
        }
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
    }

    let return_value = u64::from(success);
    let return_address = engine.return_from_win64_api(return_value)?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!WriteFile`.
pub fn handle_write_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for WriteFile")?;

    let buffer_ptr = engine
        .read_rdx()
        .context("failed to read RDX for WriteFile")?;

    let bytes_to_write = engine
        .read_r8()
        .context("failed to read R8 for WriteFile")?;

    let bytes_written_ptr = engine
        .read_r9()
        .context("failed to read R9 for WriteFile")?;

    // Mirror ReadFile: zero the optional out-count before validation.
    if bytes_written_ptr != 0 {
        write_guest_u32(engine, bytes_written_ptr, 0)?;
    }

    if buffer_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    // Console stdout/stderr → host console.
    if is_console_output_handle(handle) {
        let write_len =
            usize::try_from(bytes_to_write).context("WriteFile byte count does not fit usize")?;
        let mut data = vec![0_u8; write_len];
        if write_len > 0 {
            engine
                .mem_read(buffer_ptr, &mut data)
                .context("failed to read WriteFile console buffer")?;
        }
        write_host_console_handle(handle, &data);
        if bytes_written_ptr != 0 {
            let write_len_u32 =
                u32::try_from(write_len).context("WriteFile byte count does not fit u32")?;
            write_guest_u32(engine, bytes_written_ptr, write_len_u32)?;
        }
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(1)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 1,
        });
    }

    // Console stdin is not writable.
    if handle == FAKE_STDIN_HANDLE {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let success = is_open_file_handle(state, handle);

    if success {
        let write_len =
            usize::try_from(bytes_to_write).context("WriteFile byte count does not fit usize")?;

        let mut data = vec![0_u8; write_len];
        if write_len > 0 {
            engine
                .mem_read(buffer_ptr, &mut data)
                .context("failed to read WriteFile source buffer")?;
        }

        let streaming = find_open_file(state, handle).is_some_and(|f| f.streaming);
        let (path, cursor_before, cursor_after, file_size) = if streaming {
            let open_file =
                find_open_file(state, handle).context("open file vanished during WriteFile")?;
            let host = open_file
                .host_path
                .clone()
                .ok_or_else(|| anyhow::anyhow!("streaming file missing host_path"))?;
            let cursor_before = open_file.cursor;
            let path = open_file.path.clone();
            let cached = state
                .file_io
                .cached_streams
                .get(&handle)
                .cloned()
                .or_else(|| {
                    let f = crate::vfs::open_stream_cached(&host).ok()?;
                    state.file_io.cached_streams.insert(handle, f.clone());
                    Some(f)
                });
            if let Some(ref f) = cached {
                crate::vfs::cached_write_at(f, cursor_before, &data)
                    .map_err(|e| anyhow::anyhow!("host WriteFile (cached): {e}"))?;
            } else {
                crate::vfs::host_write_at(&host, cursor_before, &data)
                    .map_err(|e| anyhow::anyhow!("host WriteFile: {e}"))?;
            }
            let write_len_u64 =
                u64::try_from(write_len).context("WriteFile byte count does not fit u64")?;
            let cursor_after = cursor_before
                .checked_add(write_len_u64)
                .context("WriteFile cursor overflow")?;
            if let Some(open_file) = find_open_file_mut(state, handle) {
                open_file.cursor = cursor_after;
            }
            let file_size = find_open_file(state, handle).map_or(0, OpenGuestFile::size);
            (path, cursor_before, cursor_after, file_size)
        } else {
            let open_file =
                find_open_file_mut(state, handle).context("open file vanished during WriteFile")?;

            let cursor_before = open_file.cursor;
            let path = open_file.path.clone();

            let cursor_usize =
                usize::try_from(cursor_before).context("file cursor does not fit usize")?;

            let end = cursor_usize
                .checked_add(write_len)
                .context("WriteFile end offset overflow")?;

            if end > open_file.bytes.len() {
                open_file.bytes.resize(end, 0);
            }

            open_file
                .bytes
                .get_mut(cursor_usize..end)
                .context("WriteFile slice out of range")?
                .copy_from_slice(&data);

            let write_len_u64 =
                u64::try_from(write_len).context("WriteFile byte count does not fit u64")?;

            open_file.cursor = cursor_before
                .checked_add(write_len_u64)
                .context("WriteFile cursor overflow")?;

            let cursor_after = open_file.cursor;
            let file_size = u64::try_from(open_file.bytes.len()).unwrap_or(0);
            (path, cursor_before, cursor_after, file_size)
        };

        // Do **not** full-clone / full-rewrite the file on every WriteFile.
        // That was O(n²) host I/O and a 2× temporary RAM spike while the archive
        // grew (classic progressive leak during 7za create). Buffered host files
        // spill once when they cross the streaming threshold; durable flush is
        // CloseHandle / FlushFileBuffers / SetEndOfFile.
        if !streaming {
            maybe_promote_open_file_to_streaming(engine, state, handle);
        }

        if is_main_module_path(state, &path) {
            state.file_io.executable_file_cursor = cursor_after;
        }

        let write_len_u32 =
            u32::try_from(write_len).context("WriteFile byte count does not fit u32")?;

        if bytes_written_ptr != 0 {
            write_guest_u32(engine, bytes_written_ptr, write_len_u32)?;
        }

        tracing::debug!(
            handle,
            buffer = buffer_ptr,
            requested = bytes_to_write,
            cursor_before,
            actual_write = write_len,
            cursor_after,
            file_size,
            path = %path,
            "WriteFile"
        );

        state.process.last_error = 0;
    } else {
        tracing::debug!(
            handle,
            buffer = buffer_ptr,
            requested = bytes_to_write,
            "WriteFile invalid handle"
        );
        state.process.last_error = ERROR_INVALID_HANDLE;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from WriteFile")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_get_disk_free_space_ex_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _path = engine.read_rcx()?;
    let free_caller = engine.read_rdx()?;
    let total = engine.read_r8()?;
    let free_total = engine.read_r9()?;
    if free_caller != 0 {
        write_guest_u64(engine, free_caller, 50 * FAKE_DISK_GIB)?;
    }
    if total != 0 {
        write_guest_u64(engine, total, 100 * FAKE_DISK_GIB)?;
    }
    if free_total != 0 {
        write_guest_u64(engine, free_total, 50 * FAKE_DISK_GIB)?;
    }
    ret_bool_true(engine, "GetDiskFreeSpaceExW")
}
pub fn handle_get_disk_free_space_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _path = engine.read_rcx()?;
    let spc = engine.read_rdx()?; // sectors per cluster
    let bps = engine.read_r8()?; // bytes per sector
    let free_clusters = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let total_clusters = read_guest_u64(
        engine,
        checked_address(rsp, 0x28, "GetDiskFreeSpaceW total"),
    )?;
    if spc != 0 {
        write_guest_u32(engine, spc, 8)?;
    }
    if bps != 0 {
        write_guest_u32(engine, bps, 512)?;
    }
    let half = FAKE_DISK_CLUSTERS.wrapping_shr(1);
    if free_clusters != 0 {
        write_guest_u32(engine, free_clusters, half)?;
    }
    if total_clusters != 0 {
        write_guest_u32(engine, total_clusters, FAKE_DISK_CLUSTERS)?;
    }
    ret_bool_true(engine, "GetDiskFreeSpaceW")
}
pub fn handle_get_logical_drive_strings_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let n_buffer = low_u32(engine.read_rcx()?, "GetLogicalDriveStringsW nBufferLength")?;
    let buffer = engine.read_rdx()?;
    if buffer == 0 || n_buffer == 0 || n_buffer < LOGICAL_DRIVE_TCHARS {
        return ret_u64(
            engine,
            u64::from(LOGICAL_DRIVE_TCHARS),
            "GetLogicalDriveStringsW",
        );
    }
    // C : \ \0 + extra terminator WCHAR
    let bytes: [u8; 10] = [
        0x43, 0x00, // C
        0x3A, 0x00, // :
        0x5C, 0x00, // \
        0x00, 0x00, // NUL
        0x00, 0x00, // final NUL
    ];
    engine
        .mem_write(buffer, &bytes)
        .context("GetLogicalDriveStringsW write")?;
    ret_u64(
        engine,
        u64::from(LOGICAL_DRIVE_TCHARS),
        "GetLogicalDriveStringsW",
    )
}
pub fn handle_set_file_attributes_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _path = engine.read_rcx()?;
    let _attrs = engine.read_rdx()?;
    // Best-effort success (VFS does not track Win32 attributes yet).
    ret_bool_true(engine, "SetFileAttributesW")
}
pub fn handle_set_file_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _handle = engine.read_rcx()?;
    let _creation = engine.read_rdx()?;
    let _access = engine.read_r8()?;
    let _write = engine.read_r9()?;
    ret_bool_true(engine, "SetFileTime")
}
pub fn handle_move_file_with_progress_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    // Same first two args as MoveFileW (existing/new).
    handle_move_file_w(ctx)
}
pub fn handle_create_hard_link_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _ = (engine.read_rcx()?, engine.read_rdx()?, engine.read_r8()?);
    state.process.last_error = 1; // ERROR_INVALID_FUNCTION-ish
    ret_u64(engine, 0, "CreateHardLinkW")
}
pub fn handle_find_first_stream_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _ = (engine.read_rcx()?, engine.read_rdx()?, engine.read_r8()?);
    state.process.last_error = 38; // ERROR_HANDLE_EOF
    ret_u64(engine, u64::MAX, "FindFirstStreamW") // INVALID_HANDLE_VALUE
}
pub fn handle_find_next_stream_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _ = (engine.read_rcx()?, engine.read_rdx()?);
    state.process.last_error = 38;
    ret_u64(engine, 0, "FindNextStreamW")
}
pub fn handle_device_io_control(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _ = (
        engine.read_rcx()?,
        engine.read_rdx()?,
        engine.read_r8()?,
        engine.read_r9()?,
    );
    state.process.last_error = 1;
    ret_u64(engine, 0, "DeviceIoControl")
}
/// Handles `KERNEL32.dll!GetCompressedFileSizeA` — return real uncompressed size via VFS.
pub fn handle_get_compressed_file_size_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine.read_rcx()?;
    let _high_ptr = engine.read_rdx()?;
    let path = read_ansi_string_from_cpu(engine, path_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, &path);
    let st = stat_guest_path(state, &full);
    if st.kind == crate::vfs::PathKind::NotFound {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        let return_address = engine.return_from_win64_api(INVALID_FILE_ATTRIBUTES)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: INVALID_FILE_ATTRIBUTES,
        });
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(st.size)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: st.size,
    })
}
/// Handles `KERNEL32.dll!GetCompressedFileSizeW` — return real uncompressed size via VFS.
pub fn handle_get_compressed_file_size_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine.read_rcx()?;
    let _high_ptr = engine.read_rdx()?;
    let path = read_wide_string_from_cpu(engine, path_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, &path);
    let st = stat_guest_path(state, &full);
    if st.kind == crate::vfs::PathKind::NotFound {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        let return_address = engine.return_from_win64_api(INVALID_FILE_ATTRIBUTES)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: INVALID_FILE_ATTRIBUTES,
        });
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(st.size)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: st.size,
    })
}
/// Handles `KERNEL32.dll!GetVolumeInformationW` — real bottle volume info.
pub fn handle_get_volume_information_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _root = engine.read_rcx()?;
    let vol_name = engine.read_rdx()?;
    let vol_name_len = engine.read_r8()?;
    let _serial = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let max_comp_ptr = checked_address(rsp, 0x28, "lpMaximumComponentLength");
    let flags_ptr = checked_address(rsp, 0x30, "lpFileSystemFlags");
    let name_ptr = checked_address(rsp, 0x38, "lpFileSystemNameBuffer");
    let fs_len_ptr = checked_address(rsp, 0x40, "lpFileSystemNameLength");

    // Derive volume label from the bottle root name, or use a default.
    let label = state
        .file_io
        .bottle_root
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("Bottle")
        .to_owned();
    write_mock_string_w(engine, state, &label, vol_name, vol_name_len)?;

    // MaximumComponentLength = 255 (NTFS)
    if max_comp_ptr != 0 {
        let _unused = write_guest_u32(engine, max_comp_ptr, 255);
    }
    // FileSystemFlags: FILE_CASE_SENSITIVE_SEARCH | FILE_CASE_PRESERVED_NAMES |
    //                   FILE_UNICODE_ON_DISK | FILE_PERSISTENT_ACLS | FILE_NAMED_STREAMS |
    //                   FILE_FILE_COMPRESSION
    let fs_flags: u32 =
        0x0000_0008 | 0x0000_0002 | 0x0000_0004 | 0x0000_0010 | 0x0000_0040 | 0x0020_0000;
    if flags_ptr != 0 {
        let _unused = write_guest_u32(engine, flags_ptr, fs_flags);
    }
    // FileSystemName = "NTFS"
    if name_ptr != 0 {
        let _unused = write_mock_string_w(engine, state, "NTFS", name_ptr, 16);
    }
    if fs_len_ptr != 0 {
        let _unused = write_guest_u32(engine, fs_len_ptr, 4);
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!GetVolumeInformationA` — real bottle volume info.
pub fn handle_get_volume_information_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _root = engine.read_rcx()?;
    let vol_name = engine.read_rdx()?;
    let vol_name_len = engine.read_r8()?;
    let _serial = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let max_comp_ptr = checked_address(rsp, 0x28, "lpMaximumComponentLength");
    let flags_ptr = checked_address(rsp, 0x30, "lpFileSystemFlags");
    let name_ptr = checked_address(rsp, 0x38, "lpFileSystemNameBuffer");
    let fs_len_ptr = checked_address(rsp, 0x40, "lpFileSystemNameLength");

    let label = state
        .file_io
        .bottle_root
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("Bottle")
        .to_owned();
    write_mock_string_a(engine, state, &label, vol_name, vol_name_len)?;

    if max_comp_ptr != 0 {
        let _unused = write_guest_u32(engine, max_comp_ptr, 255);
    }
    let fs_flags: u32 =
        0x0000_0008 | 0x0000_0002 | 0x0000_0004 | 0x0000_0010 | 0x0000_0040 | 0x0020_0000;
    if flags_ptr != 0 {
        let _unused = write_guest_u32(engine, flags_ptr, fs_flags);
    }
    if name_ptr != 0 {
        let _unused = write_mock_string_a(engine, state, "NTFS", name_ptr, 16);
    }
    if fs_len_ptr != 0 {
        let _unused = write_guest_u32(engine, fs_len_ptr, 4);
    }

    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!LockFile` — validate file handle and return TRUE.
pub fn handle_lock_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    if !is_open_file_handle(state, handle) && handle != FAKE_STDIN_HANDLE {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!UnlockFile` — validate file handle and return TRUE.
pub fn handle_unlock_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    if !is_open_file_handle(state, handle) && handle != FAKE_STDIN_HANDLE {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!SetFileValidData` — validate file handle and return TRUE.
pub fn handle_set_file_valid_data(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    if !is_open_file_handle(state, handle) {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!GetFileAttributesExW` — real extended attributes via VFS.
pub fn handle_get_file_attributes_ex_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine.read_rcx()?;
    let _info_level = engine.read_rdx()?;
    let info_ptr = engine.read_r8()?;
    let path = read_wide_string_from_cpu(engine, path_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, &path);
    let st = stat_guest_path(state, &full);
    if st.kind == crate::vfs::PathKind::NotFound {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    if info_ptr != 0 {
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 0, "dwFileAttributes"),
            st.attributes,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 4, "ftCreationTime"),
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 12, "ftLastAccessTime"),
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 20, "ftLastWriteTime"),
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 28, "nFileSizeHigh"),
            u32::try_from(st.size >> 32).unwrap_or(0),
        )?;
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 32, "nFileSizeLow"),
            u32::try_from(st.size & 0xFFFF_FFFF).unwrap_or(0),
        )?;
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!GetFileAttributesExA` — real extended attributes via VFS.
pub fn handle_get_file_attributes_ex_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine.read_rcx()?;
    let _info_level = engine.read_rdx()?;
    let info_ptr = engine.read_r8()?;
    let path = read_ansi_string_from_cpu(engine, path_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, &path);
    let st = stat_guest_path(state, &full);
    if st.kind == crate::vfs::PathKind::NotFound {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    if info_ptr != 0 {
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 0, "dwFileAttributes"),
            st.attributes,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 4, "ftCreationTime"),
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 12, "ftLastAccessTime"),
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 20, "ftLastWriteTime"),
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 28, "nFileSizeHigh"),
            u32::try_from(st.size >> 32).unwrap_or(0),
        )?;
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 32, "nFileSizeLow"),
            u32::try_from(st.size & 0xFFFF_FFFF).unwrap_or(0),
        )?;
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!BackupRead` — read from open file bytes.
pub fn handle_backup_read(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let to_read = engine.read_r8()?;
    let bytes_read_ptr = engine.read_r9()?;
    let _context = read_stack_u64(engine, 0x28).unwrap_or(0);
    let _secured = read_stack_u64(engine, 0x30).unwrap_or(0);
    if bytes_read_ptr != 0 {
        write_guest_u32(engine, bytes_read_ptr, 0)?;
    }
    if buf == 0 || to_read == 0 {
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(1)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 1,
        });
    }
    if let Some(file) = state.file_io.open_files.get_mut(&handle) {
        let cursor_usize = usize::try_from(file.cursor).unwrap_or(0);
        let available = file.bytes.len().saturating_sub(cursor_usize);
        let to_read_usize = usize::try_from(to_read).unwrap_or(0);
        let read_len = to_read_usize.min(available);
        if read_len > 0 {
            if let Some(data) = file
                .bytes
                .get(cursor_usize..cursor_usize.saturating_add(read_len))
            {
                engine.mem_write(buf, data)?;
            }
            file.cursor = file
                .cursor
                .saturating_add(u64::try_from(read_len).unwrap_or(0));
        }
        if bytes_read_ptr != 0 {
            write_guest_u32(engine, bytes_read_ptr, u32::try_from(read_len).unwrap_or(0))?;
        }
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(1)?;
        Ok(WinApiHandlerResult {
            return_address,
            return_value: 1,
        })
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        })
    }
}
/// Handles `KERNEL32.dll!BackupSeek` — seek within open file bytes.
pub fn handle_backup_seek(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let lo = engine.read_rdx()?;
    let hi = engine.read_r8()?;
    let lo_ptr = engine.read_r9()?;
    let _hi_ptr = read_stack_u64(engine, 0x28).unwrap_or(0);
    let _context = read_stack_u64(engine, 0x30).unwrap_or(0);
    if let Some(file) = state.file_io.open_files.get_mut(&handle) {
        let offset = lo | (hi << 32);
        file.cursor = offset;
        if lo_ptr != 0 {
            write_guest_u32(
                engine,
                lo_ptr,
                u32::try_from(offset & 0xFFFF_FFFF).unwrap_or(0),
            )?;
        }
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(1)?;
        Ok(WinApiHandlerResult {
            return_address,
            return_value: 1,
        })
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        })
    }
}
/// Handles `KERNEL32.dll!BackupWrite` — write to open file bytes.
pub fn handle_backup_write(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let to_write = engine.read_r8()?;
    let written_ptr = engine.read_r9()?;
    let _context = read_stack_u64(engine, 0x28).unwrap_or(0);
    let _secured = read_stack_u64(engine, 0x30).unwrap_or(0);
    if written_ptr != 0 {
        write_guest_u32(engine, written_ptr, 0)?;
    }
    if buf == 0 || to_write == 0 {
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(1)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 1,
        });
    }
    let to_write_usize = usize::try_from(to_write).unwrap_or(0);
    if let Some(file) = state.file_io.open_files.get_mut(&handle) {
        let mut chunk = vec![0_u8; to_write_usize];
        engine.mem_read(buf, &mut chunk)?;
        let cursor_usize = usize::try_from(file.cursor).unwrap_or(0);
        // Extend the file bytes if needed.
        if cursor_usize.saturating_add(to_write_usize) > file.bytes.len() {
            file.bytes
                .resize(cursor_usize.saturating_add(to_write_usize), 0);
        }
        if let Some(dst) = file
            .bytes
            .get_mut(cursor_usize..cursor_usize.saturating_add(to_write_usize))
        {
            dst.copy_from_slice(&chunk);
        }
        file.cursor = file.cursor.saturating_add(to_write);
        if written_ptr != 0 {
            write_guest_u32(engine, written_ptr, u32::try_from(to_write).unwrap_or(0))?;
        }
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(1)?;
        Ok(WinApiHandlerResult {
            return_address,
            return_value: 1,
        })
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        })
    }
}
/// Handles `KERNEL32.dll!GetTempPathW`.
pub fn handle_get_temp_path_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let buffer_len = engine.read_rcx()?;
    let buffer_ptr = engine.read_rdx()?;
    // Trailing backslash per Microsoft Learn.
    let temp = format!("{}\\", crate::vfs::GUEST_TEMP_PATH.trim_end_matches('\\'));
    let units: Vec<u16> = temp.encode_utf16().collect();
    let required = u64::try_from(units.len().saturating_add(1)).unwrap_or(0);
    let return_value = if buffer_ptr == 0 || buffer_len < required {
        required
    } else {
        let mut terminated = units;
        terminated.push(0);
        write_guest_utf16_units(engine, buffer_ptr, &terminated)?;
        u64::try_from(terminated.len().saturating_sub(1)).unwrap_or(0)
    };
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetTempPathA`.
pub fn handle_get_temp_path_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let buffer_len = engine.read_rcx()?;
    let buffer_ptr = engine.read_rdx()?;
    let temp = format!("{}\\", crate::vfs::GUEST_TEMP_PATH.trim_end_matches('\\'));
    let bytes = crate::vfs::encode_acp(&temp);
    let required = u64::try_from(bytes.len().saturating_add(1)).unwrap_or(0);
    let return_value = if buffer_ptr == 0 || buffer_len < required {
        required
    } else {
        let mut out = bytes;
        out.push(0);
        engine.mem_write(buffer_ptr, &out)?;
        u64::try_from(out.len().saturating_sub(1)).unwrap_or(0)
    };
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetTempFileNameW` (unique name under path; creates 0-byte file).
pub fn handle_get_temp_file_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine.read_rcx()?;
    let prefix_ptr = engine.read_rdx()?;
    let unique = engine.read_r8()?;
    let buffer_ptr = engine.read_r9()?;
    let path = if path_ptr == 0 {
        crate::vfs::GUEST_TEMP_PATH.to_owned()
    } else {
        read_wide_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let prefix = if prefix_ptr == 0 {
        "WIE".to_owned()
    } else {
        read_wide_string_from_cpu(engine, prefix_ptr, 16)?
    };
    let prefix: String = prefix.chars().take(3).collect();
    let id = if unique == 0 {
        state.window_state().tick_count = state.window_state().tick_count.wrapping_add(1);
        state.window_state().tick_count
    } else {
        unique
    };
    let id_u32 = temp_name_id_u32(id);
    let name = format!(
        "{}\\{}{:04X}.tmp",
        path.trim_end_matches('\\'),
        prefix,
        id_u32
    );
    finish_create_file_create_only(state, &name);
    if buffer_ptr != 0 {
        let mut units: Vec<u16> = name.encode_utf16().collect();
        units.push(0);
        write_guest_utf16_units(engine, buffer_ptr, &units)?;
    }
    state.process.last_error = 0;
    let return_value = u64::from(id_u32).max(1);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetTempFileNameA`.
pub fn handle_get_temp_file_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine.read_rcx()?;
    let prefix_ptr = engine.read_rdx()?;
    let unique = engine.read_r8()?;
    let buffer_ptr = engine.read_r9()?;
    let path = if path_ptr == 0 {
        crate::vfs::GUEST_TEMP_PATH.to_owned()
    } else {
        read_ansi_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let prefix = if prefix_ptr == 0 {
        "WIE".to_owned()
    } else {
        read_ansi_string_from_cpu(engine, prefix_ptr, 16)?
    };
    let prefix: String = prefix.chars().take(3).collect();
    let id = if unique == 0 {
        state.window_state().tick_count = state.window_state().tick_count.wrapping_add(1);
        state.window_state().tick_count
    } else {
        unique
    };
    let id_u32 = temp_name_id_u32(id);
    let name = format!(
        "{}\\{}{:04X}.tmp",
        path.trim_end_matches('\\'),
        prefix,
        id_u32
    );
    finish_create_file_create_only(state, &name);
    if buffer_ptr != 0 {
        let mut out = crate::vfs::encode_acp(&name);
        out.push(0);
        engine.mem_write(buffer_ptr, &out)?;
    }
    state.process.last_error = 0;
    let return_value = u64::from(id_u32).max(1);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetDriveTypeW`.
pub fn handle_get_drive_type_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, path_ptr, 16)?
    };
    let return_value = u64::from(crate::vfs::get_drive_type(&state.file_io.volumes, &path));
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetDriveTypeA`.
pub fn handle_get_drive_type_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, path_ptr, 16)?
    };
    let return_value = u64::from(crate::vfs::get_drive_type(&state.file_io.volumes, &path));
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetLogicalDrives`.
pub fn handle_get_logical_drives(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = u64::from(crate::vfs::logical_drives_mask(&state.file_io.volumes));
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetSystemDirectoryW`.
pub fn handle_get_system_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    write_fixed_dir_w(engine, crate::vfs::GUEST_SYSTEM_DIR)
}
/// Handles `KERNEL32.dll!GetSystemDirectoryA`.
pub fn handle_get_system_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    write_fixed_dir_a(engine, crate::vfs::GUEST_SYSTEM_DIR)
}
/// Handles `KERNEL32.dll!GetWindowsDirectoryW`.
pub fn handle_get_windows_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    write_fixed_dir_w(engine, crate::vfs::GUEST_WINDOWS_DIR)
}
/// Handles `KERNEL32.dll!GetWindowsDirectoryA`.
pub fn handle_get_windows_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    write_fixed_dir_a(engine, crate::vfs::GUEST_WINDOWS_DIR)
}
/// Handles `KERNEL32.dll!GetFileSizeEx`.
pub fn handle_get_file_size_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let size_ptr = engine.read_rdx()?;
    let return_value = if let Some(open_file) = find_open_file(state, handle) {
        if size_ptr != 0 {
            write_guest_u64(engine, size_ptr, open_file.size())?;
        }
        state.process.last_error = 0;
        1
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!SetFilePointerEx`.
pub fn handle_set_file_pointer_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    // Win64: DistanceToMove is LARGE_INTEGER by value in RDX (signed 64-bit).
    let distance_raw = engine.read_rdx()?;
    let distance = i64::from_le_bytes(distance_raw.to_le_bytes());
    let move_method = engine.read_r8()?;
    let new_pos_ptr = engine.read_r9()?;

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
            if new_pos_ptr != 0 {
                write_guest_u64(engine, new_pos_ptr, new_cursor)?;
            }
            state.process.last_error = 0;
            1
        }
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!FlushFileBuffers`.
pub fn handle_flush_file_buffers(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let return_value = if is_open_file_handle(state, handle) {
        persist_open_file_to_host(state, handle);
        state.process.last_error = 0;
        1
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
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
    find_data_ptr: u64,
    attributes: u32,
    file_size: u64,
) -> Result<()> {
    if find_data_ptr == 0 {
        return Ok(());
    }

    // Build the 44-byte WIN32_FIND_DATA common header on the host stack and
    // push it in a single mem_write. Previously nine scalar write_guest_u32/u64
    // calls, each taking a fresh guest memory RwLock and page-walk.
    //
    // Layout (from wine/mingw headers, matches Microsoft SDK):
    //   +0  dwFileAttributes  u32
    //   +4  ftCreationTime    u64
    //   +12 ftLastAccessTime  u64
    //   +20 ftLastWriteTime   u64
    //   +28 nFileSizeHigh     u32
    //   +32 nFileSizeLow      u32
    //   +36 dwReserved0       u32
    //   +40 dwReserved1       u32
    let mut header = [0_u8; 44];
    header[0..4].copy_from_slice(&attributes.to_le_bytes());
    header[4..12].copy_from_slice(&FIXED_SYSTEM_FILETIME.to_le_bytes());
    header[12..20].copy_from_slice(&FIXED_SYSTEM_FILETIME.to_le_bytes());
    header[20..28].copy_from_slice(&FIXED_SYSTEM_FILETIME.to_le_bytes());
    let hi = u32::try_from(file_size >> 32).unwrap_or(0);
    let lo = u32::try_from(file_size & 0xffff_ffff).unwrap_or(0);
    header[28..32].copy_from_slice(&hi.to_le_bytes());
    header[32..36].copy_from_slice(&lo.to_le_bytes());
    // header[36..44] already zero (dwReserved0 / dwReserved1)
    engine
        .mem_write(find_data_ptr, &header)
        .context("failed to write WIN32_FIND_DATA header")
}

pub(crate) fn write_find_data_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    find_data_ptr: u64,
    file_name: &str,
    attributes: u32,
    file_size: u64,
) -> Result<()> {
    write_find_data_common(engine, find_data_ptr, attributes, file_size)?;

    if find_data_ptr == 0 {
        return Ok(());
    }

    // cFileName is at offset 44 (after dwReserved1), not 48.
    let file_name_address = checked_field_address(find_data_ptr, 44, "WIN32_FIND_DATAW.cFileName");
    // cAlternateFileName[14] starts at 44 + MAX_PATH*2 = 564.
    let alt_name_address =
        checked_field_address(find_data_ptr, 564, "WIN32_FIND_DATAW.cAlternateFileName");

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
    find_data_ptr: u64,
    file_name: &str,
    attributes: u32,
    file_size: u64,
) -> Result<()> {
    write_find_data_common(engine, find_data_ptr, attributes, file_size)?;

    if find_data_ptr == 0 {
        return Ok(());
    }

    // Same header as W; cFileName is CHAR[MAX_PATH] at offset 44.
    let file_name_address = checked_field_address(find_data_ptr, 44, "WIN32_FIND_DATAA.cFileName");
    let alt_name_address =
        checked_field_address(find_data_ptr, 304, "WIN32_FIND_DATAA.cAlternateFileName");

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
    find_data_ptr: u64,
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
            find_data_ptr,
            &first.name,
            first.attributes,
            first.size,
        )?;
    } else {
        write_find_data_a(
            engine,
            find_data_ptr,
            &first.name,
            first.attributes,
            first.size,
        )?;
    }

    let handle = state.file_io.next_find_handle;
    state.file_io.next_find_handle = state
        .file_io
        .next_find_handle
        .checked_add(1)
        .context("find handle overflow")?;

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
    find_data_ptr: u64,
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
        write_find_data_w(
            engine,
            find_data_ptr,
            &next.name,
            next.attributes,
            next.size,
        )?;
    } else {
        write_find_data_a(
            engine,
            find_data_ptr,
            &next.name,
            next.attributes,
            next.size,
        )?;
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
                tracing::debug!(
                    path = %file_name,
                    desired_access,
                    creation_disposition,
                    win_error,
                    "{api_name} open failed"
                );
                state.process.last_error = win_error;
                INVALID_HANDLE_VALUE
            }
        };

    if return_value != INVALID_HANDLE_VALUE {
        tracing::debug!(
            path = %file_name,
            desired_access,
            creation_disposition,
            handle = return_value,
            "{api_name}"
        );
        let _ = crate::guest_io_host::register_open_file(engine, state, return_value).ok();
    }

    return_value
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
    let buffer_ptr = engine.read_rdx()?;
    let units: Vec<u16> = dir.encode_utf16().collect();
    let required = u64::try_from(units.len().saturating_add(1)).unwrap_or(0);
    let return_value = if buffer_ptr == 0 || buffer_len < required {
        required
    } else {
        let mut t = units;
        t.push(0);
        write_guest_utf16_units(engine, buffer_ptr, &t)?;
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
    let buffer_ptr = engine.read_rdx()?;
    let bytes = crate::vfs::encode_acp(dir);
    let required = u64::try_from(bytes.len().saturating_add(1)).unwrap_or(0);
    let return_value = if buffer_ptr == 0 || buffer_len < required {
        required
    } else {
        let mut out = bytes;
        out.push(0);
        engine.mem_write(buffer_ptr, &out)?;
        u64::try_from(out.len().saturating_sub(1)).unwrap_or(0)
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
