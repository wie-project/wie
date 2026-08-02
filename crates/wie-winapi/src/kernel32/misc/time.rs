use super::{
    Context, ERROR_INVALID_PARAMETER, HandlerContext, Result, TIME_ZONE_ID_INVALID,
    TIME_ZONE_ID_UNKNOWN, WinApiHandlerResult, checked_field_address, write_guest_u16,
    write_guest_u32,
};

/// Handles `KERNEL32.dll!GetLocalTime`.
pub fn handle_get_local_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let system_time_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetLocalTime")?;

    if system_time_ptr != 0 {
        // SYSTEMTIME:
        // WORD wYear;         offset 0
        // WORD wMonth;        offset 2
        // WORD wDayOfWeek;    offset 4
        // WORD wDay;          offset 6
        // WORD wHour;         offset 8
        // WORD wMinute;       offset 10
        // WORD wSecond;       offset 12
        // WORD wMilliseconds; offset 14

        let year_address = checked_field_address(system_time_ptr, 0, "wYear");
        let month_address = checked_field_address(system_time_ptr, 2, "wMonth");
        let day_of_week_address = checked_field_address(system_time_ptr, 4, "wDayOfWeek");
        let day_address = checked_field_address(system_time_ptr, 6, "wDay");
        let hour_address = checked_field_address(system_time_ptr, 8, "wHour");
        let minute_address = checked_field_address(system_time_ptr, 10, "wMinute");
        let second_address = checked_field_address(system_time_ptr, 12, "wSecond");
        let milliseconds_address = checked_field_address(system_time_ptr, 14, "wMilliseconds");

        // Deterministic fake local time.
        write_guest_u16(engine, year_address, 2026)?;
        write_guest_u16(engine, month_address, 7)?;
        write_guest_u16(engine, day_of_week_address, 4)?;
        write_guest_u16(engine, day_address, 9)?;
        write_guest_u16(engine, hour_address, 12)?;
        write_guest_u16(engine, minute_address, 0)?;
        write_guest_u16(engine, second_address, 0)?;
        write_guest_u16(engine, milliseconds_address, 0)?;
    }

    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from GetLocalTime")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `KERNEL32.dll!GetTimeZoneInformation`.
pub fn handle_get_time_zone_information(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let time_zone_info_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetTimeZoneInformation")?;

    let success = time_zone_info_ptr != 0;

    if success {
        // TIME_ZONE_INFORMATION:
        // LONG       Bias;              offset 0
        // WCHAR      StandardName[32];  offset 4
        // SYSTEMTIME StandardDate;      offset 68
        // LONG       StandardBias;      offset 84
        // WCHAR      DaylightName[32];  offset 88
        // SYSTEMTIME DaylightDate;      offset 152
        // LONG       DaylightBias;      offset 168

        let bias_address = checked_field_address(time_zone_info_ptr, 0, "Bias");
        let standard_name_address = checked_field_address(time_zone_info_ptr, 4, "StandardName");
        let standard_date_address = checked_field_address(time_zone_info_ptr, 68, "StandardDate");
        let standard_bias_address = checked_field_address(time_zone_info_ptr, 84, "StandardBias");
        let daylight_name_address = checked_field_address(time_zone_info_ptr, 88, "DaylightName");
        let daylight_date_address = checked_field_address(time_zone_info_ptr, 152, "DaylightDate");
        let daylight_bias_address = checked_field_address(time_zone_info_ptr, 168, "DaylightBias");

        let empty_name = [0_u8; 64];
        let empty_system_time = [0_u8; 16];

        // Deterministic UTC-like fake timezone:
        // Bias = 0, no daylight/standard transition dates.
        write_guest_u32(engine, bias_address, 0)?;
        engine
            .mem_write(standard_name_address, &empty_name)
            .context("failed to write TIME_ZONE_INFORMATION StandardName")?;
        engine
            .mem_write(standard_date_address, &empty_system_time)
            .context("failed to write TIME_ZONE_INFORMATION StandardDate")?;
        write_guest_u32(engine, standard_bias_address, 0)?;
        engine
            .mem_write(daylight_name_address, &empty_name)
            .context("failed to write TIME_ZONE_INFORMATION DaylightName")?;
        engine
            .mem_write(daylight_date_address, &empty_system_time)
            .context("failed to write TIME_ZONE_INFORMATION DaylightDate")?;
        write_guest_u32(engine, daylight_bias_address, 0)?;

        state.process.last_error = 0;
    } else {
        state.process.last_error = ERROR_INVALID_PARAMETER;
    }

    let return_value = if success {
        TIME_ZONE_ID_UNKNOWN
    } else {
        TIME_ZONE_ID_INVALID
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetTimeZoneInformation")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
