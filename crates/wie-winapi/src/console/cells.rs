//! Console cell APIs: the framebuffer surface a text-mode game draws through.
//!
//! Every entry point here writes into a [`ScreenBuffer`] and then asks
//! [`crate::console::screen::flush`] to reconcile the terminal with it. Nothing
//! writes escape sequences directly, so the diff always sees a truthful "before"
//! and cannot be desynchronised by a partial paint.
//!
//! Used to be `kernel32::console_cells`: `DllId::Console`'s owner is
//! [`crate::console`], so its callers live here too (ADR-0005). Each entry
//! point takes a [`ConsoleCall`], so the compiler refuses any reach outside the
//! console capability.

use anyhow::Context;

use crate::capability::{ConsoleAccess, ConsoleCall};
use crate::console::{
    CharInfo, Coord, RenderMode, ScreenBuffer, SmallRect, codepage, host_term, screen,
};
use crate::guest_memory::{read_bytes as read_guest_bytes, read_u16};
use crate::kernel32::{
    Result, WinApiHandlerResult, low_u32, ret_bool_true, ret_u64, write_guest_u32,
};

/// `ERROR_INVALID_HANDLE`.
const ERROR_INVALID_HANDLE: u32 = 6;

/// Bytes per `CHAR_INFO`: a `WCHAR` union followed by a `WORD` of attributes.
const CHAR_INFO_SIZE: usize = 4;

/// Cap on a single cell operation, so a bogus length cannot allocate wildly.
const MAX_CELLS: usize = 4 * 1024 * 1024;

/// Read a stack-passed argument.
///
/// The Win64 ABI puts the 5th argument at `RSP + 0x28` on entry: 0x20 of shadow
/// space plus the 8-byte return address.
fn stack_arg(cx: &mut ConsoleCall<'_>, index: usize, api: &str) -> Result<u64> {
    let rsp = cx.engine.read_rsp().with_context(|| format!("{api} RSP"))?;
    let offset = 0x28_u64.saturating_add((u64::try_from(index).unwrap_or(0)).saturating_mul(8));
    let address = crate::kernel32::checked_address(rsp, offset, api);
    crate::kernel32::read_u64(cx.engine, address)
}

/// Resolve an output handle to a screen-buffer handle.
///
/// Read-only on purpose: a `WriteConsoleOutput*` for a handle that is not a
/// console must not allocate the console slot as a side effect of answering
/// "not mine".
fn buffer_handle_for(access: &dyn ConsoleAccess, handle: u64) -> Option<u64> {
    access
        .console_if_loaded()
        .and_then(|console| super::api::buffer_handle_for(console, handle))
}

/// Switch into Cells mode the first time a cell API is used.
///
/// The alternate screen goes with it: a program painting whole frames should
/// not shred the user's scrollback, and their shell prompt should return intact.
fn enter_cells_mode(cx: &mut ConsoleCall<'_>) {
    if cx.state.console().render_mode == RenderMode::Cells {
        return;
    }
    cx.state.console().render_mode = RenderMode::Cells;
    cx.state.console().rendered = None;
    screen::enter_alternate_screen();
}

/// Repaint the terminal from the active buffer.
///
/// A write to a non-displayed buffer changes nothing on screen, which is
/// exactly the property double buffering relies on.
fn flush_active(cx: &mut ConsoleCall<'_>) {
    if cx.state.console().render_mode != RenderMode::Cells || !host_term::is_tty() {
        return;
    }
    let active = cx.state.console().active_buffer;
    let Some(buffer) = cx.state.console().buffer(active).cloned() else {
        return;
    };
    let previous = cx.state.console().rendered.take();
    let output = screen::flush(&buffer, previous.as_ref());
    if !output.is_empty() {
        host_term::write_stdout(output.as_bytes());
    }
    cx.state.console().rendered = Some(buffer);
    cx.state.console().repaint_forced = false;
}

fn ret_invalid_handle(cx: &mut ConsoleCall<'_>, api: &str) -> Result<WinApiHandlerResult> {
    cx.state.set_process_last_error(ERROR_INVALID_HANDLE);
    ret_u64(cx.engine, 0, api)
}

/// `SetConsoleCursorPosition(HANDLE, COORD)` — the COORD arrives packed in RDX.
pub fn handle_set_console_cursor_position(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    let handle = cx
        .engine
        .read_rcx()
        .context("SetConsoleCursorPosition RCX")?;
    let position = Coord::from_packed(
        cx.engine
            .read_rdx()
            .context("SetConsoleCursorPosition RDX")?,
    );
    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, "SetConsoleCursorPosition");
    };
    let in_bounds = cx
        .state
        .console()
        .buffer(buffer_handle)
        .is_some_and(|buffer| buffer.index_of(position.x, position.y).is_some());
    if !in_bounds {
        cx.state
            .set_process_last_error(crate::kernel32::ERROR_INVALID_PARAMETER);
        return ret_u64(cx.engine, 0, "SetConsoleCursorPosition");
    }
    if let Some(buffer) = cx.state.console().buffer_mut(buffer_handle) {
        buffer.cursor = position;
    }
    // Stream mode has no grid to repaint, so move the real cursor now.
    if cx.state.console().render_mode == RenderMode::Stream {
        screen::move_cursor(position.x, position.y);
    } else {
        flush_active(cx);
    }
    ret_bool_true(cx.engine, "SetConsoleCursorPosition")
}

/// `SetConsoleTextAttribute(HANDLE, WORD)`.
pub fn handle_set_console_text_attribute(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    let handle = cx
        .engine
        .read_rcx()
        .context("SetConsoleTextAttribute RCX")?;
    let attributes = low_u32(
        cx.engine
            .read_rdx()
            .context("SetConsoleTextAttribute RDX")?,
        "SetConsoleTextAttribute",
    )?;
    let attributes = u16::try_from(attributes & 0xFFFF).unwrap_or(0);
    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, "SetConsoleTextAttribute");
    };
    if let Some(buffer) = cx.state.console().buffer_mut(buffer_handle) {
        buffer.attributes = attributes;
    }
    if cx.state.console().render_mode == RenderMode::Stream {
        screen::apply_attributes(attributes);
    }
    ret_bool_true(cx.engine, "SetConsoleTextAttribute")
}

/// `GetConsoleCursorInfo(HANDLE, PCONSOLE_CURSOR_INFO)`.
///
/// `CONSOLE_CURSOR_INFO` is `{ DWORD dwSize; BOOL bVisible; }`.
pub fn handle_get_console_cursor_info(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    let handle = cx.engine.read_rcx().context("GetConsoleCursorInfo RCX")?;
    let info_va = cx.engine.read_rdx().context("GetConsoleCursorInfo RDX")?;
    if info_va == 0 {
        return ret_invalid_handle(cx, "GetConsoleCursorInfo");
    }
    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, "GetConsoleCursorInfo");
    };
    let (size, visible) = cx
        .state
        .console()
        .buffer(buffer_handle)
        .map_or((25, 1), |buffer| {
            (buffer.cursor_size, u32::from(buffer.cursor_visible))
        });
    write_guest_u32(cx.engine, info_va, size)?;
    let visible_va = crate::kernel32::checked_address(info_va, 4, "GetConsoleCursorInfo bVisible");
    write_guest_u32(cx.engine, visible_va, visible)?;
    ret_bool_true(cx.engine, "GetConsoleCursorInfo")
}

/// `SetConsoleCursorInfo(HANDLE, const CONSOLE_CURSOR_INFO*)`.
pub fn handle_set_console_cursor_info(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    let handle = cx.engine.read_rcx().context("SetConsoleCursorInfo RCX")?;
    let info_va = cx.engine.read_rdx().context("SetConsoleCursorInfo RDX")?;
    if info_va == 0 {
        return ret_invalid_handle(cx, "SetConsoleCursorInfo");
    }
    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, "SetConsoleCursorInfo");
    };
    let size = crate::kernel32::read_u32(cx.engine, info_va)?;
    let visible_va = crate::kernel32::checked_address(info_va, 4, "SetConsoleCursorInfo bVisible");
    let visible = crate::kernel32::read_u32(cx.engine, visible_va)? != 0;
    if let Some(buffer) = cx.state.console().buffer_mut(buffer_handle) {
        buffer.cursor_size = size;
        buffer.cursor_visible = visible;
    }
    // Only the displayed buffer's caret is the one the user sees.
    if buffer_handle == cx.state.console().active_buffer {
        screen::set_cursor_visible(visible);
    }
    ret_bool_true(cx.engine, "SetConsoleCursorInfo")
}

/// Shared body of the `FillConsoleOutput*` family.
///
/// ABI: `HANDLE`, the fill value, `DWORD nLength`, `COORD dwWriteCoord`
/// (packed in R9), `LPDWORD lpNumberOfCellsWritten` (stack).
///
/// Fills run in reading order and wrap to the next row, which is what makes the
/// "clear the screen" idiom — one call with the whole cell count — work.
fn fill_console_output(
    cx: &mut ConsoleCall<'_>,
    api: &str,
    fill: FillKind,
) -> Result<WinApiHandlerResult> {
    let handle = cx.engine.read_rcx().context("FillConsoleOutput RCX")?;
    let raw_value = cx.engine.read_rdx().context("FillConsoleOutput RDX")?;
    let length = low_u32(
        cx.engine.read_r8().context("FillConsoleOutput R8")?,
        "FillConsoleOutput length",
    )?;
    let start = Coord::from_packed(cx.engine.read_r9().context("FillConsoleOutput R9")?);
    let written_va = stack_arg(cx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, api);
    };
    enter_cells_mode(cx);

    let unit = match fill {
        FillKind::Wide => u16::try_from(raw_value & 0xFFFF).unwrap_or(u16::from(b' ')),
        FillKind::Ansi => {
            let byte = u8::try_from(raw_value & 0xFF).unwrap_or(b' ');
            let code_page = cx.state.console().output_code_page;
            codepage::decode_to_units(code_page, &[byte])
                .first()
                .copied()
                .unwrap_or(u16::from(b' '))
        }
        FillKind::Attribute => u16::try_from(raw_value & 0xFFFF).unwrap_or(0),
    };

    let mut cells_filled = 0_u32;
    if let Some(buffer) = cx.state.console().buffer_mut(buffer_handle) {
        let total = usize::try_from(length).unwrap_or(0).min(MAX_CELLS);
        let Some(origin) = buffer.index_of(start.x, start.y) else {
            cx.state
                .set_process_last_error(crate::kernel32::ERROR_INVALID_PARAMETER);
            return ret_u64(cx.engine, 0, api);
        };
        for step in 0..total {
            let Some(index) = origin.checked_add(step) else {
                break;
            };
            let Some(cell) = buffer.cells.get_mut(index) else {
                break;
            };
            match fill {
                FillKind::Attribute => cell.attributes = unit,
                FillKind::Wide | FillKind::Ansi => cell.unit = unit,
            }
            cells_filled = cells_filled.saturating_add(1);
        }
    }

    flush_active(cx);
    if written_va != 0 {
        write_guest_u32(cx.engine, written_va, cells_filled)?;
    }
    ret_bool_true(cx.engine, api)
}

/// Which field a `FillConsoleOutput*` call writes.
#[derive(Debug, Clone, Copy)]
enum FillKind {
    Wide,
    Ansi,
    Attribute,
}

pub fn handle_fill_console_output_character_w(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    fill_console_output(cx, "FillConsoleOutputCharacterW", FillKind::Wide)
}

pub fn handle_fill_console_output_character_a(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    fill_console_output(cx, "FillConsoleOutputCharacterA", FillKind::Ansi)
}

pub fn handle_fill_console_output_attribute(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    fill_console_output(cx, "FillConsoleOutputAttribute", FillKind::Attribute)
}

/// Read a `SMALL_RECT` from guest memory (four `SHORT`s, inclusive edges).
fn read_small_rect(cx: &mut ConsoleCall<'_>, address: u64, api: &str) -> Result<SmallRect> {
    let left = read_u16(cx.engine, address)?;
    let top = read_u16(cx.engine, crate::kernel32::checked_address(address, 2, api))?;
    let right = read_u16(cx.engine, crate::kernel32::checked_address(address, 4, api))?;
    let bottom = read_u16(cx.engine, crate::kernel32::checked_address(address, 6, api))?;
    Ok(SmallRect {
        left: i16::from_ne_bytes(left.to_ne_bytes()),
        top: i16::from_ne_bytes(top.to_ne_bytes()),
        right: i16::from_ne_bytes(right.to_ne_bytes()),
        bottom: i16::from_ne_bytes(bottom.to_ne_bytes()),
    })
}

/// Write a `SMALL_RECT` back to guest memory.
fn write_small_rect(
    cx: &mut ConsoleCall<'_>,
    address: u64,
    rect: SmallRect,
    api: &str,
) -> Result<()> {
    for (offset, value) in [
        (0_u64, rect.left),
        (2, rect.top),
        (4, rect.right),
        (6, rect.bottom),
    ] {
        let slot = crate::kernel32::checked_address(address, offset, api);
        crate::kernel32::write_guest_u16(cx.engine, slot, u16::from_ne_bytes(value.to_ne_bytes()))?;
    }
    Ok(())
}

/// `WriteConsoleOutputW` / `WriteConsoleOutputA` — the frame blit.
///
/// ABI: `HANDLE`, `const CHAR_INFO* lpBuffer`, `COORD dwBufferSize` (packed in
/// R8), `COORD dwBufferCoord` (packed in R9), `PSMALL_RECT lpWriteRegion`
/// (stack, in/out).
///
/// The write region is clipped to the destination buffer and written back, as
/// Windows does, so a caller that blits a region larger than the screen learns
/// what actually landed.
fn write_console_output(cx: &mut ConsoleCall<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "WriteConsoleOutputW"
    } else {
        "WriteConsoleOutputA"
    };
    let handle = cx.engine.read_rcx().context("WriteConsoleOutput RCX")?;
    let source_va = cx.engine.read_rdx().context("WriteConsoleOutput RDX")?;
    let source_size = Coord::from_packed(cx.engine.read_r8().context("WriteConsoleOutput R8")?);
    let source_origin = Coord::from_packed(cx.engine.read_r9().context("WriteConsoleOutput R9")?);
    let region_va = stack_arg(cx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, api);
    };
    if source_va == 0 || region_va == 0 {
        cx.state
            .set_process_last_error(crate::kernel32::ERROR_INVALID_PARAMETER);
        return ret_u64(cx.engine, 0, api);
    }
    enter_cells_mode(cx);

    let region = read_small_rect(cx, region_va, api)?;
    let source_cells = usize::try_from(source_size.x.max(0))
        .unwrap_or(0)
        .saturating_mul(usize::try_from(source_size.y.max(0)).unwrap_or(0))
        .min(MAX_CELLS);
    if source_cells == 0 {
        return ret_bool_true(cx.engine, api);
    }

    let mut raw = vec![0_u8; source_cells.saturating_mul(CHAR_INFO_SIZE)];
    read_guest_bytes(cx.engine, source_va, &mut raw).context("WriteConsoleOutput source")?;
    let code_page = cx.state.console().output_code_page;

    let (width, height) = cx
        .state
        .console()
        .buffer(buffer_handle)
        .map_or((0, 0), |buffer| (buffer.width, buffer.height));
    let clipped = SmallRect {
        left: region.left.max(0),
        top: region.top.max(0),
        right: region
            .right
            .min(i16::try_from(width).unwrap_or(i16::MAX).saturating_sub(1)),
        bottom: region
            .bottom
            .min(i16::try_from(height).unwrap_or(i16::MAX).saturating_sub(1)),
    };

    if let Some(buffer) = cx.state.console().buffer_mut(buffer_handle) {
        let mut row = clipped.top;
        while row <= clipped.bottom {
            let mut column = clipped.left;
            while column <= clipped.right {
                // Position within the caller's source rectangle.
                let source_x = source_origin
                    .x
                    .saturating_add(column.saturating_sub(region.left));
                let source_y = source_origin
                    .y
                    .saturating_add(row.saturating_sub(region.top));
                if source_x < 0
                    || source_y < 0
                    || source_x >= source_size.x
                    || source_y >= source_size.y
                {
                    column = column.saturating_add(1);
                    continue;
                }
                let source_index = usize::try_from(source_y)
                    .unwrap_or(0)
                    .saturating_mul(usize::try_from(source_size.x).unwrap_or(0))
                    .saturating_add(usize::try_from(source_x).unwrap_or(0));
                let byte_offset = source_index.saturating_mul(CHAR_INFO_SIZE);
                let Some(chunk) = raw.get(byte_offset..byte_offset.saturating_add(CHAR_INFO_SIZE))
                else {
                    column = column.saturating_add(1);
                    continue;
                };
                let raw_char = u16::from_le_bytes([
                    chunk.first().copied().unwrap_or(0),
                    chunk.get(1).copied().unwrap_or(0),
                ]);
                let attributes = u16::from_le_bytes([
                    chunk.get(2).copied().unwrap_or(0),
                    chunk.get(3).copied().unwrap_or(0),
                ]);
                // The union's AsciiChar member occupies the low byte, so a
                // narrow caller's glyph needs decoding through the code page.
                let unit = if wide {
                    raw_char
                } else {
                    let byte = u8::try_from(raw_char & 0xFF).unwrap_or(b' ');
                    codepage::decode_to_units(code_page, &[byte])
                        .first()
                        .copied()
                        .unwrap_or(u16::from(b' '))
                };
                if let Some(index) = buffer.index_of(column, row)
                    && let Some(cell) = buffer.cells.get_mut(index)
                {
                    *cell = CharInfo { unit, attributes };
                }
                column = column.saturating_add(1);
            }
            row = row.saturating_add(1);
        }
    }

    flush_active(cx);
    write_small_rect(cx, region_va, clipped, api)?;
    ret_bool_true(cx.engine, api)
}

pub fn handle_write_console_output_w(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    write_console_output(cx, true)
}

pub fn handle_write_console_output_a(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    write_console_output(cx, false)
}

/// `WriteConsoleOutputCharacterW/A` and `WriteConsoleOutputAttribute`.
///
/// ABI: `HANDLE`, buffer, `DWORD nLength`, `COORD dwWriteCoord` (R9),
/// `LPDWORD lpNumberOfWritten` (stack).
fn write_console_output_run(
    cx: &mut ConsoleCall<'_>,
    api: &str,
    kind: RunKind,
) -> Result<WinApiHandlerResult> {
    let handle = cx.engine.read_rcx().context("WriteConsoleOutputRun RCX")?;
    let source_va = cx.engine.read_rdx().context("WriteConsoleOutputRun RDX")?;
    let length = low_u32(
        cx.engine.read_r8().context("WriteConsoleOutputRun R8")?,
        "WriteConsoleOutputRun length",
    )?;
    let start = Coord::from_packed(cx.engine.read_r9().context("WriteConsoleOutputRun R9")?);
    let written_va = stack_arg(cx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, api);
    };
    enter_cells_mode(cx);

    let count = usize::try_from(length).unwrap_or(0).min(MAX_CELLS);
    let values: Vec<u16> = if source_va == 0 || count == 0 {
        Vec::new()
    } else {
        match kind {
            RunKind::Ansi => {
                let mut bytes = vec![0_u8; count];
                read_guest_bytes(cx.engine, source_va, &mut bytes)
                    .context("WriteConsoleOutputCharacterA source")?;
                codepage::decode_to_units(cx.state.console().output_code_page, &bytes)
            }
            RunKind::Wide | RunKind::Attribute => {
                let mut bytes = vec![0_u8; count.saturating_mul(2)];
                read_guest_bytes(cx.engine, source_va, &mut bytes)
                    .context("WriteConsoleOutputRun source")?;
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
            }
        }
    };

    let mut written = 0_u32;
    if let Some(buffer) = cx.state.console().buffer_mut(buffer_handle) {
        let Some(origin) = buffer.index_of(start.x, start.y) else {
            cx.state
                .set_process_last_error(crate::kernel32::ERROR_INVALID_PARAMETER);
            return ret_u64(cx.engine, 0, api);
        };
        for (step, value) in values.iter().enumerate() {
            let Some(index) = origin.checked_add(step) else {
                break;
            };
            let Some(cell) = buffer.cells.get_mut(index) else {
                break;
            };
            match kind {
                RunKind::Attribute => cell.attributes = *value,
                RunKind::Wide | RunKind::Ansi => cell.unit = *value,
            }
            written = written.saturating_add(1);
        }
    }

    flush_active(cx);
    if written_va != 0 {
        write_guest_u32(cx.engine, written_va, written)?;
    }
    ret_bool_true(cx.engine, api)
}

/// Which field a `WriteConsoleOutput*` run call writes.
#[derive(Debug, Clone, Copy)]
enum RunKind {
    Wide,
    Ansi,
    Attribute,
}

pub fn handle_write_console_output_character_w(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    write_console_output_run(cx, "WriteConsoleOutputCharacterW", RunKind::Wide)
}

pub fn handle_write_console_output_character_a(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    write_console_output_run(cx, "WriteConsoleOutputCharacterA", RunKind::Ansi)
}

pub fn handle_write_console_output_attribute(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    write_console_output_run(cx, "WriteConsoleOutputAttribute", RunKind::Attribute)
}

/// `ReadConsoleOutputCharacterW/A` and `ReadConsoleOutputAttribute`.
fn read_console_output_run(
    cx: &mut ConsoleCall<'_>,
    api: &str,
    kind: RunKind,
) -> Result<WinApiHandlerResult> {
    let handle = cx.engine.read_rcx().context("ReadConsoleOutputRun RCX")?;
    let dest_va = cx.engine.read_rdx().context("ReadConsoleOutputRun RDX")?;
    let length = low_u32(
        cx.engine.read_r8().context("ReadConsoleOutputRun R8")?,
        "ReadConsoleOutputRun length",
    )?;
    let start = Coord::from_packed(cx.engine.read_r9().context("ReadConsoleOutputRun R9")?);
    let read_va = stack_arg(cx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, api);
    };
    let count = usize::try_from(length).unwrap_or(0).min(MAX_CELLS);
    let code_page = cx.state.console().output_code_page;

    let mut values: Vec<u16> = Vec::with_capacity(count);
    if let Some(buffer) = cx.state.console().buffer(buffer_handle)
        && let Some(origin) = buffer.index_of(start.x, start.y)
    {
        for step in 0..count {
            let Some(index) = origin.checked_add(step) else {
                break;
            };
            let Some(cell) = buffer.cells.get(index) else {
                break;
            };
            values.push(match kind {
                RunKind::Attribute => cell.attributes,
                RunKind::Wide | RunKind::Ansi => cell.unit,
            });
        }
    }

    let read = u32::try_from(values.len()).unwrap_or(0);
    if dest_va != 0 && !values.is_empty() {
        let bytes = match kind {
            RunKind::Ansi => codepage::encode_from_units(code_page, &values),
            RunKind::Wide | RunKind::Attribute => {
                let mut out = Vec::with_capacity(values.len().saturating_mul(2));
                for value in &values {
                    out.extend_from_slice(&value.to_le_bytes());
                }
                out
            }
        };
        cx.engine
            .mem_write(dest_va, &bytes)
            .context("ReadConsoleOutputRun dest")?;
    }
    if read_va != 0 {
        write_guest_u32(cx.engine, read_va, read)?;
    }
    ret_bool_true(cx.engine, api)
}

pub fn handle_read_console_output_character_w(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    read_console_output_run(cx, "ReadConsoleOutputCharacterW", RunKind::Wide)
}

pub fn handle_read_console_output_character_a(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    read_console_output_run(cx, "ReadConsoleOutputCharacterA", RunKind::Ansi)
}

pub fn handle_read_console_output_attribute(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    read_console_output_run(cx, "ReadConsoleOutputAttribute", RunKind::Attribute)
}

/// `ReadConsoleOutputW/A` — the inverse blit of `WriteConsoleOutput`.
fn read_console_output(cx: &mut ConsoleCall<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "ReadConsoleOutputW"
    } else {
        "ReadConsoleOutputA"
    };
    let handle = cx.engine.read_rcx().context("ReadConsoleOutput RCX")?;
    let dest_va = cx.engine.read_rdx().context("ReadConsoleOutput RDX")?;
    let dest_size = Coord::from_packed(cx.engine.read_r8().context("ReadConsoleOutput R8")?);
    let dest_origin = Coord::from_packed(cx.engine.read_r9().context("ReadConsoleOutput R9")?);
    let region_va = stack_arg(cx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, api);
    };
    if dest_va == 0 || region_va == 0 {
        cx.state
            .set_process_last_error(crate::kernel32::ERROR_INVALID_PARAMETER);
        return ret_u64(cx.engine, 0, api);
    }
    let region = read_small_rect(cx, region_va, api)?;
    let cells = usize::try_from(dest_size.x.max(0))
        .unwrap_or(0)
        .saturating_mul(usize::try_from(dest_size.y.max(0)).unwrap_or(0))
        .min(MAX_CELLS);
    if cells == 0 {
        return ret_bool_true(cx.engine, api);
    }

    let code_page = cx.state.console().output_code_page;
    let mut raw = vec![0_u8; cells.saturating_mul(CHAR_INFO_SIZE)];
    let (width, height) = cx
        .state
        .console()
        .buffer(buffer_handle)
        .map_or((0, 0), |buffer| (buffer.width, buffer.height));
    let clipped = SmallRect {
        left: region.left.max(0),
        top: region.top.max(0),
        right: region
            .right
            .min(i16::try_from(width).unwrap_or(i16::MAX).saturating_sub(1)),
        bottom: region
            .bottom
            .min(i16::try_from(height).unwrap_or(i16::MAX).saturating_sub(1)),
    };

    if let Some(buffer) = cx.state.console().buffer(buffer_handle) {
        let mut row = clipped.top;
        while row <= clipped.bottom {
            let mut column = clipped.left;
            while column <= clipped.right {
                let dest_x = dest_origin
                    .x
                    .saturating_add(column.saturating_sub(region.left));
                let dest_y = dest_origin.y.saturating_add(row.saturating_sub(region.top));
                if dest_x < 0 || dest_y < 0 || dest_x >= dest_size.x || dest_y >= dest_size.y {
                    column = column.saturating_add(1);
                    continue;
                }
                let cell = buffer
                    .index_of(column, row)
                    .and_then(|index| buffer.cells.get(index).copied())
                    .unwrap_or_default();
                let stored = if wide {
                    cell.unit
                } else {
                    u16::from(
                        codepage::encode_from_units(code_page, &[cell.unit])
                            .first()
                            .copied()
                            .unwrap_or(b' '),
                    )
                };
                let dest_index = usize::try_from(dest_y)
                    .unwrap_or(0)
                    .saturating_mul(usize::try_from(dest_size.x).unwrap_or(0))
                    .saturating_add(usize::try_from(dest_x).unwrap_or(0));
                let offset = dest_index.saturating_mul(CHAR_INFO_SIZE);
                if let Some(slot) = raw.get_mut(offset..offset.saturating_add(CHAR_INFO_SIZE)) {
                    if let Some(half) = slot.get_mut(..2) {
                        half.copy_from_slice(&stored.to_le_bytes());
                    }
                    if let Some(half) = slot.get_mut(2..) {
                        half.copy_from_slice(&cell.attributes.to_le_bytes());
                    }
                }
                column = column.saturating_add(1);
            }
            row = row.saturating_add(1);
        }
    }

    cx.engine
        .mem_write(dest_va, &raw)
        .context("ReadConsoleOutput dest")?;
    write_small_rect(cx, region_va, clipped, api)?;
    ret_bool_true(cx.engine, api)
}

pub fn handle_read_console_output_w(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    read_console_output(cx, true)
}

pub fn handle_read_console_output_a(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    read_console_output(cx, false)
}

/// `ScrollConsoleScreenBufferW/A`.
///
/// ABI: `HANDLE`, `const SMALL_RECT* lpScrollRectangle`,
/// `const SMALL_RECT* lpClipRectangle`, `COORD dwDestinationOrigin` (R9),
/// `const CHAR_INFO* lpFill` (stack).
fn scroll_console_screen_buffer(
    cx: &mut ConsoleCall<'_>,
    api: &str,
) -> Result<WinApiHandlerResult> {
    let handle = cx.engine.read_rcx().context("ScrollConsole RCX")?;
    let scroll_va = cx.engine.read_rdx().context("ScrollConsole RDX")?;
    let clip_va = cx.engine.read_r8().context("ScrollConsole R8")?;
    let destination = Coord::from_packed(cx.engine.read_r9().context("ScrollConsole R9")?);
    let fill_va = stack_arg(cx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, api);
    };
    if scroll_va == 0 {
        cx.state
            .set_process_last_error(crate::kernel32::ERROR_INVALID_PARAMETER);
        return ret_u64(cx.engine, 0, api);
    }
    enter_cells_mode(cx);

    let scroll = read_small_rect(cx, scroll_va, api)?;
    let clip = if clip_va == 0 {
        None
    } else {
        Some(read_small_rect(cx, clip_va, api)?)
    };
    let fill = if fill_va == 0 {
        CharInfo::default()
    } else {
        let raw_char = read_u16(cx.engine, fill_va)?;
        let attributes = read_u16(cx.engine, crate::kernel32::checked_address(fill_va, 2, api))?;
        CharInfo {
            unit: raw_char,
            attributes,
        }
    };

    if let Some(buffer) = cx.state.console().buffer_mut(buffer_handle) {
        scroll_region(buffer, scroll, clip, destination, fill);
    }
    flush_active(cx);
    ret_bool_true(cx.engine, api)
}

/// Move `scroll` to `destination`, filling the vacated cells.
///
/// Snapshots the source before writing: an overlapping move (the common case,
/// since scrolling a region by one row overlaps itself) would otherwise read
/// cells it had already overwritten.
fn scroll_region(
    buffer: &mut ScreenBuffer,
    scroll: SmallRect,
    clip: Option<SmallRect>,
    destination: Coord,
    fill: CharInfo,
) {
    let mut moved: Vec<(i16, i16, CharInfo)> = Vec::new();
    let mut row = scroll.top;
    while row <= scroll.bottom {
        let mut column = scroll.left;
        while column <= scroll.right {
            if let Some(index) = buffer.index_of(column, row)
                && let Some(cell) = buffer.cells.get(index).copied()
            {
                let target_x = destination
                    .x
                    .saturating_add(column.saturating_sub(scroll.left));
                let target_y = destination.y.saturating_add(row.saturating_sub(scroll.top));
                moved.push((target_x, target_y, cell));
            }
            column = column.saturating_add(1);
        }
        row = row.saturating_add(1);
    }

    let inside_clip = |x: i16, y: i16| -> bool {
        clip.is_none_or(|rect| {
            x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom
        })
    };

    // Blank the source region first, then paint the moved cells over it.
    let mut row = scroll.top;
    while row <= scroll.bottom {
        let mut column = scroll.left;
        while column <= scroll.right {
            if inside_clip(column, row)
                && let Some(index) = buffer.index_of(column, row)
                && let Some(cell) = buffer.cells.get_mut(index)
            {
                *cell = fill;
            }
            column = column.saturating_add(1);
        }
        row = row.saturating_add(1);
    }

    for (x, y, cell) in moved {
        if inside_clip(x, y)
            && let Some(index) = buffer.index_of(x, y)
            && let Some(slot) = buffer.cells.get_mut(index)
        {
            *slot = cell;
        }
    }
}

pub fn handle_scroll_console_screen_buffer_w(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    scroll_console_screen_buffer(cx, "ScrollConsoleScreenBufferW")
}

pub fn handle_scroll_console_screen_buffer_a(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    scroll_console_screen_buffer(cx, "ScrollConsoleScreenBufferA")
}

/// `CreateConsoleScreenBuffer(DWORD, DWORD, const SECURITY_ATTRIBUTES*, DWORD, LPVOID)`.
pub fn handle_create_console_screen_buffer(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    let _access = cx
        .engine
        .read_rcx()
        .context("CreateConsoleScreenBuffer RCX")?;
    let _share = cx
        .engine
        .read_rdx()
        .context("CreateConsoleScreenBuffer RDX")?;
    let _security = cx
        .engine
        .read_r8()
        .context("CreateConsoleScreenBuffer R8")?;
    let _flags = cx
        .engine
        .read_r9()
        .context("CreateConsoleScreenBuffer R9")?;
    enter_cells_mode(cx);
    let handle = cx.state.console().create_buffer();
    ret_u64(cx.engine, handle, "CreateConsoleScreenBuffer")
}

/// `SetConsoleActiveScreenBuffer(HANDLE)` — the buffer flip.
pub fn handle_set_console_active_screen_buffer(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = cx
        .engine
        .read_rcx()
        .context("SetConsoleActiveScreenBuffer RCX")?;
    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, "SetConsoleActiveScreenBuffer");
    };
    cx.state.console().active_buffer = buffer_handle;
    // The newly displayed buffer has nothing in common with what is on screen,
    // so the next flush must repaint rather than diff.
    cx.state.console().rendered = None;
    flush_active(cx);
    ret_bool_true(cx.engine, "SetConsoleActiveScreenBuffer")
}

/// `SetConsoleScreenBufferSize(HANDLE, COORD)`.
///
/// The host terminal's size is not ours to change, so this resizes the grid and
/// reports success; a guest that then queries the size sees what it asked for
/// while rendering stays clipped to the real window.
pub fn handle_set_console_screen_buffer_size(
    cx: &mut ConsoleCall<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = cx
        .engine
        .read_rcx()
        .context("SetConsoleScreenBufferSize RCX")?;
    let size = Coord::from_packed(
        cx.engine
            .read_rdx()
            .context("SetConsoleScreenBufferSize RDX")?,
    );
    let Some(buffer_handle) = buffer_handle_for(cx.state, handle) else {
        return ret_invalid_handle(cx, "SetConsoleScreenBufferSize");
    };
    if size.x <= 0 || size.y <= 0 {
        cx.state
            .set_process_last_error(crate::kernel32::ERROR_INVALID_PARAMETER);
        return ret_u64(cx.engine, 0, "SetConsoleScreenBufferSize");
    }
    if let Some(buffer) = cx.state.console().buffer_mut(buffer_handle) {
        buffer.resize(
            u16::try_from(size.x).unwrap_or(0),
            u16::try_from(size.y).unwrap_or(0),
        );
    }
    cx.state.console().rendered = None;
    flush_active(cx);
    ret_bool_true(cx.engine, "SetConsoleScreenBufferSize")
}

/// `SetConsoleWindowInfo(HANDLE, BOOL, const SMALL_RECT*)`.
///
/// Accepted and recorded, but the terminal window is the user's to size. A
/// guest that shrinks its window still renders correctly because the grid, not
/// the window rect, drives the diff.
pub fn handle_set_console_window_info(cx: &mut ConsoleCall<'_>) -> Result<WinApiHandlerResult> {
    let handle = cx.engine.read_rcx().context("SetConsoleWindowInfo RCX")?;
    let _absolute = cx.engine.read_rdx().context("SetConsoleWindowInfo RDX")?;
    let rect_va = cx.engine.read_r8().context("SetConsoleWindowInfo R8")?;
    if buffer_handle_for(cx.state, handle).is_none() {
        return ret_invalid_handle(cx, "SetConsoleWindowInfo");
    }
    if rect_va == 0 {
        cx.state
            .set_process_last_error(crate::kernel32::ERROR_INVALID_PARAMETER);
        return ret_u64(cx.engine, 0, "SetConsoleWindowInfo");
    }
    let _rect = read_small_rect(cx, rect_va, "SetConsoleWindowInfo")?;
    ret_bool_true(cx.engine, "SetConsoleWindowInfo")
}

/// Dispatch the cell-API surface by name.
pub fn dispatch_console_cells(
    cx: &mut ConsoleCall<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let result = match name {
        "setconsolecursorposition" => handle_set_console_cursor_position(cx)?,
        "setconsoletextattribute" => handle_set_console_text_attribute(cx)?,
        "getconsolecursorinfo" => handle_get_console_cursor_info(cx)?,
        "setconsolecursorinfo" => handle_set_console_cursor_info(cx)?,
        "fillconsoleoutputcharacterw" => handle_fill_console_output_character_w(cx)?,
        "fillconsoleoutputcharactera" => handle_fill_console_output_character_a(cx)?,
        "fillconsoleoutputattribute" => handle_fill_console_output_attribute(cx)?,
        "writeconsoleoutputw" => handle_write_console_output_w(cx)?,
        "writeconsoleoutputa" => handle_write_console_output_a(cx)?,
        "writeconsoleoutputcharacterw" => handle_write_console_output_character_w(cx)?,
        "writeconsoleoutputcharactera" => handle_write_console_output_character_a(cx)?,
        "writeconsoleoutputattribute" => handle_write_console_output_attribute(cx)?,
        "readconsoleoutputw" => handle_read_console_output_w(cx)?,
        "readconsoleoutputa" => handle_read_console_output_a(cx)?,
        "readconsoleoutputcharacterw" => handle_read_console_output_character_w(cx)?,
        "readconsoleoutputcharactera" => handle_read_console_output_character_a(cx)?,
        "readconsoleoutputattribute" => handle_read_console_output_attribute(cx)?,
        "scrollconsolescreenbufferw" => handle_scroll_console_screen_buffer_w(cx)?,
        "scrollconsolescreenbuffera" => handle_scroll_console_screen_buffer_a(cx)?,
        "createconsolescreenbuffer" => handle_create_console_screen_buffer(cx)?,
        "setconsoleactivescreenbuffer" => handle_set_console_active_screen_buffer(cx)?,
        "setconsolescreenbuffersize" => handle_set_console_screen_buffer_size(cx)?,
        "setconsolewindowinfo" => handle_set_console_window_info(cx)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::DEFAULT_ATTRIBUTES;

    fn filled_grid(width: u16, height: u16, glyph: u8) -> ScreenBuffer {
        let mut buffer = ScreenBuffer::new(width, height);
        for cell in &mut buffer.cells {
            *cell = CharInfo {
                unit: u16::from(glyph),
                attributes: DEFAULT_ATTRIBUTES,
            };
        }
        buffer
    }

    #[test]
    fn scrolling_up_by_one_row_preserves_overlapping_content() {
        let mut buffer = ScreenBuffer::new(4, 3);
        // Row 1 holds 'B', row 2 holds 'C'.
        for (row, glyph) in [(0_u16, b'A'), (1, b'B'), (2, b'C')] {
            for column in 0..4_u16 {
                if let Some(index) = buffer.index_of(
                    i16::try_from(column).unwrap_or(0),
                    i16::try_from(row).unwrap_or(0),
                ) && let Some(cell) = buffer.cells.get_mut(index)
                {
                    cell.unit = u16::from(glyph);
                }
            }
        }
        // Move rows 1..2 up to row 0 — an overlapping move.
        scroll_region(
            &mut buffer,
            SmallRect {
                left: 0,
                top: 1,
                right: 3,
                bottom: 2,
            },
            None,
            Coord::new(0, 0),
            CharInfo {
                unit: u16::from(b' '),
                attributes: DEFAULT_ATTRIBUTES,
            },
        );
        let at = |x: i16, y: i16| {
            buffer
                .index_of(x, y)
                .and_then(|index| buffer.cells.get(index))
                .map(|cell| cell.unit)
        };
        assert_eq!(at(0, 0), Some(u16::from(b'B')));
        assert_eq!(at(0, 1), Some(u16::from(b'C')));
        // The vacated row takes the fill character.
        assert_eq!(at(0, 2), Some(u16::from(b' ')));
    }

    #[test]
    fn a_clip_rectangle_protects_cells_outside_it() {
        let mut buffer = filled_grid(4, 2, b'X');
        scroll_region(
            &mut buffer,
            SmallRect {
                left: 0,
                top: 0,
                right: 3,
                bottom: 1,
            },
            Some(SmallRect {
                left: 0,
                top: 0,
                right: 1,
                bottom: 1,
            }),
            Coord::new(0, 0),
            CharInfo {
                unit: u16::from(b'.'),
                attributes: DEFAULT_ATTRIBUTES,
            },
        );
        let at = |x: i16, y: i16| {
            buffer
                .index_of(x, y)
                .and_then(|index| buffer.cells.get(index))
                .map(|cell| cell.unit)
        };
        // Column 2 is outside the clip and must be untouched.
        assert_eq!(at(2, 0), Some(u16::from(b'X')));
    }

    #[test]
    fn resizing_preserves_the_top_left_overlap() {
        let mut buffer = filled_grid(4, 2, b'Z');
        buffer.resize(2, 2);
        assert_eq!(buffer.cells.len(), 4);
        assert!(buffer.cells.iter().all(|cell| cell.unit == u16::from(b'Z')));
    }

    #[test]
    fn scroll_up_blanks_the_freed_bottom_row() {
        let mut buffer = filled_grid(3, 2, b'Q');
        buffer.scroll_up();
        let bottom: Vec<u16> = (0..3)
            .filter_map(|column| {
                buffer
                    .index_of(column, 1)
                    .and_then(|index| buffer.cells.get(index))
                    .map(|cell| cell.unit)
            })
            .collect();
        assert_eq!(bottom, vec![u16::from(b' '); 3]);
    }
}
