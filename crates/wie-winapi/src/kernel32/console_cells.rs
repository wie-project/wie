//! Console cell APIs: the framebuffer surface a text-mode game draws through.
//!
//! Every entry point here writes into a [`ScreenBuffer`] and then asks
//! [`crate::console::screen::flush`] to reconcile the terminal with it. Nothing
//! writes escape sequences directly, so the diff always sees a truthful "before"
//! and cannot be desynchronised by a partial paint.

#![allow(clippy::map_identity, clippy::option_map_unit_fn)]

use super::{
    Context, HandlerContext, Result, WinApiHandlerResult, WinApiState, low_u32, ret_bool_true,
    ret_u64, write_guest_u32,
};
use crate::console::{
    CharInfo, Coord, RenderMode, ScreenBuffer, SmallRect, codepage, host_term, screen,
};
use crate::guest_memory::{read_bytes as read_guest_bytes, read_u16 as read_guest_u16};

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
fn stack_arg(ctx: &mut HandlerContext<'_>, index: usize, api: &str) -> Result<u64> {
    let rsp = ctx.engine.read_rsp().with_context(|| format!("{api} RSP"))?;
    let offset = 0x28_u64.saturating_add((u64::try_from(index).unwrap_or(0)).saturating_mul(8));
    let address = super::checked_address(rsp, offset, api)?;
    super::read_guest_u64(ctx.engine, address)
}

/// Resolve an output handle to a screen-buffer handle.
fn buffer_handle_for(state: &WinApiState, handle: u64) -> Option<u64> {
    state.try_console().and_then(|c| super::console::buffer_handle_for(c, handle))
}

/// Switch into Cells mode the first time a cell API is used.
///
/// The alternate screen goes with it: a program painting whole frames should
/// not shred the user's scrollback, and their shell prompt should return intact.
fn enter_cells_mode(ctx: &mut HandlerContext<'_>) {
    if ctx.state.console().render_mode == RenderMode::Cells {
        return;
    }
    ctx.state.console().render_mode = RenderMode::Cells;
    ctx.state.console().rendered = None;
    screen::enter_alternate_screen();
}

/// Repaint the terminal from the active buffer.
///
/// A write to a non-displayed buffer changes nothing on screen, which is
/// exactly the property double buffering relies on.
fn flush_active(ctx: &mut HandlerContext<'_>) {
    if ctx.state.console().render_mode != RenderMode::Cells || !host_term::is_tty() {
        return;
    }
    let active = ctx.state.console().active_buffer;
    let Some(buffer) = ctx.state.console().buffer(active).cloned() else {
        return;
    };
    let previous = ctx.state.console().rendered.take();
    let output = screen::flush(&buffer, previous.as_ref());
    if !output.is_empty() {
        host_term::write_stdout(output.as_bytes());
    }
    ctx.state.console().rendered = Some(buffer);
    ctx.state.console().repaint_forced = false;
}

fn ret_invalid_handle(ctx: &mut HandlerContext<'_>, api: &str) -> Result<WinApiHandlerResult> {
    ctx.state.process.last_error = ERROR_INVALID_HANDLE;
    ret_u64(ctx.engine, 0, api)
}

/// `SetConsoleCursorPosition(HANDLE, COORD)` — the COORD arrives packed in RDX.
pub fn handle_set_console_cursor_position(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx
        .engine
        .read_rcx()
        .context("SetConsoleCursorPosition RCX")?;
    let position = Coord::from_packed(
        ctx.engine
            .read_rdx()
            .context("SetConsoleCursorPosition RDX")?,
    );
    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, "SetConsoleCursorPosition");
    };
    let in_bounds = ctx
        .state
        .console()
        .buffer(buffer_handle)
        .is_some_and(|buffer| buffer.index_of(position.x, position.y).is_some());
    if !in_bounds {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, "SetConsoleCursorPosition");
    }
    if let Some(buffer) = ctx.state.console().buffer_mut(buffer_handle) {
        buffer.cursor = position;
    }
    // Stream mode has no grid to repaint, so move the real cursor now.
    if ctx.state.console().render_mode == RenderMode::Stream {
        screen::move_cursor(position.x, position.y);
    } else {
        flush_active(ctx);
    }
    ret_bool_true(ctx.engine, "SetConsoleCursorPosition")
}

/// `SetConsoleTextAttribute(HANDLE, WORD)`.
pub fn handle_set_console_text_attribute(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx
        .engine
        .read_rcx()
        .context("SetConsoleTextAttribute RCX")?;
    let attributes = low_u32(
        ctx.engine
            .read_rdx()
            .context("SetConsoleTextAttribute RDX")?,
        "SetConsoleTextAttribute",
    )?;
    let attributes = u16::try_from(attributes & 0xFFFF).unwrap_or(0);
    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, "SetConsoleTextAttribute");
    };
    if let Some(buffer) = ctx.state.console().buffer_mut(buffer_handle) {
        buffer.attributes = attributes;
    }
    if ctx.state.console().render_mode == RenderMode::Stream {
        screen::apply_attributes(attributes);
    }
    ret_bool_true(ctx.engine, "SetConsoleTextAttribute")
}

/// `GetConsoleCursorInfo(HANDLE, PCONSOLE_CURSOR_INFO)`.
///
/// `CONSOLE_CURSOR_INFO` is `{ DWORD dwSize; BOOL bVisible; }`.
pub fn handle_get_console_cursor_info(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("GetConsoleCursorInfo RCX")?;
    let info_ptr = ctx.engine.read_rdx().context("GetConsoleCursorInfo RDX")?;
    if info_ptr == 0 {
        return ret_invalid_handle(ctx, "GetConsoleCursorInfo");
    }
    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, "GetConsoleCursorInfo");
    };
    let (size, visible) = ctx
        .state
        .console()
        .buffer(buffer_handle)
        .map_or((25, 1), |buffer| {
            (buffer.cursor_size, u32::from(buffer.cursor_visible))
        });
    write_guest_u32(ctx.engine, info_ptr, size)?;
    let visible_ptr = super::checked_address(info_ptr, 4, "GetConsoleCursorInfo bVisible")?;
    write_guest_u32(ctx.engine, visible_ptr, visible)?;
    ret_bool_true(ctx.engine, "GetConsoleCursorInfo")
}

/// `SetConsoleCursorInfo(HANDLE, const CONSOLE_CURSOR_INFO*)`.
pub fn handle_set_console_cursor_info(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("SetConsoleCursorInfo RCX")?;
    let info_ptr = ctx.engine.read_rdx().context("SetConsoleCursorInfo RDX")?;
    if info_ptr == 0 {
        return ret_invalid_handle(ctx, "SetConsoleCursorInfo");
    }
    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, "SetConsoleCursorInfo");
    };
    let size = super::read_guest_u32(ctx.engine, info_ptr)?;
    let visible_ptr = super::checked_address(info_ptr, 4, "SetConsoleCursorInfo bVisible")?;
    let visible = super::read_guest_u32(ctx.engine, visible_ptr)? != 0;
    if let Some(buffer) = ctx.state.console().buffer_mut(buffer_handle) {
        buffer.cursor_size = size;
        buffer.cursor_visible = visible;
    }
    // Only the displayed buffer's caret is the one the user sees.
    if buffer_handle == ctx.state.console().active_buffer {
        screen::set_cursor_visible(visible);
    }
    ret_bool_true(ctx.engine, "SetConsoleCursorInfo")
}

/// Shared body of the `FillConsoleOutput*` family.
///
/// ABI: `HANDLE`, the fill value, `DWORD nLength`, `COORD dwWriteCoord`
/// (packed in R9), `LPDWORD lpNumberOfCellsWritten` (stack).
///
/// Fills run in reading order and wrap to the next row, which is what makes the
/// "clear the screen" idiom — one call with the whole cell count — work.
fn fill_console_output(
    ctx: &mut HandlerContext<'_>,
    api: &str,
    fill: FillKind,
) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("FillConsoleOutput RCX")?;
    let raw_value = ctx.engine.read_rdx().context("FillConsoleOutput RDX")?;
    let length = low_u32(
        ctx.engine.read_r8().context("FillConsoleOutput R8")?,
        "FillConsoleOutput length",
    )?;
    let start = Coord::from_packed(ctx.engine.read_r9().context("FillConsoleOutput R9")?);
    let written_ptr = stack_arg(ctx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, api);
    };
    enter_cells_mode(ctx);

    let unit = match fill {
        FillKind::Wide => u16::try_from(raw_value & 0xFFFF).unwrap_or(u16::from(b' ')),
        FillKind::Ansi => {
            let byte = u8::try_from(raw_value & 0xFF).unwrap_or(b' ');
            let code_page = ctx.state.console().output_code_page;
            codepage::decode_to_units(code_page, &[byte])
                .first()
                .copied()
                .unwrap_or(u16::from(b' '))
        }
        FillKind::Attribute => u16::try_from(raw_value & 0xFFFF).unwrap_or(0),
    };

    let mut filled = 0_u32;
    if let Some(buffer) = ctx.state.console().buffer_mut(buffer_handle) {
        let total = usize::try_from(length).unwrap_or(0).min(MAX_CELLS);
        let Some(origin) = buffer.index_of(start.x, start.y) else {
            ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
            return ret_u64(ctx.engine, 0, api);
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
            filled = filled.saturating_add(1);
        }
    }

    flush_active(ctx);
    if written_ptr != 0 {
        write_guest_u32(ctx.engine, written_ptr, filled)?;
    }
    ret_bool_true(ctx.engine, api)
}

/// Which field a `FillConsoleOutput*` call writes.
#[derive(Debug, Clone, Copy)]
enum FillKind {
    Wide,
    Ansi,
    Attribute,
}

pub fn handle_fill_console_output_character_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    fill_console_output(ctx, "FillConsoleOutputCharacterW", FillKind::Wide)
}

pub fn handle_fill_console_output_character_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    fill_console_output(ctx, "FillConsoleOutputCharacterA", FillKind::Ansi)
}

pub fn handle_fill_console_output_attribute(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    fill_console_output(ctx, "FillConsoleOutputAttribute", FillKind::Attribute)
}

/// Read a `SMALL_RECT` from guest memory (four `SHORT`s, inclusive edges).
fn read_small_rect(ctx: &mut HandlerContext<'_>, address: u64, api: &str) -> Result<SmallRect> {
    let left = read_guest_u16(ctx.engine, address)?;
    let top = read_guest_u16(ctx.engine, super::checked_address(address, 2, api)?)?;
    let right = read_guest_u16(ctx.engine, super::checked_address(address, 4, api)?)?;
    let bottom = read_guest_u16(ctx.engine, super::checked_address(address, 6, api)?)?;
    Ok(SmallRect {
        left: i16::from_ne_bytes(left.to_ne_bytes()),
        top: i16::from_ne_bytes(top.to_ne_bytes()),
        right: i16::from_ne_bytes(right.to_ne_bytes()),
        bottom: i16::from_ne_bytes(bottom.to_ne_bytes()),
    })
}

/// Write a `SMALL_RECT` back to guest memory.
fn write_small_rect(
    ctx: &mut HandlerContext<'_>,
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
        let slot = super::checked_address(address, offset, api)?;
        super::write_guest_u16(ctx.engine, slot, u16::from_ne_bytes(value.to_ne_bytes()))?;
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
fn write_console_output(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "WriteConsoleOutputW"
    } else {
        "WriteConsoleOutputA"
    };
    let handle = ctx.engine.read_rcx().context("WriteConsoleOutput RCX")?;
    let source_ptr = ctx.engine.read_rdx().context("WriteConsoleOutput RDX")?;
    let source_size = Coord::from_packed(ctx.engine.read_r8().context("WriteConsoleOutput R8")?);
    let source_origin = Coord::from_packed(ctx.engine.read_r9().context("WriteConsoleOutput R9")?);
    let region_ptr = stack_arg(ctx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, api);
    };
    if source_ptr == 0 || region_ptr == 0 {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, api);
    }
    enter_cells_mode(ctx);

    let region = read_small_rect(ctx, region_ptr, api)?;
    let source_cells = usize::try_from(source_size.x.max(0))
        .unwrap_or(0)
        .saturating_mul(usize::try_from(source_size.y.max(0)).unwrap_or(0))
        .min(MAX_CELLS);
    if source_cells == 0 {
        return ret_bool_true(ctx.engine, api);
    }

    let mut raw = vec![0_u8; source_cells.saturating_mul(CHAR_INFO_SIZE)];
    read_guest_bytes(ctx.engine, source_ptr, &mut raw).context("WriteConsoleOutput source")?;
    let code_page = ctx.state.console().output_code_page;

    let (width, height) = ctx
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

    if let Some(buffer) = ctx.state.console().buffer_mut(buffer_handle) {
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
                if source_x < 0 || source_y < 0 || source_x >= source_size.x || source_y >= source_size.y
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

    flush_active(ctx);
    write_small_rect(ctx, region_ptr, clipped, api)?;
    ret_bool_true(ctx.engine, api)
}

pub fn handle_write_console_output_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    write_console_output(ctx, true)
}

pub fn handle_write_console_output_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    write_console_output(ctx, false)
}

/// `WriteConsoleOutputCharacterW/A` and `WriteConsoleOutputAttribute`.
///
/// ABI: `HANDLE`, buffer, `DWORD nLength`, `COORD dwWriteCoord` (R9),
/// `LPDWORD lpNumberOfWritten` (stack).
fn write_console_output_run(
    ctx: &mut HandlerContext<'_>,
    api: &str,
    kind: RunKind,
) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("WriteConsoleOutputRun RCX")?;
    let source_ptr = ctx.engine.read_rdx().context("WriteConsoleOutputRun RDX")?;
    let length = low_u32(
        ctx.engine.read_r8().context("WriteConsoleOutputRun R8")?,
        "WriteConsoleOutputRun length",
    )?;
    let start = Coord::from_packed(ctx.engine.read_r9().context("WriteConsoleOutputRun R9")?);
    let written_ptr = stack_arg(ctx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, api);
    };
    enter_cells_mode(ctx);

    let count = usize::try_from(length).unwrap_or(0).min(MAX_CELLS);
    let values: Vec<u16> = if source_ptr == 0 || count == 0 {
        Vec::new()
    } else {
        match kind {
            RunKind::Ansi => {
                let mut bytes = vec![0_u8; count];
                read_guest_bytes(ctx.engine, source_ptr, &mut bytes)
                    .context("WriteConsoleOutputCharacterA source")?;
                codepage::decode_to_units(ctx.state.console().output_code_page, &bytes)
            }
            RunKind::Wide | RunKind::Attribute => {
                let mut bytes = vec![0_u8; count.saturating_mul(2)];
                read_guest_bytes(ctx.engine, source_ptr, &mut bytes)
                    .context("WriteConsoleOutputRun source")?;
                bytes
                    .chunks_exact(2)
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
    if let Some(buffer) = ctx.state.console().buffer_mut(buffer_handle) {
        let Some(origin) = buffer.index_of(start.x, start.y) else {
            ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
            return ret_u64(ctx.engine, 0, api);
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

    flush_active(ctx);
    if written_ptr != 0 {
        write_guest_u32(ctx.engine, written_ptr, written)?;
    }
    ret_bool_true(ctx.engine, api)
}

/// Which field a `WriteConsoleOutput*` run call writes.
#[derive(Debug, Clone, Copy)]
enum RunKind {
    Wide,
    Ansi,
    Attribute,
}

pub fn handle_write_console_output_character_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    write_console_output_run(ctx, "WriteConsoleOutputCharacterW", RunKind::Wide)
}

pub fn handle_write_console_output_character_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    write_console_output_run(ctx, "WriteConsoleOutputCharacterA", RunKind::Ansi)
}

pub fn handle_write_console_output_attribute(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    write_console_output_run(ctx, "WriteConsoleOutputAttribute", RunKind::Attribute)
}

/// `ReadConsoleOutputCharacterW/A` and `ReadConsoleOutputAttribute`.
fn read_console_output_run(
    ctx: &mut HandlerContext<'_>,
    api: &str,
    kind: RunKind,
) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("ReadConsoleOutputRun RCX")?;
    let dest_ptr = ctx.engine.read_rdx().context("ReadConsoleOutputRun RDX")?;
    let length = low_u32(
        ctx.engine.read_r8().context("ReadConsoleOutputRun R8")?,
        "ReadConsoleOutputRun length",
    )?;
    let start = Coord::from_packed(ctx.engine.read_r9().context("ReadConsoleOutputRun R9")?);
    let read_ptr = stack_arg(ctx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, api);
    };
    let count = usize::try_from(length).unwrap_or(0).min(MAX_CELLS);
    let code_page = ctx.state.console().output_code_page;

    let mut values: Vec<u16> = Vec::with_capacity(count);
    if let Some(buffer) = ctx.state.console().buffer(buffer_handle)
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
    if dest_ptr != 0 && !values.is_empty() {
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
        ctx.engine
            .mem_write(dest_ptr, &bytes)
            .context("ReadConsoleOutputRun dest")?;
    }
    if read_ptr != 0 {
        write_guest_u32(ctx.engine, read_ptr, read)?;
    }
    ret_bool_true(ctx.engine, api)
}

pub fn handle_read_console_output_character_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    read_console_output_run(ctx, "ReadConsoleOutputCharacterW", RunKind::Wide)
}

pub fn handle_read_console_output_character_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    read_console_output_run(ctx, "ReadConsoleOutputCharacterA", RunKind::Ansi)
}

pub fn handle_read_console_output_attribute(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    read_console_output_run(ctx, "ReadConsoleOutputAttribute", RunKind::Attribute)
}

/// `ReadConsoleOutputW/A` — the inverse blit of `WriteConsoleOutput`.
fn read_console_output(ctx: &mut HandlerContext<'_>, wide: bool) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "ReadConsoleOutputW"
    } else {
        "ReadConsoleOutputA"
    };
    let handle = ctx.engine.read_rcx().context("ReadConsoleOutput RCX")?;
    let dest_ptr = ctx.engine.read_rdx().context("ReadConsoleOutput RDX")?;
    let dest_size = Coord::from_packed(ctx.engine.read_r8().context("ReadConsoleOutput R8")?);
    let dest_origin = Coord::from_packed(ctx.engine.read_r9().context("ReadConsoleOutput R9")?);
    let region_ptr = stack_arg(ctx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, api);
    };
    if dest_ptr == 0 || region_ptr == 0 {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, api);
    }
    let region = read_small_rect(ctx, region_ptr, api)?;
    let cells = usize::try_from(dest_size.x.max(0))
        .unwrap_or(0)
        .saturating_mul(usize::try_from(dest_size.y.max(0)).unwrap_or(0))
        .min(MAX_CELLS);
    if cells == 0 {
        return ret_bool_true(ctx.engine, api);
    }

    let code_page = ctx.state.console().output_code_page;
    let mut raw = vec![0_u8; cells.saturating_mul(CHAR_INFO_SIZE)];
    let (width, height) = ctx
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

    if let Some(buffer) = ctx.state.console().buffer(buffer_handle) {
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
                    slot.get_mut(..2).map(|half| {
                        half.copy_from_slice(&stored.to_le_bytes());
                    });
                    slot.get_mut(2..).map(|half| {
                        half.copy_from_slice(&cell.attributes.to_le_bytes());
                    });
                }
                column = column.saturating_add(1);
            }
            row = row.saturating_add(1);
        }
    }

    ctx.engine
        .mem_write(dest_ptr, &raw)
        .context("ReadConsoleOutput dest")?;
    write_small_rect(ctx, region_ptr, clipped, api)?;
    ret_bool_true(ctx.engine, api)
}

pub fn handle_read_console_output_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    read_console_output(ctx, true)
}

pub fn handle_read_console_output_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    read_console_output(ctx, false)
}

/// `ScrollConsoleScreenBufferW/A`.
///
/// ABI: `HANDLE`, `const SMALL_RECT* lpScrollRectangle`,
/// `const SMALL_RECT* lpClipRectangle`, `COORD dwDestinationOrigin` (R9),
/// `const CHAR_INFO* lpFill` (stack).
fn scroll_console_screen_buffer(
    ctx: &mut HandlerContext<'_>,
    api: &str,
) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("ScrollConsole RCX")?;
    let scroll_ptr = ctx.engine.read_rdx().context("ScrollConsole RDX")?;
    let clip_ptr = ctx.engine.read_r8().context("ScrollConsole R8")?;
    let destination = Coord::from_packed(ctx.engine.read_r9().context("ScrollConsole R9")?);
    let fill_ptr = stack_arg(ctx, 0, api)?;

    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, api);
    };
    if scroll_ptr == 0 {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, api);
    }
    enter_cells_mode(ctx);

    let scroll = read_small_rect(ctx, scroll_ptr, api)?;
    let clip = if clip_ptr == 0 {
        None
    } else {
        Some(read_small_rect(ctx, clip_ptr, api)?)
    };
    let fill = if fill_ptr == 0 {
        CharInfo::default()
    } else {
        let raw_char = read_guest_u16(ctx.engine, fill_ptr)?;
        let attributes = read_guest_u16(ctx.engine, super::checked_address(fill_ptr, 2, api)?)?;
        CharInfo {
            unit: raw_char,
            attributes,
        }
    };

    if let Some(buffer) = ctx.state.console().buffer_mut(buffer_handle) {
        scroll_region(buffer, scroll, clip, destination, fill);
    }
    flush_active(ctx);
    ret_bool_true(ctx.engine, api)
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
        clip.is_none_or(|rect| x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom)
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
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    scroll_console_screen_buffer(ctx, "ScrollConsoleScreenBufferW")
}

pub fn handle_scroll_console_screen_buffer_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    scroll_console_screen_buffer(ctx, "ScrollConsoleScreenBufferA")
}

/// `CreateConsoleScreenBuffer(DWORD, DWORD, const SECURITY_ATTRIBUTES*, DWORD, LPVOID)`.
pub fn handle_create_console_screen_buffer(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _access = ctx.engine.read_rcx().context("CreateConsoleScreenBuffer RCX")?;
    let _share = ctx.engine.read_rdx().context("CreateConsoleScreenBuffer RDX")?;
    let _security = ctx.engine.read_r8().context("CreateConsoleScreenBuffer R8")?;
    let _flags = ctx.engine.read_r9().context("CreateConsoleScreenBuffer R9")?;
    enter_cells_mode(ctx);
    let handle = ctx.state.console().create_buffer();
    ret_u64(ctx.engine, handle, "CreateConsoleScreenBuffer")
}

/// `SetConsoleActiveScreenBuffer(HANDLE)` — the buffer flip.
pub fn handle_set_console_active_screen_buffer(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx
        .engine
        .read_rcx()
        .context("SetConsoleActiveScreenBuffer RCX")?;
    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, "SetConsoleActiveScreenBuffer");
    };
    ctx.state.console().active_buffer = buffer_handle;
    // The newly displayed buffer has nothing in common with what is on screen,
    // so the next flush must repaint rather than diff.
    ctx.state.console().rendered = None;
    flush_active(ctx);
    ret_bool_true(ctx.engine, "SetConsoleActiveScreenBuffer")
}

/// `SetConsoleScreenBufferSize(HANDLE, COORD)`.
///
/// The host terminal's size is not ours to change, so this resizes the grid and
/// reports success; a guest that then queries the size sees what it asked for
/// while rendering stays clipped to the real window.
pub fn handle_set_console_screen_buffer_size(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx
        .engine
        .read_rcx()
        .context("SetConsoleScreenBufferSize RCX")?;
    let size = Coord::from_packed(
        ctx.engine
            .read_rdx()
            .context("SetConsoleScreenBufferSize RDX")?,
    );
    let Some(buffer_handle) = buffer_handle_for(ctx.state, handle) else {
        return ret_invalid_handle(ctx, "SetConsoleScreenBufferSize");
    };
    if size.x <= 0 || size.y <= 0 {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, "SetConsoleScreenBufferSize");
    }
    if let Some(buffer) = ctx.state.console().buffer_mut(buffer_handle) {
        buffer.resize(
            u16::try_from(size.x).unwrap_or(0),
            u16::try_from(size.y).unwrap_or(0),
        );
    }
    ctx.state.console().rendered = None;
    flush_active(ctx);
    ret_bool_true(ctx.engine, "SetConsoleScreenBufferSize")
}

/// `SetConsoleWindowInfo(HANDLE, BOOL, const SMALL_RECT*)`.
///
/// Accepted and recorded, but the terminal window is the user's to size. A
/// guest that shrinks its window still renders correctly because the grid, not
/// the window rect, drives the diff.
pub fn handle_set_console_window_info(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let handle = ctx.engine.read_rcx().context("SetConsoleWindowInfo RCX")?;
    let _absolute = ctx.engine.read_rdx().context("SetConsoleWindowInfo RDX")?;
    let rect_ptr = ctx.engine.read_r8().context("SetConsoleWindowInfo R8")?;
    if buffer_handle_for(ctx.state, handle).is_none() {
        return ret_invalid_handle(ctx, "SetConsoleWindowInfo");
    }
    if rect_ptr == 0 {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, "SetConsoleWindowInfo");
    }
    let _rect = read_small_rect(ctx, rect_ptr, "SetConsoleWindowInfo")?;
    ret_bool_true(ctx.engine, "SetConsoleWindowInfo")
}

/// Dispatch the cell-API surface by name.
pub fn dispatch_console_cells(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let result = match name {
        "setconsolecursorposition" => handle_set_console_cursor_position(ctx)?,
        "setconsoletextattribute" => handle_set_console_text_attribute(ctx)?,
        "getconsolecursorinfo" => handle_get_console_cursor_info(ctx)?,
        "setconsolecursorinfo" => handle_set_console_cursor_info(ctx)?,
        "fillconsoleoutputcharacterw" => handle_fill_console_output_character_w(ctx)?,
        "fillconsoleoutputcharactera" => handle_fill_console_output_character_a(ctx)?,
        "fillconsoleoutputattribute" => handle_fill_console_output_attribute(ctx)?,
        "writeconsoleoutputw" => handle_write_console_output_w(ctx)?,
        "writeconsoleoutputa" => handle_write_console_output_a(ctx)?,
        "writeconsoleoutputcharacterw" => handle_write_console_output_character_w(ctx)?,
        "writeconsoleoutputcharactera" => handle_write_console_output_character_a(ctx)?,
        "writeconsoleoutputattribute" => handle_write_console_output_attribute(ctx)?,
        "readconsoleoutputw" => handle_read_console_output_w(ctx)?,
        "readconsoleoutputa" => handle_read_console_output_a(ctx)?,
        "readconsoleoutputcharacterw" => handle_read_console_output_character_w(ctx)?,
        "readconsoleoutputcharactera" => handle_read_console_output_character_a(ctx)?,
        "readconsoleoutputattribute" => handle_read_console_output_attribute(ctx)?,
        "scrollconsolescreenbufferw" => handle_scroll_console_screen_buffer_w(ctx)?,
        "scrollconsolescreenbuffera" => handle_scroll_console_screen_buffer_a(ctx)?,
        "createconsolescreenbuffer" => handle_create_console_screen_buffer(ctx)?,
        "setconsoleactivescreenbuffer" => handle_set_console_active_screen_buffer(ctx)?,
        "setconsolescreenbuffersize" => handle_set_console_screen_buffer_size(ctx)?,
        "setconsolewindowinfo" => handle_set_console_window_info(ctx)?,
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
