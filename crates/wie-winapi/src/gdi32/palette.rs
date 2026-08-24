//! GDI32 palette handlers — `CreatePalette` / `RealizePalette` /
//! `GetSystemPaletteEntries`.
//!
//! Palette-aware apps (SDL2's 8-bit video init) probe this surface at startup.
//! The emulated screen is 32-bpp with no palette state, so `CreatePalette`
//! returns a stable fake `HPALETTE` handle, `RealizePalette` reports no
//! entries, and `GetSystemPaletteEntries` fills a zeroed gray-ramp equivalent.

use anyhow::{Context, Result};

use crate::gdi32::{ArgReg, read_arg};
use crate::guest_memory::{checked_address, read_u16};
use crate::{HandlerContext, WinApiHandlerResult};

/// Fake `HPALETTE` handle returned by `CreatePalette` (0x6800 FAKE range,
/// disjoint from the classified GDI object bases 0x6820–0x6850).
const FAKE_PALETTE_HANDLE: u64 = 0x0000_0000_6800_6001;

/// Handles `GDI32.dll!CreatePalette` — a fake `HPALETTE` handle.
///
/// `HPALETTE CreatePalette(const LOGPALETTE *plpal)`: reads `plpal` (null
/// fails) and the `palNumEntries` count, then returns a stable fake handle.
/// The emulated screen is 32-bpp, so no palette entries are stored.
pub fn handle_create_palette(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let log_palette_va = read_arg(engine, ArgReg::Rcx, "CreatePalette")?;

    if log_palette_va == 0 {
        return ctx.finish(0);
    }

    // LOGPALETTE (wingdi.h): WORD palVersion @0, WORD palNumEntries @2, then
    // PALETTEENTRY palPalEntry[] @4 (each 4 bytes). The entry count is read
    // for validation; the entries themselves are unused.
    let _num_entries = read_u16(engine, checked_address(log_palette_va, 2, "palNumEntries"))?;

    ctx.finish(FAKE_PALETTE_HANDLE)
}

/// Handles `GDI32.dll!RealizePalette` — reports no entries realized.
///
/// `UINT RealizePalette(HDC hdc)`: the screen carries no palette, so 0
/// entries are realized (a valid, if minimal, result).
pub fn handle_realize_palette(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _hdc = ctx.engine.read_rcx()?;
    ctx.finish(0)
}

/// Handles `GDI32.dll!GetSystemPaletteEntries` — fills a zeroed palette.
///
/// `UINT GetSystemPaletteEntries(HDC hdc, UINT iStart, UINT cEntries,
/// LPPALETTEENTRY lppe)`: fills `lppe` with `cEntries` zeroed `PALETTEENTRY`s
/// (4 bytes each) and returns the count. Returns 0 when the output buffer is
/// null.
pub fn handle_get_system_palette_entries(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hdc = engine.read_rcx()?;
    let _i_start = engine.read_rdx()?;
    let c_entries = read_arg(engine, ArgReg::R8, "GetSystemPaletteEntries")?;
    let entries_va = read_arg(engine, ArgReg::R9, "GetSystemPaletteEntries")?;

    let count = u32::try_from(c_entries & u64::from(u32::MAX)).unwrap_or(0);
    let bytes = u64::from(count).saturating_mul(4);
    if entries_va != 0 && bytes != 0 {
        engine
            .mem_write(entries_va, &vec![0_u8; usize::try_from(bytes).unwrap_or(0)])
            .context("failed to write GetSystemPaletteEntries")?;
    }

    let return_value = if entries_va != 0 { u64::from(count) } else { 0 };
    ctx.finish(return_value)
}
