//! Handles `MSIMG32.dll` — image operations (AlphaBlend, TransparentBlt,
//! GradientFill). String dispatch. Stateless: operates on existing HDCs
//! through the gdi32 state, so no `DllId` slot is needed.

use anyhow::{Context, Result};

use crate::gdi32::{ArgReg, clip_blit_rect, read_arg};
use crate::guest_memory::{checked_address, read_i32, read_u16, read_u32, read_u64, read_uint_at};
use crate::user32::low_i32;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// `BLENDFUNCTION.AlphaFormat` bit: honor the source's per-pixel alpha.
const AC_SRC_ALPHA: u32 = 0x01;
/// `GradientFill` mode: interpolate horizontally between the vertex colors.
const GRADIENT_FILL_RECT_H: u32 = 0;
/// `GradientFill` mode: interpolate vertically between the vertex colors.
const GRADIENT_FILL_RECT_V: u32 = 1;

/// A 32-bpp DIB pixel surface resolved from an HDC.
///
/// Wraps the shared [`crate::gdi32`] resolver: the DC record must have a
/// bitmap selected and that bitmap must be a 32-bpp `CreateDIBSection`
/// record. Returns `None` (callers return FALSE) for window/screen/print
/// DCs, non-32-bpp bitmaps, and unknown handles.
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
fn resolve_dib_surface(state: &mut WinApiState, dc_handle: u64) -> Option<DibSurface> {
    let dib = crate::gdi32::resolve_32bpp_dib(state, dc_handle)?;
    Some(DibSurface {
        bits_va: dib.bits_va,
        stride: dib.stride,
        width: dib.width,
        height: dib.height,
        top_down: dib.top_down,
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

/// Handles `MSIMG32.dll!TransparentBlt` — color-keyed copy.
///
/// `BOOL TransparentBlt(hdcDst, xDst, yDst, wDst, hDst, hdcSrc, xSrc, ySrc,
/// wSrc, hSrc, COLORREF crTransparent)` — 11 args, `crTransparent` at
/// `[rsp+0x58]`. Source pixels equal to the key are skipped; everything else
/// is copied with an opaque alpha (the color key, not alpha, decides
/// transparency — matching real GDI, which ignores the source alpha here).
/// Per-API context names for the shared blit marshalling (static, so the
/// shared body stays allocation-free).
struct BltArgNames {
    /// `[rsp+0x28]`.
    h_dst: &'static str,
    /// `[rsp+0x30]`.
    hdc_src: &'static str,
    /// `[rsp+0x38]`.
    x_src: &'static str,
    /// `[rsp+0x40]`.
    y_src: &'static str,
    /// `[rsp+0x48]` (read and discarded).
    w_src: &'static str,
    /// `[rsp+0x50]` (read and discarded).
    h_src: &'static str,
}

const ALPHABLEND_NAMES: BltArgNames = BltArgNames {
    h_dst: "AlphaBlend hDst",
    hdc_src: "AlphaBlend hdcSrc",
    x_src: "AlphaBlend xSrc",
    y_src: "AlphaBlend ySrc",
    w_src: "AlphaBlend wSrc",
    h_src: "AlphaBlend hSrc",
};

const TRANSPARENTBLT_NAMES: BltArgNames = BltArgNames {
    h_dst: "TransparentBlt hDst",
    hdc_src: "TransparentBlt hdcSrc",
    x_src: "TransparentBlt xSrc",
    y_src: "TransparentBlt ySrc",
    w_src: "TransparentBlt wSrc",
    h_src: "TransparentBlt hSrc",
};

/// The per-pixel operation derived from each handler's final `[rsp+0x58]`
/// argument.
#[derive(Clone, Copy)]
enum BltPixelOp {
    /// `AlphaBlend`: source-over composite with an optional constant alpha
    /// and the source's own alpha channel.
    Blend {
        /// `BLENDFUNCTION.SourceConstantAlpha`.
        const_alpha: u32,
        /// Whether the `AC_SRC_ALPHA` format bit is set.
        use_src_alpha: bool,
    },
    /// `TransparentBlt`: copy unless the pixel matches the color key.
    ColorKey {
        /// The 0RGB key (`COLORREF` low 24 bits).
        key: u32,
    },
}

impl BltPixelOp {
    fn apply(self, sp: u32, dp: u32) -> u32 {
        match self {
            Self::Blend {
                const_alpha,
                use_src_alpha,
            } => blend_pixel(sp, dp, const_alpha, use_src_alpha),
            Self::ColorKey { key } => {
                if sp & 0x00FF_FFFF == key {
                    dp
                } else {
                    (sp & 0x00FF_FFFF) | 0xFF00_0000
                }
            }
        }
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
    blit_dib_to_dib(ctx, "AlphaBlend", &ALPHABLEND_NAMES, |engine, rsp| {
        let blend_bytes = read_uint_at::<4>(
            engine,
            checked_address(rsp, 0x58, "AlphaBlend BLENDFUNCTION"),
        )
        .context("failed to read AlphaBlend BLENDFUNCTION")?;
        // BLENDFUNCTION layout: { BlendOp, BlendFlags, SourceConstantAlpha,
        // AlphaFormat } — the AlphaFormat byte selects per-pixel alpha.
        let const_alpha = u32::from(blend_bytes.get(2).copied().unwrap_or(0));
        let alpha_format = u32::from(blend_bytes.get(3).copied().unwrap_or(0));
        Ok(BltPixelOp::Blend {
            const_alpha,
            use_src_alpha: alpha_format & AC_SRC_ALPHA != 0,
        })
    })
}

/// Handles `MSIMG32.dll!TransparentBlt` — color-keyed copy.
///
/// `BOOL TransparentBlt(hdcDst, xDst, yDst, wDst, hDst, hdcSrc, xSrc, ySrc,
/// wSrc, hSrc, COLORREF crTransparent)` — 11 args, `crTransparent` at
/// `[rsp+0x58]`. Source pixels equal to the key are skipped; everything else
/// is copied with an opaque alpha (the color key, not alpha, decides
/// transparency — matching real GDI, which ignores the source alpha here).
fn handle_transparent_blt(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    blit_dib_to_dib(
        ctx,
        "TransparentBlt",
        &TRANSPARENTBLT_NAMES,
        |engine, rsp| {
            let cr_transparent = read_u32(
                engine,
                checked_address(rsp, 0x58, "TransparentBlt crTransparent"),
            )
            .context("failed to read TransparentBlt crTransparent")?;
            // COLORREF is 0x00RRGGBB; the DIB pixel's low 24 bits are the
            // same fields.
            Ok(BltPixelOp::ColorKey {
                key: cr_transparent & 0x00FF_FFFF,
            })
        },
    )
}

/// Shared `AlphaBlend`/`TransparentBlt` body.
///
/// Marshals the identical ten leading arguments (`rcx`..r9 registers, then
/// six stack slots), resolves both HDCs to 32-bpp DIB surfaces, clips, and
/// runs the caller's pixel operation over the clipped rect. Only the final
/// `[rsp+0x58]` slot read (via `read_pixel_op`) differs per handler.
fn blit_dib_to_dib(
    ctx: &mut HandlerContext<'_>,
    api_name: &'static str,
    names: &BltArgNames,
    read_pixel_op: impl FnOnce(&mut dyn wie_cpu::CpuEngine, u64) -> Result<BltPixelOp>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc_dst = read_arg(engine, ArgReg::Rcx, api_name)?;
    let x_dst = low_i32(read_arg(engine, ArgReg::Rdx, api_name)?, "blit xDst")?;
    let y_dst = low_i32(read_arg(engine, ArgReg::R8, api_name)?, "blit yDst")?;
    let w_dst = low_i32(read_arg(engine, ArgReg::R9, api_name)?, "blit wDst")?;
    let rsp = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {api_name}"))?;
    let h_dst = read_i32(engine, checked_address(rsp, 0x28, names.h_dst))
        .with_context(|| format!("failed to read {}", names.h_dst))?;
    let hdc_src = read_u64(engine, checked_address(rsp, 0x30, names.hdc_src))
        .with_context(|| format!("failed to read {}", names.hdc_src))?;
    let x_src = read_i32(engine, checked_address(rsp, 0x38, names.x_src))
        .with_context(|| format!("failed to read {}", names.x_src))?;
    let y_src = read_i32(engine, checked_address(rsp, 0x40, names.y_src))
        .with_context(|| format!("failed to read {}", names.y_src))?;
    // wSrc/hSrc are ignored: the source and destination rects have the same
    // size by contract, so the destination size drives the copy.
    let _w_src = read_i32(engine, checked_address(rsp, 0x48, names.w_src))
        .with_context(|| format!("failed to read {}", names.w_src))?;
    let _h_src = read_i32(engine, checked_address(rsp, 0x50, names.h_src))
        .with_context(|| format!("failed to read {}", names.h_src))?;
    // The 11th argument differs per handler and yields its pixel operation.
    let op = read_pixel_op(engine, rsp)?;

    let Some(src) = resolve_dib_surface(state, hdc_src) else {
        tracing::debug!("{api_name}: invalid source HDC");
        return ctx.finish(0);
    };
    let Some(dst) = resolve_dib_surface(state, hdc_dst) else {
        tracing::debug!("{api_name}: invalid destination HDC");
        return ctx.finish(0);
    };
    let Some((dx, dy, sx, sy, cw, ch)) = clip_blit_rect(
        dst.width, dst.height, src.width, src.height, x_dst, y_dst, x_src, y_src, w_dst, h_dst,
    ) else {
        // Fully clipped: nothing to blend, but the call still succeeds.
        return ctx.finish(1);
    };
    map_pixels(engine, &src, &dst, dx, dy, sx, sy, cw, ch, |sp, dp| {
        op.apply(sp, dp)
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
    let hdc = read_arg(engine, ArgReg::Rcx, "GradientFill")?;
    let p_vertex = read_arg(engine, ArgReg::Rdx, "GradientFill")?;
    let n_vertex = low_i32(
        read_arg(engine, ArgReg::R8, "GradientFill")?,
        "GradientFill nVertex",
    )?;
    let p_mesh = read_arg(engine, ArgReg::R9, "GradientFill")?;
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
