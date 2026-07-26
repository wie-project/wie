use super::{
    CREATE_ALWAYS, CREATE_NEW, Context, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND,
    ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_PATH_NOT_FOUND, ERROR_READ_FAULT,
    FAKE_DISK_CLUSTERS, FAKE_DISK_GIB, FAKE_STDERR_HANDLE, FAKE_STDIN_HANDLE, FAKE_STDOUT_HANDLE,
    FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_BEGIN, FILE_CURRENT, FILE_END,
    FILE_TYPE_CHAR, FILE_TYPE_DISK, FILE_TYPE_UNKNOWN, FIXED_SYSTEM_FILETIME,
    INVALID_FILE_ATTRIBUTES, INVALID_HANDLE_VALUE, INVALID_SET_FILE_POINTER, LOGICAL_DRIVE_TCHARS,
    OPEN_ALWAYS, OPEN_EXISTING, OpenFileOutcome, OpenGuestFile, Path, Result, TRUNCATE_EXISTING,
    WinApiHandlerResult, WinApiState, checked_address, checked_field_address,
    file_attributes_for_path, finish_create_directory, finish_create_file,
    finish_create_file_create_only, finish_delete_file, finish_find_first, finish_find_next,
    finish_move_file, finish_remove_directory, get_user_profile_dir_impl, is_main_module_path,
    low_u32, read_ansi_string_from_cpu, read_guest_u64, read_guest_utf16_lossy, read_stack_u64,
    read_wide_string_from_cpu, refill_stdin_from_host, resolve_full_windows_path, ret_bool_true,
    ret_u64, stat_guest_path, sync_open_bytes_to_virtual, temp_name_id_u32, write_fixed_dir_a,
    write_fixed_dir_w, write_guest_u16, write_guest_u32, write_guest_u64, write_guest_utf16_units,
    write_mock_string_a, write_mock_string_w,
};

pub(crate) fn is_console_output_handle(handle: u64) -> bool {
    matches!(handle, FAKE_STDOUT_HANDLE | FAKE_STDERR_HANDLE)
}
pub(crate) fn write_host_console_handle(handle: u64, bytes: &[u8]) {
    let fd = if handle == FAKE_STDOUT_HANDLE {
        libc::STDOUT_FILENO
    } else {
        libc::STDERR_FILENO
    };
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let Some(chunk) = bytes.get(offset..) else {
            break;
        };
        // SAFETY: host stdout/stderr fd; `chunk` is a live contiguous buffer.
        #[expect(unsafe_code)]
        let n = unsafe { libc::write(fd, chunk.as_ptr().cast::<libc::c_void>(), chunk.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        offset = offset.saturating_add(usize::try_from(n).unwrap_or(0));
    }
}
/// Handles `KERNEL32.dll!GetFileType`.
pub fn handle_get_file_type(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_file_attributes_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_file_attributes_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_find_first_file_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_find_first_file_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_find_next_file_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_find_next_file_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_find_close(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_create_file_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_create_file_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_close_handle(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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

        let attributes_address = checked_field_address(info_ptr, 0, "dwFileAttributes")?;
        let creation_time_address = checked_field_address(info_ptr, 4, "ftCreationTime")?;
        let last_access_time_address = checked_field_address(info_ptr, 12, "ftLastAccessTime")?;
        let last_write_time_address = checked_field_address(info_ptr, 20, "ftLastWriteTime")?;
        let volume_serial_address = checked_field_address(info_ptr, 28, "dwVolumeSerialNumber")?;
        let file_size_high_address = checked_field_address(info_ptr, 32, "nFileSizeHigh")?;
        let file_size_low_address = checked_field_address(info_ptr, 36, "nFileSizeLow")?;
        let number_of_links_address = checked_field_address(info_ptr, 40, "nNumberOfLinks")?;
        let file_index_high_address = checked_field_address(info_ptr, 44, "nFileIndexHigh")?;
        let file_index_low_address = checked_field_address(info_ptr, 48, "nFileIndexLow")?;

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
pub(crate) fn guest_basename(path: &str) -> &str {
    crate::vfs::guest_basename(path)
}
pub(crate) fn paths_match_guest(requested: &str, candidate: &str) -> bool {
    crate::vfs::paths_equal_ci(requested, candidate)
}
pub(crate) fn find_open_file(state: &WinApiState, handle: u64) -> Option<&OpenGuestFile> {
    state.file_io.open_files.get(&handle)
}
pub(crate) fn find_open_file_mut(
    state: &mut WinApiState,
    handle: u64,
) -> Option<&mut OpenGuestFile> {
    state.file_io.open_files.get_mut(&handle)
}
pub(crate) fn is_open_file_handle(state: &WinApiState, handle: u64) -> bool {
    state.file_io.open_files.contains_key(&handle)
}
pub fn open_guest_path(state: &mut WinApiState, guest_path: &str) -> Result<u64> {
    match open_or_create_guest_path(state, guest_path, 0, OPEN_EXISTING) {
        Ok(
            OpenFileOutcome::Handle(handle)
            | OpenFileOutcome::HandleExists(handle)
            | OpenFileOutcome::HandleCreated(handle),
        ) => Ok(handle),
        Err(code) => anyhow::bail!("open_guest_path failed: win32 error {code}"),
    }
}
pub(crate) fn open_or_create_guest_path(
    state: &mut WinApiState,
    guest_path: &str,
    _desired_access: u64,
    creation_disposition: u64,
) -> std::result::Result<OpenFileOutcome, u32> {
    if guest_path.is_empty() {
        return Err(ERROR_PATH_NOT_FOUND);
    }

    // Keep volumes.bottle_root in sync with legacy field.
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }

    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = resolve_full_windows_path(&cwd, guest_path);

    let bottle_host =
        crate::vfs::guest_path_to_host(&state.file_io.volumes, &full_path).map(|m| m.host);
    let existed = guest_path_exists(state, &full_path);

    match creation_disposition {
        CREATE_NEW => {
            if existed {
                return Err(ERROR_FILE_EXISTS);
            }
            let handle = create_new_guest_file(state, &full_path, bottle_host.as_ref())?;
            Ok(OpenFileOutcome::HandleCreated(handle))
        }
        CREATE_ALWAYS => {
            let handle = if existed {
                open_existing_guest_file(state, &full_path, bottle_host.as_ref(), true)?
            } else {
                create_new_guest_file(state, &full_path, bottle_host.as_ref())?
            };
            Ok(if existed {
                OpenFileOutcome::HandleExists(handle)
            } else {
                OpenFileOutcome::HandleCreated(handle)
            })
        }
        OPEN_EXISTING => {
            if !existed {
                return Err(ERROR_FILE_NOT_FOUND);
            }
            let handle = open_existing_guest_file(state, &full_path, bottle_host.as_ref(), false)?;
            Ok(OpenFileOutcome::Handle(handle))
        }
        OPEN_ALWAYS => {
            if existed {
                let handle =
                    open_existing_guest_file(state, &full_path, bottle_host.as_ref(), false)?;
                Ok(OpenFileOutcome::HandleExists(handle))
            } else {
                let handle = create_new_guest_file(state, &full_path, bottle_host.as_ref())?;
                Ok(OpenFileOutcome::HandleCreated(handle))
            }
        }
        TRUNCATE_EXISTING => {
            if !existed {
                return Err(ERROR_FILE_NOT_FOUND);
            }
            let handle = open_existing_guest_file(state, &full_path, bottle_host.as_ref(), true)?;
            Ok(OpenFileOutcome::Handle(handle))
        }
        _ => {
            // Unknown disposition — fail closed.
            Err(ERROR_INVALID_PARAMETER)
        }
    }
}
pub(crate) fn open_existing_guest_file(
    state: &mut WinApiState,
    guest_path: &str,
    bottle_host: Option<&std::path::PathBuf>,
    truncate: bool,
) -> std::result::Result<u64, u32> {
    let host_path = bottle_host.cloned().or_else(|| {
        state
            .file_io
            .host_file_mounts
            .iter()
            .find(|m| paths_match_guest(guest_path, &m.guest_path))
            .map(|m| m.host_path.clone())
    });

    // Large host files: stream without loading into RAM.
    if let Some(ref host) = host_path
        && !is_main_module_path(state, guest_path)
        && let Ok(meta) = std::fs::metadata(host)
        && meta.is_file()
        && meta.len() > crate::vfs::BUFFER_SIZE_THRESHOLD
    {
        if truncate {
            drop(crate::vfs::host_set_len(host, 0));
        }
        return allocate_open_file_ex(state, guest_path, Vec::new(), Some(host.clone()), true)
            .map_err(|_| ERROR_FILE_NOT_FOUND);
    }

    let mut bytes =
        resolve_guest_file_bytes(state, guest_path).map_err(|_| ERROR_FILE_NOT_FOUND)?;
    if truncate {
        bytes.clear();
    }
    allocate_open_file(state, guest_path, bytes, host_path).map_err(|_| ERROR_FILE_NOT_FOUND)
}
pub(crate) fn create_new_guest_file(
    state: &mut WinApiState,
    guest_path: &str,
    bottle_host: Option<&std::path::PathBuf>,
) -> std::result::Result<u64, u32> {
    if let Some(host) = bottle_host {
        if let Some(parent) = host.parent() {
            std::fs::create_dir_all(parent).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        }
        std::fs::write(host, []).map_err(|_| ERROR_PATH_NOT_FOUND)?;
        // Host-backed creates always stream: a new archive starts at size 0, so the
        // size-threshold check would otherwise keep the entire growing file in RAM
        // (7za compression of hundreds of MiB was a classic progressive host-RSS leak).
        return allocate_open_file_ex(state, guest_path, Vec::new(), Some(host.clone()), true)
            .map_err(|_| ERROR_PATH_NOT_FOUND);
    }

    // No bottle: keep an in-memory virtual file (session-only).
    ensure_virtual_file(state, guest_path);
    allocate_open_file(state, guest_path, Vec::new(), None).map_err(|_| ERROR_PATH_NOT_FOUND)
}
pub(crate) fn ensure_virtual_file(state: &mut WinApiState, guest_path: &str) {
    if state
        .file_io
        .virtual_files
        .iter()
        .any(|entry| paths_match_guest(guest_path, &entry.guest_path))
    {
        return;
    }

    state.file_io.virtual_files.push(crate::VirtualGuestFile {
        guest_path: guest_path.to_owned(),
        bytes: Vec::new(),
    });
}
pub(crate) fn resolve_guest_file_bytes(state: &WinApiState, guest_path: &str) -> Result<Vec<u8>> {
    if is_main_module_path(state, guest_path) {
        return Ok(state.file_io.executable_file_bytes.clone());
    }

    if let Some(mount) = state
        .file_io
        .host_file_mounts
        .iter()
        .find(|mount| paths_match_guest(guest_path, &mount.guest_path))
    {
        return std::fs::read(&mount.host_path).with_context(|| {
            format!(
                "failed to read mounted host file {} for guest path {guest_path}",
                mount.host_path.display()
            )
        });
    }

    if let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, guest_path)
        && map.host.is_file()
    {
        return std::fs::read(&map.host).with_context(|| {
            format!(
                "failed to read volume file {} for guest path {guest_path}",
                map.host.display()
            )
        });
    }

    if let Some(virtual_file) = state
        .file_io
        .virtual_files
        .iter()
        .find(|entry| paths_match_guest(guest_path, &entry.guest_path))
    {
        return Ok(virtual_file.bytes.clone());
    }

    // Allow opening by host absolute path when the guest happens to pass it
    // (useful for ad-hoc testing).
    let as_path = Path::new(guest_path);
    if as_path.is_absolute() && as_path.is_file() {
        return std::fs::read(as_path)
            .with_context(|| format!("failed to read host path {guest_path}"));
    }

    anyhow::bail!("guest file not found: {guest_path}")
}
pub(crate) fn read_create_file_stack_u32(
    engine: &mut dyn wie_cpu::CpuEngine,
    offset: u64,
) -> Result<u32> {
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for CreateFile stack arg")?;

    let address = rsp
        .checked_add(offset)
        .context("CreateFile stack arg address overflow")?;

    let mut bytes = [0_u8; 4];
    engine
        .mem_read(address, &mut bytes)
        .context("failed to read CreateFile stack arg")?;

    Ok(u32::from_le_bytes(bytes))
}
pub(crate) fn allocate_open_file(
    state: &mut WinApiState,
    path: &str,
    bytes: Vec<u8>,
    host_path: Option<std::path::PathBuf>,
) -> Result<u64> {
    allocate_open_file_ex(state, path, bytes, host_path, false)
}
pub(crate) fn allocate_open_file_ex(
    state: &mut WinApiState,
    path: &str,
    bytes: Vec<u8>,
    host_path: Option<std::path::PathBuf>,
    force_stream: bool,
) -> Result<u64> {
    let handle = state.file_io.next_file_handle;

    state.file_io.next_file_handle = state
        .file_io
        .next_file_handle
        .checked_add(1)
        .context("guest file handle allocator overflow")?;

    let size_u64 = if force_stream {
        host_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len())
    } else {
        u64::try_from(bytes.len()).unwrap_or(0)
    };
    // Stream large host-backed files; keep small files fully buffered for guest I/O accel.
    let streaming = force_stream
        || (host_path.is_some()
            && size_u64 > crate::vfs::BUFFER_SIZE_THRESHOLD
            && !is_main_module_path(state, path));
    let bytes = if streaming { Vec::new() } else { bytes };

    state.file_io.open_files.insert(
        handle,
        OpenGuestFile {
            handle,
            path: path.to_owned(),
            bytes,
            cursor: 0,
            host_path,
            streaming,
            guest_data_va: None,
            guest_slot_index: None,
        },
    );

    // Keep legacy single-handle fields in sync when opening the main executable.
    if is_main_module_path(state, path) {
        state.file_io.executable_file_cursor = 0;
    }

    Ok(handle)
}
pub(crate) fn persist_open_file_to_host(state: &WinApiState, handle: u64) {
    let Some(open_file) = find_open_file(state, handle) else {
        return;
    };
    if open_file.streaming {
        return;
    }
    let Some(host_path) = open_file.host_path.as_ref() else {
        return;
    };
    if let Some(parent) = host_path.parent() {
        drop(std::fs::create_dir_all(parent));
    }
    if let Err(error) = std::fs::write(host_path, &open_file.bytes) {
        tracing::debug!(
            handle,
            path = %host_path.display(),
            error = %error,
            "failed to persist open file to host"
        );
    }
}
pub(crate) fn maybe_promote_open_file_to_streaming(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    handle: u64,
) {
    let host_path = {
        let Some(open_file) = find_open_file(state, handle) else {
            return;
        };
        if open_file.streaming {
            return;
        }
        let size_u64 = u64::try_from(open_file.bytes.len()).unwrap_or(0);
        if size_u64 <= crate::vfs::BUFFER_SIZE_THRESHOLD {
            return;
        }
        let Some(host) = open_file.host_path.as_ref() else {
            return;
        };
        host.clone()
    };
    if let Some(parent) = host_path.parent() {
        drop(std::fs::create_dir_all(parent));
    }
    {
        let Some(open_file) = find_open_file(state, handle) else {
            return;
        };
        if let Err(error) = std::fs::write(&host_path, &open_file.bytes) {
            tracing::debug!(
                handle,
                path = %host_path.display(),
                error = %error,
                "failed to spill buffered file to host for streaming promote"
            );
            return;
        }
    }
    // Drop any guest I/O mirror (streaming stays on host path only).
    let _ = crate::guest_io_host::unregister_open_file(engine, state, handle).ok();
    if let Some(open_file) = find_open_file_mut(state, handle) {
        open_file.bytes.clear();
        open_file.bytes.shrink_to_fit();
        open_file.streaming = true;
    }
}
pub fn mount_host_file(
    state: &mut WinApiState,
    guest_path: &str,
    host_path: impl AsRef<Path>,
) -> Result<()> {
    let host_path = host_path.as_ref();

    if !host_path.is_file() {
        anyhow::bail!(
            "host file does not exist or is not a regular file: {}",
            host_path.display()
        );
    }

    // Replace existing mount for the same guest path.
    state
        .file_io
        .host_file_mounts
        .retain(|mount| !paths_match_guest(guest_path, &mount.guest_path));

    state.file_io.host_file_mounts.push(crate::HostFileMount {
        guest_path: guest_path.to_owned(),
        host_path: host_path.to_path_buf(),
    });

    Ok(())
}
pub(crate) fn guest_path_exists(state: &WinApiState, path: &str) -> bool {
    if is_main_module_path(state, path) {
        return true;
    }
    if state
        .file_io
        .host_file_mounts
        .iter()
        .any(|mount| paths_match_guest(path, &mount.guest_path))
    {
        return true;
    }
    if state
        .file_io
        .virtual_files
        .iter()
        .any(|entry| paths_match_guest(path, &entry.guest_path))
    {
        return true;
    }
    if let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, path) {
        return map.host.is_file();
    }
    false
}
pub(crate) fn guest_dir_exists(state: &WinApiState, path: &str) -> bool {
    let attrs = file_attributes_for_path(state, path);
    attrs != INVALID_FILE_ATTRIBUTES && (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0
}
/// Handles `KERNEL32.dll!FileTimeToLocalFileTime`.
pub fn handle_file_time_to_local_file_time(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let input_file_time_ptr = engine
        .read_rcx()
        .context("failed to read RCX for FileTimeToLocalFileTime")?;

    let output_file_time_ptr = engine
        .read_rdx()
        .context("failed to read RDX for FileTimeToLocalFileTime")?;

    let success = input_file_time_ptr != 0 && output_file_time_ptr != 0;

    if success {
        let mut bytes = [0_u8; 8];

        engine
            .mem_read(input_file_time_ptr, &mut bytes)
            .context("failed to read input FILETIME")?;

        engine
            .mem_write(output_file_time_ptr, &bytes)
            .context("failed to write output FILETIME")?;

        state.process.last_error = 0;
    } else {
        state.process.last_error = ERROR_INVALID_PARAMETER;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from FileTimeToLocalFileTime")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!FileTimeToSystemTime`.
pub fn handle_file_time_to_system_time(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let input_file_time_ptr = engine
        .read_rcx()
        .context("failed to read RCX for FileTimeToSystemTime")?;

    let system_time_ptr = engine
        .read_rdx()
        .context("failed to read RDX for FileTimeToSystemTime")?;

    let success = input_file_time_ptr != 0 && system_time_ptr != 0;

    if success {
        // SYSTEMTIME:
        // WORD wYear;         offset 0
        // WORD wMonth;        offset 2
        // WORD wDayOfWeek;    offset 4
        // WORD wDay;          offset 6
        // WORD wHour;         offset 8
        // WORD wMinute;       offset 10
        // WORD wSecond;       offset 12
        // WORD wMilliseconds; offset 14

        let year_address = checked_field_address(system_time_ptr, 0, "wYear")?;
        let month_address = checked_field_address(system_time_ptr, 2, "wMonth")?;
        let day_of_week_address = checked_field_address(system_time_ptr, 4, "wDayOfWeek")?;
        let day_address = checked_field_address(system_time_ptr, 6, "wDay")?;
        let hour_address = checked_field_address(system_time_ptr, 8, "wHour")?;
        let minute_address = checked_field_address(system_time_ptr, 10, "wMinute")?;
        let second_address = checked_field_address(system_time_ptr, 12, "wSecond")?;
        let milliseconds_address = checked_field_address(system_time_ptr, 14, "wMilliseconds")?;

        // Deterministic fake converted time.
        write_guest_u16(engine, year_address, 2026)?;
        write_guest_u16(engine, month_address, 7)?;
        write_guest_u16(engine, day_of_week_address, 4)?;
        write_guest_u16(engine, day_address, 9)?;
        write_guest_u16(engine, hour_address, 12)?;
        write_guest_u16(engine, minute_address, 0)?;
        write_guest_u16(engine, second_address, 0)?;
        write_guest_u16(engine, milliseconds_address, 0)?;

        state.process.last_error = 0;
    } else {
        state.process.last_error = ERROR_INVALID_PARAMETER;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from FileTimeToSystemTime")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetFileTime`.
pub fn handle_get_file_time(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for GetFileTime")?;

    let creation_time_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetFileTime")?;

    let last_access_time_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetFileTime")?;

    let last_write_time_ptr = engine
        .read_r9()
        .context("failed to read R9 for GetFileTime")?;

    let success = is_open_file_handle(state, handle);

    if success {
        if creation_time_ptr != 0 {
            write_guest_u64(engine, creation_time_ptr, FIXED_SYSTEM_FILETIME)?;
        }

        if last_access_time_ptr != 0 {
            write_guest_u64(engine, last_access_time_ptr, FIXED_SYSTEM_FILETIME)?;
        }

        if last_write_time_ptr != 0 {
            write_guest_u64(engine, last_write_time_ptr, FIXED_SYSTEM_FILETIME)?;
        }

        state.process.last_error = 0;
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
    }

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFileTime")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!SetFilePointer`.
pub fn handle_set_file_pointer(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for SetFilePointer")?;

    let distance_low = engine
        .read_rdx()
        .context("failed to read RDX for SetFilePointer")?;

    let distance_high_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetFilePointer")?;

    let move_method = engine
        .read_r9()
        .context("failed to read R9 for SetFilePointer")?;

    let valid_method =
        move_method == FILE_BEGIN || move_method == FILE_CURRENT || move_method == FILE_END;

    let return_value = if !is_open_file_handle(state, handle) {
        state.process.last_error = ERROR_INVALID_HANDLE;
        INVALID_SET_FILE_POINTER
    } else if !valid_method {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        INVALID_SET_FILE_POINTER
    } else {
        let low_u32 = u32::try_from(distance_low & 0xffff_ffff)
            .context("SetFilePointer low distance does not fit u32")?;
        let signed_low = i64::from(i32::from_ne_bytes(low_u32.to_ne_bytes()));

        let (new_cursor, path) = {
            let open_file = find_open_file_mut(state, handle)
                .context("open file vanished during SetFilePointer")?;

            let file_size = open_file.size();
            let path = open_file.path.clone();

            let base = if move_method == FILE_BEGIN {
                0_i64
            } else if move_method == FILE_CURRENT {
                i64::try_from(open_file.cursor).context("file cursor does not fit i64")?
            } else {
                i64::try_from(file_size).context("file size does not fit i64")?
            };

            let new_position = base
                .checked_add(signed_low)
                .context("SetFilePointer result overflow")?;

            if new_position < 0 {
                (None, path)
            } else {
                let new_cursor =
                    u64::try_from(new_position).context("new file cursor does not fit u64")?;
                open_file.cursor = new_cursor;
                (Some(new_cursor), path)
            }
        };

        if let Some(new_cursor) = new_cursor {
            if is_main_module_path(state, &path) {
                state.file_io.executable_file_cursor = new_cursor;
            }

            if distance_high_ptr != 0 {
                let high = u32::try_from(new_cursor >> 32)
                    .context("new file cursor high does not fit u32")?;
                write_guest_u32(engine, distance_high_ptr, high)?;
            }

            state.process.last_error = 0;
            let _ = crate::guest_io_host::sync_slot_from_host(engine, state, handle).ok();
            new_cursor & 0xffff_ffff
        } else {
            state.process.last_error = ERROR_INVALID_PARAMETER;
            INVALID_SET_FILE_POINTER
        }
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetFilePointer")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetFileSize`.
pub fn handle_get_file_size(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for GetFileSize")?;

    let file_size_high_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetFileSize")?;

    let return_value = if let Some(open_file) = find_open_file(state, handle) {
        let file_size = open_file.size();

        let file_size_high =
            u32::try_from(file_size >> 32).context("open file size high does not fit u32")?;

        let file_size_low = u32::try_from(file_size & 0xffff_ffff)
            .context("open file size low does not fit u32")?;

        if file_size_high_ptr != 0 {
            write_guest_u32(engine, file_size_high_ptr, file_size_high)?;
        }

        state.process.last_error = 0;

        u64::from(file_size_low)
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0xffff_ffff
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFileSize")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!ReadFile`.
pub fn handle_read_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_write_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
/// Handles `KERNEL32.dll!GetCurrentDirectoryW`.
pub fn handle_get_current_directory_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let buffer_length = engine
        .read_rcx()
        .context("failed to read RCX for GetCurrentDirectoryW")?;

    let buffer_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetCurrentDirectoryW")?;

    let directory = state.file_io.current_directory_wide.clone();

    let character_count = u64::try_from(directory.len())
        .context("fake current directory length does not fit in u64")?;

    let required_with_nul = character_count
        .checked_add(1)
        .context("GetCurrentDirectoryW required size overflow")?;

    // Need nBufferLength > character_count so there is room for the NUL.
    let return_value = if buffer_ptr == 0 || buffer_length == 0 || buffer_length <= character_count
    {
        required_with_nul
    } else {
        let mut encoded = directory;
        encoded.push(0);

        let byte_len = encoded
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .context("GetCurrentDirectoryW byte length overflow")?;

        let mut bytes = Vec::with_capacity(byte_len);

        for code_unit in encoded {
            bytes.extend_from_slice(&code_unit.to_le_bytes());
        }

        engine
            .mem_write(buffer_ptr, &bytes)
            .context("failed to write GetCurrentDirectoryW buffer")?;

        character_count
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetCurrentDirectoryW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!SetCurrentDirectoryW`.
pub fn handle_set_current_directory_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let directory_ptr = engine
        .read_rcx()
        .context("failed to read RCX for SetCurrentDirectoryW")?;

    let success = if directory_ptr == 0 {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        false
    } else {
        let directory = read_wide_string_from_cpu(engine, directory_ptr, 32_768)
            .context("failed to read SetCurrentDirectoryW path")?;

        if directory.is_empty() {
            state.process.last_error = ERROR_PATH_NOT_FOUND;
            false
        } else {
            // Relative directory names resolve against the current directory (MSDN).
            let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
            let full = resolve_full_windows_path(&cwd, &directory);
            if guest_dir_exists(state, &full) {
                state.file_io.current_directory_wide = full.encode_utf16().collect();
                // Keep guest cwd blob in sync when stubs are installed (best-effort).
                state.process.last_error = 0;
                true
            } else {
                state.process.last_error = ERROR_PATH_NOT_FOUND;
                false
            }
        }
    };

    let return_value = u64::from(success);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from SetCurrentDirectoryW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub fn handle_compare_file_time(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let ta = if a == 0 {
        0
    } else {
        read_guest_u64(engine, a)?
    };
    let tb = if b == 0 {
        0
    } else {
        read_guest_u64(engine, b)?
    };
    let cmp: i32 = match ta.cmp(&tb) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    ret_u64(
        engine,
        u64::from_ne_bytes(i64::from(cmp).to_ne_bytes()),
        "CompareFileTime",
    )
}
pub fn handle_local_file_time_to_file_time(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let local = engine.read_rcx()?;
    let file = engine.read_rdx()?;
    if local == 0 || file == 0 {
        return ret_u64(engine, 0, "LocalFileTimeToFileTime");
    }
    // Prototype: treat local == UTC (no timezone conversion).
    let t = read_guest_u64(engine, local)?;
    write_guest_u64(engine, file, t)?;
    ret_bool_true(engine, "LocalFileTimeToFileTime")
}
pub fn handle_file_time_to_dos_date_time(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let ft = engine.read_rcx()?;
    let date_ptr = engine.read_rdx()?;
    let time_ptr = engine.read_r8()?;
    if ft == 0 {
        return ret_u64(engine, 0, "FileTimeToDosDateTime");
    }
    // Fixed DOS date/time: 2026-07-19 12:00:00 → rough encoding.
    // DOS date: day + (month<<5) + ((year-1980)<<9)
    // 2026-07-19 → DOS date word; noon → DOS time word.
    let dos_date: u16 = 0x5c_f3; // precomputed: day|month<<5|(year-1980)<<9
    let dos_time: u16 = 0x60_00; // hour 12 << 11
    if date_ptr != 0 {
        write_guest_u16(engine, date_ptr, dos_date)?;
    }
    if time_ptr != 0 {
        write_guest_u16(engine, time_ptr, dos_time)?;
    }
    ret_bool_true(engine, "FileTimeToDosDateTime")
}
pub fn handle_dos_date_time_to_file_time(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let _date = engine.read_rcx()?;
    let _time = engine.read_rdx()?;
    let ft = engine.read_r8()?;
    if ft == 0 {
        return ret_u64(engine, 0, "DosDateTimeToFileTime");
    }
    write_guest_u64(engine, ft, FIXED_SYSTEM_FILETIME)?;
    ret_bool_true(engine, "DosDateTimeToFileTime")
}
pub fn handle_get_disk_free_space_ex_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    _state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_disk_free_space_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    _state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _path = engine.read_rcx()?;
    let spc = engine.read_rdx()?; // sectors per cluster
    let bps = engine.read_r8()?; // bytes per sector
    let free_clusters = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let total_clusters = read_guest_u64(
        engine,
        checked_address(rsp, 0x28, "GetDiskFreeSpaceW total")?,
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
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_set_file_attributes_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    _state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _path = engine.read_rcx()?;
    let _attrs = engine.read_rdx()?;
    // Best-effort success (VFS does not track Win32 attributes yet).
    ret_bool_true(engine, "SetFileAttributesW")
}
pub fn handle_set_file_time(
    engine: &mut dyn wie_cpu::CpuEngine,
    _state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _handle = engine.read_rcx()?;
    let _creation = engine.read_rdx()?;
    let _access = engine.read_r8()?;
    let _write = engine.read_r9()?;
    ret_bool_true(engine, "SetFileTime")
}
pub fn handle_move_file_with_progress_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    // Same first two args as MoveFileW (existing/new).
    handle_move_file_w(engine, state)
}
pub fn handle_create_hard_link_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _ = (engine.read_rcx()?, engine.read_rdx()?, engine.read_r8()?);
    state.process.last_error = 1; // ERROR_INVALID_FUNCTION-ish
    ret_u64(engine, 0, "CreateHardLinkW")
}
pub fn handle_find_first_stream_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _ = (engine.read_rcx()?, engine.read_rdx()?, engine.read_r8()?);
    state.process.last_error = 38; // ERROR_HANDLE_EOF
    ret_u64(engine, u64::MAX, "FindFirstStreamW") // INVALID_HANDLE_VALUE
}
pub fn handle_find_next_stream_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _ = (engine.read_rcx()?, engine.read_rdx()?);
    state.process.last_error = 38;
    ret_u64(engine, 0, "FindNextStreamW")
}
pub fn handle_device_io_control(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _root = engine.read_rcx()?;
    let vol_name = engine.read_rdx()?;
    let vol_name_len = engine.read_r8()?;
    let _serial = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let max_comp_ptr = checked_address(rsp, 0x28, "lpMaximumComponentLength")
        .ok()
        .unwrap_or(0);
    let flags_ptr = checked_address(rsp, 0x30, "lpFileSystemFlags")
        .ok()
        .unwrap_or(0);
    let name_ptr = checked_address(rsp, 0x38, "lpFileSystemNameBuffer")
        .ok()
        .unwrap_or(0);
    let fs_len_ptr = checked_address(rsp, 0x40, "lpFileSystemNameLength")
        .ok()
        .unwrap_or(0);

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
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _root = engine.read_rcx()?;
    let vol_name = engine.read_rdx()?;
    let vol_name_len = engine.read_r8()?;
    let _serial = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let max_comp_ptr = checked_address(rsp, 0x28, "lpMaximumComponentLength")
        .ok()
        .unwrap_or(0);
    let flags_ptr = checked_address(rsp, 0x30, "lpFileSystemFlags")
        .ok()
        .unwrap_or(0);
    let name_ptr = checked_address(rsp, 0x38, "lpFileSystemNameBuffer")
        .ok()
        .unwrap_or(0);
    let fs_len_ptr = checked_address(rsp, 0x40, "lpFileSystemNameLength")
        .ok()
        .unwrap_or(0);

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
pub fn handle_lock_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_unlock_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_set_file_valid_data(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
/// Handles `KERNEL32.dll!GetLongPathNameW` — return same as input.
pub fn handle_get_long_path_name_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let src = engine.read_rcx()?;
    let dst = engine.read_rdx()?;
    let dst_len = engine.read_r8()?;
    let path = read_wide_string_from_cpu(engine, src, 1024)?;
    let written = write_mock_string_w(engine, state, &path, dst, dst_len)?;
    let return_address = engine.return_from_win64_api(written)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: written,
    })
}
/// Handles `KERNEL32.dll!GetLongPathNameA` — return same as input.
pub fn handle_get_long_path_name_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let src = engine.read_rcx()?;
    let dst = engine.read_rdx()?;
    let dst_len = engine.read_r8()?;
    let path = read_ansi_string_from_cpu(engine, src, 1024)?;
    let written = write_mock_string_a(engine, state, &path, dst, dst_len)?;
    let return_address = engine.return_from_win64_api(written)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: written,
    })
}
/// Handles `KERNEL32.dll!GetShortPathNameW` — return same as input.
pub fn handle_get_short_path_name_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let src = engine.read_rcx()?;
    let dst = engine.read_rdx()?;
    let dst_len = engine.read_r8()?;
    let path = read_wide_string_from_cpu(engine, src, 1024)?;
    let written = write_mock_string_w(engine, state, &path, dst, dst_len)?;
    let return_address = engine.return_from_win64_api(written)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: written,
    })
}
/// Handles `KERNEL32.dll!GetShortPathNameA` — return same as input.
pub fn handle_get_short_path_name_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let src = engine.read_rcx()?;
    let dst = engine.read_rdx()?;
    let dst_len = engine.read_r8()?;
    let path = read_ansi_string_from_cpu(engine, src, 1024)?;
    let written = write_mock_string_a(engine, state, &path, dst, dst_len)?;
    let return_address = engine.return_from_win64_api(written)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: written,
    })
}
/// Handles `KERNEL32.dll!GetUserProfileDirectoryW` — return profile path from bottle/env.
pub fn handle_get_user_profile_directory_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _h_profile = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let size_ptr = engine.read_r8()?;
    get_user_profile_dir_impl(engine, state, buf, size_ptr, true)
}
/// Handles `KERNEL32.dll!GetUserProfileDirectoryA` — return profile path from bottle/env.
pub fn handle_get_user_profile_directory_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _h_profile = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let size_ptr = engine.read_r8()?;
    get_user_profile_dir_impl(engine, state, buf, size_ptr, false)
}
/// Handles `KERNEL32.dll!GetFileAttributesExW` — real extended attributes via VFS.
pub fn handle_get_file_attributes_ex_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
            checked_field_address(info_ptr, 0, "dwFileAttributes")?,
            st.attributes,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 4, "ftCreationTime")?,
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 12, "ftLastAccessTime")?,
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 20, "ftLastWriteTime")?,
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 28, "nFileSizeHigh")?,
            u32::try_from(st.size >> 32).unwrap_or(0),
        )?;
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 32, "nFileSizeLow")?,
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
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
            checked_field_address(info_ptr, 0, "dwFileAttributes")?,
            st.attributes,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 4, "ftCreationTime")?,
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 12, "ftLastAccessTime")?,
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u64(
            engine,
            checked_field_address(info_ptr, 20, "ftLastWriteTime")?,
            FIXED_SYSTEM_FILETIME,
        )?;
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 28, "nFileSizeHigh")?,
            u32::try_from(st.size >> 32).unwrap_or(0),
        )?;
        write_guest_u32(
            engine,
            checked_field_address(info_ptr, 32, "nFileSizeLow")?,
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
pub fn handle_backup_read(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_backup_seek(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_backup_write(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub(crate) fn handle_duplicate_handle(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    // RCX = hSourceProcessHandle
    // RDX = hSourceHandle
    // R8  = hTargetProcessHandle
    // R9  = lpTargetHandle (guest pointer for the duplicated handle)
    // [RSP+0x28] = dwDesiredAccess
    // [RSP+0x30] = bInheritHandle
    // [RSP+0x38] = dwOptions
    let _source_proc = engine.read_rcx()?;
    let source_handle = engine.read_rdx()?;
    let _target_proc = engine.read_r8()?;
    let target_handle_ptr = engine.read_r9()?;

    // Read dwOptions from the guest stack to honour DUPLICATE_CLOSE_SOURCE.
    let rsp = engine.read_rsp()?;
    let mut opt_bytes = [0_u8; 4];
    let close_source = if engine
        .mem_read(rsp.wrapping_add(0x38), &mut opt_bytes)
        .is_ok()
    {
        let opts = u32::from_le_bytes(opt_bytes);
        (opts & 0x1) != 0 // DUPLICATE_CLOSE_SOURCE = 0x1
    } else {
        false
    };

    // Resolve pseudohandles to real kernel objects.
    // Windows: (HANDLE)-1 = GetCurrentProcess, (HANDLE)-2 = GetCurrentThread.
    // Both are resolved to a ThreadObject for the calling thread.  WIE does
    // not model process kernel objects — all handles map to threads.
    let tid = if source_handle == u64::MAX || source_handle == u64::MAX - 1 {
        // Pseudohandle → resolve the current TID.  For GetCurrentProcess we
        // use the primary TID since there is no process object.
        state.kernel.threads.current_tid()
    } else {
        // Real kernel handle — skip resolution, lookup directly below.
        let obj = state.kernel.sync.objects.get(&source_handle).cloned();
        if let Some(obj) = obj {
            let new_handle = state.kernel.sync.next_handle;
            state.kernel.sync.next_handle = state.kernel.sync.next_handle.wrapping_add(4);
            state.kernel.sync.objects.insert(new_handle, obj.clone());
            if close_source {
                state.kernel.sync.objects.remove(&source_handle);
            }
            engine.mem_write(target_handle_ptr, &new_handle.to_le_bytes())?;
            state.process.last_error = 0;
            let return_address = engine.return_from_win64_api(1)?;
            return Ok(WinApiHandlerResult {
                return_address,
                return_value: 1,
            });
        }
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    };

    // Pseudohandle path: find or create the ThreadObject for `tid`.
    let source_obj = state
        .kernel
        .sync
        .objects
        .values()
        .find_map(|obj| match obj {
            crate::KernelObject::Thread(t) if t.tid == tid => Some(obj.clone()),
            _ => None,
        })
        .unwrap_or_else(|| {
            let (_, th) = state
                .kernel
                .sync
                .register_thread(tid, wie_cpu::ThreadContext::default());
            crate::KernelObject::Thread(th)
        });

    let new_handle = state.kernel.sync.next_handle;
    state.kernel.sync.next_handle = state.kernel.sync.next_handle.wrapping_add(4);
    state.kernel.sync.objects.insert(new_handle, source_obj);

    // Honour DUPLICATE_CLOSE_SOURCE: close the source handle after duplication.
    if close_source && source_handle != u64::MAX && source_handle != u64::MAX - 1 {
        state.kernel.sync.objects.remove(&source_handle);
    }

    engine.mem_write(target_handle_ptr, &new_handle.to_le_bytes())?;
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?; // TRUE
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!GetFullPathNameW`.
pub fn handle_get_full_path_name_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let input_path_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetFullPathNameW")?;

    let buffer_characters_raw = engine
        .read_rdx()
        .context("failed to read RDX for GetFullPathNameW")?;

    let output_buffer_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetFullPathNameW")?;

    let file_part_ptr_ptr = engine
        .read_r9()
        .context("failed to read R9 for GetFullPathNameW")?;

    let return_value = if input_path_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        0
    } else {
        let input_path = read_guest_utf16_lossy(engine, input_path_ptr, 32_768)
            .context("failed to read GetFullPathNameW input path")?;

        if input_path.is_empty() {
            state.process.last_error = ERROR_INVALID_PARAMETER;
            0
        } else {
            let current_directory = String::from_utf16_lossy(&state.file_io.current_directory_wide);

            let full_path = resolve_full_windows_path(&current_directory, &input_path);

            let path_units = full_path.encode_utf16().collect::<Vec<_>>();

            let path_length = path_units.len();

            let required_with_null = path_length
                .checked_add(1)
                .context("GetFullPathNameW required length overflow")?;

            let buffer_characters = usize::try_from(buffer_characters_raw)
                .context("GetFullPathNameW buffer size does not fit usize")?;

            if output_buffer_ptr == 0 || buffer_characters < required_with_null {
                if file_part_ptr_ptr != 0 {
                    write_guest_u64(engine, file_part_ptr_ptr, 0)?;
                }

                state.process.last_error = 0;

                u64::try_from(required_with_null)
                    .context("GetFullPathNameW required length does not fit u64")?
            } else {
                let mut terminated_units = path_units;
                terminated_units.push(0);

                write_guest_utf16_units(engine, output_buffer_ptr, &terminated_units)?;

                if file_part_ptr_ptr != 0 {
                    let file_component_offset = full_path
                        .rfind('\\')
                        .map_or(0, |separator_index| separator_index.saturating_add(1));

                    let prefix_units = full_path
                        .get(..file_component_offset)
                        .unwrap_or_default()
                        .encode_utf16()
                        .count();

                    let byte_offset = prefix_units
                        .checked_mul(std::mem::size_of::<u16>())
                        .context("GetFullPathNameW file-part byte offset overflow")?;

                    let byte_offset_u64 = u64::try_from(byte_offset)
                        .context("GetFullPathNameW file-part offset does not fit u64")?;

                    let file_part_ptr = output_buffer_ptr
                        .checked_add(byte_offset_u64)
                        .context("GetFullPathNameW file-part pointer overflow")?;

                    write_guest_u64(engine, file_part_ptr_ptr, file_part_ptr)?;
                }

                state.process.last_error = 0;

                u64::try_from(path_length)
                    .context("GetFullPathNameW result length does not fit u64")?
            }
        }
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetFullPathNameW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetFullPathNameA`.
pub fn handle_get_full_path_name_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let input_path_ptr = engine.read_rcx()?;
    let buffer_characters_raw = engine.read_rdx()?;
    let output_buffer_ptr = engine.read_r8()?;
    let file_part_ptr_ptr = engine.read_r9()?;

    let return_value = if input_path_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        0
    } else {
        let input_path = read_ansi_string_from_cpu(engine, input_path_ptr, 32_768)?;
        if input_path.is_empty() {
            state.process.last_error = ERROR_INVALID_PARAMETER;
            0
        } else {
            let current_directory = String::from_utf16_lossy(&state.file_io.current_directory_wide);
            let full_path = resolve_full_windows_path(&current_directory, &input_path);
            let path_bytes = crate::vfs::encode_acp(&full_path);
            let path_length = path_bytes.len();
            let required_with_null = path_length.saturating_add(1);
            let buffer_characters = usize::try_from(buffer_characters_raw).unwrap_or(0);
            if output_buffer_ptr == 0 || buffer_characters < required_with_null {
                if file_part_ptr_ptr != 0 {
                    write_guest_u64(engine, file_part_ptr_ptr, 0)?;
                }
                state.process.last_error = 0;
                u64::try_from(required_with_null).unwrap_or(0)
            } else {
                let mut out = path_bytes;
                out.push(0);
                engine.mem_write(output_buffer_ptr, &out)?;
                if file_part_ptr_ptr != 0 {
                    let file_off = full_path.rfind('\\').map_or(0, |i| i.saturating_add(1));
                    let file_part_ptr =
                        output_buffer_ptr.saturating_add(u64::try_from(file_off).unwrap_or(0));
                    write_guest_u64(engine, file_part_ptr_ptr, file_part_ptr)?;
                }
                state.process.last_error = 0;
                u64::try_from(path_length).unwrap_or(0)
            }
        }
    };

    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetCurrentDirectoryA`.
pub fn handle_get_current_directory_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let buffer_length = engine.read_rcx()?;
    let buffer_ptr = engine.read_rdx()?;
    let directory = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let bytes = crate::vfs::encode_acp(&directory);
    let character_count = u64::try_from(bytes.len()).unwrap_or(0);
    let required_with_nul = character_count.saturating_add(1);
    let return_value = if buffer_ptr == 0 || buffer_length == 0 || buffer_length <= character_count
    {
        required_with_nul
    } else {
        let mut out = bytes;
        out.push(0);
        engine.mem_write(buffer_ptr, &out)?;
        character_count
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!SetCurrentDirectoryA`.
pub fn handle_set_current_directory_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let directory_ptr = engine.read_rcx()?;
    let success = if directory_ptr == 0 {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        false
    } else {
        let directory = read_ansi_string_from_cpu(engine, directory_ptr, 32_768)?;
        if directory.is_empty() {
            state.process.last_error = ERROR_PATH_NOT_FOUND;
            false
        } else {
            let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
            let full = resolve_full_windows_path(&cwd, &directory);
            if guest_dir_exists(state, &full) {
                state.file_io.current_directory_wide = full.encode_utf16().collect();
                state.process.last_error = 0;
                true
            } else {
                state.process.last_error = ERROR_PATH_NOT_FOUND;
                false
            }
        }
    };
    let return_value = u64::from(success);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!CreateDirectoryW`.
pub fn handle_create_directory_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_create_directory(state, &path);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!CreateDirectoryA`.
pub fn handle_create_directory_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_create_directory(state, &path);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!DeleteFileW`.
pub fn handle_delete_file_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_delete_file(state, &path);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!DeleteFileA`.
pub fn handle_delete_file_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_delete_file(state, &path);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!RemoveDirectoryW`.
pub fn handle_remove_directory_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_wide_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_remove_directory(state, &path);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!RemoveDirectoryA`.
pub fn handle_remove_directory_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let path_ptr = engine.read_rcx()?;
    let path = if path_ptr == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, path_ptr, 32_768)?
    };
    let return_value = finish_remove_directory(state, &path);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!MoveFileW`.
pub fn handle_move_file_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!MoveFileA`.
pub fn handle_move_file_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetTempPathW`.
pub fn handle_get_temp_path_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_temp_path_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_temp_file_name_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
        state.window_state.tick_count = state.window_state.tick_count.wrapping_add(1);
        state.window_state.tick_count
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
pub fn handle_get_temp_file_name_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
        state.window_state.tick_count = state.window_state.tick_count.wrapping_add(1);
        state.window_state.tick_count
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
pub fn handle_get_drive_type_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_drive_type_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_get_logical_drives(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let return_value = u64::from(crate::vfs::logical_drives_mask(&state.file_io.volumes));
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetSystemDirectoryW`.
pub fn handle_get_system_directory_w(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    write_fixed_dir_w(engine, crate::vfs::GUEST_SYSTEM_DIR)
}
/// Handles `KERNEL32.dll!GetSystemDirectoryA`.
pub fn handle_get_system_directory_a(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    write_fixed_dir_a(engine, crate::vfs::GUEST_SYSTEM_DIR)
}
/// Handles `KERNEL32.dll!GetWindowsDirectoryW`.
pub fn handle_get_windows_directory_w(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    write_fixed_dir_w(engine, crate::vfs::GUEST_WINDOWS_DIR)
}
/// Handles `KERNEL32.dll!GetWindowsDirectoryA`.
pub fn handle_get_windows_directory_a(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    write_fixed_dir_a(engine, crate::vfs::GUEST_WINDOWS_DIR)
}
/// Handles `KERNEL32.dll!GetFileSizeEx`.
pub fn handle_get_file_size_ex(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_set_file_pointer_ex(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_set_end_of_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
pub fn handle_flush_file_buffers(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
