use super::{
    Context, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
    ERROR_PATH_NOT_FOUND, HandlerContext, Result, WinApiHandlerResult, WinApiState,
    checked_address, get_user_profile_dir_impl, guest_dir_exists, read_ansi_string_from_cpu,
    read_guest_u16, read_guest_utf16_lossy, read_wide_string_from_cpu, write_guest_u64,
    write_guest_utf16_units,
};

/// Handles `KERNEL32.dll!GetCurrentDirectoryW`.
pub fn handle_get_current_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
pub fn handle_set_current_directory_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
/// Handles `KERNEL32.dll!GetLongPathNameW` — return same as input.
pub fn handle_get_long_path_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
pub fn handle_get_long_path_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
pub fn handle_get_short_path_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
pub fn handle_get_short_path_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
pub fn handle_get_current_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
pub fn handle_set_current_directory_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
