//! GDI32 DIB round-trip handlers — `GetDIBits` / `SetDIBits`.
//!
//! Both APIs resolve the `HBITMAP` straight to its `DibSection` record (the
//! pixel buffer `CreateDIBSection` allocated on the guest heap), so the HDC
//! parameter is ignored — matching real GDI, which ignores the DC for DIB
//! sections, and the micro-exes, which pass NULL.

use anyhow::{Context, Result};

use crate::guest_layout::BitmapInfoHeader;
use crate::guest_memory::{checked_address, read_u32, read_u64, with_typed_write};
use crate::handles::Hbitmap;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// A DIB pixel surface resolved from an HBITMAP handle.
#[derive(Debug, Clone, Copy)]
struct DibView {
    /// Guest VA of the pixel buffer.
    bits_va: u64,
    /// Row stride in bytes (4-aligned).
    stride: i32,
    /// Pixel width (always positive).
    width: i32,
    /// Signed pixel height: negative = top-down (buffer row 0 is the top).
    height: i32,
    /// Bits per pixel (1, 4, 8, 16, 24, 32).
    bit_count: u16,
}

/// Resolve an HBITMAP to its DIB pixel surface.
///
/// Compatible bitmaps (`CreateCompatibleBitmap`) return fake handles with no
/// `DibSection` record and no backing buffer, so they are not resolvable
/// (KISS — the caller returns 0). Only `CreateDIBSection` DIBs have pixels to
/// copy.
fn resolve_dib(state: &mut WinApiState, hbm: u64) -> Option<DibView> {
    let dib = state.gdi_state().find_dib(Hbitmap::from(hbm))?;
    Some(DibView {
        bits_va: dib.bits_va,
        stride: dib.stride,
        width: dib.width.max(0),
        height: dib.height,
        bit_count: dib.bit_count,
    })
}

/// Bytes of pixel data per row (width × bytes-per-pixel; the stride may pad).
#[must_use]
fn row_bytes(dib: &DibView) -> usize {
    usize::try_from(dib.width)
        .unwrap_or(0)
        .saturating_mul(usize::from(dib.bit_count.div_ceil(8)))
}

/// Buffer row index for a scanline (scanline 0 = the top of the image).
///
/// Top-down DIBs store scanline 0 in buffer row 0; bottom-up DIBs store it in
/// the LAST buffer row, mirroring the msimg32 lane's row handling. Returns
/// `None` when the scanline lies outside the DIB.
#[must_use]
fn buffer_row_for_scanline(dib: &DibView, scanline: i64) -> Option<usize> {
    let h = i64::from(dib.height.unsigned_abs());
    if scanline < 0 || scanline >= h {
        return None;
    }
    let idx = if dib.height < 0 {
        scanline
    } else {
        h.saturating_sub(1).saturating_sub(scanline)
    };
    usize::try_from(idx).ok()
}

/// The image size in bytes (stride × |height|) — the `biSizeImage` value.
#[must_use]
fn image_size(dib: &DibView) -> u64 {
    let h = u64::from(dib.height.unsigned_abs());
    u64::try_from(dib.stride.max(0))
        .unwrap_or(0)
        .saturating_mul(h)
}

/// Describe the DIB in the caller's `BITMAPINFO` (the `lpvBits == NULL`
/// query mode of `GetDIBits`).
///
/// Writes the fixed 40-byte `BITMAPINFOHEADER`: `biSize` @0, `biWidth` @4
/// (positive), `biHeight` @8 (signed — negative signals top-down),
/// `biPlanes` @12 = 1, `biBitCount` @14, `biCompression` @16 = BI_RGB (0),
/// `biSizeImage` @20. The typed write zero-fills the remaining fields.
fn write_bitmap_info_header(
    engine: &mut dyn wie_cpu::CpuEngine,
    dib: &DibView,
    lpbi: u64,
) -> Result<()> {
    let size = image_size(dib);
    with_typed_write::<BitmapInfoHeader, _, _>(engine, lpbi, |header| {
        header.bi_size = u32::try_from(std::mem::size_of::<BitmapInfoHeader>()).unwrap_or(0);
        header.bi_width = dib.width;
        header.bi_height = dib.height; // signed: negative = top-down
        header.bi_planes = 1;
        header.bi_bit_count = dib.bit_count;
        header.bi_compression = 0; // BI_RGB
        header.bi_size_image = u32::try_from(size).unwrap_or(0);
        Ok(())
    })
    .context("failed to write GetDIBits BITMAPINFOHEADER")
}

/// Copy `cLines` scan lines between a DIB buffer and a guest row buffer.
///
/// `from_dib` selects the direction: `true` copies DIB → guest (`GetDIBits`),
/// `false` copies guest → DIB (`SetDIBits`). Scan lines are numbered from the
/// top of the image and mapped through [`buffer_row_for_scanline`], so the
/// copy honors the DIB's top-down / bottom-up orientation. Returns the number
/// of scan lines actually moved (fewer than `cLines` when the DIB is shorter
/// than `start + cLines`).
fn copy_scan_lines(
    engine: &mut dyn wie_cpu::CpuEngine,
    dib: &DibView,
    start: u32,
    c_lines: u32,
    buf_va: u64,
    from_dib: bool,
) -> Result<u64> {
    let bytes_per_row = row_bytes(dib);
    if bytes_per_row == 0 || buf_va == 0 {
        return Ok(0);
    }
    let stride = usize::try_from(dib.stride.max(0)).unwrap_or(0);
    let mut row = vec![0_u8; bytes_per_row];
    let mut copied = 0_u64;
    for i in 0..c_lines {
        let scanline = i64::from(start) + i64::from(i);
        let Some(buf_row) = buffer_row_for_scanline(dib, scanline) else {
            break;
        };
        let dib_row_va = dib
            .bits_va
            .saturating_add(u64::try_from(buf_row.saturating_mul(stride)).unwrap_or(0));
        let buf_row_va = buf_va.saturating_add(
            u64::try_from(
                usize::try_from(i)
                    .unwrap_or(0)
                    .saturating_mul(bytes_per_row),
            )
            .unwrap_or(0),
        );
        if from_dib {
            if engine.mem_read(dib_row_va, &mut row).is_err()
                || engine.mem_write(buf_row_va, &row).is_err()
            {
                break;
            }
        } else if engine.mem_read(buf_row_va, &mut row).is_err()
            || engine.mem_write(dib_row_va, &row).is_err()
        {
            break;
        }
        copied = copied.saturating_add(1);
    }
    Ok(copied)
}

/// Read the shared 7-argument prologue of `GetDIBits` / `SetDIBits`.
///
/// `int GetDIBits(HDC hdc, HBITMAP hbm, UINT start, UINT cLines, LPVOID
/// lpvBits, LPBITMAPINFO lpbi, UINT usage)` — hdc/hbm/start/cLines in the four
/// register slots, then lpvBits @0x28, lpbi @0x30, usage @0x38. `SetDIBits`
/// has the identical signature.
#[allow(clippy::type_complexity)]
fn read_dib_bits_args(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
) -> Result<(u64, u32, u32, u64, u64)> {
    let engine = &mut *ctx.engine;
    let _hdc = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let hbm = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let start_raw = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let c_lines_raw = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;
    let rsp = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {api_name}"))?;
    let lpv_bits = read_u64(engine, checked_address(rsp, 0x28, "lpvBits"))
        .with_context(|| format!("failed to read {api_name} lpvBits"))?;
    let lpbi = read_u64(engine, checked_address(rsp, 0x30, "lpbi"))
        .with_context(|| format!("failed to read {api_name} lpbi"))?;
    let _usage = read_u32(engine, checked_address(rsp, 0x38, "usage"))
        .with_context(|| format!("failed to read {api_name} usage"))?;
    // UINT args arrive zero-extended or garbage in the high bits; keep the
    // low 32 bits.
    let start = u32::try_from(start_raw & u64::from(u32::MAX)).unwrap_or(0);
    let c_lines = u32::try_from(c_lines_raw & u64::from(u32::MAX)).unwrap_or(0);
    Ok((hbm, start, c_lines, lpv_bits, lpbi))
}

/// Handles `GDI32.dll!GetDIBits` — copy scan lines out of a DIB into a guest
/// buffer, or describe the DIB when `lpvBits` is NULL.
///
/// Returns the number of scan lines copied (or the row count for a size-only
/// query); 0 on failure (unknown bitmap, no buffer, read error).
pub fn handle_get_dib_bits(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let (hbm, start, c_lines, lpv_bits, lpbi) = read_dib_bits_args(ctx, "GetDIBits")?;
    let state = &mut *ctx.state;
    let Some(dib) = resolve_dib(state, hbm) else {
        // A compatible bitmap (`CreateCompatibleBitmap`) has no DIB record.
        // A size-only query still gets a header so callers can detect the
        // pixel format (SDL2's video init probes a 1×1 compatible bitmap);
        // report 32bpp BI_BITFIELDS with the standard RGB888 channel masks.
        if lpv_bits == 0 && lpbi != 0 {
            write_compatible_bitmap_info_header(ctx.engine, lpbi)?;
            let rows = if c_lines > 0 { u64::from(c_lines) } else { 1 };
            return ctx.finish(rows);
        }
        tracing::debug!(hbm, "GetDIBits: unknown DIB");
        return ctx.finish(0);
    };

    if lpv_bits == 0 {
        // Size-only query: describe the DIB, return the requested row count
        // (or the full height when cLines is 0 — the canonical probe).
        if lpbi == 0 {
            return ctx.finish(0);
        }
        write_bitmap_info_header(ctx.engine, &dib, lpbi)?;
        let rows = if c_lines > 0 {
            u64::from(c_lines)
        } else {
            u64::from(dib.height.unsigned_abs())
        };
        return ctx.finish(rows);
    }

    let copied = copy_scan_lines(ctx.engine, &dib, start, c_lines, lpv_bits, true)?;
    ctx.finish(copied)
}

/// Write the 40-byte `BITMAPINFOHEADER` for a 1×1 32bpp `BI_BITFIELDS` bitmap
/// plus the three channel masks, then the header for a compatible bitmap.
fn write_compatible_bitmap_info_header(
    engine: &mut dyn wie_cpu::CpuEngine,
    lpbi: u64,
) -> Result<()> {
    with_typed_write::<BitmapInfoHeader, _, _>(engine, lpbi, |header| {
        header.bi_size = u32::try_from(std::mem::size_of::<BitmapInfoHeader>()).unwrap_or(0);
        header.bi_width = 1;
        header.bi_height = 1;
        header.bi_planes = 1;
        header.bi_bit_count = 32;
        header.bi_compression = 3; // BI_BITFIELDS
        header.bi_size_image = 4;
        Ok(())
    })
    .context("failed to write compatible-bitmap BITMAPINFOHEADER")?;
    // The three BI_BITFIELDS masks follow the header: R, G, B (RGB888).
    let masks = [0x00FF0000_u32, 0x0000FF00, 0x000000FF];
    for (i, mask) in masks.iter().enumerate() {
        let off = u64::try_from(std::mem::size_of::<BitmapInfoHeader>())
            .unwrap_or(0)
            .saturating_add(u64::try_from(i * 4).unwrap_or(0));
        crate::guest_memory::write_u32(
            engine,
            crate::guest_memory::checked_address(lpbi, off, "bmiColors"),
            *mask,
        )?;
    }
    Ok(())
}

/// Handles `GDI32.dll!SetDIBits` — copy scan lines from a guest buffer into a
/// DIB (the reverse of `GetDIBits`).
///
/// Returns the number of scan lines written; 0 on failure.
pub fn handle_set_dib_bits(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let (hbm, start, c_lines, lpv_bits, _lpbi) = read_dib_bits_args(ctx, "SetDIBits")?;
    let state = &mut *ctx.state;
    let Some(dib) = resolve_dib(state, hbm) else {
        tracing::debug!(hbm, "SetDIBits: unknown DIB");
        return ctx.finish(0);
    };
    if lpv_bits == 0 {
        return ctx.finish(0);
    }
    let copied = copy_scan_lines(ctx.engine, &dib, start, c_lines, lpv_bits, false)?;
    ctx.finish(copied)
}
