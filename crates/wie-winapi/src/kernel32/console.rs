use super::{
    Context, FAKE_STDERR_HANDLE, FAKE_STDIN_HANDLE, FAKE_STDOUT_HANDLE, HandlerContext,
    INVALID_HANDLE_VALUE, MAX_HOST_STDIN_LINE, Result, STD_ERROR_HANDLE_ID, STD_INPUT_HANDLE_ID,
    STD_OUTPUT_HANDLE_ID, WinApiHandlerResult, WinApiState, low_u32, read_guest_utf16_lossy,
    ret_bool_true, ret_u64, write_guest_u32,
};
use crate::console::{
    self, CP_OEM_437, CP_UTF8, CP_WINDOWS_1252, ConsoleState, PRIMARY_BUFFER_HANDLE,
    VALID_INPUT_MODE, VALID_OUTPUT_MODE, codepage, host_term,
};
use crate::guest_memory::read_bytes as read_guest_bytes;
use crate::guest_string::read_ansi_bytes;

/// `ERROR_INVALID_HANDLE` — returned for console calls on a non-console handle.
const ERROR_INVALID_HANDLE: u32 = 6;

/// Fake `HWND` for `GetConsoleWindow`. Non-zero so `if (hwnd)` guards pass,
/// and outside the window-handle range `user32` hands out so it can never be
/// mistaken for a real emulated window.
const FAKE_CONSOLE_HWND: u64 = 0x0000_0000_6000_00F0;

pub(crate) fn read_host_console_stdin_line() -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;

    let mut out = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    let mut stdin = std::io::stdin().lock();
    loop {
        if out.len() >= MAX_HOST_STDIN_LINE {
            break;
        }
        if stdin.read(&mut byte)? == 0 {
            break;
        }
        out.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

pub(crate) fn refill_stdin_from_host(state: &mut WinApiState) -> Result<bool, ()> {
    match read_host_console_stdin_line() {
        Ok(None) => Ok(false),
        Ok(Some(line)) => {
            state.file_io.stdin_bytes = line;
            state.file_io.stdin_cursor = 0;
            Ok(true)
        }
        Err(_) => Err(()),
    }
}

/// True for the three fake std handles plus any allocated screen buffer.
fn is_console_output_handle(state: &ConsoleState, handle: u64) -> bool {
    handle == FAKE_STDOUT_HANDLE || handle == FAKE_STDERR_HANDLE || state.buffer(handle).is_some()
}

/// Map an output handle to the screen buffer it is bound to.
///
/// `STD_OUTPUT_HANDLE` and `STD_ERROR_HANDLE` are permanently bound to the
/// primary buffer. That binding is deliberate and matches Windows: a handle
/// refers to one specific screen buffer, and `SetConsoleActiveScreenBuffer`
/// changes only which buffer is *displayed*. Resolving stdout to "whichever
/// buffer is active" would break the standard double-buffering idiom, where a
/// game draws into the back buffer while stdout still names the front one.
pub(crate) fn buffer_handle_for(state: &ConsoleState, handle: u64) -> Option<u64> {
    if handle == FAKE_STDOUT_HANDLE || handle == FAKE_STDERR_HANDLE {
        return Some(PRIMARY_BUFFER_HANDLE);
    }
    state.buffer(handle).map(|_| handle)
}

/// Return `FALSE` with `SetLastError(ERROR_INVALID_HANDLE)`.
pub(crate) fn ret_invalid_handle(
    ctx: &mut HandlerContext<'_>,
    api: &str,
) -> Result<WinApiHandlerResult> {
    ctx.state.process.last_error = ERROR_INVALID_HANDLE;
    ret_u64(ctx.engine, 0, api)
}

/// Handles `KERNEL32.dll!GetStdHandle`.
pub fn handle_get_std_handle(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let std_handle_id_raw = engine
        .read_rcx()
        .context("failed to read RCX for GetStdHandle")?;

    let std_handle_id = low_u32(std_handle_id_raw, "GetStdHandle id")?;

    let return_value = match std_handle_id {
        STD_INPUT_HANDLE_ID => FAKE_STDIN_HANDLE,
        STD_OUTPUT_HANDLE_ID => FAKE_STDOUT_HANDLE,
        STD_ERROR_HANDLE_ID => FAKE_STDERR_HANDLE,
        // Microsoft Learn: invalid standard device → INVALID_HANDLE_VALUE.
        _ => INVALID_HANDLE_VALUE,
    };

    ctx.finish(return_value)
}

pub fn handle_set_console_ctrl_handler(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _handler = engine.read_rcx().context("SetConsoleCtrlHandler RCX")?;
    let _add = engine.read_rdx().context("SetConsoleCtrlHandler RDX")?;
    ret_bool_true(engine, "SetConsoleCtrlHandler")
}

pub fn handle_get_console_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("GetConsoleMode RCX")?;
    let mode_va = ctx.engine.read_rdx().context("GetConsoleMode RDX")?;
    if mode_va == 0 {
        return ret_invalid_handle(ctx, "GetConsoleMode");
    }
    let mode = if handle == FAKE_STDIN_HANDLE {
        ctx.state.console().input_mode
    } else if let Some(buffer) = buffer_handle_for(ctx.state.console(), handle) {
        ctx.state.console().output_mode(buffer)
    } else {
        return ret_invalid_handle(ctx, "GetConsoleMode");
    };
    write_guest_u32(ctx.engine, mode_va, mode)?;
    ret_bool_true(ctx.engine, "GetConsoleMode")
}

pub fn handle_set_console_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("SetConsoleMode RCX")?;
    let requested = low_u32(
        ctx.engine.read_rdx().context("SetConsoleMode RDX")?,
        "SetConsoleMode mode",
    )?;

    if handle == FAKE_STDIN_HANDLE {
        // Microsoft Learn: undefined bits fail the call outright rather than
        // being silently dropped, so a guest probing for VT support gets a
        // truthful answer.
        if requested & !VALID_INPUT_MODE != 0 {
            ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
            return ret_u64(ctx.engine, 0, "SetConsoleMode");
        }
        let prev_mode = ctx.state.console().input_mode;
        ctx.state.console().input_mode = requested;
        apply_input_mode_to_host(ctx.state.console());
        // Toggle xterm mouse reporting when the guest changes ENABLE_MOUSE_INPUT
        if (prev_mode ^ requested) & console::ENABLE_MOUSE_INPUT != 0 {
            crate::console::pump::set_mouse_reporting(requested & console::ENABLE_MOUSE_INPUT != 0);
        }
        return ret_bool_true(ctx.engine, "SetConsoleMode");
    }

    let Some(buffer) = buffer_handle_for(ctx.state.console(), handle) else {
        return ret_invalid_handle(ctx, "SetConsoleMode");
    };
    if requested & !VALID_OUTPUT_MODE != 0 {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, "SetConsoleMode");
    }
    ctx.state.console().set_output_mode(buffer, requested);
    ret_bool_true(ctx.engine, "SetConsoleMode")
}

/// Push the guest's input mode down to the host terminal.
///
/// Windows expresses "give me raw keystrokes" as clearing `ENABLE_LINE_INPUT`;
/// the Unix equivalent is cbreak mode. Echo maps directly, and
/// `ENABLE_PROCESSED_INPUT` decides whether Ctrl+C stays a signal or arrives as
/// a key event.
pub(crate) fn apply_input_mode_to_host(state: &ConsoleState) {
    let line_mode = state.input_mode & console::ENABLE_LINE_INPUT != 0;
    let echo = state.input_mode & console::ENABLE_ECHO_INPUT != 0;
    if line_mode && echo {
        if host_term::raw_active() {
            host_term::restore_now();
        }
        return;
    }
    let processed = state.input_mode & console::ENABLE_PROCESSED_INPUT != 0;
    host_term::enter_raw(processed);
}

pub fn handle_get_console_screen_buffer_info(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx
        .engine
        .read_rcx()
        .context("GetConsoleScreenBufferInfo RCX")?;
    let info_va = ctx
        .engine
        .read_rdx()
        .context("GetConsoleScreenBufferInfo RDX")?;
    if info_va == 0 {
        return ret_invalid_handle(ctx, "GetConsoleScreenBufferInfo");
    }
    let Some(buffer_handle) = buffer_handle_for(ctx.state.console(), handle) else {
        return ret_invalid_handle(ctx, "GetConsoleScreenBufferInfo");
    };
    // Adopt any terminal resize that happened since the last call, so a guest
    // that sizes its playfield from this struct tracks the real window.
    let _ = ctx.state.console().sync_window_size();

    let Some(buffer) = ctx.state.console().buffer(buffer_handle) else {
        return ret_invalid_handle(ctx, "GetConsoleScreenBufferInfo");
    };
    let columns = buffer.width;
    let rows = buffer.height;
    let cursor = buffer.cursor;
    let attributes = buffer.attributes;

    // CONSOLE_SCREEN_BUFFER_INFO is 22 bytes; pad to 24 so short stacks stay safe.
    // COORD dwSize {X,Y} at 0; COORD dwCursorPosition at 4; WORD wAttributes at 8;
    // SMALL_RECT srWindow at 10; COORD dwMaximumWindowSize at 18.
    let mut buf = [0_u8; 24];
    write_le_u16(&mut buf, 0, columns);
    write_le_u16(&mut buf, 2, rows);
    write_le_i16(&mut buf, 4, cursor.x);
    write_le_i16(&mut buf, 6, cursor.y);
    write_le_u16(&mut buf, 8, attributes);
    // srWindow is inclusive on all four edges.
    write_le_u16(&mut buf, 10, 0);
    write_le_u16(&mut buf, 12, 0);
    write_le_u16(&mut buf, 14, columns.saturating_sub(1));
    write_le_u16(&mut buf, 16, rows.saturating_sub(1));
    write_le_u16(&mut buf, 18, columns);
    write_le_u16(&mut buf, 20, rows);
    ctx.engine
        .mem_write(info_va, &buf)
        .context("GetConsoleScreenBufferInfo write")?;
    ret_bool_true(ctx.engine, "GetConsoleScreenBufferInfo")
}

fn write_le_u16(buf: &mut [u8; 24], offset: usize, value: u16) {
    if let Some(slot) = buf.get_mut(offset..offset.saturating_add(2)) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

fn write_le_i16(buf: &mut [u8; 24], offset: usize, value: i16) {
    write_le_u16(buf, offset, u16::from_ne_bytes(value.to_ne_bytes()));
}

/// Shared body of `WriteConsoleW` / `WriteConsoleA`.
///
/// Win64 ABI: `hConsoleOutput`, `lpBuffer`, `nNumberOfCharsToWrite`,
/// `lpNumberOfCharsWritten`, `lpReserved` (stack).
fn write_console(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "WriteConsoleW"
    } else {
        "WriteConsoleA"
    };
    let handle = ctx.engine.read_rcx().context("WriteConsole RCX")?;
    let buffer_va = ctx.engine.read_rdx().context("WriteConsole RDX")?;
    let count = low_u32(
        ctx.engine.read_r8().context("WriteConsole R8")?,
        "WriteConsole count",
    )?;
    let written_va = ctx.engine.read_r9().context("WriteConsole R9")?;

    if !is_console_output_handle(ctx.state.console(), handle) {
        return ret_invalid_handle(ctx, api);
    }

    let count_usize = usize::try_from(count).unwrap_or(0);
    let units: Vec<u16> = if buffer_va == 0 || count_usize == 0 {
        Vec::new()
    } else if wide {
        let byte_len = count_usize.saturating_mul(2);
        let mut bytes = vec![0_u8; byte_len];
        read_guest_bytes(ctx.engine, buffer_va, &mut bytes)
            .context("WriteConsoleW guest buffer")?;
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                u16::from_le_bytes([
                    pair.first().copied().unwrap_or(0),
                    pair.get(1).copied().unwrap_or(0),
                ])
            })
            .collect()
    } else {
        let mut bytes = vec![0_u8; count_usize];
        read_guest_bytes(ctx.engine, buffer_va, &mut bytes)
            .context("WriteConsoleA guest buffer")?;
        let code_page = ctx.state.console().output_code_page;
        codepage::decode_to_units(code_page, &bytes)
    };

    emit_console_text(ctx, handle, &units);

    if written_va != 0 {
        write_guest_u32(ctx.engine, written_va, count)?;
    }
    ret_bool_true(ctx.engine, api)
}

/// Send decoded text to the terminal and advance the tracked cursor.
///
/// In Stream mode the bytes go straight out, which preserves any VT escapes the
/// guest emits itself. In Cells mode the text is folded into the grid instead,
/// so it participates in the next diff rather than fighting it.
fn emit_console_text(ctx: &mut HandlerContext<'_>, handle: u64, units: &[u16]) {
    if units.is_empty() {
        return;
    }
    let Some(buffer_handle) = buffer_handle_for(ctx.state.console(), handle) else {
        return;
    };
    // Always fold into the grid. The stream_buf/flush mechanism is no longer
    // needed — the grid diff renderer (screen::flush) emits only the changed
    // cells as targeted Ansi escapes, so the terminal never does full-frame
    // progressive painting.
    ctx.state.console().render_mode = console::RenderMode::Cells;
    fold_text_into_grid(ctx.state.console(), buffer_handle, units);
    // Mark that the grid needs flushing — the diff will be emitted on Sleep,
    // _getch, or _kbhit via flush_stream_output.
    ctx.state.console().needs_flush = true;
}

#[cfg(unix)]
#[allow(dead_code)]
fn write_host_stderr(bytes: &[u8]) {
    crate::ucrt::write_all_fd(libc::STDERR_FILENO, bytes);
}

#[cfg(not(unix))]
#[allow(dead_code)]
fn write_host_stderr(bytes: &[u8]) {
    use std::io::Write;
    drop(std::io::stderr().write_all(bytes));
}

/// Track where the cursor ends up after streaming `units` to the terminal.
///
/// Models the four control characters a console interprets plus wrapping. It is
/// deliberately not a VT parser — escape sequences are handled by
/// [`ConsoleState::note_stream_escape`] marking the position unknown.
#[allow(dead_code)]
fn advance_tracked_cursor(state: &mut ConsoleState, buffer_handle: u64, units: &[u16]) {
    let Some(buffer) = state.buffer_mut(buffer_handle) else {
        return;
    };
    let width = i16::try_from(buffer.width).unwrap_or(i16::MAX).max(1);
    let height = i16::try_from(buffer.height).unwrap_or(i16::MAX).max(1);
    for &unit in units {
        match unit {
            0x0A => {
                buffer.cursor.x = 0;
                buffer.cursor.y = buffer.cursor.y.saturating_add(1);
            }
            0x0D => buffer.cursor.x = 0,
            0x08 => buffer.cursor.x = buffer.cursor.x.saturating_sub(1),
            0x09 => {
                let next_stop = buffer.cursor.x.saturating_add(8) & !7;
                buffer.cursor.x = next_stop.min(width.saturating_sub(1));
            }
            _ => {
                buffer.cursor.x = buffer.cursor.x.saturating_add(1);
                if buffer.cursor.x >= width {
                    buffer.cursor.x = 0;
                    buffer.cursor.y = buffer.cursor.y.saturating_add(1);
                }
            }
        }
        if buffer.cursor.y >= height {
            buffer.cursor.y = height.saturating_sub(1);
            buffer.scroll_up();
        }
    }
}

/// Apply stream text to the cell grid at the cursor (Cells mode only).
/// Fold decoded text into the grid, interpreting CSI cursor positioning
/// and clear-screen escapes so programs using Ansi via WriteConsole
/// (like the snake game's `\033[H\033[2JScore:...`) render correctly.
pub(crate) fn fold_text_into_grid(state: &mut ConsoleState, buffer_handle: u64, units: &[u16]) {
    let Some(buffer) = state.buffer_mut(buffer_handle) else {
        return;
    };
    let attributes = buffer.attributes;
    let width = i16::try_from(buffer.width).unwrap_or(i16::MAX).max(1);
    let height = i16::try_from(buffer.height).unwrap_or(i16::MAX).max(1);

    // Simple CSI sequence parser state.
    let mut i = 0;
    while i < units.len() {
        let unit = units[i];
        // ESC (0x1B) starts an escape sequence.
        if unit == 0x1B && i + 1 < units.len() && units[i + 1] == u16::from(b'[') {
            i += 2; // skip ESC + '['
            if i < units.len()
                && units[i] == u16::from(b'2')
                && i + 1 < units.len()
                && units[i + 1] == u16::from(b'J')
            {
                // \033[2J — clear entire screen
                for cell in buffer.cells.iter_mut() {
                    *cell = console::CharInfo {
                        unit: u16::from(b' '),
                        attributes,
                    };
                }
                i += 2;
                continue;
            }
            if i < units.len() && units[i] == u16::from(b'J') {
                // \033[J or \033[0J — clear from cursor to end
                if let Some(start) = buffer.index_of(buffer.cursor.x, buffer.cursor.y) {
                    for cell in buffer.cells.get_mut(start..).unwrap_or(&mut []) {
                        *cell = console::CharInfo {
                            unit: u16::from(b' '),
                            attributes,
                        };
                    }
                }
                i += 1;
                continue;
            }
            if i < units.len()
                && units[i] == u16::from(b'1')
                && i + 1 < units.len()
                && units[i + 1] == u16::from(b'J')
            {
                // \033[1J — clear from start to cursor
                if let Some(end) = buffer.index_of(buffer.cursor.x, buffer.cursor.y) {
                    for cell in buffer.cells.get_mut(..=end).unwrap_or(&mut []) {
                        *cell = console::CharInfo {
                            unit: u16::from(b' '),
                            attributes,
                        };
                    }
                }
                i += 2;
                continue;
            }
            if i < units.len() && units[i] == u16::from(b'H') {
                // \033[H — cursor home (1,1 → 0,0)
                buffer.cursor.x = 0;
                buffer.cursor.y = 0;
                i += 1;
                continue;
            }
            if i + 1 < units.len() && units[i + 1] == u16::from(b'H') {
                // \033[<row>;<col>H — cursor position (1-based)
                // Parse row and col from params before H
                // We handle single-digit positions (enough for the snake game's 20x30)
                let ch = char::from_u32(u32::from(units[i])).unwrap_or(' ');
                let ch2 = char::from_u32(u32::from(units[i + 1])).unwrap_or(' ');
                if ch.is_ascii_digit() && ch2 == 'H' {
                    let row = ch.to_digit(10).unwrap_or(1).saturating_sub(1) as i16;
                    buffer.cursor.y = row.max(0).min(height.saturating_sub(1));
                    buffer.cursor.x = 0;
                    i += 2;
                    continue;
                }
            }
            // Unknown escape — skip to the final byte (letter).
            while i < units.len() {
                let c = char::from_u32(u32::from(units[i])).unwrap_or(' ');
                if c.is_ascii_uppercase() || c.is_ascii_lowercase() {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Regular character or control code.
        match unit {
            0x0A => {
                buffer.cursor.x = 0;
                buffer.cursor.y = buffer.cursor.y.saturating_add(1);
            }
            0x0D => buffer.cursor.x = 0,
            0x08 => buffer.cursor.x = buffer.cursor.x.saturating_sub(1),
            0x09 => {
                let next_stop = buffer.cursor.x.saturating_add(8) & !7;
                buffer.cursor.x = next_stop.min(width.saturating_sub(1));
            }
            _ => {
                if let Some(index) = buffer.index_of(buffer.cursor.x, buffer.cursor.y)
                    && let Some(cell) = buffer.cells.get_mut(index)
                {
                    *cell = console::CharInfo { unit, attributes };
                }
                buffer.cursor.x = buffer.cursor.x.saturating_add(1);
                if buffer.cursor.x >= width {
                    buffer.cursor.x = 0;
                    buffer.cursor.y = buffer.cursor.y.saturating_add(1);
                }
            }
        }
        if buffer.cursor.y >= height {
            buffer.cursor.y = height.saturating_sub(1);
            buffer.scroll_up();
        }
        i += 1;
    }
}

pub fn handle_write_console_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    write_console(ctx, true)
}

pub fn handle_write_console_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    write_console(ctx, false)
}

/// Buffer CRT `fputs` output through the console module so it flushes
/// atomically on `Sleep` (or `_getch`), rather than writing directly to
/// the host fd. This eliminates flicker from per-write rendering.
pub fn emit_text_from_bytes(ctx: &mut HandlerContext<'_>, bytes: &[u8]) {
    let console = ctx.state.console();
    console.stream_buf.extend_from_slice(bytes);
    console.needs_flush = true;
}

/// Shared body of `ReadConsoleW` / `ReadConsoleA`.
///
/// Win64 ABI: `hConsoleInput`, `lpBuffer`, `nNumberOfCharsToRead`,
/// `lpNumberOfCharsRead`, `pInputControl` (stack).
///
/// Only line input is implemented here; character-at-a-time reads arrive with
/// Tier 2's `ReadConsoleInput`, which is the API programs actually use once
/// they clear `ENABLE_LINE_INPUT`.
fn read_console(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let api = if wide { "ReadConsoleW" } else { "ReadConsoleA" };
    let handle = ctx.engine.read_rcx().context("ReadConsole RCX")?;
    let buffer_va = ctx.engine.read_rdx().context("ReadConsole RDX")?;
    let capacity = low_u32(
        ctx.engine.read_r8().context("ReadConsole R8")?,
        "ReadConsole capacity",
    )?;
    let read_va = ctx.engine.read_r9().context("ReadConsole R9")?;

    if handle != FAKE_STDIN_HANDLE {
        return ret_invalid_handle(ctx, api);
    }
    let capacity_usize = usize::try_from(capacity).unwrap_or(0);
    if buffer_va == 0 || capacity_usize == 0 {
        if read_va != 0 {
            write_guest_u32(ctx.engine, read_va, 0)?;
        }
        return ret_bool_true(ctx.engine, api);
    }

    // Reuse the existing stdin buffer so ReadConsole and ReadFile on the same
    // handle stay in step — a program may mix them across its lifetime.
    if ctx.state.file_io.stdin_cursor >= ctx.state.file_io.stdin_bytes.len()
        && refill_stdin_from_host(ctx.state).is_err()
    {
        ctx.state.process.last_error = super::ERROR_READ_FAULT;
        return ret_u64(ctx.engine, 0, api);
    }

    let cursor = ctx.state.file_io.stdin_cursor;
    let available = ctx.state.file_io.stdin_bytes.get(cursor..).unwrap_or(&[]);
    let take = available.len().min(capacity_usize);
    let chunk: Vec<u8> = available.get(..take).unwrap_or(&[]).to_vec();
    ctx.state.file_io.stdin_cursor = cursor.saturating_add(take);

    let written = if wide {
        let units = codepage::decode_to_units(ctx.state.console().input_code_page, &chunk);
        let units = units.get(..units.len().min(capacity_usize)).unwrap_or(&[]);
        let mut bytes = Vec::with_capacity(units.len().saturating_mul(2));
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        ctx.engine
            .mem_write(buffer_va, &bytes)
            .context("ReadConsoleW guest buffer")?;
        u32::try_from(units.len()).unwrap_or(0)
    } else {
        ctx.engine
            .mem_write(buffer_va, &chunk)
            .context("ReadConsoleA guest buffer")?;
        u32::try_from(chunk.len()).unwrap_or(0)
    };

    if read_va != 0 {
        write_guest_u32(ctx.engine, read_va, written)?;
    }
    ret_bool_true(ctx.engine, api)
}

pub fn handle_read_console_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    read_console(ctx, true)
}

pub fn handle_read_console_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    read_console(ctx, false)
}

/// Code pages the console accepts. Anything else fails, as on Windows without
/// the matching NLS data installed.
fn is_supported_code_page(code_page: u32) -> bool {
    matches!(code_page, CP_OEM_437 | CP_WINDOWS_1252 | CP_UTF8)
}

pub fn handle_get_console_cp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let value = u64::from(ctx.state.console().input_code_page);
    ret_u64(ctx.engine, value, "GetConsoleCP")
}

pub fn handle_get_console_output_cp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let value = u64::from(ctx.state.console().output_code_page);
    ret_u64(ctx.engine, value, "GetConsoleOutputCP")
}

pub fn handle_set_console_cp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let code_page = low_u32(
        ctx.engine.read_rcx().context("SetConsoleCP RCX")?,
        "SetConsoleCP",
    )?;
    if !is_supported_code_page(code_page) {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, "SetConsoleCP");
    }
    ctx.state.console().input_code_page = code_page;
    ret_bool_true(ctx.engine, "SetConsoleCP")
}

pub fn handle_set_console_output_cp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let code_page = low_u32(
        ctx.engine.read_rcx().context("SetConsoleOutputCP RCX")?,
        "SetConsoleOutputCP",
    )?;
    if !is_supported_code_page(code_page) {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, "SetConsoleOutputCP");
    }
    ctx.state.console().output_code_page = code_page;
    ret_bool_true(ctx.engine, "SetConsoleOutputCP")
}

/// `SetConsoleTitleA` / `SetConsoleTitleW`.
///
/// Forwarded to the terminal as `OSC 2` so the window title actually changes,
/// which is what the guest asked for.
fn set_console_title(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "SetConsoleTitleW"
    } else {
        "SetConsoleTitleA"
    };
    let title_va = ctx.engine.read_rcx().context("SetConsoleTitle RCX")?;
    let title = if title_va == 0 {
        String::new()
    } else if wide {
        read_guest_utf16_lossy(ctx.engine, title_va, 1024)?
    } else {
        let bytes = read_ansi_bytes(ctx.engine, title_va, 1024)?;
        let code_page = ctx.state.console().output_code_page;
        codepage::units_to_host_utf8(&codepage::decode_to_units(code_page, &bytes))
    };

    // Strip control characters: an OSC string is terminated by BEL, so a title
    // containing one would leave the rest of the sequence on screen as text.
    let sanitised: String = title.chars().filter(|ch| !ch.is_control()).collect();
    if host_term::is_tty() {
        host_term::write_stdout(format!("\u{1b}]2;{sanitised}\u{7}").as_bytes());
    }
    ctx.state.console().title = title;
    ret_bool_true(ctx.engine, api)
}

pub fn handle_set_console_title_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    set_console_title(ctx, true)
}

pub fn handle_set_console_title_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    set_console_title(ctx, false)
}

/// `GetConsoleTitleA` / `GetConsoleTitleW` — returns the length in characters,
/// excluding the terminator, and 0 on failure.
fn get_console_title(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "GetConsoleTitleW"
    } else {
        "GetConsoleTitleA"
    };
    let buffer_va = ctx.engine.read_rcx().context("GetConsoleTitle RCX")?;
    let capacity = low_u32(
        ctx.engine.read_rdx().context("GetConsoleTitle RDX")?,
        "GetConsoleTitle size",
    )?;
    let capacity_usize = usize::try_from(capacity).unwrap_or(0);
    if buffer_va == 0 || capacity_usize == 0 {
        return ret_u64(ctx.engine, 0, api);
    }

    let title = ctx.state.console().title.clone();
    let written = if wide {
        let mut units: Vec<u16> = title.encode_utf16().collect();
        units.truncate(capacity_usize.saturating_sub(1));
        let count = units.len();
        units.push(0);
        let mut bytes = Vec::with_capacity(units.len().saturating_mul(2));
        for unit in &units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        ctx.engine
            .mem_write(buffer_va, &bytes)
            .context("GetConsoleTitleW write")?;
        count
    } else {
        let code_page = ctx.state.console().output_code_page;
        let units: Vec<u16> = title.encode_utf16().collect();
        let mut bytes = codepage::encode_from_units(code_page, &units);
        bytes.truncate(capacity_usize.saturating_sub(1));
        let count = bytes.len();
        bytes.push(0);
        ctx.engine
            .mem_write(buffer_va, &bytes)
            .context("GetConsoleTitleA write")?;
        count
    };
    ret_u64(ctx.engine, u64::try_from(written).unwrap_or(0), api)
}

pub fn handle_get_console_title_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    get_console_title(ctx, true)
}

pub fn handle_get_console_title_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    get_console_title(ctx, false)
}

/// `AllocConsole` — WIE always has a console, so this succeeds without work.
pub fn handle_alloc_console(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ret_bool_true(ctx.engine, "AllocConsole")
}

/// `FreeConsole` — detaching would orphan the terminal WIE itself runs on, so
/// this reports success and keeps the console attached.
pub fn handle_free_console(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ret_bool_true(ctx.engine, "FreeConsole")
}

/// `AttachConsole` — succeeds for `ATTACH_PARENT_PROCESS` and the emulated PID,
/// which is the only console that exists.
pub fn handle_attach_console(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _pid = ctx.engine.read_rcx().context("AttachConsole RCX")?;
    ret_bool_true(ctx.engine, "AttachConsole")
}

/// `GetConsoleWindow` — a non-NULL pseudo-`HWND`.
///
/// Returning NULL would tell the guest it has no console at all, which sends
/// some programs down a "reattach or bail" path.
pub fn handle_get_console_window(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ret_u64(ctx.engine, FAKE_CONSOLE_HWND, "GetConsoleWindow")
}

/// `GetLargestConsoleWindowSize` — returns a packed `COORD` in EAX.
pub fn handle_get_largest_console_window_size(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _handle = ctx
        .engine
        .read_rcx()
        .context("GetLargestConsoleWindowSize RCX")?;
    let (columns, rows) = host_term::window_size();
    let packed = console::Coord::new(
        i16::try_from(columns).unwrap_or(i16::MAX),
        i16::try_from(rows).unwrap_or(i16::MAX),
    )
    .to_packed();
    ret_u64(ctx.engine, u64::from(packed), "GetLargestConsoleWindowSize")
}

/// `GetNumberOfConsoleMouseButtons`.
pub fn handle_get_number_of_console_mouse_buttons(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let count_va = ctx
        .engine
        .read_rcx()
        .context("GetNumberOfConsoleMouseButtons RCX")?;
    if count_va == 0 {
        return ret_invalid_handle(ctx, "GetNumberOfConsoleMouseButtons");
    }
    // Three: xterm-style mouse reporting distinguishes left, middle, right.
    write_guest_u32(ctx.engine, count_va, 3)?;
    ret_bool_true(ctx.engine, "GetNumberOfConsoleMouseButtons")
}

/// Dispatch console APIs that are cold enough not to warrant a dense id.
pub fn dispatch_console_extra(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let result = match name {
        "getconsolecp" => handle_get_console_cp(ctx)?,
        "getconsoleoutputcp" => handle_get_console_output_cp(ctx)?,
        "setconsolecp" => handle_set_console_cp(ctx)?,
        "setconsoleoutputcp" => handle_set_console_output_cp(ctx)?,
        "setconsoletitlew" => handle_set_console_title_w(ctx)?,
        "setconsoletitlea" => handle_set_console_title_a(ctx)?,
        "getconsoletitlew" => handle_get_console_title_w(ctx)?,
        "getconsoletitlea" => handle_get_console_title_a(ctx)?,
        "allocconsole" => handle_alloc_console(ctx)?,
        "freeconsole" => handle_free_console(ctx)?,
        "attachconsole" => handle_attach_console(ctx)?,
        "getconsolewindow" => handle_get_console_window(ctx)?,
        "getlargestconsolewindowsize" => handle_get_largest_console_window_size(ctx)?,
        "getnumberofconsolemousebuttons" => handle_get_number_of_console_mouse_buttons(ctx)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::console::{
        CharInfo, ConsoleState, DEFAULT_OUTPUT_MODE, PRIMARY_BUFFER_HANDLE, ScreenBuffer,
    };

    fn test_state() -> ConsoleState {
        let mut state = ConsoleState {
            buffers: vec![(PRIMARY_BUFFER_HANDLE, ScreenBuffer::new(10, 5))],
            output_modes: vec![(PRIMARY_BUFFER_HANDLE, DEFAULT_OUTPUT_MODE)],
            ..ConsoleState::default()
        };
        // Set a known attribute so we can verify it propagates.
        state.buffer_mut(PRIMARY_BUFFER_HANDLE).unwrap().attributes = 0x1F; // white on blue
        state
    }

    fn cell(state: &ConsoleState, x: i16, y: i16) -> CharInfo {
        state
            .buffer(PRIMARY_BUFFER_HANDLE)
            .and_then(|b| b.index_of(x, y).and_then(|i| b.cells.get(i)))
            .copied()
            .unwrap_or(CharInfo {
                unit: 0,
                attributes: 0,
            })
    }

    #[test]
    fn plain_text_writes_at_cursor() {
        let mut state = test_state();
        fold_text_into_grid(
            &mut state,
            PRIMARY_BUFFER_HANDLE,
            &[u16::from(b'A'), u16::from(b'B'), u16::from(b'C')],
        );
        assert_eq!(cell(&state, 0, 0).unit, u16::from(b'A'));
        assert_eq!(cell(&state, 1, 0).unit, u16::from(b'B'));
        assert_eq!(cell(&state, 2, 0).unit, u16::from(b'C'));
        // Cursor advanced
        assert_eq!(state.buffer(PRIMARY_BUFFER_HANDLE).unwrap().cursor.x, 3);
    }

    #[test]
    fn newline_moves_cursor_to_next_row() {
        let mut state = test_state();
        fold_text_into_grid(&mut state, PRIMARY_BUFFER_HANDLE, &[0x0A]);
        let cursor = state.buffer(PRIMARY_BUFFER_HANDLE).unwrap().cursor;
        assert_eq!(cursor.x, 0);
        assert_eq!(cursor.y, 1);
    }

    #[test]
    fn carriage_return_moves_to_column_zero() {
        let mut state = test_state();
        // Move cursor right first, then CR
        fold_text_into_grid(&mut state, PRIMARY_BUFFER_HANDLE, &[u16::from(b'X')]);
        fold_text_into_grid(&mut state, PRIMARY_BUFFER_HANDLE, &[0x0D]);
        let cursor = state.buffer(PRIMARY_BUFFER_HANDLE).unwrap().cursor;
        assert_eq!(cursor.x, 0);
        assert_eq!(cursor.y, 0);
    }

    #[test]
    fn backspace_moves_cursor_left() {
        let mut state = test_state();
        fold_text_into_grid(&mut state, PRIMARY_BUFFER_HANDLE, &[u16::from(b'A'), 0x08]);
        let cursor = state.buffer(PRIMARY_BUFFER_HANDLE).unwrap().cursor;
        // After writing A at (0,0), cursor is at 1. After BS, at 0.
        assert_eq!(cursor.x, 0);
    }

    #[test]
    fn tab_advances_to_next_stop() {
        let mut state = test_state();
        fold_text_into_grid(&mut state, PRIMARY_BUFFER_HANDLE, &[0x09]);
        let cursor = state.buffer(PRIMARY_BUFFER_HANDLE).unwrap().cursor;
        // Next tab stop is column 8
        assert_eq!(cursor.x, 8);
    }

    #[test]
    fn escape_2j_clears_entire_screen() {
        let mut state = test_state();
        // Write some content first.
        for cell in state
            .buffer_mut(PRIMARY_BUFFER_HANDLE)
            .unwrap()
            .cells
            .iter_mut()
            .take(10)
        {
            cell.unit = u16::from(b'X');
        }
        fold_text_into_grid(
            &mut state,
            PRIMARY_BUFFER_HANDLE,
            &[0x1B, u16::from(b'['), u16::from(b'2'), u16::from(b'J')],
        );
        // All cells should be spaces now.
        let buf = state.buffer(PRIMARY_BUFFER_HANDLE).unwrap();
        assert!(buf.cells.iter().all(|c| c.unit == u16::from(b' ')));
        // The buffer's attribute should be used for the cleared cells.
        assert!(buf.cells.iter().all(|c| c.attributes == 0x1F));
    }

    #[test]
    fn escape_h_moves_cursor_home() {
        let mut state = test_state();
        // Move cursor to (5, 3)
        state.buffer_mut(PRIMARY_BUFFER_HANDLE).unwrap().cursor.x = 5;
        state.buffer_mut(PRIMARY_BUFFER_HANDLE).unwrap().cursor.y = 3;
        fold_text_into_grid(
            &mut state,
            PRIMARY_BUFFER_HANDLE,
            &[0x1B, u16::from(b'['), u16::from(b'H')],
        );
        let cursor = state.buffer(PRIMARY_BUFFER_HANDLE).unwrap().cursor;
        assert_eq!(cursor.x, 0);
        assert_eq!(cursor.y, 0);
    }

    #[test]
    fn escape_single_digit_row_h_moves_cursor_to_row() {
        let mut state = test_state();
        // \033[2H — row 2 (1-based -> y=1), column stays at 0
        fold_text_into_grid(
            &mut state,
            PRIMARY_BUFFER_HANDLE,
            &[0x1B, u16::from(b'['), u16::from(b'2'), u16::from(b'H')],
        );
        let cursor = state.buffer(PRIMARY_BUFFER_HANDLE).unwrap().cursor;
        assert_eq!(cursor.y, 1); // row 2 (1-based) = y=1
        assert_eq!(cursor.x, 0);
    }

    #[test]
    fn text_wrapping_at_eol_moves_to_next_row() {
        let mut state = test_state();
        // Buffer is 10 wide. Write 10 chars to fill row 0.
        let input: Vec<u16> = (0..10_u16).map(|i| u16::from(b'A') + i).collect();
        fold_text_into_grid(&mut state, PRIMARY_BUFFER_HANDLE, &input);
        // After writing char at column 9, cursor.x is 10, which triggers wrap
        let cursor = state.buffer(PRIMARY_BUFFER_HANDLE).unwrap().cursor;
        assert_eq!(cursor.x, 0);
        assert_eq!(cursor.y, 1);
    }

    #[test]
    fn unknown_escape_sequence_is_skipped() {
        let mut state = test_state();
        // Some made-up escape that we don't parse.
        fold_text_into_grid(
            &mut state,
            PRIMARY_BUFFER_HANDLE,
            &[0x1B, u16::from(b'['), u16::from(b'z')],
        );
        // Nothing written to grid; cursor unchanged.
        let cursor = state.buffer(PRIMARY_BUFFER_HANDLE).unwrap().cursor;
        assert_eq!(cursor.x, 0);
        assert_eq!(cursor.y, 0);
    }

    #[test]
    fn regular_characters_use_the_buffer_attribute() {
        let mut state = test_state();
        fold_text_into_grid(&mut state, PRIMARY_BUFFER_HANDLE, &[u16::from(b'Z')]);
        let c = cell(&state, 0, 0);
        assert_eq!(c.attributes, 0x1F);
    }
}
