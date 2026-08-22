use super::{
    Context, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_READ_FAULT, FAKE_STDERR_HANDLE,
    FAKE_STDIN_HANDLE, FAKE_STDOUT_HANDLE, HandlerContext, OpenGuestFile, Result,
    WinApiHandlerResult, find_open_file, find_open_file_mut, guest_basename, is_main_module_path,
    is_open_file_handle, maybe_promote_open_file_to_streaming, paths_match_guest,
    persist_open_file_to_host, read_stack_u64, refill_stdin_from_host, write_guest_u32,
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
    // Single shared implementation — see `ucrt::write_all_fd` for why this is
    // a raw fd write rather than `std::io`.
    crate::ucrt::write_all_fd(fd, bytes);
}
pub fn handle_read_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let buffer_va = engine.read_rdx()?;
    let bytes_to_read = engine.read_r8()?;
    let bytes_read_va = engine.read_r9()?;

    // Microsoft Learn: sets *lpNumberOfBytesRead to zero before any work/error check.
    if bytes_read_va != 0 {
        write_guest_u32(engine, bytes_read_va, 0)?;
    }

    if buffer_va == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
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
                    return ctx.finish(1);
                }
                Err(()) => {
                    state.process.last_error = ERROR_READ_FAULT;
                    return ctx.finish(0);
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
                .mem_write(buffer_va, data)
                .context("failed to write ReadFile stdin bytes")?;
            state.file_io.stdin_cursor = end;
            if bytes_read_va != 0 {
                let read_len_u32 =
                    u32::try_from(read_len).context("ReadFile byte count does not fit u32")?;
                write_guest_u32(engine, bytes_read_va, read_len_u32)?;
            }
        }
        // available == 0 && InjectOnly → inject exhausted → EOF (0 bytes, success).
        state.process.last_error = 0;
        return ctx.finish(1);
    }

    // Console stdout/stderr are not readable.
    if is_console_output_handle(handle) {
        state.process.last_error = ERROR_INVALID_HANDLE;
        return ctx.finish(0);
    }

    // Single classification lookup: existence + streaming flag (replaces the
    // separate `is_open_file_handle` probe).
    let streaming = match find_open_file(state, handle) {
        Some(f) => f.streaming,
        None => {
            state.process.last_error = ERROR_INVALID_HANDLE;
            return ctx.finish(0);
        }
    };

    // Best-effort teardown: failure to sync is not fatal.
    let _ = crate::guest_io_host::sync_host_cursor_from_guest(engine, state, handle).ok();
    let requested =
        usize::try_from(bytes_to_read).context("ReadFile byte count does not fit usize")?;

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
            return ctx.finish(0);
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
            .mem_write(buffer_va, &data)
            .context("failed to write ReadFile stream bytes")?;
        if let Some(open_file) = find_open_file_mut(state, handle) {
            open_file.cursor = cursor_before.saturating_add(u64::try_from(n).unwrap_or(0));
        }
        if bytes_read_va != 0 {
            write_guest_u32(engine, bytes_read_va, u32::try_from(n).unwrap_or(0))?;
        }
        if is_main_module_path(state, &path) {
            state.file_io.executable_file_cursor =
                cursor_before.saturating_add(u64::try_from(n).unwrap_or(0));
        }
        state.process.last_error = 0;
    } else {
        // Buffered path: exe-flag strings are cloned before the single
        // mutable borrow so no extra hash lookups are needed inside.
        let is_exe_of = {
            let p = &state.process;
            let main_path = p.main_module_path.clone();
            let main_name = p.main_module_file_name.clone();
            move |pg: &str| {
                paths_match_guest(pg, &main_path)
                    || guest_basename(pg).eq_ignore_ascii_case(&main_name)
            }
        };
        // Phase 1: advance cursor and capture slice bounds without cloning the path/body.
        let (start, end, cursor_after, is_exe, path_disp) = {
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
            let is_exe = is_exe_of(&path_for_exe);
            (cursor_usize, end, cursor_after, is_exe, path_for_exe)
        };

        // Phase 2: immutable borrow for zero-copy mem_write of the file slice.
        {
            let open_file =
                find_open_file(state, handle).context("ReadFile write: open file vanished")?;
            // [VER] temporary: which doomretro.wad offsets does the guest read?
            if open_file.path.contains("doomretro.wad") {
                eprintln!(
                    "[VER] READ doomretro.wad off={start:#x} len={} buffer_va={buffer_va:#x}",
                    end - start
                );
                // [VER] temporary: dump the DEHACKED lump read as delivered to guest
                if start == 0xc && end - start >= 48 {
                    let head: String = open_file.bytes[start..start + 40]
                        .iter()
                        .map(|&b| {
                            if (0x20..0x7f).contains(&b) {
                                b as char
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    eprintln!("[VER] DEHACKED read head: {head:?}");
                }
            }
            let data = open_file
                .bytes
                .get(start..end)
                .context("ReadFile slice out of range")?;
            engine
                .mem_write(buffer_va, data)
                .context("failed to write ReadFile bytes")?;

            let read_len_u32 =
                u32::try_from(data.len()).context("ReadFile byte count does not fit u32")?;
            if bytes_read_va != 0 {
                write_guest_u32(engine, bytes_read_va, read_len_u32)?;
            }
        }

        if is_exe {
            state.file_io.executable_file_cursor = cursor_after;
        }

        state.process.last_error = 0;
        // Best-effort teardown: failure to sync is not fatal.
        let _ = crate::guest_io_host::sync_slot_from_host(engine, state, handle).ok();
        // The read side of the open/read chain: one line per real-file read
        // (the console paths return earlier). Failures return before this
        // point, so reaching here means success.
        tracing::info!(handle, path = %path_disp, ret = 1_u64, "ReadFile");
    }

    // The read side of the open/read chain: one line per real-file read (the
    // console paths return earlier) — see the buffered branch's info! line.
    // Failures return earlier, so reaching this line means success.
    let return_value = 1_u64;
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!WriteFile`.
pub fn handle_write_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for WriteFile")?;

    let buffer_va = engine
        .read_rdx()
        .context("failed to read RDX for WriteFile")?;

    let bytes_to_write = engine
        .read_r8()
        .context("failed to read R8 for WriteFile")?;

    let bytes_written_va = engine
        .read_r9()
        .context("failed to read R9 for WriteFile")?;

    // Mirror ReadFile: zero the optional out-count before validation.
    if bytes_written_va != 0 {
        write_guest_u32(engine, bytes_written_va, 0)?;
    }

    if buffer_va == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }

    // Console stdout/stderr → host console.
    if is_console_output_handle(handle) {
        let write_len =
            usize::try_from(bytes_to_write).context("WriteFile byte count does not fit usize")?;
        let mut data = vec![0_u8; write_len];
        if write_len > 0 {
            engine
                .mem_read(buffer_va, &mut data)
                .context("failed to read WriteFile console buffer")?;
        }
        write_host_console_handle(handle, &data);
        if bytes_written_va != 0 {
            let write_len_u32 =
                u32::try_from(write_len).context("WriteFile byte count does not fit u32")?;
            write_guest_u32(engine, bytes_written_va, write_len_u32)?;
        }
        state.process.last_error = 0;
        return ctx.finish(1);
    }

    // Console stdin is not writable.
    if handle == FAKE_STDIN_HANDLE {
        state.process.last_error = ERROR_INVALID_HANDLE;
        return ctx.finish(0);
    }

    let success = is_open_file_handle(state, handle);

    if success {
        let write_len =
            usize::try_from(bytes_to_write).context("WriteFile byte count does not fit usize")?;

        let mut data = vec![0_u8; write_len];
        if write_len > 0 {
            engine
                .mem_read(buffer_va, &mut data)
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

            // Buffered bytes mutated: the guest-I/O arena mirror is now stale.
            // `sync_slot_from_host` re-mirrors (and clears) this before any guest read.
            open_file.guest_dirty = true;

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

        // Push the updated buffered bytes + cursor/size into the guest-I/O arena
        // so a following guest fast-path read (WIE_GUEST_IO=all) sees them. Gated
        // by `guest_dirty`: this re-mirrors only on a real write, never on reads.
        if !streaming {
            let _ = crate::guest_io_host::sync_slot_from_host(engine, state, handle).ok();
        }

        if is_main_module_path(state, &path) {
            state.file_io.executable_file_cursor = cursor_after;
        }

        let write_len_u32 =
            u32::try_from(write_len).context("WriteFile byte count does not fit u32")?;

        if bytes_written_va != 0 {
            write_guest_u32(engine, bytes_written_va, write_len_u32)?;
        }

        tracing::debug!(
            handle,
            buffer = buffer_va,
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
            buffer = buffer_va,
            requested = bytes_to_write,
            "WriteFile invalid handle"
        );
        state.process.last_error = ERROR_INVALID_HANDLE;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
pub fn handle_backup_read(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let to_read = engine.read_r8()?;
    let bytes_read_va = engine.read_r9()?;
    let _context = read_stack_u64(engine, 0x28).unwrap_or(0);
    let _secured = read_stack_u64(engine, 0x30).unwrap_or(0);
    if bytes_read_va != 0 {
        write_guest_u32(engine, bytes_read_va, 0)?;
    }
    if buf == 0 || to_read == 0 {
        state.process.last_error = 0;
        return ctx.finish(1);
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
        if bytes_read_va != 0 {
            write_guest_u32(engine, bytes_read_va, u32::try_from(read_len).unwrap_or(0))?;
        }
        state.process.last_error = 0;
        ctx.finish(1)
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        ctx.finish(0)
    }
}
/// Handles `KERNEL32.dll!BackupSeek` — seek within open file bytes.
pub fn handle_backup_seek(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let lo = engine.read_rdx()?;
    let hi = engine.read_r8()?;
    let lo_va = engine.read_r9()?;
    let _hi_va = read_stack_u64(engine, 0x28).unwrap_or(0);
    let _context = read_stack_u64(engine, 0x30).unwrap_or(0);
    if let Some(file) = state.file_io.open_files.get_mut(&handle) {
        let offset = lo | (hi << 32);
        file.cursor = offset;
        if lo_va != 0 {
            write_guest_u32(
                engine,
                lo_va,
                u32::try_from(offset & 0xFFFF_FFFF).unwrap_or(0),
            )?;
        }
        state.process.last_error = 0;
        ctx.finish(1)
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        ctx.finish(0)
    }
}
/// Handles `KERNEL32.dll!BackupWrite` — write to open file bytes.
pub fn handle_backup_write(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let to_write = engine.read_r8()?;
    let written_va = engine.read_r9()?;
    let _context = read_stack_u64(engine, 0x28).unwrap_or(0);
    let _secured = read_stack_u64(engine, 0x30).unwrap_or(0);
    if written_va != 0 {
        write_guest_u32(engine, written_va, 0)?;
    }
    if buf == 0 || to_write == 0 {
        state.process.last_error = 0;
        return ctx.finish(1);
    }
    let to_write_usize = usize::try_from(to_write).unwrap_or(0);
    let ok = match state.file_io.open_files.get_mut(&handle) {
        Some(file) => {
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
            // Buffered bytes mutated: mark the guest-I/O arena mirror stale.
            file.guest_dirty = true;
            file.cursor = file.cursor.saturating_add(to_write);
            if written_va != 0 {
                write_guest_u32(engine, written_va, u32::try_from(to_write).unwrap_or(0))?;
            }
            true
        }
        None => false,
    };
    if ok {
        // Push the new bytes/cursor into the guest-I/O arena before any guest read.
        let _ = crate::guest_io_host::sync_slot_from_host(engine, state, handle).ok();
    }
    state.process.last_error = if ok { 0 } else { ERROR_INVALID_HANDLE };
    ctx.finish(u64::from(ok))
}
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
    ctx.finish(return_value)
}
