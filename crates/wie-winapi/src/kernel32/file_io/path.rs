use super::{
    Context, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DRIVE, ERROR_INVALID_HANDLE,
    ERROR_INVALID_PARAMETER, ERROR_PATH_NOT_FOUND, HandlerContext, Result, WinApiHandlerResult,
    WinApiState, checked_address, get_user_profile_dir_impl, read_ansi_string_from_cpu,
    read_guest_u16, read_guest_utf16_lossy, read_wide_string_from_cpu, write_guest_u64,
    write_guest_utf16_units,
};

/// Handles `KERNEL32.dll!GetCurrentDirectoryW`.
///
/// Returns the stored guest cwd — always a confined volume path (`C:\…` in
/// the bottle, `D:\…` in the optional bridge). Buffer semantics match real
/// Windows: on success the return value is the length written excluding the
/// NUL; a buffer too small for path+NUL returns the required length including
/// the NUL and sets `ERROR_INSUFFICIENT_BUFFER`.
pub fn handle_get_current_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
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
    let return_value = if buffer_ptr == 0 || buffer_length <= character_count {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
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

        state.process.last_error = 0;
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
///
/// Stores only a guest directory that (a) confines to a configured volume
/// (C: bottle / D: bridge) and (b) exists as a host directory in that volume.
/// Relative names resolve against the current cwd first (MSDN). Failure codes
/// per real Windows: missing directory → `ERROR_PATH_NOT_FOUND`; unmapped
/// drive → `ERROR_INVALID_DRIVE`.
pub fn handle_set_current_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
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
            set_current_directory_impl(state, &directory)
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
/// Shared implementation for the GetLongPathName/GetShortPathName A/W quartet.
///
/// Real Windows resolves 8.3 short names to long names; WIE stores neither
/// form, so every variant returns the input unchanged. The `wide` flag picks
/// the W-string read/write pair; the A-pair goes through the ACP path.
fn handle_mock_long_short_path_impl(
    ctx: &mut HandlerContext<'_>,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let src = engine.read_rcx()?;
    let dst = engine.read_rdx()?;
    let dst_len = engine.read_r8()?;
    let path = if wide {
        read_wide_string_from_cpu(engine, src, 1024)?
    } else {
        read_ansi_string_from_cpu(engine, src, 1024)?
    };
    let written = if wide {
        write_mock_string_w(engine, state, &path, dst, dst_len)?
    } else {
        write_mock_string_a(engine, state, &path, dst, dst_len)?
    };
    let return_address = engine.return_from_win64_api(written)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: written,
    })
}
/// Handles `KERNEL32.dll!GetLongPathNameW` — return same as input.
pub fn handle_get_long_path_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_mock_long_short_path_impl(ctx, true)
}
/// Handles `KERNEL32.dll!GetLongPathNameA` — return same as input.
pub fn handle_get_long_path_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_mock_long_short_path_impl(ctx, false)
}
/// Handles `KERNEL32.dll!GetShortPathNameW` — return same as input.
pub fn handle_get_short_path_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_mock_long_short_path_impl(ctx, true)
}
/// Handles `KERNEL32.dll!GetShortPathNameA` — return same as input.
pub fn handle_get_short_path_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_mock_long_short_path_impl(ctx, false)
}
/// Handles `KERNEL32.dll!GetUserProfileDirectoryW` — return profile path from bottle/env.
pub fn handle_get_user_profile_directory_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _h_profile = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let size_ptr = engine.read_r8()?;
    get_user_profile_dir_impl(engine, state, buf, size_ptr, true)
}
/// Handles `KERNEL32.dll!GetUserProfileDirectoryA` — return profile path from bottle/env.
pub fn handle_get_user_profile_directory_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _h_profile = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let size_ptr = engine.read_r8()?;
    get_user_profile_dir_impl(engine, state, buf, size_ptr, false)
}
pub(crate) fn handle_duplicate_handle(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    // RCX = hSourceProcessHandle
    // RDX = hSourceHandle
    // R8  = hTargetProcessHandle
    // R9  = lpTargetHandle (guest VA for the duplicated handle)
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
        let obj = state
            .kernel
            .sync
            .objects
            .get(&crate::KernelHandle::from(source_handle))
            .cloned();
        if let Some(obj) = obj {
            let new_handle = state.kernel.sync.next_handle;
            state.kernel.sync.next_handle =
                crate::KernelHandle::from(state.kernel.sync.next_handle.as_u64().wrapping_add(4));
            let new_handle_u64 = new_handle.as_u64();
            state.kernel.sync.objects.insert(new_handle, obj.clone());
            if close_source {
                state
                    .kernel
                    .sync
                    .objects
                    .remove(&crate::KernelHandle::from(source_handle));
            }
            engine.mem_write(target_handle_ptr, &new_handle_u64.to_le_bytes())?;
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
    state.kernel.sync.next_handle =
        crate::KernelHandle::from(state.kernel.sync.next_handle.as_u64().wrapping_add(4));
    let new_handle_u64 = new_handle.as_u64();
    state.kernel.sync.objects.insert(new_handle, source_obj);

    // Honour DUPLICATE_CLOSE_SOURCE: close the source handle after duplication.
    if close_source && source_handle != u64::MAX && source_handle != u64::MAX - 1 {
        state
            .kernel
            .sync
            .objects
            .remove(&crate::KernelHandle::from(source_handle));
    }

    engine.mem_write(target_handle_ptr, &new_handle_u64.to_le_bytes())?;
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?; // TRUE
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!GetFullPathNameW`.
pub fn handle_get_full_path_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
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
pub fn handle_get_full_path_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
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
///
/// Same semantics as [`handle_get_current_directory_w`], with the path
/// ACP-encoded for the ANSI guest buffer.
pub fn handle_get_current_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let buffer_length = engine.read_rcx()?;
    let buffer_ptr = engine.read_rdx()?;
    let directory = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let bytes = crate::vfs::encode_acp(&directory);
    let character_count =
        u64::try_from(bytes.len()).context("current directory byte length does not fit u64")?;
    let required_with_nul = character_count
        .checked_add(1)
        .context("GetCurrentDirectoryA required size overflow")?;
    let return_value = if buffer_ptr == 0 || buffer_length <= character_count {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        required_with_nul
    } else {
        let mut out = bytes;
        out.push(0);
        engine.mem_write(buffer_ptr, &out)?;
        state.process.last_error = 0;
        character_count
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!SetCurrentDirectoryA`.
///
/// Same semantics as [`handle_set_current_directory_w`] (ANSI input).
pub fn handle_set_current_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
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
            set_current_directory_impl(state, &directory)
        }
    };
    let return_value = u64::from(success);
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Validate and store the guest current directory (shared by the W/A setters).
///
/// The guest cwd is ALWAYS a confined volume path: `C:\…` inside the bottle
/// (`{root}/drive_c/…`) or `D:\…` inside the optional host bridge. Rules, in
/// order:
/// 1. The resolved path must confine to a configured volume via
///    [`crate::vfs::confine_guest_path`]. A drive that is not a configured
///    volume (e.g. `E:\…`, a UNC share, a bare host path) sets
///    `ERROR_INVALID_DRIVE`; a mapped drive that still fails confinement sets
///    `ERROR_PATH_NOT_FOUND`.
/// 2. The confined path must map to an existing host *directory* via
///    [`crate::vfs::guest_path_to_host`]; otherwise `ERROR_PATH_NOT_FOUND`.
///
/// On success the stored cwd is the canonical confined guest path and `true`
/// is returned with `last_error` cleared. On failure `last_error` is set and
/// the stored cwd is left untouched (it never holds an unconfined path).
fn set_current_directory_impl(state: &mut WinApiState, directory: &str) -> bool {
    // Relative directory names resolve against the current directory (MSDN).
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, directory);
    let Some(confined) = crate::vfs::confine_guest_path(&state.file_io.volumes, &full) else {
        // Unmapped drive → ERROR_INVALID_DRIVE; a mapped drive that fails
        // confinement (escape probe / UNC) keeps the path-not-found code.
        let mapped = crate::vfs::path::drive_letter(&full)
            .is_some_and(|drive| drive_is_configured(&state.file_io.volumes, drive));
        state.process.last_error = if mapped {
            ERROR_PATH_NOT_FOUND
        } else {
            ERROR_INVALID_DRIVE
        };
        return false;
    };
    // The confined guest path must name a real host directory in the volume.
    let exists = crate::vfs::guest_path_to_host(&state.file_io.volumes, &confined)
        .is_some_and(|map| map.host.is_dir());
    if exists {
        state.file_io.current_directory_wide = confined.encode_utf16().collect();
        state.process.last_error = 0;
        true
    } else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        false
    }
}

/// Whether `drive` names a configured guest volume (C: bottle / D: bridge).
fn drive_is_configured(volumes: &crate::vfs::VolumeConfig, drive: char) -> bool {
    match drive {
        'C' => volumes.bottle_root.is_some(),
        'D' => volumes.drive_d_root.is_some(),
        _ => false,
    }
}
// Copy a NUL-terminated ANSI path into a guest buffer.
///
/// Returns `(chars_written_or_nSize, truncated)` per Microsoft Learn
/// `GetModuleFileNameA` semantics.
pub(crate) fn copy_path_a_to_guest_buffer(
    engine: &mut dyn wie_cpu::CpuEngine,
    source_ptr: u64,
    dest_ptr: u64,
    dest_len: u64,
) -> Result<(u64, bool)> {
    if dest_ptr == 0 || dest_len == 0 {
        return Ok((0, false));
    }

    let dest_len_usize =
        usize::try_from(dest_len).context("guest buffer length does not fit usize")?;

    // Read full source path (bounded) including room to detect truncation.
    let mut source_bytes = Vec::new();
    let max_scan = dest_len_usize.saturating_add(1).max(1);
    for index in 0..max_scan {
        let index_u64 = u64::try_from(index).context("guest string index does not fit u64")?;
        let source_address = checked_address(source_ptr, index_u64, "guest source string");
        let mut byte = [0_u8; 1];
        engine
            .mem_read(source_address, &mut byte)
            .context("failed to read guest source string byte")?;
        if byte[0] == 0 {
            break;
        }
        source_bytes.push(byte[0]);
    }

    let path_len = source_bytes.len();
    // Need room for path + NUL. If dest_len is too small, truncate and NUL-terminate.
    let truncated = path_len >= dest_len_usize;
    if truncated {
        let keep = dest_len_usize.saturating_sub(1);
        let mut out = source_bytes.get(..keep).unwrap_or(&[]).to_vec();
        out.push(0);
        engine
            .mem_write(dest_ptr, &out)
            .context("failed to write truncated guest path")?;
        Ok((dest_len, true))
    } else {
        let mut out = source_bytes;
        out.push(0);
        engine
            .mem_write(dest_ptr, &out)
            .context("failed to write guest path")?;
        let written = u64::try_from(path_len).context("path length does not fit u64")?;
        Ok((written, false))
    }
}

// Copy a NUL-terminated UTF-16 path into a guest buffer (WCHAR units).
pub(crate) fn copy_path_w_to_guest_buffer(
    engine: &mut dyn wie_cpu::CpuEngine,
    source_ptr: u64,
    dest_ptr: u64,
    dest_len: u64,
) -> Result<(u64, bool)> {
    if dest_ptr == 0 || dest_len == 0 {
        return Ok((0, false));
    }

    let dest_len_usize =
        usize::try_from(dest_len).context("wide guest buffer length does not fit usize")?;

    let mut units = Vec::new();
    let max_scan = dest_len_usize.saturating_add(1).max(1);
    for index in 0..max_scan {
        let index_u64 = u64::try_from(index).context("wide guest string index does not fit u64")?;
        let source_offset = index_u64
            .checked_mul(2)
            .context("wide guest string source offset overflow")?;
        let source_address = checked_address(source_ptr, source_offset, "wide guest source string");
        let unit = read_guest_u16(engine, source_address)?;
        if unit == 0 {
            break;
        }
        units.push(unit);
    }

    let path_len = units.len();
    let truncated = path_len >= dest_len_usize;
    if truncated {
        let keep = dest_len_usize.saturating_sub(1);
        let mut out_units = units.get(..keep).unwrap_or(&[]).to_vec();
        out_units.push(0);
        let byte_cap = out_units.len().saturating_mul(2);
        let mut bytes = Vec::with_capacity(byte_cap);
        for unit in out_units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        engine
            .mem_write(dest_ptr, &bytes)
            .context("failed to write truncated wide guest path")?;
        Ok((dest_len, true))
    } else {
        let mut out_units = units;
        out_units.push(0);
        let byte_cap = out_units.len().saturating_mul(2);
        let mut bytes = Vec::with_capacity(byte_cap);
        for unit in out_units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        engine
            .mem_write(dest_ptr, &bytes)
            .context("failed to write wide guest path")?;
        let written = u64::try_from(path_len).context("wide path length does not fit u64")?;
        Ok((written, false))
    }
}

/// Write a NUL-terminated ANSI string into a guest buffer at `buf` with room
/// for `buf_len` bytes.  Returns the number of characters written (excluding
/// NUL), or 0 with `ERROR_INSUFFICIENT_BUFFER` on truncation.
pub(crate) fn write_mock_string_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    s: &str,
    buf: u64,
    buf_len: u64,
) -> Result<u64> {
    if buf == 0 || buf_len == 0 {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        return Ok(0);
    }
    let encoded = crate::vfs::encode_acp(s);
    let needed = encoded.len(); // bytes (excluding NUL)
    let cap = usize::try_from(buf_len).unwrap_or(0);
    if cap < needed.saturating_add(1) {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        return Ok(0);
    }
    let mut payload = encoded;
    payload.push(0);
    engine.mem_write(buf, &payload)?;
    state.process.last_error = 0;
    Ok(u64::try_from(needed).unwrap_or(0))
}

/// Write a NUL-terminated UTF-16 string into a guest buffer at `buf` with room
/// for `buf_len` WCHARs.  Returns the number of characters written (excluding
/// NUL), or 0 with `ERROR_INSUFFICIENT_BUFFER` on truncation.
pub(crate) fn write_mock_string_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    s: &str,
    buf: u64,
    buf_len: u64,
) -> Result<u64> {
    if buf == 0 || buf_len == 0 {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        return Ok(0);
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let needed = units.len();
    let cap = usize::try_from(buf_len).unwrap_or(0);
    if cap < needed.saturating_add(1) {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        return Ok(0);
    }
    let mut bytes = Vec::with_capacity(needed.saturating_add(1).saturating_mul(2));
    for u in &units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(buf, &bytes)?;
    state.process.last_error = 0;
    Ok(u64::try_from(needed).unwrap_or(0))
}

// Resolve a Windows path against the process current directory.
pub(crate) fn resolve_full_windows_path(current_directory: &str, input_path: &str) -> String {
    crate::vfs::resolve_full_windows_path(current_directory, input_path)
}

#[cfg(test)]
pub(crate) fn normalize_windows_path_components(path: &str) -> String {
    crate::vfs::normalize_windows_path_components(path)
}

#[cfg(test)]
mod path_resolve_tests {
    use super::{normalize_windows_path_components, resolve_full_windows_path};

    #[test]
    fn relative_dot_slash_against_cwd() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r".\config.ini"),
            r"C:\App\config.ini"
        );
    }

    #[test]
    fn relative_bare_name() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r"config.ini"),
            r"C:\App\config.ini"
        );
    }

    #[test]
    fn relative_dotdot() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App\data", r"..\config.ini"),
            r"C:\App\config.ini"
        );
    }

    #[test]
    fn rooted_on_current_drive() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r"\Windows\win.ini"),
            r"C:\Windows\win.ini"
        );
    }

    #[test]
    fn absolute_unchanged_after_normalize() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r"D:\other\file.txt"),
            r"D:\other\file.txt"
        );
    }

    #[test]
    fn collapses_dot_components() {
        assert_eq!(
            normalize_windows_path_components(r"C:\App\.\sub\..\x.txt"),
            r"C:\App\x.txt"
        );
    }
}

/// Guest cwd semantics inside the bottle: `SetCurrentDirectory` only stores a
/// confined (C: bottle / D: bridge), existing guest directory, and
/// `GetCurrentDirectory` reports it back with real-Windows buffer semantics.
#[cfg(test)]
mod cwd_tests {
    use super::*;
    use crate::guest_heap::GuestHeap;
    use crate::state::{
        DllStateMap, FileIoState, HeapState, KernelState, ModuleState, ProcessState,
        WinApiEnvironment,
    };
    use crate::sync_obj::SyncState;
    use crate::vfs::VolumeConfig;
    use crate::{HandlerContext, ThreadState, WinApiState};
    use ahash::HashMapExt;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use wie_cpu::{CpuEngine, IcedCpu};

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    const STACK_TOP: u64 = 0x100_FF00;
    const STR_BUF: u64 = 0x6000; // guest string buffer (path in / result out)
    const OUT_BUF: u64 = 0x6400; // guest GetCurrentDirectory result buffer

    /// Minimal engine for handler unit tests (mirrors `state/tests.rs`).
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

    fn test_env() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0x0000_0000_1400_0000,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 1,
        }
    }

    /// Default state seeded with cwd `C:\` (mirrors the runtime seed).
    fn winapi_state_default() -> WinApiState {
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
                open_files: ahash::HashMap::new(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: r"C:\".encode_utf16().collect(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: crate::GuestStdinMode::InjectOnly,
                ucrt_files: ahash::HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: ahash::HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: crate::RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: ahash::HashMap::new(),
                environment: crate::DEFAULT_ENVIRONMENT
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
                seh_pending: ahash::HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(crate::present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: ahash::HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: ahash::HashMap::new(),
                next_module_handle: crate::ModuleHandle::from(
                    crate::dll_loader::REAL_MODULE_HANDLE_BASE,
                ),
            },
        }
    }

    /// Temp fixture: a bottle root with `drive_c` plus an optional D: bridge.
    struct Fixture {
        bottle_root: PathBuf,
        drive_d_root: Option<PathBuf>,
    }

    impl Fixture {
        fn bottle() -> Self {
            let root = unique_dir("bottle");
            std::fs::create_dir_all(root.join("drive_c")).expect("create bottle drive_c");
            Self {
                bottle_root: root,
                drive_d_root: None,
            }
        }

        fn bottle_and_drive_d() -> Self {
            let mut fixture = Self::bottle();
            let drive_d = unique_dir("drived");
            std::fs::create_dir_all(drive_d.join("archive")).expect("create bridge dir");
            fixture.drive_d_root = Some(drive_d);
            fixture
        }

        fn volumes(&self) -> VolumeConfig {
            VolumeConfig::from_parts(Some(self.bottle_root.clone()), self.drive_d_root.clone())
        }

        /// Create a host directory under the bottle's `drive_c`.
        fn mkdir_c(&self, rel: &str) {
            std::fs::create_dir_all(self.bottle_root.join("drive_c").join(rel))
                .expect("create bottle subdir");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.bottle_root));
            if let Some(drive_d) = &self.drive_d_root {
                drop(std::fs::remove_dir_all(drive_d));
            }
        }
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("wie-cwd-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    fn write_utf16(cpu: &mut IcedCpu, addr: u64, s: &str) {
        let mut bytes = Vec::new();
        for unit in s.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        cpu.mem_write(addr, &bytes).expect("write utf16 string");
    }

    fn write_ansi(cpu: &mut IcedCpu, addr: u64, s: &str) {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        cpu.mem_write(addr, &bytes).expect("write ansi string");
    }

    fn read_utf16(cpu: &mut IcedCpu, addr: u64) -> String {
        let mut units = Vec::new();
        for index in 0_u64..256 {
            let mut raw = [0_u8; 2];
            cpu.mem_read(addr + 2 * index, &mut raw)
                .expect("read utf16 unit");
            let unit = u16::from_le_bytes(raw);
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        String::from_utf16(&units).expect("valid utf16")
    }

    fn read_ansi(cpu: &mut IcedCpu, addr: u64) -> String {
        let mut bytes = Vec::new();
        for index in 0_u64..512 {
            let mut raw = [0_u8; 1];
            cpu.mem_read(addr + index, &mut raw)
                .expect("read ansi byte");
            if raw[0] == 0 {
                break;
            }
            bytes.push(raw[0]);
        }
        String::from_utf8(bytes).expect("valid ansi")
    }

    fn run_set_w(cpu: &mut IcedCpu, state: &mut WinApiState, path_ptr: u64) -> u64 {
        cpu.write_rcx(path_ptr).ok();
        cpu.write_rsp(STACK_TOP).ok();
        let r = handle_set_current_directory_w(&mut HandlerContext::new(cpu, test_env(), state))
            .expect("SetCurrentDirectoryW handler");
        r.return_value
    }

    fn run_get_w(cpu: &mut IcedCpu, state: &mut WinApiState, buf_len: u64, buf_ptr: u64) -> u64 {
        cpu.write_rcx(buf_len).ok();
        cpu.write_rdx(buf_ptr).ok();
        cpu.write_rsp(STACK_TOP).ok();
        let r = handle_get_current_directory_w(&mut HandlerContext::new(cpu, test_env(), state))
            .expect("GetCurrentDirectoryW handler");
        r.return_value
    }

    fn stored_cwd(state: &WinApiState) -> String {
        String::from_utf16_lossy(&state.file_io.current_directory_wide)
    }

    #[test]
    fn set_get_round_trip_in_bottle_dir() {
        let fixture = Fixture::bottle();
        fixture.mkdir_c("App/Work");
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();

        write_utf16(&mut cpu, STR_BUF, r"C:\App\Work");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 1);
        assert_eq!(state.process.last_error, 0);
        assert_eq!(stored_cwd(&state), r"C:\App\Work");

        let got = run_get_w(&mut cpu, &mut state, 64, OUT_BUF);
        assert_eq!(got, 11); // len of "C:\App\Work" without NUL
        assert_eq!(read_utf16(&mut cpu, OUT_BUF), r"C:\App\Work");
        assert_eq!(state.process.last_error, 0);
    }

    #[test]
    fn set_to_bottle_root() {
        let fixture = Fixture::bottle();
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();

        write_utf16(&mut cpu, STR_BUF, r"C:\");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 1);
        assert_eq!(stored_cwd(&state), r"C:\");
        assert_eq!(run_get_w(&mut cpu, &mut state, 64, OUT_BUF), 3);
        assert_eq!(read_utf16(&mut cpu, OUT_BUF), r"C:\");
    }

    #[test]
    fn missing_in_bottle_dir_is_path_not_found() {
        let fixture = Fixture::bottle();
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();

        write_utf16(&mut cpu, STR_BUF, r"C:\NoSuchDir");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 0);
        assert_eq!(state.process.last_error, 3); // ERROR_PATH_NOT_FOUND
        // Failed set leaves the stored cwd untouched.
        assert_eq!(stored_cwd(&state), r"C:\");
    }

    #[test]
    fn unmapped_drive_is_invalid_drive() {
        let fixture = Fixture::bottle();
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();

        // E: is not a configured volume at all.
        write_utf16(&mut cpu, STR_BUF, r"E:\anything");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 0);
        assert_eq!(state.process.last_error, 15); // ERROR_INVALID_DRIVE
        // D: exists only when the bridge is configured.
        write_utf16(&mut cpu, STR_BUF, r"D:\x");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 0);
        assert_eq!(state.process.last_error, 15);
        assert_eq!(stored_cwd(&state), r"C:\");
    }

    #[test]
    fn drive_d_bridge_round_trip() {
        let fixture = Fixture::bottle_and_drive_d();
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();

        write_utf16(&mut cpu, STR_BUF, r"D:\archive");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 1);
        assert_eq!(stored_cwd(&state), r"D:\archive");

        // A missing dir under a bridged drive is still path-not-found.
        write_utf16(&mut cpu, STR_BUF, r"D:\nope");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 0);
        assert_eq!(state.process.last_error, 3);
        assert_eq!(stored_cwd(&state), r"D:\archive");
    }

    #[test]
    fn cwd_never_holds_unconfined_path() {
        let fixture = Fixture::bottle();
        fixture.mkdir_c("App");
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();

        // Establish a valid cwd first.
        write_utf16(&mut cpu, STR_BUF, r"C:\App");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 1);

        // Every failing set must leave the stored cwd unchanged.
        let failing_paths = [
            r"E:\x",             // unmapped drive
            r"C:\Missing",       // missing in-bottle dir
            r"D:\x",             // D: bridge not configured
            r"\\server\share\x", // UNC share (unmapped)
            r"/Users/me/x",      // bare host path → resolves to a missing C: path
        ];
        for path in failing_paths {
            write_utf16(&mut cpu, STR_BUF, path);
            assert_eq!(
                run_set_w(&mut cpu, &mut state, STR_BUF),
                0,
                "set {path} must fail"
            );
            assert_eq!(stored_cwd(&state), r"C:\App");
        }

        // The stored cwd always re-confines to itself (canonical guest form).
        let stored = stored_cwd(&state);
        assert_eq!(
            crate::vfs::confine_guest_path(&state.file_io.volumes, &stored),
            Some(stored)
        );
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        let fixture = Fixture::bottle();
        fixture.mkdir_c("App/Sub");
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();
        write_utf16(&mut cpu, STR_BUF, r"C:\App");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 1);

        write_utf16(&mut cpu, STR_BUF, r".\Sub");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 1);
        assert_eq!(stored_cwd(&state), r"C:\App\Sub");

        write_utf16(&mut cpu, STR_BUF, r"..");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 1);
        assert_eq!(stored_cwd(&state), r"C:\App");
    }

    #[test]
    fn get_w_insufficient_buffer_reports_required_size() {
        let fixture = Fixture::bottle();
        fixture.mkdir_c("App");
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();
        write_utf16(&mut cpu, STR_BUF, r"C:\App");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 1);

        // Buffer of exactly the path length leaves no room for the NUL.
        assert_eq!(run_get_w(&mut cpu, &mut state, 6, OUT_BUF), 7);
        assert_eq!(state.process.last_error, 122); // ERROR_INSUFFICIENT_BUFFER

        // Zero-length / null buffer behaves the same.
        assert_eq!(run_get_w(&mut cpu, &mut state, 0, 0), 7);
        assert_eq!(state.process.last_error, 122);

        // An adequate buffer returns the path length (without NUL).
        assert_eq!(run_get_w(&mut cpu, &mut state, 64, OUT_BUF), 6);
        assert_eq!(read_utf16(&mut cpu, OUT_BUF), r"C:\App");
        assert_eq!(state.process.last_error, 0);
    }

    #[test]
    fn ansi_round_trip_and_insufficient_buffer() {
        let fixture = Fixture::bottle();
        fixture.mkdir_c("App");
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();

        write_ansi(&mut cpu, STR_BUF, r"C:\App");
        cpu.write_rcx(STR_BUF).ok();
        cpu.write_rsp(STACK_TOP).ok();
        let r = handle_set_current_directory_a(&mut HandlerContext::new(
            &mut cpu,
            test_env(),
            &mut state,
        ))
        .expect("SetCurrentDirectoryA handler");
        assert_eq!(r.return_value, 1);
        assert_eq!(state.process.last_error, 0);

        // Too-small ANSI buffer → required size + ERROR_INSUFFICIENT_BUFFER.
        cpu.write_rcx(3).ok();
        cpu.write_rdx(OUT_BUF).ok();
        cpu.write_rsp(STACK_TOP).ok();
        let r = handle_get_current_directory_a(&mut HandlerContext::new(
            &mut cpu,
            test_env(),
            &mut state,
        ))
        .expect("GetCurrentDirectoryA handler");
        assert_eq!(r.return_value, 7);
        assert_eq!(state.process.last_error, 122);

        // Adequate ANSI buffer round-trips the stored cwd.
        cpu.write_rcx(64).ok();
        cpu.write_rdx(OUT_BUF).ok();
        cpu.write_rsp(STACK_TOP).ok();
        let r = handle_get_current_directory_a(&mut HandlerContext::new(
            &mut cpu,
            test_env(),
            &mut state,
        ))
        .expect("GetCurrentDirectoryA handler");
        assert_eq!(r.return_value, 6);
        assert_eq!(read_ansi(&mut cpu, OUT_BUF), r"C:\App");
        assert_eq!(state.process.last_error, 0);
    }

    #[test]
    fn empty_or_null_path_fails_path_not_found() {
        let fixture = Fixture::bottle();
        let mut cpu = test_engine();
        let mut state = winapi_state_default();
        state.file_io.volumes = fixture.volumes();

        assert_eq!(run_set_w(&mut cpu, &mut state, 0), 0);
        assert_eq!(state.process.last_error, 3);
        write_utf16(&mut cpu, STR_BUF, "");
        assert_eq!(run_set_w(&mut cpu, &mut state, STR_BUF), 0);
        assert_eq!(state.process.last_error, 3);
        assert_eq!(stored_cwd(&state), r"C:\");
    }
}
