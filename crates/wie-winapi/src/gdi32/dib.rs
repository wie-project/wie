//! GDI32 DIB round-trip handlers — `GetDIBits` / `SetDIBits`.
//!
//! Both APIs resolve the `HBITMAP` straight to its `DibSection` record (the
//! pixel buffer `CreateDIBSection` allocated on the guest heap), so the HDC
//! parameter is ignored — matching real GDI, which ignores the DC for DIB
//! sections, and the micro-exes, which pass NULL.

use anyhow::{Context, Result};

use crate::gdi32::blit::{IRect, resolve_dest_info};
use crate::guest_layout::BitmapInfoHeader;
use crate::guest_memory::{checked_address, read_u32, read_u64, with_typed_write};
use crate::handles::Hbitmap;
use crate::user32::low_i32;
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

/// Read the destination geometry from a `BITMAPINFO` header.
///
/// Returns `(image_width, image_height, bottom_up, bit_count)`; `None` when the
/// header is absent or has no valid 24/32-bpp image. `biWidth` @+4, `biHeight`
/// @+8 (signed; negative = top-down), `biBitCount` @+14.
fn read_bmi_geometry(
    engine: &mut dyn wie_cpu::CpuEngine,
    lpbi: u64,
) -> Result<Option<(u32, u32, bool, u16)>> {
    if lpbi == 0 {
        return Ok(None);
    }
    let mut b = [0_u8; 16];
    if engine.mem_read(lpbi, &mut b).is_err() {
        return Ok(None);
    }
    let w = i32::from_le_bytes(b[4..8].try_into().unwrap_or([0; 4]));
    let h = i32::from_le_bytes(b[8..12].try_into().unwrap_or([0; 4]));
    let bpp = u16::from_le_bytes(b[14..16].try_into().unwrap_or([0; 2]));
    if w <= 0 || h == 0 || (bpp != 24 && bpp != 32) {
        return Ok(None);
    }
    Ok(Some((
        u32::try_from(w).unwrap_or(0),
        h.unsigned_abs(),
        h > 0, // a positive biHeight stores rows bottom-up
        bpp,
    )))
}

/// Copy a DIB source region into a window present surface, nearest-neighbour
/// scaled when the source and dest extents differ (StretchDIBits).
///
/// The guest buffer is laid out with full-image stride (`image_w × bpp`),
/// 4-byte aligned. `bottom_up` mirrors real GDI for positive `biHeight`
/// (buffer row 0 is the image's bottom row, so image row 0 maps to the last
/// buffer row). Sampling clips to the surface; out-of-bounds source pixels are
/// skipped, so an over-large destination simply crops.
#[allow(clippy::too_many_arguments)]
fn blit_dib_region(
    engine: &mut dyn wie_cpu::CpuEngine,
    surface_w: u32,
    surface_h: u32,
    dest: &mut [u32],
    src_va: u64,
    image_w: u32,
    image_h: u32,
    bpp: u16,
    bottom_up: bool,
    surface_x: i32,
    surface_y: i32,
    dst_cx: i32,
    dst_cy: i32,
    src_x: i32,
    src_y: i32,
    src_cx: i32,
    src_cy: i32,
) {
    if dst_cx <= 0 || dst_cy <= 0 || src_cx <= 0 || src_cy <= 0 {
        return;
    }
    let sw = i32::try_from(surface_w).unwrap_or(0).max(1);
    let sh = i32::try_from(surface_h).unwrap_or(0).max(1);
    // Clip the dest rect to the surface; the source origin shifts with it.
    let clip_left = surface_x.max(0);
    let clip_top = surface_y.max(0);
    let clip_right = surface_x.saturating_add(dst_cx).min(sw);
    let clip_bottom = surface_y.saturating_add(dst_cy).min(sh);
    if clip_left >= clip_right || clip_top >= clip_bottom {
        return;
    }

    let bpp_bytes = usize::from(bpp / 8);
    let stride = ((usize::try_from(image_w).unwrap_or(0)).saturating_mul(bpp_bytes) + 3) & !3;
    let src_w_i = i32::try_from(image_w).unwrap_or(0).max(1);
    let src_h_i = i32::try_from(image_h).unwrap_or(0).max(1);
    let surf_w_us = usize::try_from(surface_w).unwrap_or(0);

    let mut row = vec![0_u8; stride];
    for dy in clip_top..clip_bottom {
        // Nearest-neighbour source row for this dest row (downsampling safe).
        let in_row = dy - surface_y;
        let sy = if dst_cy == src_cy {
            src_y.saturating_add(in_row)
        } else {
            src_y.saturating_add(in_row.saturating_mul(src_cy) / dst_cy)
        };
        if sy < 0 || sy >= src_h_i {
            continue;
        }
        // Buffer row index: bottom-up stores the image flipped.
        let buf_row = if bottom_up {
            src_h_i.saturating_sub(1).saturating_sub(sy)
        } else {
            sy
        };
        let row_va = src_va.saturating_add(
            u64::try_from(buf_row.saturating_mul(i32::try_from(stride).unwrap_or(0))).unwrap_or(0),
        );
        if engine.mem_read(row_va, &mut row).is_err() {
            continue;
        }
        let dst_row_base = usize::try_from(dy).unwrap_or(0).saturating_mul(surf_w_us);
        for dx in clip_left..clip_right {
            let in_col = dx - surface_x;
            let sx = if dst_cx == src_cx {
                src_x.saturating_add(in_col)
            } else {
                src_x.saturating_add(in_col.saturating_mul(src_cx) / dst_cx)
            };
            if sx < 0 || sx >= src_w_i {
                continue;
            }
            let byte = usize::try_from(sx).unwrap_or(0).saturating_mul(bpp_bytes);
            let Some(src_slice) = row.get(byte..byte.saturating_add(bpp_bytes)) else {
                continue;
            };
            // 0x00RRGGBB for the present surface (mask_bgra, or manual for 24bpp).
            let px = if bpp == 32 {
                u32::from_le_bytes(src_slice.try_into().unwrap_or([0, 0, 0, 0])) & 0x00FF_FFFF
            } else {
                let (r, g, b) = (src_slice[2], src_slice[1], src_slice[0]);
                (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
            };
            let idx = dst_row_base.saturating_add(usize::try_from(dx).unwrap_or(0));
            if let Some(slot) = dest.get_mut(idx) {
                *slot = px;
            }
        }
    }
}

/// Wire a device-DIB blit (`SetDIBitsToDevice` / `StretchDIBits`) into the
/// present lane.
///
/// Resolves the destination HDC to its top-level window surface, blits the
/// guest pixel buffer into it, marks the region dirty and defers a publish —
/// exactly the `ensure_surface` + `mark_dirty` + `publish_deferred` pattern
/// `BitBlt` uses (`gdi32/blit.rs`). Non-window DCs (memory/screen) have no host
/// surface and publish nothing. Returns `Ok(true)` when a window surface was
/// written.
#[allow(clippy::too_many_arguments)]
fn publish_device_dib_blit(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    hdc: u64,
    bits_va: u64,
    lpbi: u64,
    dest_x: i32,
    dest_y: i32,
    dst_cx: i32,
    dst_cy: i32,
    src_x: i32,
    src_y: i32,
    src_cx: i32,
    src_cy: i32,
) -> Result<bool> {
    let Some(info) = resolve_dest_info(state, hdc) else {
        return Ok(false); // memory/screen DC — no host surface to paint
    };
    let Some((image_w, image_h, bottom_up, bpp)) = read_bmi_geometry(engine, lpbi)? else {
        return Ok(false);
    };
    state
        .present()
        .ensure_surface(info.hwnd, info.width, info.height);
    let surface_x = dest_x.saturating_add(info.offset_x);
    let surface_y = dest_y.saturating_add(info.offset_y);
    {
        let Some(dest) = state
            .present()
            .surfaces
            .get_mut(&info.hwnd)
            .map(|s| &mut s.pixels[..])
        else {
            return Ok(false);
        };
        blit_dib_region(
            engine,
            info.width,
            info.height,
            dest,
            bits_va,
            image_w,
            image_h,
            bpp,
            bottom_up,
            surface_x,
            surface_y,
            dst_cx,
            dst_cy,
            src_x,
            src_y,
            src_cx,
            src_cy,
        );
    }
    // Report the clipped blit region as dirty so the deferred publish uploads
    // only the repainted area (under-marking would corrupt the GPU staging).
    let dirty_w = i32::try_from(info.width).unwrap_or(0);
    let dirty_h = i32::try_from(info.height).unwrap_or(0);
    let left = surface_x.clamp(0, dirty_w);
    let top = surface_y.clamp(0, dirty_h);
    let right = surface_x.saturating_add(dst_cx).clamp(0, dirty_w);
    let bottom = surface_y.saturating_add(dst_cy).clamp(0, dirty_h);
    if right > left && bottom > top {
        state.present().mark_dirty(
            info.hwnd,
            IRect {
                left,
                top,
                right,
                bottom,
            },
        );
    }
    state.present().publish_deferred(info.hwnd);
    Ok(true)
}

/// Handles `GDI32.dll!SetDIBitsToDevice` — a top-level DIB blit into the
/// window's present surface.
///
/// `int SetDIBitsToDevice(HDC hdc, int xDest, int yDest, DWORD w, DWORD h,
/// int xSrc, int ySrc, UINT StartScan, UINT cLines, const VOID *lpvBits,
/// const BITMAPINFO *lpbmi, UINT ColorUse)`.
///
/// Pixels are copied 1:1 from `lpvBits` into the destination window surface and
/// a frame is published (the present-lane path). The documented return value —
/// the number of scan lines set (`cLines`) — is preserved so SDL's video-init
/// probe sees success. Returns 0 when the source bit buffer is null.
pub fn handle_set_dib_bits_to_device(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine.read_rcx()?;
    let x_dest = low_i32(engine.read_rdx()?, "SetDIBitsToDevice xDest")?;
    let y_dest = low_i32(engine.read_r8()?, "SetDIBitsToDevice yDest")?;
    let w = low_i32(engine.read_r9()?, "SetDIBitsToDevice w")?;
    let rsp = engine.read_rsp()?;
    let h = low_i32(
        u64::from(read_u32(engine, checked_address(rsp, 0x28, "h"))?),
        "SetDIBitsToDevice h",
    )?;
    let x_src = low_i32(
        u64::from(read_u32(engine, checked_address(rsp, 0x30, "xSrc"))?),
        "SetDIBitsToDevice xSrc",
    )?;
    let y_src = low_i32(
        u64::from(read_u32(engine, checked_address(rsp, 0x38, "ySrc"))?),
        "SetDIBitsToDevice ySrc",
    )?;
    let _start_scan = read_u32(engine, checked_address(rsp, 0x40, "StartScan"))?;
    let c_lines = read_u32(engine, checked_address(rsp, 0x48, "cLines"))?;
    let lpv_bits = read_u64(engine, checked_address(rsp, 0x50, "lpvBits"))?;
    let lpbmi = read_u64(engine, checked_address(rsp, 0x58, "lpbmi"))?;

    if lpv_bits == 0 {
        return ctx.finish(0); // null source buffer: no scan lines set
    }
    publish_device_dib_blit(
        state, engine, hdc, lpv_bits, lpbmi, x_dest, y_dest, w, h, x_src, y_src, w, h,
    )?;
    ctx.finish(u64::from(c_lines))
}

/// Handles `GDI32.dll!StretchDIBits` — a scaled top-level DIB blit.
///
/// `int StretchDIBits(HDC hdc, int XDest, int YDest, int nDestWidth, int
/// nDestHeight, int XSrc, int YSrc, int nSrcWidth, int nSrcHeight, const VOID
/// *lpBits, const BITMAPINFO *lpBitsInfo, UINT iUsage, DWORD dwRop)`.
///
/// The source region is nearest-neighbour scaled into the destination window
/// surface and a frame is published. The documented return value — the number
/// of scan lines copied, sized by the source height (`nSrcHeight`) — is
/// preserved. Returns 0 when the source bit buffer is null.
pub fn handle_stretch_dib_bits(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine.read_rcx()?;
    let x_dest = low_i32(engine.read_rdx()?, "StretchDIBits XDest")?;
    let y_dest = low_i32(engine.read_r8()?, "StretchDIBits YDest")?;
    let n_dest_width = low_i32(engine.read_r9()?, "StretchDIBits nDestWidth")?;
    let rsp = engine.read_rsp()?;
    let n_dest_height = low_i32(
        u64::from(read_u32(engine, checked_address(rsp, 0x28, "nDestHeight"))?),
        "StretchDIBits nDestHeight",
    )?;
    let x_src = low_i32(
        u64::from(read_u32(engine, checked_address(rsp, 0x30, "XSrc"))?),
        "StretchDIBits XSrc",
    )?;
    let y_src = low_i32(
        u64::from(read_u32(engine, checked_address(rsp, 0x38, "YSrc"))?),
        "StretchDIBits YSrc",
    )?;
    let n_src_width = low_i32(
        u64::from(read_u32(engine, checked_address(rsp, 0x40, "nSrcWidth"))?),
        "StretchDIBits nSrcWidth",
    )?;
    let n_src_height = low_i32(
        u64::from(read_u32(engine, checked_address(rsp, 0x48, "nSrcHeight"))?),
        "StretchDIBits nSrcHeight",
    )?;
    let lp_bits = read_u64(engine, checked_address(rsp, 0x50, "lpBits"))?;
    let lp_bits_info = read_u64(engine, checked_address(rsp, 0x58, "lpBitsInfo"))?;

    if lp_bits == 0 {
        return ctx.finish(0); // null source buffer: no scan lines copied
    }
    publish_device_dib_blit(
        state,
        engine,
        hdc,
        lp_bits,
        lp_bits_info,
        x_dest,
        y_dest,
        n_dest_width,
        n_dest_height,
        x_src,
        y_src,
        n_src_width,
        n_src_height,
    )?;
    ctx.finish(u64::try_from(n_src_height.max(0)).unwrap_or(0))
}
