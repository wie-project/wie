//! Native `SDL_FillRect` / `SDL_FillRects`.
//!
//! SDL's fill picks a `fill_function` by destination bytes-per-pixel and calls
//! it per clipped rect; each one is a per-pixel guest loop. The host performs
//! the same rects with the same pattern, word-wise, straight into the
//! destination buffer.
//!
//! Faithful to `src/video/SDL_fillrect.c`:
//!
//! - the pattern is the one SDL's `SDL_FillRects` computes for the width —
//!   8bpp replicates the low byte, 16bpp the low word, 24bpp the low three
//!   bytes, 32bpp the whole value;
//! - rects are clipped against `dst->clip_rect` with `SDL_IntersectRect`
//!   semantics, and an empty intersection is skipped, not filled;
//! - `dst->w == 0 || dst->h == 0` returns 0 and `!dst->pixels` is an error:
//!   the error cases fall through so the guest sets its own error string;
//! - only the rect is written. SDL's SSE fills scribble a few pixels past the
//!   row end as an alignment side effect; the pixels *inside* the rect are
//!   identical, and the ones outside are not part of the fill.
//!
//! 4bpp surfaces (SDL's whole-surface special case), and formats whose
//! bytes-per-pixel is not 1, 2, 3 or 4, fall through.

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;

use super::surface::{
    SURFACE_CLIP_RECT, Surface, intersect_rect, read_format, read_rect, read_surface,
};
use super::{arg, declined};
use crate::gdi32::ArgReg;
use crate::{HandlerContext, WinApiHandlerResult};

/// The byte pattern SDL replicates across a row for one destination pixel
/// width, or `None` for a width SDL cannot fill.
fn pattern(bytes_per_pixel: u8, color: u32) -> Option<[u8; 8]> {
    let bytes = color.to_le_bytes();
    let mut pattern = [0_u8; 8];
    match bytes_per_pixel {
        1 => {
            // `color |= (color << 8); color |= (color << 16);`
            pattern.fill(bytes[0]);
        }
        2 => {
            // `color |= (color << 16);`
            pattern[0..2].copy_from_slice(&bytes[0..2]);
            pattern[2..4].copy_from_slice(&bytes[0..2]);
            pattern[4..6].copy_from_slice(&bytes[0..2]);
            pattern[6..8].copy_from_slice(&bytes[0..2]);
        }
        3 => {
            // `SDL_FillRect3` writes the three low bytes, no replication.
            pattern[0..3].copy_from_slice(&bytes[0..3]);
        }
        4 => {
            pattern[0..4].copy_from_slice(&bytes);
            pattern[4..8].copy_from_slice(&bytes);
        }
        _ => return None,
    }
    Some(pattern)
}

/// Fill one clipped rect of `surface` with `pattern`.
fn fill_rect(
    engine: &mut dyn CpuEngine,
    surface: &Surface,
    rect: super::surface::Rect,
    bytes_per_pixel: u8,
    pattern: &[u8; 8],
) -> Result<()> {
    let bpp = usize::from(bytes_per_pixel);
    let width = usize::try_from(rect.w).context("fill width out of range")?;
    let height = usize::try_from(rect.h).context("fill height out of range")?;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let row_bytes = width * bpp;
    let row_stride = i64::from(surface.pitch);
    // `pixels = dst->pixels + rect->y * dst->pitch + rect->x * BytesPerPixel`
    let start = i64::from(rect.y)
        .checked_mul(row_stride)
        .and_then(|y| {
            i64::from(rect.x)
                .checked_mul(i64::from(bytes_per_pixel))
                .and_then(|x| y.checked_add(x))
        })
        .context("fill offset overflow")?;
    let base = surface
        .pixels
        .checked_add_signed(start)
        .context("fill base address overflow")?;
    for row in 0..height {
        let delta = row_stride
            .checked_mul(i64::try_from(row).unwrap_or(0))
            .context("fill row offset overflow")?;
        let row_va = base
            .checked_add_signed(delta)
            .context("fill row address overflow")?;
        // Direct write into the guest row — this is the buffer the presenter
        // reads, so there is no intermediate destination buffer.
        if let Some(span) = engine.host_slice_mut(row_va, row_bytes) {
            write_pattern(span, bpp, pattern);
            continue;
        }
        let mut staged = vec![0_u8; row_bytes];
        write_pattern(&mut staged, bpp, pattern);
        engine
            .mem_write(row_va, &staged)
            .context("failed to write a staged fill row")?;
    }
    Ok(())
}

/// Fill `span` with whole-pixel copies of `pattern`, eight bytes at a time
/// (two 32-bit or four 16-bit pixels) and a pixel-granular tail.
fn write_pattern(span: &mut [u8], bytes_per_pixel: usize, pattern: &[u8; 8]) {
    let pixel = &pattern[..bytes_per_pixel.min(8)];
    if bytes_per_pixel == 3 {
        // 24bpp has no 8-byte period, so fill pixel by pixel.
        let (slots, _tail) = span.as_chunks_mut::<3>();
        for slot in slots.iter_mut() {
            slot.copy_from_slice(pixel);
        }
        return;
    }
    let span_len = span.len();
    let (slots, tail) = span.as_chunks_mut::<8>();
    for slot in slots.iter_mut() {
        slot.copy_from_slice(pattern);
    }
    let mut offset = span_len - tail.len();
    while offset + bytes_per_pixel <= span_len {
        span[offset..offset + bytes_per_pixel].copy_from_slice(pixel);
        offset += bytes_per_pixel;
    }
}

/// Shared body of both fill APIs: SDL's `SDL_FillRects` over `rects`.
///
/// `Ok(Some(value))` = performed natively, return `value`.
/// `Ok(None)` = a case the guest must run itself (to set its error string).
fn fill_rects(
    engine: &mut dyn CpuEngine,
    surface: &Surface,
    rects_va: u64,
    count: i32,
    color: u32,
) -> Result<Option<i64>> {
    let format = read_format(engine, surface.format_va)?;
    // `SDL_FillRects`: nothing to do for an empty surface.
    if surface.w == 0 || surface.h == 0 {
        return Ok(Some(0));
    }
    if surface.pixels == 0 {
        // "SDL_FillRects(): You must lock the surface".
        return Ok(None);
    }
    if rects_va == 0 {
        // `SDL_InvalidParamError("rects")`.
        return Ok(None);
    }
    if format.bits_per_pixel < 8 {
        // 4bpp gets a whole-surface special case that returns 1; everything
        // else is "Unsupported surface format". Both are guest-only paths.
        return Ok(None);
    }
    let Some(pattern) = pattern(format.bytes_per_pixel, color) else {
        // "Unsupported pixel format" for any other width.
        return Ok(None);
    };
    for index in 0..count.max(0) {
        let rect_va = rects_va
            .checked_add(u64::try_from(index).unwrap_or(0) * 8)
            .context("fill rect list address overflow")?;
        let rect = read_rect(engine, rect_va)?;
        let Some(clipped) = intersect_rect(rect, surface.clip_rect) else {
            continue;
        };
        fill_rect(engine, surface, clipped, format.bytes_per_pixel, &pattern)?;
    }
    Ok(Some(0))
}

/// `int SDL_FillRect(SDL_Surface *dst, const SDL_Rect *rect, Uint32 color)`.
pub(crate) fn handle_fill_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dst_va = arg(engine, ArgReg::Rcx, "SDL_FillRect")?;
    let rect_va = arg(engine, ArgReg::Rdx, "SDL_FillRect")?;
    let color = arg(engine, ArgReg::R8, "SDL_FillRect")? as u32;

    let Some(surface) = read_surface(engine, dst_va)? else {
        // `SDL_InvalidParamError("SDL_FillRect(): dst")`.
        return declined(ctx, "SDL_FillRect");
    };
    if rect_va == 0 {
        // `rect = &dst->clip_rect`; an empty clip rect fills nothing at all —
        // and SDL returns before even looking at the pixel buffer.
        if surface.clip_rect.is_empty() {
            return ctx.finish(0);
        }
        let clip_va = surface
            .va
            .checked_add(SURFACE_CLIP_RECT)
            .context("surface clip rect address overflow")?;
        return match fill_rects(engine, &surface, clip_va, 1, color)? {
            Some(value) => ctx.finish(u64::try_from(value).unwrap_or(0)),
            None => declined(ctx, "SDL_FillRect"),
        };
    }
    match fill_rects(engine, &surface, rect_va, 1, color)? {
        Some(value) => ctx.finish(u64::try_from(value).unwrap_or(0)),
        None => declined(ctx, "SDL_FillRect"),
    }
}

/// `int SDL_FillRects(SDL_Surface *dst, const SDL_Rect *rects, int count,
/// Uint32 color)`.
pub(crate) fn handle_fill_rects(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dst_va = arg(engine, ArgReg::Rcx, "SDL_FillRects")?;
    let rects_va = arg(engine, ArgReg::Rdx, "SDL_FillRects")?;
    let count = arg(engine, ArgReg::R8, "SDL_FillRects")? as u32 as i32;
    let color = arg(engine, ArgReg::R9, "SDL_FillRects")? as u32;

    let Some(surface) = read_surface(engine, dst_va)? else {
        // `SDL_InvalidParamError("SDL_FillRects(): dst")`.
        return declined(ctx, "SDL_FillRects");
    };
    match fill_rects(engine, &surface, rects_va, count, color)? {
        Some(value) => ctx.finish(u64::try_from(value).unwrap_or(0)),
        None => declined(ctx, "SDL_FillRects"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SDL's pattern rules, per width.
    #[test]
    fn patterns_match_sdl_fill_rects() {
        let p = pattern(1, 0x1234_5678).expect("8bpp pattern");
        assert_eq!(p, [0x78; 8]);
        let p = pattern(2, 0x1234_5678).expect("16bpp pattern");
        assert_eq!(p, [0x78, 0x56, 0x78, 0x56, 0x78, 0x56, 0x78, 0x56]);
        let p = pattern(3, 0x1234_5678).expect("24bpp pattern");
        assert_eq!(&p[..3], &[0x78, 0x56, 0x34]);
        let p = pattern(4, 0x1234_5678).expect("32bpp pattern");
        assert_eq!(p, [0x78, 0x56, 0x34, 0x12, 0x78, 0x56, 0x34, 0x12]);
        assert!(pattern(5, 0).is_none());
    }

    /// The row writer must fill exactly `w * bpp` bytes for every width,
    /// including widths that are not a multiple of the 8-byte period.
    #[test]
    fn write_pattern_covers_every_width() {
        for bpp in [1_usize, 2, 3, 4] {
            let p = pattern(u8::try_from(bpp).unwrap_or(1), 0xAABB_CCDD).expect("pattern");
            for width in 0..19_usize {
                let mut span = vec![0_u8; width * bpp];
                write_pattern(&mut span, bpp, &p);
                for pixel in 0..width {
                    let at = pixel * bpp;
                    assert_eq!(
                        &span[at..at + bpp],
                        &p[..bpp],
                        "bpp {bpp} width {width} pixel {pixel}"
                    );
                }
            }
        }
    }
}
