//! Native `SDL_SoftStretch` nearest-neighbour scaling.
//!
//! # What this is
//!
//! `SDL_SoftStretch` is SDL's own scaler, not a blitter: `SDL_BlitScaled` on a
//! pair of same-format, non-indexed surfaces goes `SDL_PrivateLowerBlitScaled`
//! → `SDL_SoftStretch` → `SDL_LowerSoftStretchNearest` → `scale_mat_nearest_N`
//! for the destination's bytes-per-pixel. That per-pixel loop is the single
//! largest consumer of guest time in the DOOM Retro workload
//! (`docs/architecture/scaled-blit-block-attribution.md`: block `0x1800d8fb0`,
//! 54.1 % of guest time, 174 host instructions per 8 guest instructions), so
//! serving it moves the whole render phase off the JIT.
//!
//! # Faithful to `src/video/SDL_stretch.c` (2.32.10)
//!
//! The stepping is transcribed from the source *and* checked against the
//! guest's compiled loop (`0x1800d8f20`, reached from `0x1800d8fdd` in
//! `SDL_UpperSoftStretch`'s bpp dispatch):
//!
//! ```c
//! incy = ((Uint64)src_h << 16) / dst_h;      // 64-bit, truncating
//! incx = ((Uint64)src_w << 16) / dst_w;
//! dst_gap = dst_pitch - bpp * dst_w;
//! posy = incy / 2;                           // centre-of-pixel sampling
//! for (i = 0; i < dst_h; i++) {
//!     src_h0 = src_ptr + (posy >> 16) * src_pitch;
//!     posy += incy;
//!     posx = incx / 2;
//!     for (n = dst_w; n--;)
//!         dst[..bpp] = src_h0[bpp * (posx >> 16)..][..bpp], posx += incx;
//! }
//! ```
//!
//! The `incy / 2` start is the part a "step from zero" implementation gets
//! wrong: every row and column samples the *centre* of the destination pixel,
//! so a 2:1 downscale picks source columns 0, 2, 4 … only because the bias
//! rounds that way — the same code at 3:2 picks 0, 1, 2, 3, 5 …, not 0, 1, 3.
//!
//! The observability rules are `SDL_UpperSoftStretch`'s: a mismatched format
//! pair, an out-of-bounds rect, a size above 65535 or an RLE surface all fall
//! through so the guest produces its own `SDL_SetError` string; a zero-size
//! destination rect returns 0 and touches nothing; and the surfaces are
//! expected to be plain software surfaces, which is what makes writing into
//! `dst->pixels` equivalent to SDL's own writes.
//!
//! # No copy, no staging
//!
//! Rows are written straight into the destination surface's pixel buffer, the
//! same buffer the presenter reads. Nothing is buffered, so a declined call
//! costs nothing and a served one costs one load and one store per destination
//! pixel instead of ~174 host instructions per 8 guest instructions.

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;

use super::surface::{Rect, Surface, read_format, read_rect, read_surface};
use super::{Decline, arg, declined, note_decline, note_shadow_shadowed};
use crate::gdi32::ArgReg;
use crate::{HandlerContext, WinApiHandlerResult};

/// A resolved nearest-neighbour stretch: everything the loop needs, already
/// validated against both surfaces.
#[derive(Debug, Clone, Copy)]
pub(super) struct Stretch {
    /// Guest VA of the first source pixel (rect origin included).
    pub src: u64,
    /// Guest VA of the first destination pixel.
    pub dst: u64,
    pub src_pitch: i64,
    pub dst_pitch: i64,
    pub src_w: i64,
    pub src_h: i64,
    pub dst_w: i64,
    pub dst_h: i64,
    pub bpp: i64,
}

impl Stretch {
    /// `incy = ((Uint64)src_h << 16) / dst_h`.
    #[must_use]
    fn incy(&self) -> u64 {
        (u64::try_from(self.src_h).unwrap_or(0) << 16) / u64::try_from(self.dst_h).unwrap_or(1)
    }

    /// `incx = ((Uint64)src_w << 16) / dst_w`.
    #[must_use]
    fn incx(&self) -> u64 {
        (u64::try_from(self.src_w).unwrap_or(0) << 16) / u64::try_from(self.dst_w).unwrap_or(1)
    }
}

/// Resolve the rects and validate that the stretch is addressable and lands
/// inside both surfaces. `None` means "let the guest do it".
fn plan(src: &Surface, srcrect: Rect, dst: &Surface, dstrect: Rect, bpp: u8) -> Option<Stretch> {
    if dstrect.w <= 0 || dstrect.h <= 0 {
        return None;
    }
    // Both rects must be inside their surfaces: SDL's own validation, and what
    // makes the sampled indices provably in range below.
    if srcrect.x < 0
        || srcrect.y < 0
        || srcrect.w <= 0
        || srcrect.h <= 0
        || srcrect.x.saturating_add(srcrect.w) > src.w
        || srcrect.y.saturating_add(srcrect.h) > src.h
    {
        return None;
    }
    if dstrect.x < 0
        || dstrect.y < 0
        || dstrect.x.saturating_add(dstrect.w) > dst.w
        || dstrect.y.saturating_add(dstrect.h) > dst.h
    {
        return None;
    }
    let bpp = i64::from(bpp);
    let src_pitch = i64::from(src.pitch);
    let dst_pitch = i64::from(dst.pitch);
    if src_pitch <= 0 || dst_pitch <= 0 {
        return None;
    }
    let stretch = Stretch {
        src: src.pixels.checked_add(
            u64::try_from(i64::from(srcrect.x) * bpp + i64::from(srcrect.y) * src_pitch).ok()?,
        )?,
        dst: dst.pixels.checked_add(
            u64::try_from(i64::from(dstrect.x) * bpp + i64::from(dstrect.y) * dst_pitch).ok()?,
        )?,
        src_pitch,
        dst_pitch,
        src_w: i64::from(srcrect.w),
        src_h: i64::from(srcrect.h),
        dst_w: i64::from(dstrect.w),
        dst_h: i64::from(dstrect.h),
        bpp,
    };
    // The last sampled source column and row, computed exactly as the loop
    // will compute them. If either is out of range the guest's own loop would
    // read past its row too, so this is a "we are not looking at what SDL is"
    // check, not a behaviour difference.
    let incx = stretch.incx();
    let incy = stretch.incy();
    let last_x = (incx / 2 + (u64::try_from(stretch.dst_w).ok()? - 1) * incx) >> 16;
    let last_y = (incy / 2 + (u64::try_from(stretch.dst_h).ok()? - 1) * incy) >> 16;
    if last_x >= u64::try_from(stretch.src_w).ok()?
        || last_y >= u64::try_from(stretch.src_h).ok()?
    {
        return None;
    }
    Some(stretch)
}

/// Run the stretch, writing straight into the destination surface.
fn run(engine: &mut dyn CpuEngine, stretch: &Stretch) -> Result<()> {
    let bpp = usize::try_from(stretch.bpp).context("stretch bpp out of range")?;
    let dst_w = usize::try_from(stretch.dst_w).context("stretch width out of range")?;
    let dst_h = usize::try_from(stretch.dst_h).context("stretch height out of range")?;
    let src_w = usize::try_from(stretch.src_w).context("stretch width out of range")?;
    let src_row_bytes = usize::try_from(stretch.src_pitch).context("stretch pitch out of range")?;
    let dst_row_bytes = usize::try_from(stretch.dst_pitch).context("stretch pitch out of range")?;
    let incy = stretch.incy();
    let mut posy = incy / 2;
    let mut src_row = vec![0_u8; src_w * bpp];
    for row in 0..dst_h {
        let srcy = usize::try_from(posy >> 16).context("stretch row index out of range")?;
        posy += incy;
        let src_va = stretch
            .src
            .checked_add(u64::try_from(srcy * src_row_bytes).context("stretch source offset")?)
            .context("stretch source address overflow")?;
        // One row of source, then a per-pixel gather into the destination row:
        // the sampled indices only ever move forward, so a straight walk is all
        // the scaling needs.
        engine
            .mem_read(src_va, &mut src_row)
            .context("failed to read a guest stretch source row")?;
        let dst_va = stretch
            .dst
            .checked_add(u64::try_from(row * dst_row_bytes).context("stretch dest offset")?)
            .context("stretch destination address overflow")?;
        let row_bytes = dst_w * bpp;
        // The destination row is the buffer the presenter reads: gather into it
        // directly. A row that is not one host span (it crosses two arenas)
        // falls back to staging, which is a straight overwrite — every byte of
        // the row is written, so nothing has to be read back first.
        if let Some(dst_row) = engine.host_slice_mut(dst_va, row_bytes) {
            gather_row(stretch, &src_row, dst_row)?;
            continue;
        }
        let mut staged = vec![0_u8; row_bytes];
        gather_row(stretch, &src_row, &mut staged)?;
        engine
            .mem_write(dst_va, &staged)
            .context("failed to write a staged stretch destination row")?;
    }
    Ok(())
}

/// One destination row: `dst[..bpp] = src_row[bpp * (posx >> 16)..][..bpp]` for
/// every destination pixel, with `posx` stepping by `incx` from `incx / 2`.
fn gather_row(stretch: &Stretch, src_row: &[u8], dst_row: &mut [u8]) -> Result<()> {
    let bpp = usize::try_from(stretch.bpp).context("stretch bpp out of range")?;
    let incx = stretch.incx();
    let mut posx = incx / 2;
    for target in dst_row.chunks_mut(bpp) {
        let index = usize::try_from(posx >> 16).context("stretch column index")?;
        posx += incx;
        let at = index * bpp;
        // `plan` proved this in range; the check keeps a future caller from
        // turning a wrong plan into a slice panic.
        let Some(source) = src_row.get(at..at + bpp) else {
            anyhow::bail!("stretch sampled outside its source row");
        };
        target.copy_from_slice(source);
    }
    Ok(())
}

/// `WIE_HOST_EXPORTS=shadow`: run the native stepper and compare it with what
/// the guest's own loop produces, on the real call path, in one process.
///
/// This is the end-to-end correctness proof the screenshot differential cannot
/// give: frames are not comparable across runs (DOOM animates off the wall
/// clock), but a *call* is. The comparison is therefore deferred by exactly one
/// call: this invocation computes the native result into a host buffer and then
/// lets the guest write the real pixels; the next invocation reads them back and
/// compares before doing anything else.
///
/// The stashed result is valid until something else writes that region. On this
/// guest the region is the renderer's back buffer, written by exactly this call
/// once per frame, so the next entry is also the first opportunity to look. A
/// guest that drew into the same region between two stretches would show up as
/// a mismatch whose first differing pixel is reported — which is why the tally
/// names the pixel rather than just counting.
mod shadow {
    use std::sync::Mutex;

    /// One deferred comparison.
    pub(super) struct Pending {
        /// Destination surface pixel base the comparison covers.
        pub dst: u64,
        /// Destination row pitch and dimensions.
        pub pitch: usize,
        pub width: usize,
        pub height: usize,
        /// What the native stepper produced for this call.
        pub expected: Vec<u8>,
    }

    /// The stash, plus the running tally.
    pub(super) struct Shadow {
        pub pending: Option<Pending>,
        pub rows_compared: u64,
        pub rows_mismatched: u64,
        pub first_mismatch: Option<(usize, usize, u32, u32)>,
    }

    static SHADOW: Mutex<Option<Shadow>> = Mutex::new(None);

    /// Run `f` with the shadow state (creating it on first use).
    pub(super) fn with<R>(f: impl FnOnce(&mut Shadow) -> R) -> R {
        let mut guard = match SHADOW.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let state = guard.get_or_insert(Shadow {
            pending: None,
            rows_compared: 0,
            rows_mismatched: 0,
            first_mismatch: None,
        });
        f(state)
    }

    /// The report line, or `None` while nothing has been compared.
    pub(super) fn report_line() -> Option<String> {
        with(|state| {
            if state.rows_compared == 0 && state.rows_mismatched == 0 {
                return None;
            }
            let first = state
                .first_mismatch
                .map_or_else(String::new, |(row, x, expected, got)| {
                    format!(
                        " first mismatch: row {row} x {x} expected {expected:#010x} got {got:#010x}"
                    )
                });
            Some(format!(
                "sdl2_stretch_shadow: rows_compared={} rows_mismatched={}{first} \
                 (native stepper vs the guest's own loop, same call, same inputs)",
                state.rows_compared, state.rows_mismatched
            ))
        })
    }
}

/// Compare a stashed native result against what the guest actually wrote.
///
/// Runs before this call touches anything, so the region still holds the
/// previous call's output.
fn compare_shadow(engine: &mut dyn CpuEngine, stretch: &Stretch) {
    shadow::with(|state| {
        let Some(pending) = state.pending.take() else {
            return;
        };
        if pending.dst != stretch.dst
            || pending.width != usize::try_from(stretch.dst_w).unwrap_or(0)
            || pending.height != usize::try_from(stretch.dst_h).unwrap_or(0)
        {
            // A different destination or geometry: the stash cannot be
            // compared soundly, so drop it rather than report noise.
            return;
        }
        let bpp = usize::try_from(stretch.bpp).unwrap_or(1);
        let row_bytes = pending.width * bpp;
        let mut row = vec![0_u8; row_bytes];
        for index in 0..pending.height {
            let dst_va = match u64::try_from(index)
                .ok()
                .and_then(|row| row.checked_mul(u64::try_from(pending.pitch).ok()?))
                .and_then(|offset| pending.dst.checked_add(offset))
            {
                Some(va) => va,
                None => return,
            };
            if engine.mem_read(dst_va, &mut row).is_err() {
                return;
            }
            let at = index * row_bytes;
            let Some(expected_row) = pending.expected.get(at..at + row_bytes) else {
                return;
            };
            state.rows_compared = state.rows_compared.saturating_add(1);
            if row == expected_row {
                continue;
            }
            state.rows_mismatched = state.rows_mismatched.saturating_add(1);
            if state.first_mismatch.is_none() {
                for x in 0..pending.width {
                    let at = x * bpp;
                    if row[at..at + bpp] != expected_row[at..at + bpp] {
                        let word = |bytes: &[u8]| {
                            let mut w = [0_u8; 4];
                            w[..bpp.min(4)].copy_from_slice(&bytes[..bpp.min(4)]);
                            u32::from_le_bytes(w)
                        };
                        state.first_mismatch =
                            Some((index, x, word(&expected_row[at..]), word(&row[at..])));
                        break;
                    }
                }
            }
        }
    });
}

/// Compute the native result into a host buffer for the next call to compare.
fn stash_shadow(engine: &mut dyn CpuEngine, stretch: &Stretch) -> Result<()> {
    let bpp = usize::try_from(stretch.bpp).context("stretch bpp out of range")?;
    let width = usize::try_from(stretch.dst_w).context("stretch width out of range")?;
    let height = usize::try_from(stretch.dst_h).context("stretch height out of range")?;
    let pitch = usize::try_from(stretch.dst_pitch).context("stretch pitch out of range")?;
    let row_bytes = width * bpp;
    let mut expected = vec![0_u8; row_bytes * height];
    for (index, row) in expected.chunks_mut(row_bytes).enumerate() {
        gather_row_into(engine, stretch, index, row)?;
    }
    shadow::with(|state| {
        state.pending = Some(shadow::Pending {
            dst: stretch.dst,
            pitch,
            width,
            height,
            expected,
        });
    });
    Ok(())
}

/// One destination row of the native stepper, gathered from guest memory.
fn gather_row_into(
    engine: &mut dyn CpuEngine,
    stretch: &Stretch,
    row: usize,
    dst_row: &mut [u8],
) -> Result<()> {
    let bpp = usize::try_from(stretch.bpp).context("stretch bpp out of range")?;
    let src_w = usize::try_from(stretch.src_w).context("stretch width out of range")?;
    let src_pitch = usize::try_from(stretch.src_pitch).context("stretch pitch out of range")?;
    let incx = stretch.incx();
    let incy = stretch.incy();
    let posy = incy / 2 + incy * u64::try_from(row).context("stretch row")?;
    let srcy = usize::try_from(posy >> 16).context("stretch row index")?;
    let mut src_row = vec![0_u8; src_w * bpp];
    let src_va = stretch
        .src
        .checked_add(u64::try_from(srcy * src_pitch).context("stretch source offset")?)
        .context("stretch source overflow")?;
    engine
        .mem_read(src_va, &mut src_row)
        .context("failed to read a guest stretch source row")?;
    let mut posx = incx / 2;
    for target in dst_row.chunks_mut(bpp) {
        let index = usize::try_from(posx >> 16).context("stretch column index")?;
        posx += incx;
        let at = index * bpp;
        let Some(source) = src_row.get(at..at + bpp) else {
            anyhow::bail!("stretch sampled outside its source row");
        };
        target.copy_from_slice(source);
    }
    Ok(())
}

/// `int SDL_SoftStretch(SDL_Surface *src, const SDL_Rect *srcrect,
/// SDL_Surface *dst, const SDL_Rect *dstrect)`.
pub(crate) fn handle_soft_stretch(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let src_va = arg(engine, ArgReg::Rcx, "SDL_SoftStretch")?;
    let srcrect_va = arg(engine, ArgReg::Rdx, "SDL_SoftStretch")?;
    let dst_va = arg(engine, ArgReg::R8, "SDL_SoftStretch")?;
    let dstrect_va = arg(engine, ArgReg::R9, "SDL_SoftStretch")?;
    let Some(src) = read_surface(engine, src_va)? else {
        note_decline(Decline::NullArgument);
        return declined(ctx, "SDL_SoftStretch");
    };
    let Some(dst) = read_surface(engine, dst_va)? else {
        note_decline(Decline::NullArgument);
        return declined(ctx, "SDL_SoftStretch");
    };
    // `SDL_UpperSoftStretch` refuses a mismatched pair with an error string, and
    // its own lock handling would change surface state.
    if src.must_lock() || dst.must_lock() {
        note_decline(Decline::SurfaceLocked);
        return declined(ctx, "SDL_SoftStretch");
    }
    let src_fmt = read_format(engine, src.format_va)?;
    let dst_fmt = read_format(engine, dst.format_va)?;
    if src_fmt.format != dst_fmt.format {
        note_decline(Decline::StretchFormat);
        return declined(ctx, "SDL_SoftStretch");
    }
    let bpp = dst_fmt.bytes_per_pixel;
    if !(1..=4).contains(&bpp) || !src.is_direct(bpp) || !dst.is_direct(bpp) {
        note_decline(Decline::NotDirect);
        return declined(ctx, "SDL_SoftStretch");
    }
    // A NULL rect means the whole surface, exactly as SDL substitutes one.
    let srcrect = match srcrect_va {
        0 => Rect {
            x: 0,
            y: 0,
            w: src.w,
            h: src.h,
        },
        _ => read_rect(engine, srcrect_va)?,
    };
    let dstrect = match dstrect_va {
        0 => Rect {
            x: 0,
            y: 0,
            w: dst.w,
            h: dst.h,
        },
        _ => read_rect(engine, dstrect_va)?,
    };
    // `dstrect->w/h <= 0` is a documented no-op success.
    if dstrect.w <= 0 || dstrect.h <= 0 {
        return ctx.finish(0);
    }
    if srcrect.w > i32::from(u16::MAX)
        || srcrect.h > i32::from(u16::MAX)
        || dstrect.w > i32::from(u16::MAX)
        || dstrect.h > i32::from(u16::MAX)
    {
        note_decline(Decline::StretchTooLarge);
        return declined(ctx, "SDL_SoftStretch");
    }
    let Some(stretch) = plan(&src, srcrect, &dst, dstrect, bpp) else {
        note_decline(Decline::Geometry);
        return declined(ctx, "SDL_SoftStretch");
    };
    // `WIE_HOST_EXPORTS=shadow`: prove the stepper against the guest's own loop
    // on the real call path. The previous call's output is compared first (this
    // is the first moment it can be read back), then this call's native result
    // is stashed, and the guest still does the drawing — so the frame is exactly
    // what the unaccelerated run would show.
    if crate::host_exports::shadow_mode() {
        compare_shadow(engine, &stretch);
        stash_shadow(engine, &stretch)?;
        note_shadow_shadowed();
        return declined(ctx, "SDL_SoftStretch");
    }
    run(engine, &stretch)?;
    ctx.finish(0)
}

/// Report line for the shadow tally.
#[must_use]
pub(crate) fn shadow_report_line() -> Option<String> {
    shadow::report_line()
}

#[cfg(test)]
pub(crate) mod reference {
    //! An independent transcription of `scale_mat_nearest_N`
    //! (`src/video/SDL_stretch.c`, 2.32.10), written from the C so the tests
    //! compare two implementations instead of one against itself.
    //!
    //! The four `N` variants share their stepping and differ only in the pixel
    //! width, so one generic version is the whole family.

    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors scale_mat_nearest_N's signature; a struct would only \
                  restate the arguments the C call already names"
    )]
    /// Stretch `src` (row-major, `src_pitch` bytes per row) into a
    /// `dst_pitch * dst_h` buffer, leaving the row gaps at `gap_fill`.
    pub(crate) fn nearest(
        src: &[u8],
        src_w: usize,
        src_h: usize,
        src_pitch: usize,
        dst_w: usize,
        dst_h: usize,
        dst_pitch: usize,
        bpp: usize,
        gap_fill: u8,
    ) -> Vec<u8> {
        let mut out = vec![gap_fill; dst_pitch * dst_h];
        // `incy`/`incx` are Uint64 in SDL, and the halving is integer division.
        let incy = ((src_h as u64) << 16) / dst_h as u64;
        let incx = ((src_w as u64) << 16) / dst_w as u64;
        let mut posy = incy / 2;
        let mut dst = 0_usize;
        for _ in 0..dst_h {
            let srcy = (posy >> 16) as usize;
            posy += incy;
            let src_row = srcy * src_pitch;
            let mut posx = incx / 2;
            for _ in 0..dst_w {
                let srcx = bpp * ((posx >> 16) as usize);
                posx += incx;
                out[dst..dst + bpp].copy_from_slice(&src[src_row + srcx..src_row + srcx + bpp]);
                dst += bpp;
            }
            dst += dst_pitch - bpp * dst_w;
        }
        out
    }
}
