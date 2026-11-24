//! Native `SDL_LowerBlit` / `SDL_UpperBlit`.
//!
//! # What this replaces, and what it must not change
//!
//! `SDL_LowerBlit` is SDL's "rects are already clipped" entry point; every
//! other blit API in SDL2 ends up in it, and it calls a per-format blitter
//! through `SDL_BlitMap::data`. Those blitters are 3-6 guest instructions per
//! pixel, which dominates a 320x200 palette game upscaled into a 640x480
//! window (256K+ guest pixels per frame).
//!
//! The cases below are reproduced *exactly*, including the quirks that are
//! observable:
//!
//! - the palette table is the guest's own `map->info.table`, so palette
//!   remapping, colour modulation (`map->info.r/g/b/a`), dithered 32→8 tables
//!   and `SDL_FindColor` all stay SDL's business — the host never rebuilds it;
//! - row stepping uses the same `src_skip` / `dst_skip` / pitch mixtures the
//!   compiled blitters use, so a padded surface copies the same bytes and
//!   leaves the same gaps;
//! - the `(Uint16)` truncation `SDL_SoftBlit` applies to `srcrect->x/y` is
//!   applied here too (SDL casts, it does not sign-extend).
//!
//! Everything else — scaling, blending, colour/alpha modulation, RLE
//! acceleration, odd formats, a mapping SDL has not rebuilt yet — falls
//! through to the guest's own implementation by tail-jumping to it, so the
//! guest observes its own code running. See [`crate::host_exports`].

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;

use super::surface::{
    BlitMap, Rect, Surface, intersect_rect, palette_version, read_blit_map, read_format, read_rect,
    read_surface, write_rect,
};
use super::{Decline, arg, declined, note_decline, read_scratch};
use crate::gdi32::ArgReg;
use crate::{HandlerContext, WinApiHandlerResult};

// ── `SDL_COPY_*` mapping flags (`src/video/SDL_blit.h`) ─────────────────────
const SDL_COPY_COLORKEY: i32 = 0x0000_0100;
const SDL_COPY_NEAREST: i32 = 0x0000_0200;
const SDL_COPY_RLE_DESIRED: i32 = 0x0000_1000;
const SDL_COPY_RLE_COLORKEY: i32 = 0x0000_2000;
const SDL_COPY_RLE_ALPHAKEY: i32 = 0x0000_4000;
/// `SDL_COPY_RLE_MASK`.
const SDL_COPY_RLE_MASK: i32 = SDL_COPY_RLE_DESIRED | SDL_COPY_RLE_COLORKEY | SDL_COPY_RLE_ALPHAKEY;

/// One blit the host can perform bit-exactly.
enum Plan {
    /// `SDL_BlitCopy`: identical formats, so a row-wise `memmove`.
    Copy,
    /// `Blit1toN` / `Blit1toNKey`: an 8-bit index widened through the guest's
    /// palette table.
    Indexed {
        /// Destination bytes per pixel (1..=4).
        dst_bpp: usize,
        /// The guest's 256-entry expansion table. Boxed: it is 1 KiB, and a
        /// `Plan` lives on the stack of the blit handler.
        lut: Box<Lut>,
        /// `SDL_COPY_COLORKEY`: source indices equal to this are skipped.
        colorkey: Option<u32>,
    },
}

/// A palette expansion table, decoded from the guest's own bytes.
enum Lut {
    Bpp1([u8; 256]),
    Bpp2([u16; 256]),
    /// Table stride is 4 for 3-byte pixels (`Map1toN` rounds the width up).
    Bpp3([u32; 256]),
    Bpp4([u32; 256]),
}

/// Blit geometry, computed exactly as `SDL_SoftBlit` computes `SDL_BlitInfo`.
struct Geometry {
    /// `info->src` — top-left source pixel.
    src_va: u64,
    /// `info->dst` — top-left destination pixel.
    dst_va: u64,
    /// `info->src_skip` (`src_pitch - src_w * src_bpp`).
    src_skip: i64,
    /// `info->dst_skip` (`dst_pitch - dst_w * dst_bpp`).
    dst_skip: i64,
    /// `info->dst_w` — the count every blitter loops over.
    dst_w: i64,
    /// `info->dst_h`.
    dst_h: i64,
    /// `info->src_pitch` / `info->dst_pitch`.
    src_pitch: i64,
    dst_pitch: i64,
    /// `src->format->BytesPerPixel` / `dst->format->BytesPerPixel`.
    dst_bpp: i64,
}

impl Geometry {
    /// `None` when the rects cannot describe an addressable blit.
    fn new(
        src: &Surface,
        dst: &Surface,
        src_bpp: u8,
        dst_bpp: u8,
        srcrect: Rect,
        dstrect: Rect,
    ) -> Option<Self> {
        let (src_bpp, dst_bpp) = (i64::from(src_bpp), i64::from(dst_bpp));
        if src.pixels == 0 || dst.pixels == 0 || src_bpp == 0 || dst_bpp == 0 {
            return None;
        }
        let src_pitch = i64::from(src.pitch);
        let dst_pitch = i64::from(dst.pitch);
        // `SDL_SoftBlit` casts the rect origins through `Uint16`, so a negative
        // origin wraps to a large positive offset. Reproduced, not corrected.
        let src_x = i64::from(srcrect.x as u16);
        let src_y = i64::from(srcrect.y as u16);
        let dst_x = i64::from(dstrect.x as u16);
        let dst_y = i64::from(dstrect.y as u16);
        Some(Self {
            src_va: src
                .pixels
                .checked_add(u64::try_from(src_y * src_pitch + src_x * src_bpp).ok()?)?,
            dst_va: dst
                .pixels
                .checked_add(u64::try_from(dst_y * dst_pitch + dst_x * dst_bpp).ok()?)?,
            src_skip: src_pitch - i64::from(srcrect.w) * src_bpp,
            dst_skip: dst_pitch - i64::from(dstrect.w) * dst_bpp,
            dst_w: i64::from(dstrect.w),
            dst_h: i64::from(dstrect.h),
            src_pitch,
            dst_pitch,
            dst_bpp,
        })
    }
}

/// Decide which SDL blitter the guest would run, and whether it is one we
/// reproduce. `None` means "let the guest do it".
fn plan(
    engine: &mut dyn CpuEngine,
    src: &Surface,
    dst: &Surface,
    src_fmt: &super::surface::Format,
    dst_fmt: &super::surface::Format,
    map: &BlitMap,
) -> Result<Option<Plan>> {
    // `SDL_CalculateBlit`: a destination below 8bpp fails with "Blit
    // combination not supported", and an RLE-accelerated surface takes a
    // completely different path (SDL un-RLEs it first).
    if dst_fmt.bits_per_pixel < 8 || src.must_lock() || dst.must_lock() {
        return Ok(None);
    }
    if map.flags & SDL_COPY_RLE_MASK != 0 {
        return Ok(None);
    }

    // `map->identity && !(flags & ~SDL_COPY_RLE_DESIRED)` → `SDL_BlitCopy`.
    // With the RLE bits already excluded that is exactly `flags == 0`.
    if map.identity != 0 && map.flags == 0 {
        return Ok(Some(Plan::Copy));
    }

    // `SDL_CalculateBlit1`: an 8bpp indexed source selects
    // `one_blit[dst_bytes_per_pixel]`, or `one_blitkey[...]` under a colour key
    // alone. `Blit1toN` only exists when the destination is not palettized and
    // has 1-4 bytes per pixel.
    if src_fmt.bytes_per_pixel != 1 || !src_fmt.is_indexed() || dst_fmt.palette != 0 {
        return Ok(None);
    }
    let colorkey = match map.flags & !SDL_COPY_RLE_MASK {
        0 => None,
        f if f == SDL_COPY_COLORKEY => Some(map.colorkey),
        // `COLORKEY|BLEND` collapses to the key blitter only when
        // `info.a == 255`, and the `MODULATE_ALPHA` variants need per-pixel
        // table math: both are guest work.
        _ => return Ok(None),
    };
    if map.table == 0 {
        return Ok(None);
    }
    let dst_bpp = usize::from(dst_fmt.bytes_per_pixel);
    if !(1..=4).contains(&dst_bpp) {
        return Ok(None);
    }
    if dst_bpp == 1 && colorkey.is_some() {
        // `Blit1to1Key` also tolerates a NULL palette map; not worth modelling.
        return Ok(None);
    }
    // 3-byte pixels drive `Blit1to3`, which reads a 4-byte-strided table.
    let lane = if dst_bpp == 3 { 4 } else { dst_bpp };
    let Some(bytes) = read_scratch(engine, map.table, lane * 256)? else {
        return Ok(None);
    };
    let Some(lut) = decode_lut(&bytes, dst_bpp) else {
        return Ok(None);
    };
    Ok(Some(Plan::Indexed {
        dst_bpp,
        lut: Box::new(lut),
        colorkey,
    }))
}

/// Decode the guest's 256-entry expansion table bytes.
fn decode_lut(bytes: &[u8], dst_bpp: usize) -> Option<Lut> {
    Some(match dst_bpp {
        1 => {
            let mut lut = [0_u8; 256];
            lut.copy_from_slice(bytes.get(..256)?);
            Lut::Bpp1(lut)
        }
        2 => {
            let mut lut = [0_u16; 256];
            for (i, slot) in lut.iter_mut().enumerate() {
                let at = i * 2;
                *slot = u16::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]);
            }
            Lut::Bpp2(lut)
        }
        3 => {
            let mut lut = [0_u32; 256];
            for (i, slot) in lut.iter_mut().enumerate() {
                let at = i * 4;
                *slot = u32::from_le_bytes([
                    *bytes.get(at)?,
                    *bytes.get(at + 1)?,
                    *bytes.get(at + 2)?,
                    0,
                ]);
            }
            Lut::Bpp3(lut)
        }
        4 => {
            let mut lut = [0_u32; 256];
            for (i, slot) in lut.iter_mut().enumerate() {
                let at = i * 4;
                *slot = u32::from_le_bytes([
                    *bytes.get(at)?,
                    *bytes.get(at + 1)?,
                    *bytes.get(at + 2)?,
                    *bytes.get(at + 3)?,
                ]);
            }
            Lut::Bpp4(lut)
        }
        _ => return None,
    })
}

/// Run a planned blit.
fn run(engine: &mut dyn CpuEngine, plan: &Plan, geom: &Geometry) -> Result<()> {
    match plan {
        Plan::Copy => run_copy(engine, geom),
        Plan::Indexed {
            dst_bpp,
            lut,
            colorkey,
        } => run_indexed(engine, geom, *dst_bpp, lut, *colorkey),
    }
}

/// `SDL_BlitCopy`: `dst_w * dst_bpp` bytes per row, `dst_h` rows, full pitches,
/// memmove semantics with SDL's overlap direction choice.
fn run_copy(engine: &mut dyn CpuEngine, geom: &Geometry) -> Result<()> {
    let row_bytes =
        usize::try_from(geom.dst_w * geom.dst_bpp).context("blit row width out of range")?;
    let rows = usize::try_from(geom.dst_h).context("blit row count out of range")?;
    if row_bytes == 0 || rows == 0 {
        return Ok(());
    }
    // SDL picks the direction from the buffer relationship: a blit onto itself
    // with a vertical offset must not smear.
    let bottom_up = geom.dst_va > geom.src_va;
    for i in 0..rows {
        let index = if bottom_up { rows - 1 - i } else { i };
        let src_row = offset(geom.src_va, index, geom.src_pitch)?;
        let dst_row = offset(geom.dst_va, index, geom.dst_pitch)?;
        copy_row(engine, dst_row, src_row, row_bytes)?;
    }
    Ok(())
}

/// `Blit1toN` / `Blit1toNKey`: widen one source byte per pixel through the
/// guest's palette table, writing straight into the destination surface.
fn run_indexed(
    engine: &mut dyn CpuEngine,
    geom: &Geometry,
    dst_bpp: usize,
    lut: &Lut,
    colorkey: Option<u32>,
) -> Result<()> {
    let width = usize::try_from(geom.dst_w).context("blit width out of range")?;
    let rows = usize::try_from(geom.dst_h).context("blit row count out of range")?;
    if width == 0 || rows == 0 {
        return Ok(());
    }
    let dst_row_bytes = width * dst_bpp;
    // The compiled loops advance the source by `dst_w + src_skip` per row, not
    // by a whole pitch: identical when `src_w == dst_w`, which is every
    // non-scaled blit, and faithfully different when it is not.
    let src_step = i64::try_from(width).unwrap_or(0) + geom.src_skip;
    let dst_step = i64::try_from(dst_row_bytes).unwrap_or(0) + geom.dst_skip;

    let mut src_row_buffer = [0_u8; 1024];
    for row in 0..rows {
        let src_row = step(geom.src_va, row, src_step)?;
        let dst_row = step(geom.dst_va, row, dst_step)?;
        // The source is an 8-bit surface and the destination is not, so the two
        // guest ranges cannot alias; still, staging the (small) source row
        // keeps the destination borrow exclusive and the aliasing argument
        // trivial.
        let Some(src_buffer) = src_row_buffer.get_mut(..width) else {
            return Err(anyhow::anyhow!(
                "blit source row wider than the staging buffer"
            ));
        };
        if src_buffer.is_empty() {
            continue;
        }
        engine
            .mem_read(src_row, src_buffer)
            .context("failed to read a blit source row")?;
        // Destination: borrow the row in guest memory and expand in place — no
        // intermediate destination buffer, these are the bytes the presenter
        // reads.
        if let Some(dst) = engine.host_slice_mut(dst_row, dst_row_bytes) {
            expand_row(dst, src_buffer, lut, colorkey);
            continue;
        }
        // No host span (the row crosses two arenas, or the guest range is not
        // directly addressable). Stage it — but *read it first*: a colour-keyed
        // pixel is deliberately left untouched, and a zeroed staging buffer
        // would black it out instead.
        let mut staged = vec![0_u8; dst_row_bytes];
        engine
            .mem_read(dst_row, &mut staged)
            .context("failed to read a staged blit destination row")?;
        expand_row(&mut staged, src_buffer, lut, colorkey);
        engine
            .mem_write(dst_row, &staged)
            .context("failed to write a staged blit destination row")?;
    }
    Ok(())
}

/// Widen one source row into `dst`.
fn expand_row(dst: &mut [u8], src: &[u8], lut: &Lut, colorkey: Option<u32>) {
    match lut {
        Lut::Bpp1(table) => {
            for (slot, &index) in dst.iter_mut().zip(src) {
                if skip(index, colorkey) {
                    continue;
                }
                *slot = table[usize::from(index)];
            }
        }
        Lut::Bpp2(table) => {
            let (slots, _tail) = dst.as_chunks_mut::<2>();
            for (slot, &index) in slots.iter_mut().zip(src) {
                if skip(index, colorkey) {
                    continue;
                }
                *slot = table[usize::from(index)].to_le_bytes();
            }
        }
        Lut::Bpp3(table) => {
            let (slots, _tail) = dst.as_chunks_mut::<3>();
            for (slot, &index) in slots.iter_mut().zip(src) {
                if skip(index, colorkey) {
                    continue;
                }
                let pixel = table[usize::from(index)].to_le_bytes();
                slot.copy_from_slice(&pixel[..3]);
            }
        }
        Lut::Bpp4(table) => {
            let (slots, _tail) = dst.as_chunks_mut::<4>();
            for (slot, &index) in slots.iter_mut().zip(src) {
                if skip(index, colorkey) {
                    continue;
                }
                *slot = table[usize::from(index)].to_le_bytes();
            }
        }
    }
}

/// `*src != ckey` in SDL's key blitters: the source byte is compared
/// zero-extended against the 32-bit key, so a key above 0xff never matches.
#[inline]
fn skip(index: u8, colorkey: Option<u32>) -> bool {
    matches!(colorkey, Some(key) if u32::from(index) == key)
}

/// `base + index * pitch` for the row-stepping blitters.
fn offset(base: u64, index: usize, pitch: i64) -> Result<u64> {
    step(base, index, pitch)
}

/// `base + index * stride`.
fn step(base: u64, index: usize, stride: i64) -> Result<u64> {
    let delta = stride
        .checked_mul(i64::try_from(index).unwrap_or(0))
        .context("blit row offset overflow")?;
    base.checked_add_signed(delta)
        .context("blit row address overflow")
}

/// Copy one row guest→guest, staging through a host buffer when the range is
/// not one host span.
fn copy_row(engine: &mut dyn CpuEngine, dst: u64, src: u64, len: usize) -> Result<()> {
    if engine.mem_copy(dst, src, len) {
        return Ok(());
    }
    let mut scratch = vec![0_u8; len];
    engine
        .mem_read(src, &mut scratch)
        .context("failed to stage-read a blit row")?;
    engine
        .mem_write(dst, &scratch)
        .context("failed to stage-write a blit row")?;
    Ok(())
}

/// Shared body of `SDL_LowerBlit` and the `SDL_UpperBlit` tail.
///
/// `Ok(Some(value))` = performed natively, return `value` to the guest.
/// `Ok(None)` = not a case we reproduce; the caller tail-jumps.
fn lower_blit(
    engine: &mut dyn CpuEngine,
    src: &Surface,
    srcrect: Rect,
    dst: &Surface,
    dstrect: Rect,
) -> Result<Option<i64>> {
    // Scaling is a different blitter: `SDL_PrivateUpperBlitScaled` installs a
    // `*Scale` variant whose fixed-point stepping walks the source on its own
    // schedule, so the plain per-pixel loop below would read the wrong source
    // for every column. Only the 1:1 case is reproduced.
    if srcrect.w != dstrect.w || srcrect.h != dstrect.h {
        note_decline(Decline::Scaled);
        return Ok(None);
    }
    let Some(map) = read_blit_map(engine, src.map_va)? else {
        note_decline(Decline::MapMissing);
        return Ok(None);
    };
    // `SDL_LowerBlit`'s own mapping-validity test: a stale map means SDL is
    // about to rebuild it (`SDL_MapSurface`), which is guest work we must not
    // skip — it is what installs the palette table we would otherwise read.
    if map.dst != dst.va {
        note_decline(Decline::MapStale);
        return Ok(None);
    }
    let src_fmt = read_format(engine, src.format_va)?;
    let dst_fmt = read_format(engine, dst.format_va)?;
    for (palette, version) in [
        (dst_fmt.palette, map.dst_palette_version),
        (src_fmt.palette, map.src_palette_version),
    ] {
        if palette != 0 && palette_version(engine, palette)? != Some(version) {
            note_decline(Decline::PaletteStale);
            return Ok(None);
        }
    }
    // Layout cross-check: `SDL_CalculateBlit` mirrors both surface formats and
    // pitches into the map. If those agree, the offsets this module reads are
    // the ones the guest is using; if not, we are looking at the wrong fields
    // and must not touch a pixel.
    if map.src_fmt != src.format_va
        || map.dst_fmt != dst.format_va
        || map.src_pitch != src.pitch
        || map.dst_pitch != dst.pitch
    {
        note_decline(Decline::MapMirror);
        return Ok(None);
    }
    if !src.is_direct(src_fmt.bytes_per_pixel) || !dst.is_direct(dst_fmt.bytes_per_pixel) {
        note_decline(Decline::NotDirect);
        return Ok(None);
    }
    let Some(plan) = plan(engine, src, dst, &src_fmt, &dst_fmt, &map)? else {
        note_decline(Decline::UnsupportedKind);
        return Ok(None);
    };
    let Some(geom) = Geometry::new(
        src,
        dst,
        src_fmt.bytes_per_pixel,
        dst_fmt.bytes_per_pixel,
        srcrect,
        dstrect,
    ) else {
        note_decline(Decline::Geometry);
        return Ok(None);
    };
    run(engine, &plan, &geom)?;
    Ok(Some(0))
}

/// `int SDL_LowerBlit(SDL_Surface *src, SDL_Rect *srcrect, SDL_Surface *dst,
/// SDL_Rect *dstrect)`.
pub(crate) fn handle_lower_blit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let src_va = arg(engine, ArgReg::Rcx, "SDL_LowerBlit")?;
    let srcrect_va = arg(engine, ArgReg::Rdx, "SDL_LowerBlit")?;
    let dst_va = arg(engine, ArgReg::R8, "SDL_LowerBlit")?;
    let dstrect_va = arg(engine, ArgReg::R9, "SDL_LowerBlit")?;

    let (Some(src), Some(dst)) = (read_surface(engine, src_va)?, read_surface(engine, dst_va)?)
    else {
        note_decline(Decline::NullArgument);
        return declined(ctx, "SDL_LowerBlit");
    };
    // A NULL rect is a guest crash in SDL_LowerBlit (`srcrect->y`), so the
    // guest's own code must produce it.
    if srcrect_va == 0 || dstrect_va == 0 {
        note_decline(Decline::NullArgument);
        return declined(ctx, "SDL_LowerBlit");
    }
    let srcrect = read_rect(engine, srcrect_va)?;
    let dstrect = read_rect(engine, dstrect_va)?;
    match lower_blit(engine, &src, srcrect, &dst, dstrect)? {
        Some(value) => ctx.finish(u64::try_from(value).unwrap_or(0)),
        None => declined(ctx, "SDL_LowerBlit"),
    }
}

/// `int SDL_UpperBlit(SDL_Surface *src, const SDL_Rect *srcrect,
/// SDL_Surface *dst, SDL_Rect *dstrect)`.
///
/// `SDL_surface.h` defines `SDL_BlitSurface` as this function, so it is the
/// entry point every guest blit reaches through the import table.
pub(crate) fn handle_upper_blit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let src_va = arg(engine, ArgReg::Rcx, "SDL_UpperBlit")?;
    let srcrect_va = arg(engine, ArgReg::Rdx, "SDL_UpperBlit")?;
    let dst_va = arg(engine, ArgReg::R8, "SDL_UpperBlit")?;
    let dstrect_va = arg(engine, ArgReg::R9, "SDL_UpperBlit")?;

    let (Some(src), Some(dst)) = (read_surface(engine, src_va)?, read_surface(engine, dst_va)?)
    else {
        // `SDL_InvalidParamError("src"/"dst")` — the guest must set the error.
        return declined(ctx, "SDL_UpperBlit");
    };
    if src.locked != 0 || dst.locked != 0 {
        // "Surfaces must not be locked during blit".
        note_decline(Decline::SurfaceLocked);
        return declined(ctx, "SDL_UpperBlit");
    }
    // A pending stretch (`SDL_COPY_NEAREST`) makes SDL invalidate and rebuild
    // the mapping before blitting; that rebuild is guest state we must not
    // skip, and the blitter it installs is a scaling one.
    if let Some(map) = read_blit_map(engine, src.map_va)?
        && map.flags & SDL_COPY_NEAREST != 0
    {
        note_decline(Decline::NearestPending);
        return declined(ctx, "SDL_UpperBlit");
    }

    let whole_src = Rect {
        x: 0,
        y: 0,
        w: src.w,
        h: src.h,
    };
    let mut r_src = whole_src;
    let mut r_dst = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };
    if dstrect_va != 0 {
        // Only the origin of `dstrect` is an input; w/h come from the source.
        let out = read_rect(engine, dstrect_va)?;
        r_dst.x = out.x;
        r_dst.y = out.y;
    }
    if srcrect_va != 0 {
        let requested = read_rect(engine, srcrect_va)?;
        let Some(clipped) = intersect_rect(requested, whole_src) else {
            return clear_dstrect_and_finish(ctx, dstrect_va);
        };
        r_dst.x = r_dst.x.wrapping_add(clipped.x - requested.x);
        r_dst.y = r_dst.y.wrapping_add(clipped.y - requested.y);
        r_src = clipped;
    }
    r_dst.w = r_src.w;
    r_dst.h = r_src.h;
    let Some(clipped) = intersect_rect(r_dst, dst.clip_rect) else {
        return clear_dstrect_and_finish(ctx, dstrect_va);
    };
    r_src.x = r_src.x.wrapping_add(clipped.x - r_dst.x);
    r_src.y = r_src.y.wrapping_add(clipped.y - r_dst.y);
    r_src.w = clipped.w;
    r_src.h = clipped.h;
    r_dst = clipped;

    if r_dst.w <= 0 || r_dst.h <= 0 {
        return clear_dstrect_and_finish(ctx, dstrect_va);
    }
    // The out-parameter is written before the blit runs — SDL does the same.
    if dstrect_va != 0 {
        write_rect(engine, dstrect_va, r_dst)?;
    }
    match lower_blit(engine, &src, r_src, &dst, r_dst)? {
        Some(value) => ctx.finish(u64::try_from(value).unwrap_or(0)),
        None => declined(ctx, "SDL_UpperBlit"),
    }
}

/// SDL's `end:` label: `*dstrect = {0, 0, 0, 0}` and success.
fn clear_dstrect_and_finish(
    ctx: &mut HandlerContext<'_>,
    dstrect_va: u64,
) -> Result<WinApiHandlerResult> {
    if dstrect_va != 0 {
        write_rect(
            ctx.engine,
            dstrect_va,
            Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
        )?;
    }
    ctx.finish(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row-expansion kill test: every width class of destination pixel must
    /// land on the bytes the guest table names, including the 4-byte-strided
    /// 24bpp table, and a colour key must leave the destination untouched.
    #[test]
    fn expand_row_matches_the_guest_table() {
        let mut table = [0_u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            *slot = 0x0011_2233_u32
                .wrapping_add(u32::try_from(i).unwrap_or(0).wrapping_mul(0x0101_0101));
        }
        let src = [0_u8, 1, 2, 255];

        let mut dst4 = [0_u8; 16];
        expand_row(&mut dst4, &src, &Lut::Bpp4(table), None);
        assert_eq!(&dst4[0..4], &table[0].to_le_bytes());
        assert_eq!(&dst4[12..16], &table[255].to_le_bytes());

        let mut dst2 = [0_u8; 8];
        let mut table16 = [0_u16; 256];
        for (i, slot) in table16.iter_mut().enumerate() {
            *slot = u16::try_from(i).unwrap_or(0).wrapping_mul(3);
        }
        expand_row(&mut dst2, &src, &Lut::Bpp2(table16), None);
        assert_eq!(u16::from_le_bytes([dst2[0], dst2[1]]), 0);
        assert_eq!(u16::from_le_bytes([dst2[2], dst2[3]]), 3);
        assert_eq!(u16::from_le_bytes([dst2[6], dst2[7]]), 255 * 3);

        let mut dst3 = [0_u8; 12];
        expand_row(&mut dst3, &src, &Lut::Bpp3(table), None);
        assert_eq!(&dst3[0..3], &table[0].to_le_bytes()[..3]);

        // Colour key: the keyed index keeps the destination byte, the rest are
        // overwritten.
        let mut keyed = [0xEE_u8; 16];
        expand_row(&mut keyed, &src, &Lut::Bpp4(table), Some(1));
        assert_eq!(&keyed[4..8], &[0xEE; 4], "index 1 must be skipped");
        assert_eq!(&keyed[0..4], &table[0].to_le_bytes());
    }
}
