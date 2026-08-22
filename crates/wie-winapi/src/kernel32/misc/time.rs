use super::{
    Context, ERROR_INVALID_PARAMETER, HandlerContext, Result, TIME_ZONE_ID_INVALID,
    TIME_ZONE_ID_UNKNOWN, WinApiHandlerResult, checked_address, write_guest_u16, write_guest_u32,
};
use crate::guest_memory::{read_u16, read_u64};
use crate::guest_string::{read_utf16_lossy as read_guest_utf16_lossy, write_utf16_c_string};

/// Handles `KERNEL32.dll!GetLocalTime`.
pub fn handle_get_local_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let system_time_va = engine
        .read_rcx()
        .context("failed to read RCX for GetLocalTime")?;

    if system_time_va != 0 {
        // SYSTEMTIME:
        // WORD wYear;         offset 0
        // WORD wMonth;        offset 2
        // WORD wDayOfWeek;    offset 4
        // WORD wDay;          offset 6
        // WORD wHour;         offset 8
        // WORD wMinute;       offset 10
        // WORD wSecond;       offset 12
        // WORD wMilliseconds; offset 14

        let year_address = checked_address(system_time_va, 0, "wYear");
        let month_address = checked_address(system_time_va, 2, "wMonth");
        let day_of_week_address = checked_address(system_time_va, 4, "wDayOfWeek");
        let day_address = checked_address(system_time_va, 6, "wDay");
        let hour_address = checked_address(system_time_va, 8, "wHour");
        let minute_address = checked_address(system_time_va, 10, "wMinute");
        let second_address = checked_address(system_time_va, 12, "wSecond");
        let milliseconds_address = checked_address(system_time_va, 14, "wMilliseconds");

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

    ctx.finish(0)
}
/// Convert a `FILETIME` (100-ns units since 1601-01-01 UTC) to the eight
/// `WORD` fields of a `SYSTEMTIME` (`wYear`, `wMonth`, `wDayOfWeek`, `wDay`,
/// `wHour`, `wMinute`, `wSecond`, `wMilliseconds`) in UTC.
///
/// Shares the civil-date conversion (Hinnant's `civil_from_days` algorithm)
/// with no other code in this crate — the only other SYSTEMTIME producer is
/// the deterministic fake `GetLocalTime`. Tested against the fixed-clock
/// [`FIXED_SYSTEM_FILETIME`] constant, which maps to 2024-01-01T00:00:00Z.
#[must_use]
fn filetime_to_system_time(filetime: u64) -> [u16; 8] {
    let hundred_ns = u128::from(filetime);
    let total_seconds = hundred_ns / 10_000_000;
    let sub_second = hundred_ns % 10_000_000;
    // Whole seconds from the UNIX epoch (1601-01-01 + 11_644_473_600 s).
    let unix_seconds = total_seconds.saturating_sub(11_644_473_600);
    let days_since_epoch = i64::try_from(unix_seconds / 86_400).unwrap_or(0);
    let day_seconds = unix_seconds % 86_400;
    let hour = u16::try_from(day_seconds / 3_600).unwrap_or(0);
    let minute = u16::try_from((day_seconds % 3_600) / 60).unwrap_or(0);
    let second = u16::try_from(day_seconds % 60).unwrap_or(0);
    // 1 ms = 10,000 hundred-ns units; the sub-second part is < 1 s.
    let milliseconds = u16::try_from(sub_second / 10_000).unwrap_or(0);

    // Hinnant's civil_from_days: days since 1970-01-01 → (y, m, d).
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y0 = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u16::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(0);
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let mut year = y0;
    if month <= 2 {
        year += 1;
    }
    let month = u16::try_from(month).unwrap_or(0);
    let year = u16::try_from(year).unwrap_or(0);
    // 1970-01-01 was a Thursday = wDayOfWeek 4 (Sunday is 0).
    let day_of_week = u16::try_from(days_since_epoch.rem_euclid(7) + 4).unwrap_or(0) % 7;

    [
        year,
        month,
        day_of_week,
        day,
        hour,
        minute,
        second,
        milliseconds,
    ]
}

/// Handles `KERNEL32.dll!GetSystemTime` — fill a UTC `SYSTEMTIME`.
///
/// Reads the current wall-clock `FILETIME` from the same clock source as
/// `GetSystemTimeAsFileTime` (so `WIE_FIXED_CLOCK=1` freezes it too) and
/// converts it to the sub-fielded `SYSTEMTIME`. Returns void.
pub fn handle_get_system_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let system_time_va = engine
        .read_rcx()
        .context("failed to read RCX for GetSystemTime")?;

    if system_time_va != 0 {
        let filetime = crate::kernel32::clock::system_time_filetime();
        let fields = filetime_to_system_time(filetime);
        for (i, value) in fields.iter().enumerate() {
            let offset = u64::try_from(i * 2).unwrap_or(0);
            write_guest_u16(
                engine,
                checked_address(system_time_va, offset, "SYSTEMTIME"),
                *value,
            )?;
        }
    }

    ctx.finish(0)
}
/// Handles `KERNEL32.dll!GetTimeZoneInformation`.
pub fn handle_get_time_zone_information(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let time_zone_info_va = engine
        .read_rcx()
        .context("failed to read RCX for GetTimeZoneInformation")?;

    let success = time_zone_info_va != 0;

    if success {
        // TIME_ZONE_INFORMATION:
        // LONG       Bias;              offset 0
        // WCHAR      StandardName[32];  offset 4
        // SYSTEMTIME StandardDate;      offset 68
        // LONG       StandardBias;      offset 84
        // WCHAR      DaylightName[32];  offset 88
        // SYSTEMTIME DaylightDate;      offset 152
        // LONG       DaylightBias;      offset 168

        let bias_address = checked_address(time_zone_info_va, 0, "Bias");
        let standard_name_address = checked_address(time_zone_info_va, 4, "StandardName");
        let standard_date_address = checked_address(time_zone_info_va, 68, "StandardDate");
        let standard_bias_address = checked_address(time_zone_info_va, 84, "StandardBias");
        let daylight_name_address = checked_address(time_zone_info_va, 88, "DaylightName");
        let daylight_date_address = checked_address(time_zone_info_va, 152, "DaylightDate");
        let daylight_bias_address = checked_address(time_zone_info_va, 168, "DaylightBias");

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

    ctx.finish(return_value)
}

// ── GetTimeFormatW / GetDateFormatW ────────────────────────────────────────

/// `DATE_LONGDATE` flag — the long date form (notepad's Edit → Time/Date uses
/// it; the default with no flag is the short date).
const DATE_LONGDATE: u32 = 0x0000_0002;

/// `SYSTEMTIME` fields read by the format engine (offsets in bytes).
#[derive(Debug, Clone, Copy)]
struct TimeParts {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
}

/// English month names (the `MMMM`/`MMM` specifiers of the long date).
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// English weekday names (the `dddd`/`ddd` specifiers; Sunday = 0).
const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Whether the locale's default time format uses a 12-hour clock.
///
/// Windows `LOCALE_ITIME`: en-US and en-CA default to `h:mm:ss tt`; the
/// overwhelming majority of other locales default to 24-hour time. This is a
/// fidelity heuristic — the exact per-locale `TIME_` constants are not
/// carried on the host.
#[must_use]
fn uses_12_hour_clock(locale: u32) -> bool {
    matches!(locale & 0xFFFF, 0x0409 | 0x1009)
}

/// The default time format string for `GetTimeFormat` with a NULL format.
#[must_use]
fn default_time_format(locale: u32) -> &'static str {
    if uses_12_hour_clock(locale) {
        "h:mm:ss tt"
    } else {
        "HH:mm:ss"
    }
}

/// The default date format string for `GetDateFormat` with a NULL format.
#[must_use]
fn default_date_format(flags: u32) -> &'static str {
    if flags & DATE_LONGDATE != 0 {
        "dddd, MMMM d, yyyy"
    } else {
        "M/d/yyyy"
    }
}

/// Handles `KERNEL32.dll!GetTimeFormatW`.
pub fn handle_get_time_format_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_time_date_format(ctx, true, "GetTimeFormatW")
}
/// Handles `KERNEL32.dll!GetDateFormatW`.
pub fn handle_get_date_format_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_get_time_date_format(ctx, false, "GetDateFormatW")
}

/// Shared `GetTimeFormatW` / `GetDateFormatW` implementation.
///
/// `GetTimeFormatW(locale, flags, lpTime, lpFormat, lpTimeStr, cchTime)` and
/// `GetDateFormatW(locale, flags, lpDate, lpFormat, lpDateStr, cchDate)` share
/// argument order and buffer semantics: returns the written character count
/// (0 on failure), reports the required size when `cch == 0`, and is
/// locale-aware for the NULL-format default.
fn handle_get_time_date_format(
    ctx: &mut HandlerContext<'_>,
    is_time: bool,
    api_name: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let locale_raw = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let flags_raw = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let system_time_va = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let format_va = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;
    let rsp = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {api_name}"))?;
    let buffer_va = read_u64(engine, rsp.wrapping_add(0x28))
        .with_context(|| format!("failed to read 5th arg for {api_name}"))?;
    let buffer_len_raw = read_u64(engine, rsp.wrapping_add(0x30))
        .with_context(|| format!("failed to read 6th arg for {api_name}"))?;

    let locale = u32::try_from(locale_raw & u64::from(u32::MAX)).unwrap_or(0);
    let flags = u32::try_from(flags_raw & u64::from(u32::MAX)).unwrap_or(0);

    let return_value = if system_time_va == 0 || buffer_va == 0 {
        0
    } else {
        let parts = read_system_time(engine, system_time_va)?;
        let format = if format_va == 0 {
            if is_time {
                default_time_format(locale).to_owned()
            } else {
                default_date_format(flags).to_owned()
            }
        } else {
            read_guest_utf16_lossy(engine, format_va, 256)?
        };
        let text = if is_time {
            format_time_parts(&parts, &format)
        } else {
            format_date_parts(&parts, &format)
        };
        // Required size includes the NUL terminator (cch == 0 asks for it).
        let required = text.encode_utf16().count().saturating_add(1);
        let buffer_len = usize::try_from(buffer_len_raw).unwrap_or(0);
        if buffer_len == 0 {
            u64::try_from(required).unwrap_or(0)
        } else if buffer_len < required {
            0
        } else {
            let copied = write_utf16_c_string(engine, buffer_va, buffer_len, &text)?;
            u64::try_from(copied).unwrap_or(0)
        }
    };

    state.process.last_error = if return_value == 0 {
        ERROR_INVALID_PARAMETER
    } else {
        0
    };

    ctx.finish(return_value)
}

/// Read the `SYSTEMTIME` fields the format engine needs.
fn read_system_time(engine: &mut dyn wie_cpu::CpuEngine, system_time_va: u64) -> Result<TimeParts> {
    Ok(TimeParts {
        year: read_u16(engine, checked_address(system_time_va, 0, "wYear"))?,
        month: read_u16(engine, checked_address(system_time_va, 2, "wMonth"))?,
        day_of_week: read_u16(engine, checked_address(system_time_va, 4, "wDayOfWeek"))?,
        day: read_u16(engine, checked_address(system_time_va, 6, "wDay"))?,
        hour: read_u16(engine, checked_address(system_time_va, 8, "wHour"))?,
        minute: read_u16(engine, checked_address(system_time_va, 10, "wMinute"))?,
        second: read_u16(engine, checked_address(system_time_va, 12, "wSecond"))?,
    })
}

/// Format a time with a Windows time-format picture string.
///
/// Tokens are runs of one ASCII letter: `h`/`hh` 12-hour, `H`/`HH` 24-hour,
/// `m`/`mm` minutes, `s`/`ss` seconds, `t`/`tt` AM/PM marker. Punctuation and
/// unknown letters pass through literally (Windows tokenization).
#[must_use]
fn format_time_parts(parts: &TimeParts, format: &str) -> String {
    let mut out = String::with_capacity(format.len().saturating_add(8));
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_alphabetic() {
            let mut run_len = 1_usize;
            while chars.peek() == Some(&ch) {
                chars.next();
                run_len = run_len.saturating_add(1);
            }
            push_time_token(&mut out, parts, ch, run_len);
        } else {
            out.push(ch);
        }
    }
    out
}

fn push_time_token(out: &mut String, parts: &TimeParts, token: char, run_len: usize) {
    // 12-hour hour: 0..11 → 12, 1..11 → as-is (Windows 12-hour clock).
    let hour12 = parts.hour % 12;
    let hour12 = if hour12 == 0 { 12 } else { hour12 };
    match token {
        'h' => push_padded(out, hour12, if run_len >= 2 { 2 } else { 1 }),
        'H' => push_padded(out, parts.hour, if run_len >= 2 { 2 } else { 1 }),
        'm' => push_padded(out, parts.minute, if run_len >= 2 { 2 } else { 1 }),
        's' => push_padded(out, parts.second, if run_len >= 2 { 2 } else { 1 }),
        't' => {
            let am = parts.hour < 12;
            if run_len >= 2 {
                out.push_str(if am { "AM" } else { "PM" });
            } else {
                out.push(if am { 'A' } else { 'P' });
            }
        }
        other => {
            for _ in 0..run_len {
                out.push(other);
            }
        }
    }
}

/// Format a date with a Windows date-format picture string.
///
/// Tokens: `d`/`dd` day, `ddd`/`dddd` weekday, `M`/`MM` month, `MMM`/`MMMM`
/// month name, `y`/`yy`/`yyyy` year (run length = digit count). Punctuation
/// and unknown letters pass through literally.
#[must_use]
fn format_date_parts(parts: &TimeParts, format: &str) -> String {
    let mut out = String::with_capacity(format.len().saturating_add(8));
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_alphabetic() {
            let mut run_len = 1_usize;
            while chars.peek() == Some(&ch) {
                chars.next();
                run_len = run_len.saturating_add(1);
            }
            push_date_token(&mut out, parts, ch, run_len);
        } else {
            out.push(ch);
        }
    }
    out
}

fn push_date_token(out: &mut String, parts: &TimeParts, token: char, run_len: usize) {
    match token {
        'M' => {
            // Windows month is 1-based; a 0 (uninitialised) SYSTEMTIME clamps
            // to January rather than indexing off the front of the table.
            let index = usize::from(parts.month.saturating_sub(1)).min(MONTH_NAMES.len() - 1);
            match run_len {
                1 => out.push_str(&parts.month.to_string()),
                2 => push_padded(out, parts.month, 2),
                _ => out.push_str(MONTH_NAMES.get(index).copied().unwrap_or("")),
            }
        }
        'd' => {
            let index = usize::from(parts.day_of_week).min(WEEKDAY_NAMES.len() - 1);
            match run_len {
                1 => out.push_str(&parts.day.to_string()),
                2 => push_padded(out, parts.day, 2),
                3 => out.push_str(
                    WEEKDAY_NAMES
                        .get(index)
                        .map_or("", |name| name.get(..3).unwrap_or(name)),
                ),
                _ => out.push_str(WEEKDAY_NAMES.get(index).copied().unwrap_or("")),
            }
        }
        'y' => match run_len {
            1 => out.push_str(&parts.year.to_string()),
            2 => push_padded(out, parts.year % 100, 2),
            3 => push_padded(out, parts.year % 1000, 3),
            _ => out.push_str(&parts.year.to_string()),
        },
        other => {
            for _ in 0..run_len {
                out.push(other);
            }
        }
    }
}

/// Write `value` right-aligned in `width` digits with leading zeros.
fn push_padded(out: &mut String, value: u16, width: usize) {
    let text = value.to_string();
    for _ in text.len()..width {
        out.push('0');
    }
    out.push_str(&text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> TimeParts {
        TimeParts {
            year: 2026,
            month: 7,
            day_of_week: 4,
            day: 9,
            hour: 14,
            minute: 5,
            second: 3,
        }
    }

    #[test]
    fn default_time_format_en_us_is_12_hour() {
        assert_eq!(default_time_format(0x0409), "h:mm:ss tt");
        assert_eq!(format_time_parts(&parts(), "h:mm:ss tt"), "2:05:03 PM");
    }

    #[test]
    fn default_time_format_other_locales_are_24_hour() {
        // de-DE / fr-FR / ja-JP all default to 24-hour time.
        for locale in [0x0407, 0x040C, 0x0411] {
            assert_eq!(
                default_time_format(locale),
                "HH:mm:ss",
                "locale {locale:#x}"
            );
        }
        assert_eq!(format_time_parts(&parts(), "HH:mm:ss"), "14:05:03");
    }

    #[test]
    fn explicit_time_format_handles_specifiers() {
        let morning = TimeParts { hour: 9, ..parts() };
        assert_eq!(format_time_parts(&morning, "h:mm:ss tt"), "9:05:03 AM");
        assert_eq!(format_time_parts(&morning, "hh:mm:ss tt"), "09:05:03 AM");
        // Punctuation passes through literally; apostrophes are just characters.
        assert_eq!(format_time_parts(&morning, "[hh]mm"), "[09]05");
        // Windows tokenizes recognized single letters even inside words: 't'
        // is the AM/PM marker, 'm' is minutes, while 'i'/'e' are literals.
        assert_eq!(format_time_parts(&morning, "time"), "Ai5e");
    }

    #[test]
    fn default_date_format_short_and_long() {
        assert_eq!(default_date_format(0), "M/d/yyyy");
        assert_eq!(default_date_format(DATE_LONGDATE), "dddd, MMMM d, yyyy");
        assert_eq!(format_date_parts(&parts(), "M/d/yyyy"), "7/9/2026");
        assert_eq!(
            format_date_parts(&parts(), "dddd, MMMM d, yyyy"),
            "Thursday, July 9, 2026"
        );
    }

    #[test]
    fn explicit_date_format_handles_specifiers() {
        assert_eq!(format_date_parts(&parts(), "MM/dd/yy"), "07/09/26");
        assert_eq!(format_date_parts(&parts(), "yyyy-MM-dd"), "2026-07-09");
        assert_eq!(format_date_parts(&parts(), "ddd"), "Thu");
        assert_eq!(format_date_parts(&parts(), "MMMM"), "July");
    }

    /// GetSystemTime fills a UTC SYSTEMTIME — the pure conversion core is
    /// pinned against known FILETIMEs:
    /// - the fixed-clock `FIXED_SYSTEM_FILETIME` (2024-01-01T00:00:00Z, Monday),
    /// - the UNIX epoch (1970-01-01T00:00:00Z, Thursday),
    /// - a real date/time (2026-07-09T14:05:03Z, Thursday).
    #[test]
    fn filetime_to_system_time_matches_known_utc_instants() {
        assert_eq!(
            filetime_to_system_time(crate::kernel32::FIXED_SYSTEM_FILETIME),
            [2024, 1, 1, 1, 0, 0, 0, 0],
            "fixed clock = 2024-01-01T00:00:00Z, Monday"
        );
        let epoch = 11_644_473_600_u64.saturating_mul(10_000_000);
        assert_eq!(
            filetime_to_system_time(epoch),
            [1970, 1, 4, 1, 0, 0, 0, 0],
            "UNIX epoch = 1970-01-01T00:00:00Z, Thursday"
        );
        // 2026-07-09T14:05:03Z in 100-ns FILETIME units.
        let filetime = 11_644_473_600_u64
            .saturating_add(1_783_605_903)
            .saturating_mul(10_000_000);
        assert_eq!(
            filetime_to_system_time(filetime),
            [2026, 7, 4, 9, 14, 5, 3, 0],
            "a real UTC instant round-trips"
        );
    }
}
