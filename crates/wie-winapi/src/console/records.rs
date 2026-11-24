//! Console input APIs: `ReadConsoleInput`, `PeekConsoleInput`, and friends.
//!
//! The decoding itself lives in [`crate::console::input`]; this module is the
//! ABI boundary — reading arguments, draining the queue, and writing
//! `INPUT_RECORD`s into guest memory.
//!
//! Used to be `kernel32::console_input`. `DllId::Console`'s owner is
//! [`crate::console`], so the callers moved here with it (ADR-0005), and each
//! entry point takes a [`ConsoleCall`]: the compiler refuses any reach outside
//! the console capability.

use anyhow::Context;

use crate::capability::ConsoleCall;
use crate::console::{
    InputRecord,
    pump::{self, INPUT_RECORD_SIZE},
};
use crate::kernel32::{
    Result, WinApiHandlerResult, low_u32, ret_bool_true, ret_u64, write_guest_u32,
};

/// `ERROR_INVALID_HANDLE`.
const ERROR_INVALID_HANDLE: u32 = 6;

/// Cap on records serviced by one call, so a huge `nLength` cannot allocate
/// without bound. Callers loop anyway.
const MAX_RECORDS_PER_CALL: usize = 8192;

fn ret_invalid_handle(cx: &mut ConsoleCall<'_>, api: &str) -> Result<WinApiHandlerResult> {
    cx.state.set_process_last_error(ERROR_INVALID_HANDLE);
    ret_u64(cx.engine, 0, api)
}

/// Shared body of `ReadConsoleInputW` / `ReadConsoleInputA` and the `Peek`
/// variants.
///
/// ABI: `HANDLE`, `PINPUT_RECORD lpBuffer`, `DWORD nLength`,
/// `LPDWORD lpNumberOfEventsRead`.
///
/// `Read` removes the records it returns; `Peek` leaves them queued. Both block
/// only in the `Read` case, and only when the queue is empty — that is the
/// documented difference and what a poll loop depends on.
fn read_or_peek(cx: &mut ConsoleCall<'_>, api: &str, consume: bool) -> Result<WinApiHandlerResult> {
    let handle = cx.engine.read_rcx().context("ReadConsoleInput RCX")?;
    let buffer_va = cx.engine.read_rdx().context("ReadConsoleInput RDX")?;
    let length = low_u32(
        cx.engine.read_r8().context("ReadConsoleInput R8")?,
        "ReadConsoleInput length",
    )?;
    let count_va = cx.engine.read_r9().context("ReadConsoleInput R9")?;

    if handle != crate::kernel32::FAKE_STDIN_HANDLE {
        return ret_invalid_handle(cx, api);
    }
    pump::ensure_input_ready(cx.state.console());

    let capacity = usize::try_from(length)
        .unwrap_or(0)
        .min(MAX_RECORDS_PER_CALL);
    if buffer_va == 0 || capacity == 0 {
        cx.state
            .set_process_last_error(crate::kernel32::ERROR_INVALID_PARAMETER);
        return ret_u64(cx.engine, 0, api);
    }

    // ReadConsoleInput blocks until at least one record exists; Peek returns
    // immediately with whatever is already queued.
    if cx.state.console().pending_input.is_empty() {
        let timeout = if consume { -1 } else { 0 };
        let added = pump::pump(cx.state.console(), timeout);
        if added == 0 && consume {
            // No terminal to wait on (piped stdin): report zero events rather
            // than spin forever.
            if count_va != 0 {
                write_guest_u32(cx.engine, count_va, 0)?;
            }
            return ret_bool_true(cx.engine, api);
        }
    }

    let take = capacity.min(cx.state.console().pending_input.len());
    let mut bytes = Vec::with_capacity(take.saturating_mul(INPUT_RECORD_SIZE));
    for index in 0..take {
        let record = if consume {
            cx.state.console().pending_input.pop_front()
        } else {
            cx.state.console().pending_input.get(index).copied()
        };
        let Some(record) = record else {
            break;
        };
        bytes.extend_from_slice(&pump::encode_record(record));
    }

    let written = bytes.len() / INPUT_RECORD_SIZE;
    if !bytes.is_empty() {
        cx.engine
            .mem_write(buffer_va, &bytes)
            .context("ReadConsoleInput guest buffer")?;
    }
    if count_va != 0 {
        write_guest_u32(cx.engine, count_va, u32::try_from(written).unwrap_or(0))?;
    }
    ret_bool_true(cx.engine, api)
}

pub fn handle_read_console_input_w(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    read_or_peek(cx, "ReadConsoleInputW", true)
}

pub fn handle_read_console_input_a(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    read_or_peek(cx, "ReadConsoleInputA", true)
}

pub fn handle_peek_console_input_w(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    read_or_peek(cx, "PeekConsoleInputW", false)
}

pub fn handle_peek_console_input_a(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    read_or_peek(cx, "PeekConsoleInputA", false)
}

/// `GetNumberOfConsoleInputEvents(HANDLE, LPDWORD)`.
///
/// Pumps without blocking first, so a guest polling this in a frame loop sees
/// keys arrive rather than a permanent zero.
pub fn handle_get_number_of_console_input_events(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = cx
        .engine
        .read_rcx()
        .context("GetNumberOfConsoleInputEvents RCX")?;
    let count_va = cx
        .engine
        .read_rdx()
        .context("GetNumberOfConsoleInputEvents RDX")?;
    if handle != crate::kernel32::FAKE_STDIN_HANDLE || count_va == 0 {
        return ret_invalid_handle(cx, "GetNumberOfConsoleInputEvents");
    }
    pump::ensure_input_ready(cx.state.console());
    let _ = pump::pump(cx.state.console(), 0);
    let count = u32::try_from(cx.state.console().pending_input.len()).unwrap_or(u32::MAX);
    write_guest_u32(cx.engine, count_va, count)?;
    ret_bool_true(cx.engine, "GetNumberOfConsoleInputEvents")
}

/// `FlushConsoleInputBuffer(HANDLE)`.
pub fn handle_flush_console_input_buffer(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    let handle = cx
        .engine
        .read_rcx()
        .context("FlushConsoleInputBuffer RCX")?;
    if handle != crate::kernel32::FAKE_STDIN_HANDLE {
        return ret_invalid_handle(cx, "FlushConsoleInputBuffer");
    }
    // Drain the host side too: bytes already read but not yet decoded would
    // otherwise reappear as records immediately after the flush.
    let _ = pump::pump(cx.state.console(), 0);
    cx.state.console().pending_input.clear();
    cx.state.console().input_bytes.clear();
    ret_bool_true(cx.engine, "FlushConsoleInputBuffer")
}

/// Dispatch the console input surface by name.
pub fn dispatch_console_input(
    cx: &mut ConsoleCall<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let result = match name {
        "readconsoleinputw" => handle_read_console_input_w(cx)?,
        "readconsoleinputa" => handle_read_console_input_a(cx)?,
        "peekconsoleinputw" => handle_peek_console_input_w(cx)?,
        "peekconsoleinputa" => handle_peek_console_input_a(cx)?,
        "getnumberofconsoleinputevents" => handle_get_number_of_console_input_events(cx)?,
        "flushconsoleinputbuffer" => handle_flush_console_input_buffer(cx)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

/// Take one key press for the `conio` family (`_getch`, `_getche`).
///
/// Returns the character, or `None` when no terminal is attached.
pub fn next_conio_char(cx: &mut ConsoleCall<'_>, block: bool) -> Option<u16> {
    pump::ensure_input_ready(cx.state.console());
    let event = pump::next_key_press(cx.state.console(), block)?;
    if event.unit != 0 {
        return Some(event.unit);
    }
    // Extended keys (arrows, function keys) are reported by `_getch` as a
    // zero byte followed by the scan code on the next call. Queue the second
    // half so the guest's two-call sequence works.
    Some(0)
}

/// True when a key press is already queued.
pub fn conio_key_waiting(cx: &mut ConsoleCall<'_>) -> bool {
    pump::ensure_input_ready(cx.state.console());
    let _ = pump::pump(cx.state.console(), 0);
    cx.state
        .console()
        .pending_input
        .iter()
        .any(|record| matches!(record, InputRecord::Key(event) if event.key_down))
}
