//! Console input APIs: `ReadConsoleInput`, `PeekConsoleInput`, and friends.
//!
//! The decoding itself lives in [`crate::console::input`]; this module is the
//! ABI boundary — reading arguments, draining the queue, and writing
//! `INPUT_RECORD`s into guest memory.

use super::{
    Context, HandlerContext, Result, WinApiHandlerResult, low_u32, ret_bool_true, ret_u64,
    write_guest_u32,
};
use crate::console::{
    InputRecord,
    pump::{self, INPUT_RECORD_SIZE},
};

/// `ERROR_INVALID_HANDLE`.
const ERROR_INVALID_HANDLE: u32 = 6;

/// Cap on records serviced by one call, so a huge `nLength` cannot allocate
/// without bound. Callers loop anyway.
const MAX_RECORDS_PER_CALL: usize = 8192;

fn ret_invalid_handle(ctx: &mut HandlerContext<'_>, api: &str) -> Result<WinApiHandlerResult> {
    ctx.state.process.last_error = ERROR_INVALID_HANDLE;
    ret_u64(ctx.engine, 0, api)
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
fn read_or_peek(
    ctx: &mut HandlerContext<'_>,
    api: &str,
    consume: bool,
) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("ReadConsoleInput RCX")?;
    let buffer_va = ctx.engine.read_rdx().context("ReadConsoleInput RDX")?;
    let length = low_u32(
        ctx.engine.read_r8().context("ReadConsoleInput R8")?,
        "ReadConsoleInput length",
    )?;
    let count_va = ctx.engine.read_r9().context("ReadConsoleInput R9")?;

    if handle != super::FAKE_STDIN_HANDLE {
        return ret_invalid_handle(ctx, api);
    }
    pump::ensure_input_ready(ctx.state);

    let capacity = usize::try_from(length)
        .unwrap_or(0)
        .min(MAX_RECORDS_PER_CALL);
    if buffer_va == 0 || capacity == 0 {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, api);
    }

    // ReadConsoleInput blocks until at least one record exists; Peek returns
    // immediately with whatever is already queued.
    if ctx.state.console().pending_input.is_empty() {
        let timeout = if consume { -1 } else { 0 };
        let added = pump::pump(ctx.state, timeout);
        if added == 0 && consume {
            // No terminal to wait on (piped stdin): report zero events rather
            // than spin forever.
            if count_va != 0 {
                write_guest_u32(ctx.engine, count_va, 0)?;
            }
            return ret_bool_true(ctx.engine, api);
        }
    }

    let take = capacity.min(ctx.state.console().pending_input.len());
    let mut bytes = Vec::with_capacity(take.saturating_mul(INPUT_RECORD_SIZE));
    for index in 0..take {
        let record = if consume {
            ctx.state.console().pending_input.pop_front()
        } else {
            ctx.state.console().pending_input.get(index).copied()
        };
        let Some(record) = record else {
            break;
        };
        bytes.extend_from_slice(&pump::encode_record(record));
    }

    let written = bytes.len() / INPUT_RECORD_SIZE;
    if !bytes.is_empty() {
        ctx.engine
            .mem_write(buffer_va, &bytes)
            .context("ReadConsoleInput guest buffer")?;
    }
    if count_va != 0 {
        write_guest_u32(ctx.engine, count_va, u32::try_from(written).unwrap_or(0))?;
    }
    ret_bool_true(ctx.engine, api)
}

pub fn handle_read_console_input_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    read_or_peek(ctx, "ReadConsoleInputW", true)
}

pub fn handle_read_console_input_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    read_or_peek(ctx, "ReadConsoleInputA", true)
}

pub fn handle_peek_console_input_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    read_or_peek(ctx, "PeekConsoleInputW", false)
}

pub fn handle_peek_console_input_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    read_or_peek(ctx, "PeekConsoleInputA", false)
}

/// `GetNumberOfConsoleInputEvents(HANDLE, LPDWORD)`.
///
/// Pumps without blocking first, so a guest polling this in a frame loop sees
/// keys arrive rather than a permanent zero.
pub fn handle_get_number_of_console_input_events(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx
        .engine
        .read_rcx()
        .context("GetNumberOfConsoleInputEvents RCX")?;
    let count_va = ctx
        .engine
        .read_rdx()
        .context("GetNumberOfConsoleInputEvents RDX")?;
    if handle != super::FAKE_STDIN_HANDLE || count_va == 0 {
        return ret_invalid_handle(ctx, "GetNumberOfConsoleInputEvents");
    }
    pump::ensure_input_ready(ctx.state);
    let _ = pump::pump(ctx.state, 0);
    let count = u32::try_from(ctx.state.console().pending_input.len()).unwrap_or(u32::MAX);
    write_guest_u32(ctx.engine, count_va, count)?;
    ret_bool_true(ctx.engine, "GetNumberOfConsoleInputEvents")
}

/// `FlushConsoleInputBuffer(HANDLE)`.
pub fn handle_flush_console_input_buffer(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx
        .engine
        .read_rcx()
        .context("FlushConsoleInputBuffer RCX")?;
    if handle != super::FAKE_STDIN_HANDLE {
        return ret_invalid_handle(ctx, "FlushConsoleInputBuffer");
    }
    // Drain the host side too: bytes already read but not yet decoded would
    // otherwise reappear as records immediately after the flush.
    let _ = pump::pump(ctx.state, 0);
    ctx.state.console().pending_input.clear();
    ctx.state.console().input_bytes.clear();
    ret_bool_true(ctx.engine, "FlushConsoleInputBuffer")
}

/// Dispatch the console input surface by name.
pub fn dispatch_console_input(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let result = match name {
        "readconsoleinputw" => handle_read_console_input_w(ctx)?,
        "readconsoleinputa" => handle_read_console_input_a(ctx)?,
        "peekconsoleinputw" => handle_peek_console_input_w(ctx)?,
        "peekconsoleinputa" => handle_peek_console_input_a(ctx)?,
        "getnumberofconsoleinputevents" => handle_get_number_of_console_input_events(ctx)?,
        "flushconsoleinputbuffer" => handle_flush_console_input_buffer(ctx)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

/// Take one key press for the `conio` family (`_getch`, `_getche`).
///
/// Returns the character, or `None` when no terminal is attached.
#[expect(dead_code)]
pub(crate) fn next_conio_char(ctx: &mut HandlerContext<'_>, block: bool) -> Option<u16> {
    pump::ensure_input_ready(ctx.state);
    let event = pump::next_key_press(ctx.state, block)?;
    if event.unit != 0 {
        return Some(event.unit);
    }
    // Extended keys (arrows, function keys) are reported by `_getch` as a
    // zero byte followed by the scan code on the next call. Queue the second
    // half so the guest's two-call sequence works.
    Some(0)
}

/// True when a key press is already queued.
#[expect(dead_code)]
pub(crate) fn conio_key_waiting(ctx: &mut HandlerContext<'_>) -> bool {
    pump::ensure_input_ready(ctx.state);
    let _ = pump::pump(ctx.state, 0);
    ctx.state
        .console()
        .pending_input
        .iter()
        .any(|record| matches!(record, InputRecord::Key(event) if event.key_down))
}
