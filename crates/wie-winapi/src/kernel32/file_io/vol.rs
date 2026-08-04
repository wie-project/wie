use super::{
    Context, ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, FAKE_DISK_CLUSTERS, FAKE_DISK_GIB,
    FAKE_STDIN_HANDLE, FIXED_SYSTEM_FILETIME, HandlerContext, INVALID_FILE_ATTRIBUTES,
    LOGICAL_DRIVE_TCHARS, Result, WinApiHandlerResult, checked_address,
    finish_create_file_create_only, handle_move_file_w, is_open_file_handle, low_u32,
    read_ansi_string_from_cpu, read_guest_u64, read_wide_string_from_cpu,
    resolve_full_windows_path, ret_bool_true, ret_u64, stat_guest_path, temp_name_id_u32,
    write_fixed_dir_a, write_fixed_dir_w, write_guest_u32, write_guest_u64,
    write_guest_utf16_units, write_mock_string_a, write_mock_string_w,
};
use crate::guest_layout::FileAttributeData;
use crate::guest_memory::with_typed_write;

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
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
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
        with_typed_write::<FileAttributeData, _, _>(engine, info_ptr, |data| {
            data.dw_file_attributes = st.attributes;
            let ft_low = u32::try_from(FIXED_SYSTEM_FILETIME & 0xffff_ffff).unwrap_or(0);
            let ft_high = u32::try_from(FIXED_SYSTEM_FILETIME >> 32).unwrap_or(0);
            data.ft_creation_time_low = ft_low;
            data.ft_creation_time_high = ft_high;
            data.ft_last_access_time_low = ft_low;
            data.ft_last_access_time_high = ft_high;
            data.ft_last_write_time_low = ft_low;
            data.ft_last_write_time_high = ft_high;
            data.n_file_size_high = u32::try_from(st.size >> 32).unwrap_or(0);
            data.n_file_size_low = u32::try_from(st.size & 0xFFFF_FFFF).unwrap_or(0);
            Ok(())
        })
        .context("failed to write WIN32_FILE_ATTRIBUTE_DATA")?;
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
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
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
        with_typed_write::<FileAttributeData, _, _>(engine, info_ptr, |data| {
            data.dw_file_attributes = st.attributes;
            let ft_low = u32::try_from(FIXED_SYSTEM_FILETIME & 0xffff_ffff).unwrap_or(0);
            let ft_high = u32::try_from(FIXED_SYSTEM_FILETIME >> 32).unwrap_or(0);
            data.ft_creation_time_low = ft_low;
            data.ft_creation_time_high = ft_high;
            data.ft_last_access_time_low = ft_low;
            data.ft_last_access_time_high = ft_high;
            data.ft_last_write_time_low = ft_low;
            data.ft_last_write_time_high = ft_high;
            data.n_file_size_high = u32::try_from(st.size >> 32).unwrap_or(0);
            data.n_file_size_low = u32::try_from(st.size & 0xFFFF_FFFF).unwrap_or(0);
            Ok(())
        })
        .context("failed to write WIN32_FILE_ATTRIBUTE_DATA")?;
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub fn handle_get_temp_path_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
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
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
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
