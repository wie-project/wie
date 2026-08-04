use super::{
    Context, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, FILE_BEGIN, FILE_CURRENT, FILE_END,
    FIXED_SYSTEM_FILETIME, HandlerContext, INVALID_SET_FILE_POINTER, Result, WinApiHandlerResult,
    find_open_file, find_open_file_mut, is_main_module_path, is_open_file_handle, read_guest_u64,
    ret_bool_true, ret_u64, write_guest_u16, write_guest_u32, write_guest_u64,
};
use crate::guest_layout::SystemTime;
use crate::guest_memory::with_typed_write;

/// Handles `KERNEL32.dll!FileTimeToLocalFileTime`.
pub fn handle_file_time_to_local_file_time(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let input_file_time_ptr = engine
        .read_rcx()
        .context("failed to read RCX for FileTimeToSystemTime")?;

    let system_time_ptr = engine
        .read_rdx()
        .context("failed to read RDX for FileTimeToSystemTime")?;

    let success = input_file_time_ptr != 0 && system_time_ptr != 0;

    if success {
        // Deterministic fake converted time — one typed write covers all eight
        // WORD fields (the view starts zeroed, matching the old per-field
        // writes that explicitly set every field).
        with_typed_write::<SystemTime, _, _>(engine, system_time_ptr, |st| {
            st.w_year = 2026;
            st.w_month = 7;
            st.w_day_of_week = 4;
            st.w_day = 9;
            st.w_hour = 12;
            st.w_minute = 0;
            st.w_second = 0;
            st.w_milliseconds = 0;
            Ok(())
        })
        .context("failed to write SYSTEMTIME")?;

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
pub fn handle_get_file_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
pub fn handle_set_file_pointer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
            // Best-effort teardown: failure to sync is not fatal.
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
pub fn handle_get_file_size(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
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
pub fn handle_compare_file_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
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
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
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
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
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
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _date = engine.read_rcx()?;
    let _time = engine.read_rdx()?;
    let ft = engine.read_r8()?;
    if ft == 0 {
        return ret_u64(engine, 0, "DosDateTimeToFileTime");
    }
    write_guest_u64(engine, ft, FIXED_SYSTEM_FILETIME)?;
    ret_bool_true(engine, "DosDateTimeToFileTime")
}
