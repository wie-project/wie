use super::{
    Context, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, FILE_BEGIN, FILE_CURRENT, FILE_END,
    FIXED_SYSTEM_FILETIME, HandlerContext, INVALID_SET_FILE_POINTER, Result, WinApiHandlerResult,
    find_open_file, find_open_file_mut, is_main_module_path, is_open_file_handle, read_u64,
    ret_bool_true, ret_u64, write_guest_u16, write_guest_u32, write_guest_u64,
};
use crate::guest_layout::SystemTime;
use crate::guest_memory::with_typed_write;

/// Handles `KERNEL32.dll!FileTimeToLocalFileTime`.
pub fn handle_file_time_to_local_file_time(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let input_file_time_va = engine
        .read_rcx()
        .context("failed to read RCX for FileTimeToLocalFileTime")?;

    let output_file_time_va = engine
        .read_rdx()
        .context("failed to read RDX for FileTimeToLocalFileTime")?;

    let success = input_file_time_va != 0 && output_file_time_va != 0;

    if success {
        let mut bytes = [0_u8; 8];

        engine
            .mem_read(input_file_time_va, &mut bytes)
            .context("failed to read input FILETIME")?;

        engine
            .mem_write(output_file_time_va, &bytes)
            .context("failed to write output FILETIME")?;

        ctx.last_error().set_process_last_error(0);
    } else {
        ctx.last_error()
            .set_process_last_error(ERROR_INVALID_PARAMETER);
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// Convert `filetime` and write the resulting `SYSTEMTIME` to guest memory.
///
/// `false` (and no write) when the instant is one Windows' own
/// `FileTimeToSystemTime` refuses: past 30827-08-31 the civil year no longer
/// fits a `SYSTEMTIME`'s `WORD`, and the API reports `ERROR_INVALID_PARAMETER`
/// instead of wrapping.
fn write_file_time_as_system_time(
    engine: &mut dyn wie_cpu::CpuEngine,
    system_time_va: u64,
    filetime: u64,
) -> Result<bool> {
    let Some(fields) = crate::kernel32::misc::filetime_to_system_time(filetime) else {
        return Ok(false);
    };
    let [
        year,
        month,
        day_of_week,
        day,
        hour,
        minute,
        second,
        milliseconds,
    ] = fields;
    // One typed write covers all eight WORD fields (the view starts zeroed,
    // which is what the per-field writes used to produce).
    with_typed_write::<SystemTime, _, _>(engine, system_time_va, |st| {
        st.w_year = year;
        st.w_month = month;
        st.w_day_of_week = day_of_week;
        st.w_day = day;
        st.w_hour = hour;
        st.w_minute = minute;
        st.w_second = second;
        st.w_milliseconds = milliseconds;
        Ok(())
    })
    .context("failed to write SYSTEMTIME")?;
    Ok(true)
}

/// Handles `KERNEL32.dll!FileTimeToSystemTime`.
///
/// UTC in, UTC out — Windows performs **no** timezone shift here (that is
/// `FileTimeToLocalFileTime`, which every caller runs first; this emulator's
/// guest zone has `Bias = 0`, see `GetTimeZoneInformation`).
///
/// This used to be a stub that ignored its input and wrote a fixed
/// `2026-07-09 12:00:00`, so every date any guest formatted came out as that
/// date. 7-Zip's `l` reaches it once per listed item: `7za l` on an archive
/// showed the same fabricated row date for every file while the timestamps
/// stored in the archive were correct.
pub fn handle_file_time_to_system_time(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let input_file_time_va = engine
        .read_rcx()
        .context("failed to read RCX for FileTimeToSystemTime")?;

    let system_time_va = engine
        .read_rdx()
        .context("failed to read RDX for FileTimeToSystemTime")?;

    let mut success = false;
    if input_file_time_va != 0 && system_time_va != 0 {
        let mut bytes = [0_u8; 8];
        // A FILETIME that cannot be read is the same failure as a bad pointer:
        // FALSE + ERROR_INVALID_PARAMETER, not a raise.
        if engine.mem_read(input_file_time_va, &mut bytes).is_ok() {
            success =
                write_file_time_as_system_time(engine, system_time_va, u64::from_le_bytes(bytes))?;
        }
    }

    ctx.last_error()
        .set_process_last_error(if success { 0 } else { ERROR_INVALID_PARAMETER });

    let return_value = u64::from(success);

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GetFileTime`.
pub fn handle_get_file_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for GetFileTime")?;

    let creation_time_va = engine
        .read_rdx()
        .context("failed to read RDX for GetFileTime")?;

    let last_access_time_va = engine
        .read_r8()
        .context("failed to read R8 for GetFileTime")?;

    let last_write_time_va = engine
        .read_r9()
        .context("failed to read R9 for GetFileTime")?;

    let success = is_open_file_handle(state, handle);

    if success {
        if creation_time_va != 0 {
            write_guest_u64(engine, creation_time_va, FIXED_SYSTEM_FILETIME)?;
        }

        if last_access_time_va != 0 {
            write_guest_u64(engine, last_access_time_va, FIXED_SYSTEM_FILETIME)?;
        }

        if last_write_time_va != 0 {
            write_guest_u64(engine, last_write_time_va, FIXED_SYSTEM_FILETIME)?;
        }

        state.process.last_error = 0;
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
    }

    let return_value = u64::from(success);

    ctx.finish(return_value)
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

    let distance_high_va = engine
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

            if distance_high_va != 0 {
                let high = u32::try_from(new_cursor >> 32)
                    .context("new file cursor high does not fit u32")?;
                write_guest_u32(engine, distance_high_va, high)?;
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

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GetFileSize`.
pub fn handle_get_file_size(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for GetFileSize")?;

    let file_size_high_va = engine
        .read_rdx()
        .context("failed to read RDX for GetFileSize")?;

    let return_value = if let Some(open_file) = find_open_file(state, handle) {
        let file_size = open_file.size();

        let file_size_high =
            u32::try_from(file_size >> 32).context("open file size high does not fit u32")?;

        let file_size_low = u32::try_from(file_size & 0xffff_ffff)
            .context("open file size low does not fit u32")?;

        if file_size_high_va != 0 {
            write_guest_u32(engine, file_size_high_va, file_size_high)?;
        }

        state.process.last_error = 0;

        u64::from(file_size_low)
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0xffff_ffff
    };

    ctx.finish(return_value)
}
pub fn handle_compare_file_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let ta = if a == 0 { 0 } else { read_u64(engine, a)? };
    let tb = if b == 0 { 0 } else { read_u64(engine, b)? };
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
    // Guest zone is UTC: `GetTimeZoneInformation` reports `Bias = 0` with no
    // daylight dates, so the local↔UTC shift is the identity here by policy
    // rather than by omission. A guest that asks for its zone gets the same
    // answer this applies.
    let t = read_u64(engine, local)?;
    write_guest_u64(engine, file, t)?;
    ret_bool_true(engine, "LocalFileTimeToFileTime")
}
pub fn handle_file_time_to_dos_date_time(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ft = engine.read_rcx()?;
    let date_va = engine.read_rdx()?;
    let time_va = engine.read_r8()?;
    if ft == 0 {
        return ret_u64(engine, 0, "FileTimeToDosDateTime");
    }
    // Fixed DOS date/time: 2026-07-19 12:00:00 → rough encoding.
    // DOS date: day + (month<<5) + ((year-1980)<<9)
    // 2026-07-19 → DOS date word; noon → DOS time word.
    let dos_date: u16 = 0x5c_f3; // precomputed: day|month<<5|(year-1980)<<9
    let dos_time: u16 = 0x60_00; // hour 12 << 11
    if date_va != 0 {
        write_guest_u16(engine, date_va, dos_date)?;
    }
    if time_va != 0 {
        write_guest_u16(engine, time_va, dos_time)?;
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

#[cfg(test)]
mod tests {
    use super::write_file_time_as_system_time;
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    /// Where the SYSTEMTIME lands in the test guest's mapped page.
    const SYSTEM_TIME_VA: u64 = 0x3000;
    /// 2024-01-01T00:00:00Z — the FILETIME the VFS reports for every file
    /// (`FIXED_SYSTEM_FILETIME`), i.e. what an archive listing formats.
    const FIXED_FILETIME: u64 = 133_485_408_000_000_000;

    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu
    }

    fn read_system_time(cpu: &mut IcedCpu) -> [u16; 8] {
        let mut bytes = [0_u8; 16];
        cpu.mem_read(SYSTEM_TIME_VA, &mut bytes)
            .expect("read SYSTEMTIME");
        let mut fields = [0_u16; 8];
        for (index, slot) in fields.iter_mut().enumerate() {
            let low = bytes[index * 2];
            let high = bytes[index * 2 + 1];
            *slot = u16::from_le_bytes([low, high]);
        }
        fields
    }

    /// The conversion must depend on its **input**. The handler this was
    /// extracted from ignored it and wrote `2026-07-09 12:00:00` for every
    /// FILETIME, which is what made every row of `7za l` show one fabricated
    /// date while the archive's stored timestamps were correct.
    #[test]
    fn converts_the_input_instant_not_a_constant() {
        let mut cpu = test_engine();

        assert!(
            write_file_time_as_system_time(&mut cpu, SYSTEM_TIME_VA, FIXED_FILETIME)
                .expect("write SYSTEMTIME"),
            "a representable instant converts"
        );
        assert_eq!(
            read_system_time(&mut cpu),
            [2024, 1, 1, 1, 0, 0, 0, 0],
            "2024-01-01T00:00:00Z, Monday (wDayOfWeek 1)"
        );

        // A different instant must produce a different date: 1970-01-01Z.
        let unix_epoch = 11_644_473_600_u64.saturating_mul(10_000_000);
        assert!(
            write_file_time_as_system_time(&mut cpu, SYSTEM_TIME_VA, unix_epoch)
                .expect("write SYSTEMTIME"),
            "the epoch converts"
        );
        assert_eq!(
            read_system_time(&mut cpu),
            [1970, 1, 4, 1, 0, 0, 0, 0],
            "1970-01-01T00:00:00Z, Thursday"
        );
    }

    /// Windows refuses instants past 30827-08-31 rather than wrapping the year,
    /// and leaves the output buffer untouched on failure. The handler turns the
    /// `false` into `ERROR_INVALID_PARAMETER`.
    #[test]
    fn refuses_an_unrepresentable_instant_and_writes_nothing() {
        let mut cpu = test_engine();
        cpu.mem_write(SYSTEM_TIME_VA, &[0xAA_u8; 16])
            .expect("seed SYSTEMTIME");

        assert!(
            !write_file_time_as_system_time(&mut cpu, SYSTEM_TIME_VA, u64::MAX)
                .expect("conversion is not an error"),
            "u64::MAX is far past the last accepted instant"
        );
        assert_eq!(
            read_system_time(&mut cpu),
            [0xAAAA; 8],
            "the output SYSTEMTIME must not be touched on failure"
        );
    }
}
