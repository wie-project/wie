//! Handles `MSIMG32.dll` — image operations (AlphaBlend, TransparentBlt,
//! GradientFill). String dispatch. Stateless: operates on existing HDCs
//! through the gdi32 state, so no `DllId` slot is needed.

use anyhow::{Context, Result};

use crate::guest_memory::{checked_address, read_i32, read_u16, read_u32, read_u64, read_uint_at};
use crate::handles::Hdc;
use crate::user32::low_i32;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// `BLENDFUNCTION.AlphaFormat` bit: honor the source's per-pixel alpha.
const AC_SRC_ALPHA: u32 = 0x01;
/// `GradientFill` mode: interpolate horizontally between the vertex colors.
const GRADIENT_FILL_RECT_H: u32 = 0;
/// `GradientFill` mode: interpolate vertically between the vertex colors.
const GRADIENT_FILL_RECT_V: u32 = 1;

/// A 32-bpp DIB pixel surface resolved from an HDC.
#[derive(Debug, Clone, Copy)]
struct DibSurface {
    /// Guest VA of the pixel buffer.
    bits_va: u64,
    /// Row stride in bytes (4-aligned).
    stride: i32,
    /// Pixel width (always positive).
    width: i32,
    /// Pixel height (always positive).
    height: i32,
    /// `true` = top-down (buffer row 0 is the top); `false` = bottom-up.
    top_down: bool,
}

/// Resolve an HDC to the 32-bpp DIB surface selected into it.
///
/// Mirrors `gdi32::blit::resolve_src_info`: the DC record must have a bitmap
/// selected and that bitmap must be a 32-bpp `CreateDIBSection` record.
/// Returns `None` (callers return FALSE) for window/screen/print DCs,
/// non-32-bpp bitmaps, and unknown handles.
fn resolve_dib_surface(state: &mut WinApiState, dc_handle: u64) -> Option<DibSurface> {
    let (bits_va, stride, width, height, top_down, bit_count) = {
        let gdi = state.gdi_state();
        let dc = gdi.find_dc(Hdc::from(dc_handle))?;
        let dib = gdi.find_dib(dc.selected_bitmap?)?;
        (
            dib.bits_va,
            dib.stride,
            dib.width,
            dib.height,
            dib.height < 0,
            dib.bit_count,
        )
    };
    if bit_count != 32 {
        return None;
    }
    // Saturate at i32::MAX: the old `as` wrapped i32::MIN's magnitude negative.
    let w = i32::try_from(width.unsigned_abs()).unwrap_or(i32::MAX);
    let h = i32::try_from(height.unsigned_abs()).unwrap_or(i32::MAX);
    Some(DibSurface {
        bits_va,
        stride,
        width: w,
        height: h,
        top_down,
    })
}

/// The buffer row index for a guest `y` coordinate.
///
/// Top-down DIBs index directly; bottom-up DIBs flip so guest row 0 is the
/// buffer's last row. `None` when `y` is outside the surface.
#[must_use]
fn buffer_row(surf: &DibSurface, guest_y: i32) -> Option<usize> {
    if guest_y < 0 || guest_y >= surf.height {
        return None;
    }
    let idx = if surf.top_down {
        guest_y
    } else {
        surf.height - 1 - guest_y
    };
    usize::try_from(idx).ok()
}

/// Clip a copy rect to the intersection of both DIB surfaces' bounds.
///
/// Returns the adjusted `(dest_x, dest_y, src_x, src_y, width, height)` or
/// `None` when the rect is fully outside either surface.
#[must_use]
#[allow(clippy::too_many_arguments)]
fn clip_copy_rect(
    dst: &DibSurface,
    src: &DibSurface,
    dest_x: i32,
    dest_y: i32,
    src_x: i32,
    src_y: i32,
    width: i32,
    height: i32,
) -> Option<(i32, i32, i32, i32, i32, i32)> {
    let mut dest_x = dest_x;
    let mut dest_y = dest_y;
    let mut src_x = src_x;
    let mut src_y = src_y;
    let mut width = width;
    let mut height = height;

    // Clip left: a negative dest_x shifts the source right by the same amount.
    if dest_x < 0 {
        src_x = src_x.saturating_sub(dest_x);
        width = width.saturating_add(dest_x);
        dest_x = 0;
    }
    if src_x < 0 {
        dest_x = dest_x.saturating_sub(src_x);
        width = width.saturating_add(src_x);
        src_x = 0;
    }
    // Clip top.
    if dest_y < 0 {
        src_y = src_y.saturating_sub(dest_y);
        height = height.saturating_add(dest_y);
        dest_y = 0;
    }
    if src_y < 0 {
        dest_y = dest_y.saturating_sub(src_y);
        height = height.saturating_add(src_y);
        src_y = 0;
    }
    // Clip right / bottom against both surfaces.
    let dest_right = dest_x.saturating_add(width).min(dst.width);
    width = dest_right.saturating_sub(dest_x);
    let src_right = src_x.saturating_add(width).min(src.width);
    width = src_right.saturating_sub(src_x);
    let dest_bottom = dest_y.saturating_add(height).min(dst.height);
    height = dest_bottom.saturating_sub(dest_y);
    let src_bottom = src_y.saturating_add(height).min(src.height);
    height = src_bottom.saturating_sub(src_y);

    if width <= 0 || height <= 0 {
        return None;
    }
    Some((dest_x, dest_y, src_x, src_y, width, height))
}

// Reusable scratch buffer for the per-row src/dst spans of an MSIMG32 copy.
//
// Growth-only like `gdi32::blit::BLIT_SCRATCH`: repeated AlphaBlend calls in
// one repaint cycle stop re-allocating after the first, largest copy. Guests
// paint on a single thread and `mem_read`/`mem_write` are leaf operations
// that never re-enter msimg32, so the `RefCell` borrow inside the closure
// cannot alias.
std::thread_local! {
    static MSIMG32_SCRATCH: std::cell::RefCell<Vec<u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Read a BGRA pixel from a row buffer (out-of-range reads yield 0).
#[must_use]
fn read_pixel(row: &[u8], px: usize) -> u32 {
    let start = px.saturating_mul(4);
    let Some(bytes) = row.get(start..start.saturating_add(4)) else {
        return 0;
    };
    let mut arr = [0_u8; 4];
    arr.copy_from_slice(bytes);
    u32::from_le_bytes(arr)
}

/// Write a BGRA pixel into a row buffer (out-of-range writes are dropped).
fn write_pixel(row: &mut [u8], px: usize, value: u32) {
    let start = px.saturating_mul(4);
    let Some(bytes) = row.get_mut(start..start.saturating_add(4)) else {
        return;
    };
    bytes.copy_from_slice(&value.to_le_bytes());
}

/// Run `op(src_px, dst_px) -> dst_px` over the clipped copy rect.
///
/// Reads each source row and destination row through the engine once and
/// writes the blended row back, so the per-pixel cost stays on the host. The
/// pixel values are `0xAARRGGBB` (the LE bytes of a BGRA DIB pixel).
#[allow(clippy::too_many_arguments)]
fn map_pixels(
    engine: &mut dyn wie_cpu::CpuEngine,
    src: &DibSurface,
    dst: &DibSurface,
    dest_x: i32,
    dest_y: i32,
    src_x: i32,
    src_y: i32,
    width: i32,
    height: i32,
    op: impl Fn(u32, u32) -> u32,
) -> Result<()> {
    let cw = usize::try_from(width).unwrap_or(0);
    let ch = usize::try_from(height).unwrap_or(0);
    let row_bytes = cw.saturating_mul(4);
    if row_bytes == 0 || ch == 0 {
        return Ok(());
    }
    let src_stride = usize::try_from(src.stride).unwrap_or(0);
    let dst_stride = usize::try_from(dst.stride).unwrap_or(0);
    MSIMG32_SCRATCH.with(|cell| {
        let mut guard = cell.borrow_mut();
        let need = row_bytes.saturating_mul(2);
        if guard.len() < need {
            guard.resize(need, 0);
        }
        // split_at_mut on a retained (larger) buffer still yields exactly
        // `row_bytes` for the source half; the destination half starts at
        // offset `row_bytes` so the two halves never overlap.
        let (src_row, rest) = guard.split_at_mut(row_bytes);
        let Some(dst_row) = rest.get_mut(..row_bytes) else {
            return Ok(());
        };
        for row in 0..ch {
            let guest_sy = src_y.saturating_add(i32::try_from(row).unwrap_or(0));
            let guest_dy = dest_y.saturating_add(i32::try_from(row).unwrap_or(0));
            let Some(src_buf_row) = buffer_row(src, guest_sy) else {
                continue;
            };
            let Some(dst_buf_row) = buffer_row(dst, guest_dy) else {
                continue;
            };
            let src_va = src
                .bits_va
                .saturating_add(u64::try_from(src_buf_row.saturating_mul(src_stride)).unwrap_or(0))
                .saturating_add(
                    u64::try_from(usize::try_from(src_x.max(0)).unwrap_or(0).saturating_mul(4))
                        .unwrap_or(0),
                );
            let dst_va = dst
                .bits_va
                .saturating_add(u64::try_from(dst_buf_row.saturating_mul(dst_stride)).unwrap_or(0))
                .saturating_add(
                    u64::try_from(
                        usize::try_from(dest_x.max(0))
                            .unwrap_or(0)
                            .saturating_mul(4),
                    )
                    .unwrap_or(0),
                );
            engine.mem_read(src_va, src_row)?;
            engine.mem_read(dst_va, dst_row)?;
            for px in 0..cw {
                let out = op(read_pixel(src_row, px), read_pixel(dst_row, px));
                write_pixel(dst_row, px, out);
            }
            engine.mem_write(dst_va, dst_row)?;
        }
        Ok(())
    })
}

/// Per-pixel source-over compositing (straight alpha).
///
/// `const_alpha` is `BLENDFUNCTION.SourceConstantAlpha` (255 = no modulation).
/// When `use_src_alpha` (the `AC_SRC_ALPHA` format bit) is set, the source's
/// own alpha channel scales the blend; otherwise the source is treated as
/// opaque. Channels blend with the classic straight-alpha formula and the
/// output alpha accumulates source-over.
#[must_use]
fn blend_pixel(src_px: u32, dst_px: u32, const_alpha: u32, use_src_alpha: bool) -> u32 {
    let src_alpha = (src_px >> 24) & 0xFF;
    let base_alpha = if use_src_alpha { src_alpha } else { 0xFF };
    let alpha = base_alpha.saturating_mul(const_alpha) / 255;
    let inv = 255 - alpha;
    let dst_alpha = (dst_px >> 24) & 0xFF;
    let out_alpha = alpha + dst_alpha.saturating_mul(inv) / 255;
    let blend_channel =
        |sc: u32, dc: u32| (sc.saturating_mul(alpha) + dc.saturating_mul(inv)) / 255;
    (out_alpha << 24)
        | (blend_channel((src_px >> 16) & 0xFF, (dst_px >> 16) & 0xFF) << 16)
        | (blend_channel((src_px >> 8) & 0xFF, (dst_px >> 8) & 0xFF) << 8)
        | blend_channel(src_px & 0xFF, dst_px & 0xFF)
}

/// Dispatch an `MSIMG32.dll` export by name (case-insensitive).
pub fn dispatch_msimg32(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "alphablend" => Ok(Some(handle_alpha_blend(ctx)?)),
        "transparentblt" => Ok(Some(handle_transparent_blt(ctx)?)),
        "gradientfill" => Ok(Some(handle_gradient_fill(ctx)?)),
        _ => Ok(None),
    }
}

/// Handles `MSIMG32.dll!AlphaBlend` — per-pixel source-alpha compositing.
///
/// `BOOL AlphaBlend(hdcDst, xDst, yDst, wDst, hDst, hdcSrc, xSrc, ySrc, wSrc,
/// hSrc, BLENDFUNCTION blendFunction)` — 11 args, so the 4-byte
/// `blendFunction` (passed by value) sits at `[rsp+0x58]`. The copy is sized
/// by the destination rect; the source rect offset is honored. Returns FALSE
/// (0) when either HDC does not resolve to a 32-bpp DIB surface.
fn handle_alpha_blend(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc_dst = engine
        .read_rcx()
        .context("failed to read RCX for AlphaBlend")?;
    let x_dst = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for AlphaBlend")?,
        "AlphaBlend xDst",
    )?;
    let y_dst = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for AlphaBlend")?,
        "AlphaBlend yDst",
    )?;
    let w_dst = low_i32(
        engine
            .read_r9()
            .context("failed to read R9 for AlphaBlend")?,
        "AlphaBlend wDst",
    )?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for AlphaBlend")?;
    let h_dst = read_i32(engine, checked_address(rsp, 0x28, "AlphaBlend hDst"))
        .context("failed to read AlphaBlend hDst")?;
    let hdc_src = read_u64(engine, checked_address(rsp, 0x30, "AlphaBlend hdcSrc"))
        .context("failed to read AlphaBlend hdcSrc")?;
    let x_src = read_i32(engine, checked_address(rsp, 0x38, "AlphaBlend xSrc"))
        .context("failed to read AlphaBlend xSrc")?;
    let y_src = read_i32(engine, checked_address(rsp, 0x40, "AlphaBlend ySrc"))
        .context("failed to read AlphaBlend ySrc")?;
    // wSrc/hSrc are ignored: the source and destination rects have the same
    // size by contract, so the destination size drives the copy.
    let _w_src = read_i32(engine, checked_address(rsp, 0x48, "AlphaBlend wSrc"))
        .context("failed to read AlphaBlend wSrc")?;
    let _h_src = read_i32(engine, checked_address(rsp, 0x50, "AlphaBlend hSrc"))
        .context("failed to read AlphaBlend hSrc")?;
    let blend_bytes = read_uint_at::<4>(
        engine,
        checked_address(rsp, 0x58, "AlphaBlend BLENDFUNCTION"),
    )
    .context("failed to read AlphaBlend BLENDFUNCTION")?;
    // BLENDFUNCTION layout: { BlendOp, BlendFlags, SourceConstantAlpha,
    // AlphaFormat } — the AlphaFormat byte selects per-pixel alpha.
    let const_alpha = u32::from(blend_bytes.get(2).copied().unwrap_or(0));
    let alpha_format = u32::from(blend_bytes.get(3).copied().unwrap_or(0));
    let use_src_alpha = alpha_format & AC_SRC_ALPHA != 0;

    let Some(src) = resolve_dib_surface(state, hdc_src) else {
        tracing::debug!("AlphaBlend: invalid source HDC");
        return ctx.finish(0);
    };
    let Some(dst) = resolve_dib_surface(state, hdc_dst) else {
        tracing::debug!("AlphaBlend: invalid destination HDC");
        return ctx.finish(0);
    };
    let Some((dx, dy, sx, sy, cw, ch)) =
        clip_copy_rect(&dst, &src, x_dst, y_dst, x_src, y_src, w_dst, h_dst)
    else {
        // Fully clipped: nothing to blend, but the call still succeeds.
        return ctx.finish(1);
    };
    map_pixels(engine, &src, &dst, dx, dy, sx, sy, cw, ch, |sp, dp| {
        blend_pixel(sp, dp, const_alpha, use_src_alpha)
    })?;
    ctx.finish(1)
}

/// Handles `MSIMG32.dll!TransparentBlt` — color-keyed copy.
///
/// `BOOL TransparentBlt(hdcDst, xDst, yDst, wDst, hDst, hdcSrc, xSrc, ySrc,
/// wSrc, hSrc, COLORREF crTransparent)` — 11 args, `crTransparent` at
/// `[rsp+0x58]`. Source pixels equal to the key are skipped; everything else
/// is copied with an opaque alpha (the color key, not alpha, decides
/// transparency — matching real GDI, which ignores the source alpha here).
fn handle_transparent_blt(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc_dst = engine
        .read_rcx()
        .context("failed to read RCX for TransparentBlt")?;
    let x_dst = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for TransparentBlt")?,
        "TransparentBlt xDst",
    )?;
    let y_dst = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for TransparentBlt")?,
        "TransparentBlt yDst",
    )?;
    let w_dst = low_i32(
        engine
            .read_r9()
            .context("failed to read R9 for TransparentBlt")?,
        "TransparentBlt wDst",
    )?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for TransparentBlt")?;
    let h_dst = read_i32(engine, checked_address(rsp, 0x28, "TransparentBlt hDst"))
        .context("failed to read TransparentBlt hDst")?;
    let hdc_src = read_u64(engine, checked_address(rsp, 0x30, "TransparentBlt hdcSrc"))
        .context("failed to read TransparentBlt hdcSrc")?;
    let x_src = read_i32(engine, checked_address(rsp, 0x38, "TransparentBlt xSrc"))
        .context("failed to read TransparentBlt xSrc")?;
    let y_src = read_i32(engine, checked_address(rsp, 0x40, "TransparentBlt ySrc"))
        .context("failed to read TransparentBlt ySrc")?;
    let _w_src = read_i32(engine, checked_address(rsp, 0x48, "TransparentBlt wSrc"))
        .context("failed to read TransparentBlt wSrc")?;
    let _h_src = read_i32(engine, checked_address(rsp, 0x50, "TransparentBlt hSrc"))
        .context("failed to read TransparentBlt hSrc")?;
    let cr_transparent = read_u32(
        engine,
        checked_address(rsp, 0x58, "TransparentBlt crTransparent"),
    )
    .context("failed to read TransparentBlt crTransparent")?;
    // COLORREF is 0x00RRGGBB; the DIB pixel's low 24 bits are the same fields.
    let key = cr_transparent & 0x00FF_FFFF;

    let Some(src) = resolve_dib_surface(state, hdc_src) else {
        tracing::debug!("TransparentBlt: invalid source HDC");
        return ctx.finish(0);
    };
    let Some(dst) = resolve_dib_surface(state, hdc_dst) else {
        tracing::debug!("TransparentBlt: invalid destination HDC");
        return ctx.finish(0);
    };
    let Some((dx, dy, sx, sy, cw, ch)) =
        clip_copy_rect(&dst, &src, x_dst, y_dst, x_src, y_src, w_dst, h_dst)
    else {
        return ctx.finish(1);
    };
    map_pixels(engine, &src, &dst, dx, dy, sx, sy, cw, ch, |sp, dp| {
        if sp & 0x00FF_FFFF == key {
            dp
        } else {
            (sp & 0x00FF_FFFF) | 0xFF00_0000
        }
    })?;
    ctx.finish(1)
}

/// A guest `TRIVERTEX` (16 bytes: `x`/`y` LONGs then four `COLOR16`s).
struct TriVertex {
    x: i32,
    y: i32,
    red: u32,
    green: u32,
    blue: u32,
    alpha: u32,
}

/// Read a `TRIVERTEX` from guest memory.
///
/// `COLOR16` channels are 0..65535 values scaled to 0..255 by `>> 8`.
fn read_trivertex(engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Result<TriVertex> {
    Ok(TriVertex {
        x: read_i32(engine, checked_address(va, 0, "TRIVERTEX.x"))?,
        y: read_i32(engine, checked_address(va, 4, "TRIVERTEX.y"))?,
        red: u32::from(read_u16(engine, checked_address(va, 8, "TRIVERTEX.Red"))?) >> 8,
        green: u32::from(read_u16(
            engine,
            checked_address(va, 10, "TRIVERTEX.Green"),
        )?) >> 8,
        blue: u32::from(read_u16(engine, checked_address(va, 12, "TRIVERTEX.Blue"))?) >> 8,
        alpha: u32::from(read_u16(
            engine,
            checked_address(va, 14, "TRIVERTEX.Alpha"),
        )?) >> 8,
    })
}

/// Linear interpolation of one 8-bit channel across a rect span.
///
/// `num` is the pixel's offset within the span and `den` the span length
/// (≥ 1). A 1-px span yields the start color, matching a degenerate rect.
#[must_use]
fn lerp_channel(start: u32, end: u32, num: i32, den: i32) -> u32 {
    let diff = i64::from(end) - i64::from(start);
    let value = i64::from(start) + diff.saturating_mul(i64::from(num)) / i64::from(den);
    u32::try_from(value.clamp(0, 255)).unwrap_or(0)
}

/// Fill the axis-aligned rect `[x0,x1) × [y0,y1)` of `dst` with a linear
/// gradient from `c0` to `c1`.
///
/// `horizontal` selects RECT_H (interpolate along x) vs RECT_V (along y); the
/// span is measured on the interpolated axis. The rect is clipped to `dst`.
#[allow(clippy::too_many_arguments)]
fn fill_gradient_rect(
    engine: &mut dyn wie_cpu::CpuEngine,
    dst: &DibSurface,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    c0: (u32, u32, u32, u32),
    c1: (u32, u32, u32, u32),
    horizontal: bool,
) -> Result<()> {
    let left = x0.max(0);
    let top = y0.max(0);
    let right = x1.min(dst.width);
    let bottom = y1.min(dst.height);
    if left >= right || top >= bottom {
        return Ok(());
    }
    let cw = usize::try_from(right.saturating_sub(left)).unwrap_or(0);
    let ch = usize::try_from(bottom.saturating_sub(top)).unwrap_or(0);
    let row_bytes = cw.saturating_mul(4);
    if row_bytes == 0 || ch == 0 {
        return Ok(());
    }
    // Span length on the interpolated axis (≥ 1 so a 1-px rect yields c0).
    let span = if horizontal {
        x1.saturating_sub(x0).max(1)
    } else {
        y1.saturating_sub(y0).max(1)
    };
    let (r0, g0, b0, a0) = c0;
    let (r1, g1, b1, a1) = c1;
    let stride = usize::try_from(dst.stride).unwrap_or(0);
    let mut row = vec![0_u8; row_bytes];
    for y in top..bottom {
        let Some(buf_row) = buffer_row(dst, y) else {
            continue;
        };
        let va = dst
            .bits_va
            .saturating_add(u64::try_from(buf_row.saturating_mul(stride)).unwrap_or(0))
            .saturating_add(
                u64::try_from(usize::try_from(left).unwrap_or(0).saturating_mul(4)).unwrap_or(0),
            );
        for (px, slot) in row.chunks_exact_mut(4).enumerate() {
            let x = left.saturating_add(i32::try_from(px).unwrap_or(0));
            let num = if horizontal {
                x.saturating_sub(x0)
            } else {
                y.saturating_sub(y0)
            };
            let red = lerp_channel(r0, r1, num, span);
            let green = lerp_channel(g0, g1, num, span);
            let blue = lerp_channel(b0, b1, num, span);
            let alpha = lerp_channel(a0, a1, num, span);
            slot.copy_from_slice(
                &((alpha << 24) | (red << 16) | (green << 8) | blue).to_le_bytes(),
            );
        }
        engine.mem_write(va, &row)?;
    }
    Ok(())
}

/// Handles `MSIMG32.dll!GradientFill` — linear gradient into the target rect.
///
/// `BOOL GradientFill(hdc, pVertex, nVertex, pMesh, nMesh, ulMode)` — 6 args,
/// `nMesh` at `[rsp+0x28]` and `ulMode` at `[rsp+0x30]`. Each `GRADIENT_RECT`
/// entry references two `TRIVERTEX` indices (UpperLeft = start, LowerRight =
/// end); the filled rect spans the two vertices' coordinates. Only the
/// RECT_H (0) and RECT_V (1) modes are supported (KISS); the TRIANGLE modes
/// return FALSE.
fn handle_gradient_fill(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for GradientFill")?;
    let p_vertex = engine
        .read_rdx()
        .context("failed to read RDX for GradientFill")?;
    let n_vertex = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for GradientFill")?,
        "GradientFill nVertex",
    )?;
    let p_mesh = engine
        .read_r9()
        .context("failed to read R9 for GradientFill")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for GradientFill")?;
    let n_mesh = read_i32(engine, checked_address(rsp, 0x28, "GradientFill nMesh"))
        .context("failed to read GradientFill nMesh")?;
    let ul_mode = read_u32(engine, checked_address(rsp, 0x30, "GradientFill ulMode"))
        .context("failed to read GradientFill ulMode")?;

    let horizontal = match ul_mode {
        GRADIENT_FILL_RECT_H => true,
        GRADIENT_FILL_RECT_V => false,
        _ => return ctx.finish(0), // TRIANGLE modes unsupported (KISS)
    };
    if p_vertex == 0 || p_mesh == 0 || n_vertex <= 0 || n_mesh <= 0 {
        return ctx.finish(0);
    }
    let Some(dst) = resolve_dib_surface(state, hdc) else {
        tracing::debug!("GradientFill: invalid HDC");
        return ctx.finish(0);
    };
    let n_vertex_u = u32::try_from(n_vertex).unwrap_or(0);

    for entry in 0..n_mesh {
        let mesh_va = checked_address(
            p_mesh,
            u64::try_from(entry).unwrap_or(0).saturating_mul(8),
            "GradientFill mesh entry",
        );
        let upper_left = read_u32(
            engine,
            checked_address(mesh_va, 0, "GRADIENT_RECT.UpperLeft"),
        )?;
        let lower_right = read_u32(
            engine,
            checked_address(mesh_va, 4, "GRADIENT_RECT.LowerRight"),
        )?;
        if upper_left >= n_vertex_u || lower_right >= n_vertex_u {
            continue;
        }
        let start = read_trivertex(
            engine,
            checked_address(
                p_vertex,
                u64::from(upper_left).saturating_mul(16),
                "GradientFill start TRIVERTEX",
            ),
        )?;
        let end = read_trivertex(
            engine,
            checked_address(
                p_vertex,
                u64::from(lower_right).saturating_mul(16),
                "GradientFill end TRIVERTEX",
            ),
        )?;
        fill_gradient_rect(
            engine,
            &dst,
            start.x,
            start.y,
            end.x,
            end.y,
            (start.red, start.green, start.blue, start.alpha),
            (end.red, end.green, end.blue, end.alpha),
            horizontal,
        )?;
    }
    ctx.finish(1)
}
