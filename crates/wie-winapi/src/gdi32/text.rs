//! Guest-side text rasterization with real macOS system fonts.
//!
//! TextOutA/W, ExtTextOutW and DrawTextA/W resolve the DC's selected font
//! through the font engine and blend per-glyph coverage into the DC's backing
//! store: guest DIB memory for memory DCs, the per-window present surface for
//! window DCs (the guest's WM_PAINT BitBlt publishes the frame). Screen DCs
//! have no backing store and are skipped.

use anyhow::{Context, Result};

use crate::gdi32::state::dc_resolved_font;
use crate::gdi32::{FontEngine, FontKey, IRect, RasterizedGlyph, ResolvedFont};
use crate::guest_memory::{
    checked_field_address, read_i32 as read_guest_i32, read_u32 as read_guest_u32,
    read_u64 as read_guest_u64, write_i32 as write_guest_i32,
};
use crate::guest_string::{read_ansi_bytes as read_guest_ansi_bytes, read_utf16_lossy};
use crate::user32::{low_i32, window_client_size};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState, gdi32::DcKind};

/// `BkMode` value for "fill the background" (wingdi.h).
const OPAQUE_BK_MODE: u32 = 2;

/// `ExtTextOutW` option flags (wingdi.h).
const ETO_OPAQUE: u32 = 0x0002;
const ETO_CLIPPED: u32 = 0x0004;

/// `DrawText` format flags (winuser.h) — the subset implemented here.
const DT_CENTER: u32 = 0x0000_0001;
const DT_RIGHT: u32 = 0x0000_0002;
const DT_NOCLIP: u32 = 0x0000_0100;
const DT_CALCRECT: u32 = 0x0000_0400;

/// Text-drawing attributes resolved from a DC (colors and background mode;
/// the font itself is resolved separately through the font engine).
#[derive(Debug, Clone, Copy)]
struct TextAttrs {
    /// 0RGB foreground color.
    text_color: u32,
    /// 0RGB background color.
    bk_color: u32,
    /// `BkMode`: 1 = TRANSPARENT, 2 = OPAQUE.
    bk_mode: u32,
}

/// Where glyph pixels land.
enum TextTarget<'a> {
    /// 32-bpp guest DIB. `top_down` mirrors `DibSection.height < 0`.
    Dib {
        bits_va: u64,
        stride: u32,
        width: u32,
        height: u32,
        top_down: bool,
    },
    /// Window present surface (0RGB pixels, top-down).
    Surface {
        pixels: &'a mut [u32],
        width: u32,
        height: u32,
    },
    /// A print job's active page canvas (0RGB white pixels, top-down). Same
    /// shape as [`Self::Surface`] — the row blend is shared — but there is no
    /// present-surface dirty rect to mark (the page is handed to EndDoc as-is).
    Page {
        pixels: &'a mut [u32],
        width: u32,
        height: u32,
    },
}

impl TextTarget<'_> {
    /// (width, height) in pixels.
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Dib { width, height, .. }
            | Self::Surface { width, height, .. }
            | Self::Page { width, height, .. } => (*width, *height),
        }
    }

    /// Blend a row of coverage alphas into the target with uniform `fg`.
    ///
    /// `alpha 0` leaves the existing pixel untouched (TRANSPARENT
    /// background); `alpha 255` replaces it with `fg`; anything between is
    /// blended with coverage alpha.
    fn write_row(
        &mut self,
        engine: &mut dyn wie_cpu::CpuEngine,
        visual_row: i32,
        x0: i32,
        x1: i32,
        fg: u32,
        alphas: &[u8],
    ) -> Result<()> {
        match self {
            Self::Dib {
                bits_va,
                stride,
                height,
                top_down,
                ..
            } => {
                if *bits_va == 0 {
                    return Ok(());
                }
                // Visual row → guest row (bottom-up DIBs store rows flipped).
                let visual = u32::try_from(visual_row).unwrap_or(0);
                let guest_row = if *top_down {
                    visual
                } else {
                    height.saturating_sub(1).saturating_sub(visual)
                };
                let start = bits_va
                    .saturating_add(u64::from(guest_row).saturating_mul(u64::from(*stride)))
                    .saturating_add(u64::try_from(x0).unwrap_or(0).saturating_mul(4));
                let byte_len = usize::try_from(x1.saturating_sub(x0))
                    .unwrap_or(0)
                    .saturating_mul(4);
                if byte_len == 0 {
                    return Ok(());
                }
                // Read-modify-write the row so TRANSPARENT leaves pixels alone.
                let mut buf = vec![0_u8; byte_len];
                if engine.mem_read(start, &mut buf).is_err() {
                    return Ok(()); // unmapped row — nothing to draw
                }
                blend_row_dib(&mut buf, fg, alphas);
                engine.mem_write(start, &buf)?;
            }
            Self::Surface {
                pixels: surf,
                width,
                height: _,
            }
            | Self::Page {
                pixels: surf,
                width,
                height: _,
            } => {
                let row_w = usize::try_from(*width).unwrap_or(0);
                let start = usize::try_from(visual_row)
                    .unwrap_or(0)
                    .saturating_mul(row_w)
                    .saturating_add(usize::try_from(x0).unwrap_or(0));
                let len = usize::try_from(x1.saturating_sub(x0)).unwrap_or(0);
                let Some(dst) = surf.get_mut(start..start.saturating_add(len)) else {
                    return Ok(());
                };
                blend_row_surface(dst, fg, alphas);
            }
        }
        Ok(())
    }
}

/// Alpha-blend `fg` over `dst` (both 0RGB) with coverage `alpha` (0..=255).
///
/// Uses `>> 8` instead of `/ 255` — a 0.4%-bright approximation that avoids
/// division (and is visually identical). Test-only now: the production row
/// blends inline this math, and the property tests pin this function as the
/// byte-identical scalar reference.
#[cfg(test)]
fn blend_pixel(dst: u32, fg: u32, alpha: u8) -> u32 {
    let a = u32::from(alpha);
    if a >= 255 {
        return fg & 0x00FF_FFFF;
    }
    let inv = 255_u32.saturating_sub(a);
    let fr = (fg >> 16) & 0xFF;
    let fg_g = (fg >> 8) & 0xFF;
    let fb = fg & 0xFF;
    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;
    let r = fr.saturating_mul(a).saturating_add(dr.saturating_mul(inv)) >> 8;
    let g = fg_g
        .saturating_mul(a)
        .saturating_add(dg.saturating_mul(inv))
        >> 8;
    let b = fb.saturating_mul(a).saturating_add(db.saturating_mul(inv)) >> 8;
    (r << 16) | (g << 8) | b
}

/// Blend a run of coverage alphas over a 0RGB pixel slice with uniform `fg`.
///
/// The glyph-coverage blend is `(fg_ch*a + dst_ch*(255-a)) >> 8` per channel
/// (the `>>8` instead of `/255` is the pipeline's documented 0.4%-bright
/// approximation; the weighted sum is bounded by 255·255, so plain u32
/// arithmetic cannot overflow). The alpha endpoints are NOT representable by
/// the formula — alpha 0 must leave the pixel untouched (TRANSPARENT), alpha
/// 255 must replace it with `fg` — so the loop guards them: the math runs
/// only for the AA pixels (1..=254), and LLVM's aarch64 backend lowers the
/// guard to a skip / direct-store with `csel`.
///
/// `#[inline(never)]` keeps the loop out of `write_row`'s large guest-memory
/// match, where inlining degrades its register allocation (2.34 ns/px
/// release); as a standalone function it compiles to a tight scalar loop
/// (~1.2 ns/px). The AUTOVECTORIZER does not fire in this repo's real build
/// (`lto = true` suppresses loop vectorization at crate compile — the final
/// binary is scalar `mul`/`madd`/`csel`), and an explicit 4-lane NEON variant
/// (`wie_cpu::blend_0rgb_4x`) measured SLOWER (1.51 vs 1.2 ns/px) because its
/// gather/scatter + endpoint fixups outweigh the parallel math on sparse
/// coverage. Byte-identity with the original scalar path is pinned by the
/// property tests below.
#[inline(never)]
fn blend_row_surface(dst: &mut [u32], fg: u32, alphas: &[u8]) {
    let fr = (fg >> 16) & 0xFF;
    let fg_g = (fg >> 8) & 0xFF;
    let fb = fg & 0xFF;
    let opaque = fg & 0x00FF_FFFF;
    for (slot, &alpha) in dst.iter_mut().zip(alphas.iter()) {
        let a = u32::from(alpha);
        if a == 0 {
            continue; // transparent — leave the pixel untouched
        }
        let inv = 255_u32.wrapping_sub(a);
        let dr = (*slot >> 16) & 0xFF;
        let dg = (*slot >> 8) & 0xFF;
        let db = *slot & 0xFF;
        let r = (fr * a + dr * inv) >> 8;
        let g = (fg_g * a + dg * inv) >> 8;
        let b = (fb * a + db * inv) >> 8;
        let blended = (r << 16) | (g << 8) | b;
        *slot = if a == 255 { opaque } else { blended };
    }
}

/// Blend a run of coverage alphas into a 32-bpp DIB row (`BGRA` bytes,
/// alpha forced opaque), with uniform `fg`.
///
/// Same math as `blend_row_surface`, byte-sliced because the guest DIB is a
/// byte buffer. The u32 read/write per 4-byte slot keeps this a straight
/// read-modify-write (no intermediate row copy).
fn blend_row_dib(buf: &mut [u8], fg: u32, alphas: &[u8]) {
    let n = alphas.len().min(buf.len() / 4);
    let fr = (fg >> 16) & 0xFF;
    let fg_g = (fg >> 8) & 0xFF;
    let fb = fg & 0xFF;
    let opaque = fg & 0x00FF_FFFF;
    for (slot, &alpha) in buf[..n.saturating_mul(4)]
        .chunks_exact_mut(4)
        .zip(alphas.iter())
    {
        let a = u32::from(alpha);
        if a == 0 {
            continue;
        }
        let inv = 255_u32.wrapping_sub(a);
        let existing = u32::from_le_bytes(slot.try_into().unwrap_or([0; 4]));
        let dr = (existing >> 16) & 0xFF;
        let dg = (existing >> 8) & 0xFF;
        let db = existing & 0xFF;
        let r = (fr * a + dr * inv) >> 8;
        let g = (fg_g * a + dg * inv) >> 8;
        let b = (fb * a + db * inv) >> 8;
        let blended = if a == 255 {
            opaque
        } else {
            (r << 16) | (g << 8) | b
        };
        let [b, g, r, _] = blended.to_le_bytes();
        slot.copy_from_slice(&[b, g, r, 0xFF]);
    }
}

/// Resolve the DC's text colors and background mode.
fn dc_text_attrs(state: &WinApiState, dc_handle: u64) -> TextAttrs {
    let dc = state
        .try_gdi_state()
        .and_then(|gdi| gdi.find_dc(crate::handles::Hdc::from(dc_handle)));
    TextAttrs {
        text_color: dc.map_or(0, |d| d.text_color),
        bk_color: dc.map_or(0x00FF_FFFF, |d| d.bk_color),
        bk_mode: dc.map_or(OPAQUE_BK_MODE, |d| d.bk_mode),
    }
}

/// A resolved text target: the pixel buffer the run renders into.
struct ResolvedTextTarget<'a> {
    /// The pixel target the run renders into.
    target: TextTarget<'a>,
    /// The surface hwnd for a window-DC target (`None` for DIBs) — the
    /// rendered band must be marked dirty on this surface so the next
    /// publish's region covers the text. The surface is keyed by the DC
    /// window's hwnd (see `resolve_text_target`), so the band's local
    /// coordinates ARE the surface coordinates.
    surface_hwnd: Option<crate::handles::Hwnd>,
}

/// Resolve a DC handle to a writable pixel target.
///
/// Memory DCs target the selected 32-bpp DIB; window DCs target the hwnd's
/// present surface (created on demand); print DCs target the job's active
/// page canvas (`StartPage` must have run — otherwise the text is a silent
/// no-op). Screen DCs resolve to `None`.
fn resolve_text_target(state: &mut WinApiState, dc_handle: u64) -> Option<ResolvedTextTarget<'_>> {
    let kind = state
        .try_gdi_state()?
        .find_dc(crate::handles::Hdc::from(dc_handle))?
        .kind;
    match kind {
        DcKind::Memory => {
            let gdi = state.try_gdi_state()?;
            let dc = gdi.find_dc(crate::handles::Hdc::from(dc_handle))?;
            let dib = gdi.find_dib(dc.selected_bitmap?)?;
            if dib.bit_count != 32 || dib.bits_va == 0 {
                return None;
            }
            Some(ResolvedTextTarget {
                target: TextTarget::Dib {
                    bits_va: dib.bits_va,
                    stride: u32::try_from(dib.stride).ok()?,
                    width: dib.width.unsigned_abs(),
                    height: dib.height.unsigned_abs(),
                    top_down: dib.height < 0,
                },
                surface_hwnd: None,
            })
        }
        DcKind::Window(hwnd) => {
            let (w, h) = window_client_size(state, hwnd.as_u64());
            let (w, h) = (u32::try_from(w.max(1)).ok()?, u32::try_from(h.max(1)).ok()?);
            state.present().ensure_surface(hwnd, w, h);
            let surface = state.present().surfaces.get_mut(&hwnd)?;
            Some(ResolvedTextTarget {
                target: TextTarget::Surface {
                    pixels: &mut surface.pixels[..],
                    width: surface.width,
                    height: surface.height,
                },
                surface_hwnd: Some(hwnd),
            })
        }
        // The print DC's canvas lives in the job (not the DcRecord — a
        // 300-DPI letter page is ~34 MB). `current` is `Some` only between
        // StartPage and EndPage, so TextOut before any page is a no-op.
        DcKind::Print(dc) => {
            let job = state.gdi_state().find_print_job_mut(dc)?;
            let canvas = job.current.as_mut()?;
            let width = canvas.width;
            let height = canvas.height;
            Some(ResolvedTextTarget {
                target: TextTarget::Page {
                    pixels: &mut canvas.pixels[..],
                    width,
                    height,
                },
                surface_hwnd: None,
            })
        }
        DcKind::Screen => None,
    }
}

/// Mark `rect` (already clipped to the surface, in surface coordinates) as
/// dirty on the surface a window-DC target resolves to. No-op for DIB targets
/// (memory DCs have no present-surface dirty state) and for degenerate rects.
///
/// Called AFTER the target's surface borrow has ended (NLL), so it can take
/// `&mut state` while the render's `TextTarget` no longer borrows it.
fn mark_surface_dirty(
    state: &mut WinApiState,
    surface_hwnd: Option<crate::handles::Hwnd>,
    rect: IRect,
) {
    if let Some(hwnd) = surface_hwnd
        && rect.right > rect.left
        && rect.bottom > rect.top
    {
        state.present().mark_dirty(hwnd, rect);
    }
}

/// Map a Unicode code point to a `char` (surrogates and non-characters → the
/// space glyph behavior).
fn char_for_code_point(ch: u32) -> char {
    char::from_u32(ch).unwrap_or(' ')
}

/// Round an `f32` px metric to an `i32`.
fn round_i32(value: f32) -> i32 {
    value.round() as i32
}

/// Clip a text run's line band to the target bounds and (optionally) to the
/// caller's clip rect.
///
/// The unclipped band is `(x, y, x + total_advance, y + line_height)` — the
/// full line the run occupies (`line_height` is ascent + descent, the same
/// height `GetTextExtentPoint32` reports). `OPAQUE` bk_mode fills exactly this
/// band with the background color, so the band is also the correct dirty rect
/// for a window-DC text pass: it covers the whole repainted line, not just the
/// glyph ink. Returns `None` when the band is empty or entirely off-target.
fn clip_run_band(
    x: i32,
    y: i32,
    total_advance: i32,
    line_height: i32,
    target_w: i32,
    target_h: i32,
    clip: Option<(i32, i32, i32, i32)>,
) -> Option<IRect> {
    let mut x0 = x.max(0);
    let mut y0 = y.max(0);
    let mut x1 = x.saturating_add(total_advance).min(target_w);
    let mut y1 = y.saturating_add(line_height).min(target_h);
    if let Some((cl, ct, cr, cb)) = clip {
        x0 = x0.max(cl);
        y0 = y0.max(ct);
        x1 = x1.min(cr);
        y1 = y1.min(cb);
    }
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(IRect {
        left: x0,
        top: y0,
        right: x1,
        bottom: y1,
    })
}

/// Rasterize a run of characters at (`x`, `y`) into `target`.
///
/// Glyphs are proportional: each is rasterized through the font engine (with
/// the CJK fallback) and advances by its own advance width. The baseline sits
/// `ascent` px below the line top (`y`). `OPAQUE` fills the line band
/// (`y` .. `y` + line height) with the background color first, then blends
/// glyph coverage over it; `TRANSPARENT` blends glyph pixels only. Fake bold
/// (no real bold face) draws each glyph twice, shifted +1 px. The band is
/// clipped to the target bounds and, when `clip` is given, to that rect.
///
/// Returns the clipped band actually written (`None` when nothing was drawn):
/// window-DC callers mark it as the surface's dirty rect, so the next publish
/// copies only the repainted line instead of the whole surface.
// Wide signature: one text run needs font + target + position + attrs + clip.
#[allow(clippy::too_many_arguments)]
fn render_run(
    engine: &mut dyn wie_cpu::CpuEngine,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
    target: &mut TextTarget<'_>,
    x: i32,
    y: i32,
    chars: &[u32],
    attrs: &TextAttrs,
    clip: Option<(i32, i32, i32, i32)>,
) -> Result<Option<IRect>> {
    if chars.is_empty() {
        return Ok(None);
    }
    let line_height = resolved.line_height().max(1);
    let baseline = y.saturating_add(round_i32(resolved.ascent));

    // Rasterize every glyph up front (so the OPAQUE band can use the summed
    // advance) and total the pen travel.
    let mut glyphs: Vec<(RasterizedGlyph, char)> = Vec::with_capacity(chars.len());
    let mut total_advance = 0_i32;
    for &code in chars {
        let ch = char_for_code_point(code);
        let glyph = font_engine.rasterize(resolved, key, ch);
        total_advance = total_advance.saturating_add(glyph.advance);
        glyphs.push((glyph, ch));
    }
    if total_advance <= 0 {
        return Ok(None);
    }

    let (tw, th) = target.dimensions();
    let tw_i = i32::try_from(tw).unwrap_or(i32::MAX);
    let th_i = i32::try_from(th).unwrap_or(i32::MAX);

    // Clip the band to the target and (optionally) to the caller's rect.
    let Some(band) = clip_run_band(x, y, total_advance, line_height, tw_i, th_i, clip) else {
        return Ok(None);
    };
    let (x0, y0, x1, y1) = (band.left, band.top, band.right, band.bottom);

    // OPAQUE background: fill the line band (clipped to the visible band
    // computed above, so ETO_CLIPPED / DrawText rects bound the fill) before
    // drawing glyphs.
    if attrs.bk_mode == OPAQUE_BK_MODE {
        fill_rect(
            engine,
            target,
            x0,
            y0,
            x1.saturating_sub(x0),
            y1.saturating_sub(y0),
            attrs.bk_color,
        )?;
    }

    let span = usize::try_from(x1.saturating_sub(x0)).unwrap_or(0);
    let x0_us = usize::try_from(x0).unwrap_or(0);
    // Accumulate each glyph's coverage into the row's alpha buffer row by
    // row (0 = transparent), then blend the run color over the canvas.
    for row in y0..y1 {
        let mut row_alphas: Vec<u8> = vec![0; span];
        let mut pen_x = x;
        for (glyph, _ch) in &glyphs {
            // Fake bold: re-draw the glyph one pixel right (foreground only).
            let passes = if resolved.fake_bold { 2 } else { 1 };
            for pass in 0..passes {
                let offset = pass;
                blend_glyph_row(
                    &mut row_alphas,
                    glyph,
                    pen_x.saturating_add(offset),
                    baseline,
                    row,
                    x0,
                    x0_us,
                );
            }
            pen_x = pen_x.saturating_add(glyph.advance);
        }
        target.write_row(engine, row, x0, x1, attrs.text_color, &row_alphas)?;
    }
    Ok(Some(band))
}

/// Blend one glyph's coverage for `row` into the row's alpha accumulator.
///
/// `row_pixels` holds one coverage byte per pixel (0 = transparent); later
/// glyphs overwrite earlier ones at overlaps, exactly as the previous
/// `Option` slots did. Coverage is color-independent — the run's uniform fg
/// is applied when the row hits the canvas.
// Wide signature: one row of one glyph needs pen position + row bounds.
#[allow(clippy::too_many_arguments)]
fn blend_glyph_row(
    row_pixels: &mut [u8],
    glyph: &RasterizedGlyph,
    pen_x: i32,
    baseline: i32,
    row: i32,
    x0: i32,
    x0_us: usize,
) {
    let glyph_top = baseline.saturating_add(glyph.top);
    let r = row.saturating_sub(glyph_top);
    if r < 0 || r >= glyph.height {
        return;
    }
    let width_us = usize::try_from(glyph.width).unwrap_or(0);
    let row_start = usize::try_from(r).unwrap_or(0).saturating_mul(width_us);
    let ink_left = pen_x.saturating_add(glyph.left);
    for c in 0..width_us {
        let alpha = *glyph
            .coverage
            .get(row_start.saturating_add(c))
            .unwrap_or(&0);
        if alpha == 0 {
            continue;
        }
        let px = ink_left.saturating_add(i32::try_from(c).unwrap_or(0));
        if px < x0 {
            continue;
        }
        let Some(slot) = row_pixels.get_mut(usize::try_from(px).unwrap_or(0).saturating_sub(x0_us))
        else {
            continue;
        };
        *slot = alpha;
    }
}

/// Fill a rectangle with a solid color (used by `ETO_OPAQUE` and the OPAQUE
/// line band).
fn fill_rect(
    engine: &mut dyn wie_cpu::CpuEngine,
    target: &mut TextTarget<'_>,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    color: u32,
) -> Result<()> {
    let (tw, th) = target.dimensions();
    let tw_i = i32::try_from(tw).unwrap_or(i32::MAX);
    let th_i = i32::try_from(th).unwrap_or(i32::MAX);
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = x.saturating_add(cx).min(tw_i);
    let y1 = y.saturating_add(cy).min(th_i);
    if x1 <= x0 || y1 <= y0 {
        return Ok(());
    }
    let span = usize::try_from(x1.saturating_sub(x0)).unwrap_or(0);
    // Uniform opaque fill: every pixel gets alpha 255, so the row blend
    // replaces each canvas pixel with the fill color.
    let row_alphas = vec![255_u8; span];
    for row in y0..y1 {
        target.write_row(engine, row, x0, x1, color, &row_alphas)?;
    }
    Ok(())
}

/// Read the character list for a text API, honoring an explicit count.
///
/// The read is capped at 4096 characters so a bogus guest count can never
/// scan unbounded guest memory; NUL-terminated strings stop early.
pub(crate) fn read_text_chars(
    engine: &mut dyn wie_cpu::CpuEngine,
    ptr: u64,
    count: u32,
    wide: bool,
) -> Result<Vec<u32>> {
    let count_us = usize::try_from(count).unwrap_or(0).min(4096);
    if ptr == 0 || count_us == 0 {
        return Ok(Vec::new());
    }
    if wide {
        let text = read_utf16_lossy(engine, ptr, count_us)?;
        Ok(text.chars().map(u32::from).collect())
    } else {
        let bytes = read_guest_ansi_bytes(engine, ptr, count_us)?;
        // UTF-8-first like the other A-string readers, so a multi-byte
        // UTF-8 literal rasterizes as one glyph per char, not one glyph
        // per byte (which rendered `—` as `â€"`).
        Ok(crate::vfs::decode_ansi_utf8_first(&bytes)
            .chars()
            .map(u32::from)
            .collect())
    }
}

/// Shared `TextOutA/W` implementation.
///
/// Draws a single run at (`x`, `y`) into the DC's backing store. Returns the
/// caller-supplied character count, as real GDI does.
fn handle_text_out_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let x = low_i32(
        engine
            .read_rdx()
            .with_context(|| format!("failed to read RDX for {api_name}"))?,
        api_name,
    )?;
    let y = low_i32(
        engine
            .read_r8()
            .with_context(|| format!("failed to read R8 for {api_name}"))?,
        api_name,
    )?;
    let text_ptr = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;
    // `cchString` is the 5th argument — first stack slot.
    let rsp = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {api_name}"))?;
    let cch = read_guest_u32(engine, checked_field_address(rsp, 0x28, "cchString"))
        .with_context(|| format!("failed to read {api_name} cchString"))?;

    if cch != 0 {
        let chars = read_text_chars(engine, text_ptr, cch, wide)?;
        let attrs = dc_text_attrs(state, hdc);
        if !chars.is_empty() {
            // Take the font engine out of gdi state so the surface borrow
            // below can coexist; put it back when the render is done.
            let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
            let resolved = dc_resolved_font(state, hdc, &mut font_engine);
            let rendered: Result<()> = if let Some((key, resolved)) = resolved {
                match resolve_text_target(state, hdc) {
                    Some(ResolvedTextTarget {
                        mut target,
                        surface_hwnd,
                    }) => {
                        // Window-DC text marks its clipped line band as the
                        // surface's dirty rect, so the next publish uploads
                        // only the repainted line.
                        let band = render_run(
                            engine,
                            &mut font_engine,
                            &resolved,
                            &key,
                            &mut target,
                            x,
                            y,
                            &chars,
                            &attrs,
                            None,
                        )?;
                        mark_surface_dirty(state, surface_hwnd, band.unwrap_or(IRect::empty()));
                        Ok(())
                    }
                    None => Ok(()),
                }
            } else {
                Ok(())
            };
            state.gdi_state().font_engine = font_engine;
            rendered?;
        }
    }

    let return_address = engine
        .return_from_win64_api(u64::from(cch))
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(cch),
    })
}

/// Handles `GDI32.dll!TextOutA`.
pub fn handle_text_out_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_text_out_impl(ctx, "TextOutA", false)
}

/// Handles `GDI32.dll!TextOutW`.
pub fn handle_text_out_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_text_out_impl(ctx, "TextOutW", true)
}

/// Handles `GDI32.dll!ExtTextOutW`.
///
/// Supports `ETO_OPAQUE` (pre-fill the option rect with the background color)
/// and `ETO_CLIPPED` (clip glyphs to the option rect); other option bits are
/// ignored. Always returns 1 (success).
pub fn handle_ext_text_out_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for ExtTextOutW")?;
    let x = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for ExtTextOutW")?,
        "ExtTextOutW",
    )?;
    let y = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for ExtTextOutW")?,
        "ExtTextOutW",
    )?;
    let options_raw = engine
        .read_r9()
        .context("failed to read R9 for ExtTextOutW")?;
    let options = u32::try_from(options_raw & u64::from(u32::MAX))
        .context("ExtTextOutW options do not fit u32")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for ExtTextOutW")?;
    let rect_ptr = read_guest_u64(engine, checked_field_address(rsp, 0x28, "lprect"))
        .context("failed to read ExtTextOutW lprect")?;
    let text_ptr = read_guest_u64(engine, checked_field_address(rsp, 0x30, "lpString"))
        .context("failed to read ExtTextOutW lpString")?;
    let cch = read_guest_u32(engine, checked_field_address(rsp, 0x38, "cch"))
        .context("failed to read ExtTextOutW cch")?;

    let mut clip = None;
    if options & (ETO_OPAQUE | ETO_CLIPPED) != 0 && rect_ptr != 0 {
        let left = read_guest_i32(engine, rect_ptr).context("failed to read RECT.left")?;
        let top = read_guest_i32(engine, checked_field_address(rect_ptr, 4, "RECT.top"))
            .context("failed to read RECT.top")?;
        let right = read_guest_i32(engine, checked_field_address(rect_ptr, 8, "RECT.right"))
            .context("failed to read RECT.right")?;
        let bottom = read_guest_i32(engine, checked_field_address(rect_ptr, 12, "RECT.bottom"))
            .context("failed to read RECT.bottom")?;
        if options & ETO_OPAQUE != 0 {
            let attrs = dc_text_attrs(state, hdc);
            if let Some(ResolvedTextTarget {
                mut target,
                surface_hwnd,
            }) = resolve_text_target(state, hdc)
            {
                // The fill is clipped to the surface dims inside `fill_rect`;
                // mirror that clip so the marked rect is the exact written
                // region (over-marking is safe, under-marking corrupts).
                let (tw, th) = target.dimensions();
                let tw_i = i32::try_from(tw).unwrap_or(i32::MAX);
                let th_i = i32::try_from(th).unwrap_or(i32::MAX);
                let x0 = left.max(0);
                let y0 = top.max(0);
                let x1 = left.saturating_add(right.saturating_sub(left)).min(tw_i);
                let y1 = top.saturating_add(bottom.saturating_sub(top)).min(th_i);
                fill_rect(
                    engine,
                    &mut target,
                    left,
                    top,
                    right.saturating_sub(left),
                    bottom.saturating_sub(top),
                    attrs.bk_color,
                )?;
                mark_surface_dirty(
                    state,
                    surface_hwnd,
                    IRect {
                        left: x0,
                        top: y0,
                        right: x1,
                        bottom: y1,
                    },
                );
            }
        }
        if options & ETO_CLIPPED != 0 {
            clip = Some((left, top, right, bottom));
        }
    }

    if cch != 0 {
        let chars = read_text_chars(engine, text_ptr, cch, true)?;
        let attrs = dc_text_attrs(state, hdc);
        if !chars.is_empty() {
            let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
            let resolved = dc_resolved_font(state, hdc, &mut font_engine);
            let rendered: Result<()> = if let Some((key, resolved)) = resolved {
                match resolve_text_target(state, hdc) {
                    Some(ResolvedTextTarget {
                        mut target,
                        surface_hwnd,
                    }) => {
                        let band = render_run(
                            engine,
                            &mut font_engine,
                            &resolved,
                            &key,
                            &mut target,
                            x,
                            y,
                            &chars,
                            &attrs,
                            clip,
                        )?;
                        mark_surface_dirty(state, surface_hwnd, band.unwrap_or(IRect::empty()));
                        Ok(())
                    }
                    None => Ok(()),
                }
            } else {
                Ok(())
            };
            state.gdi_state().font_engine = font_engine;
            rendered?;
        }
    }

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from ExtTextOutW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}

/// Shared `DrawTextA/W` implementation.
///
/// Scope: `DT_SINGLELINE`, `DT_CALCRECT`, `DT_NOCLIP`, and horizontal
/// alignment (`DT_LEFT`/`DT_CENTER`/`DT_RIGHT`). Word wrap and `DT_VCENTER`
/// are deferred. `DT_CALCRECT` measures only; otherwise the line is rendered
/// into the rect. Returns the text height in logical units.
fn handle_draw_text_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let text_ptr = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;
    let cch_raw = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;
    let rect_ptr = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;
    // `format` is the 5th argument — first stack slot.
    let rsp = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {api_name}"))?;
    let format = read_guest_u32(engine, checked_field_address(rsp, 0x28, "format"))
        .with_context(|| format!("failed to read {api_name} format"))?;

    let return_value = if rect_ptr != 0 {
        let left = read_guest_i32(engine, rect_ptr)
            .with_context(|| format!("failed to read {api_name} RECT.left"))?;
        let top = read_guest_i32(engine, checked_field_address(rect_ptr, 4, "RECT.top"))
            .with_context(|| format!("failed to read {api_name} RECT.top"))?;
        let right = read_guest_i32(engine, checked_field_address(rect_ptr, 8, "RECT.right"))
            .with_context(|| format!("failed to read {api_name} RECT.right"))?;
        let bottom = read_guest_i32(engine, checked_field_address(rect_ptr, 12, "RECT.bottom"))
            .with_context(|| format!("failed to read {api_name} RECT.bottom"))?;

        // `cchText == -1` means the string is NUL-terminated.
        let cch = low_i32(cch_raw, api_name)?;
        let chars = if cch == -1 {
            read_text_chars(engine, text_ptr, 4096, wide)?
        } else if cch > 0 {
            read_text_chars(engine, text_ptr, u32::try_from(cch).unwrap_or(0), wide)?
        } else {
            Vec::new()
        };

        let attrs = dc_text_attrs(state, hdc);
        let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
        let resolved_font = dc_resolved_font(state, hdc, &mut font_engine);
        let (line_h, text_w) = match &resolved_font {
            Some((key, resolved)) => {
                let mut width = 0_i32;
                for &code in &chars {
                    if let Some(ch) = char::from_u32(code) {
                        width = width.saturating_add(font_engine.char_advance(resolved, key, ch));
                    }
                }
                (resolved.line_height(), width)
            }
            None => (16, 0),
        };
        state.gdi_state().font_engine = font_engine;

        // Horizontal alignment inside the rect.
        let rect_w = right.saturating_sub(left);
        let x = if format & DT_RIGHT != 0 {
            left.saturating_add(rect_w.saturating_sub(text_w)).max(left)
        } else if format & DT_CENTER != 0 {
            left.saturating_add(rect_w.saturating_sub(text_w).saturating_div(2))
                .max(left)
        } else {
            left
        };
        let y = top;

        if format & DT_CALCRECT != 0 {
            // Measure only: shrink the rect to the text bounds.
            let new_right = x.saturating_add(text_w);
            let new_bottom = y.saturating_add(line_h);
            write_guest_i32(
                engine,
                checked_field_address(rect_ptr, 8, "RECT.right"),
                new_right,
            )
            .with_context(|| format!("failed to write {api_name} RECT.right"))?;
            write_guest_i32(
                engine,
                checked_field_address(rect_ptr, 12, "RECT.bottom"),
                new_bottom,
            )
            .with_context(|| format!("failed to write {api_name} RECT.bottom"))?;
        } else {
            let clip = if format & DT_NOCLIP != 0 {
                None
            } else {
                Some((left, top, right, bottom))
            };
            if !chars.is_empty() {
                let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
                let rendered: Result<()> = if let Some((key, resolved)) =
                    dc_resolved_font(state, hdc, &mut font_engine)
                {
                    match resolve_text_target(state, hdc) {
                        Some(ResolvedTextTarget {
                            mut target,
                            surface_hwnd,
                        }) => {
                            let band = render_run(
                                engine,
                                &mut font_engine,
                                &resolved,
                                &key,
                                &mut target,
                                x,
                                y,
                                &chars,
                                &attrs,
                                clip,
                            )?;
                            mark_surface_dirty(state, surface_hwnd, band.unwrap_or(IRect::empty()));
                            Ok(())
                        }
                        None => Ok(()),
                    }
                } else {
                    Ok(())
                };
                state.gdi_state().font_engine = font_engine;
                rendered?;
            }
        }

        u64::try_from(line_h).unwrap_or(0)
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `USER32.dll!DrawTextA`.
pub fn handle_draw_text_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_draw_text_impl(ctx, "DrawTextA", false)
}

/// Handles `USER32.dll!DrawTextW`.
pub fn handle_draw_text_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_draw_text_impl(ctx, "DrawTextW", true)
}

/// Render text into a caller-owned surface pixel buffer (control painting).
///
/// TRANSPARENT background: only glyph pixels are blended, so the control's
/// face fill shows through. `clip` restricts the glyph band (control rect).
/// The caller supplies the resolved default font (sans-serif 16 px) — the
/// same rasterizer the text APIs use, exposed for the host-side control
/// WndProcs which have no DC to resolve.
// Wide signature: text-into-surface carries target + position + text + color + clip.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_text_into_surface(
    engine: &mut dyn wie_cpu::CpuEngine,
    font_engine: &mut FontEngine,
    pixels: &mut [u32],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    clip: Option<(i32, i32, i32, i32)>,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> Result<()> {
    let chars: Vec<u32> = text.chars().map(u32::from).collect();
    if chars.is_empty() {
        return Ok(());
    }
    let attrs = TextAttrs {
        text_color: color,
        bk_color: 0,
        bk_mode: 1, // TRANSPARENT — the caller's fill shows through
    };
    let mut target = TextTarget::Surface {
        pixels,
        width,
        height,
    };
    render_run(
        engine,
        font_engine,
        resolved,
        key,
        &mut target,
        x,
        y,
        &chars,
        &attrs,
        clip,
    )
    .map(|_| ())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{blend_pixel, blend_row_dib, blend_row_surface, clip_run_band, handle_text_out_w};
    use crate::gdi32::IRect;

    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    use std::sync::{Arc, Mutex};

    use crate::guest_heap::GuestHeap;
    use crate::handles::Hdc;
    use crate::present::MessageQueue;
    use crate::sync_obj::SyncState;
    use crate::thread::ThreadState;
    use crate::vfs::VolumeConfig;
    use crate::{
        DllStateMap, FileIoState, GuestStdinMode, HandlerContext, HeapState, KernelState,
        ModuleState, ProcessState, WinApiEnvironment, WinApiState,
    };

    const STACK_VA: u64 = 0x100_0000;
    const STACK_TOP: u64 = 0x100_FF00;

    /// The dirty rect for a window-DC text run must equal the run's clipped
    /// line band: `left = run x`, `top = baseline − ascent = y`,
    /// `right = x + summed advance`, `bottom = y + line_height` — the same
    /// extents `GetTextExtentPoint32` reports (ascent + descent).
    #[test]
    fn text_run_dirty_rect_equals_run_extents() {
        let band = clip_run_band(8, 8, 90, 30, 200, 100, None).expect("band on-surface");
        assert_eq!(
            band,
            IRect {
                left: 8,
                top: 8,
                right: 98,
                bottom: 38,
            }
        );
    }

    #[test]
    fn text_run_dirty_rect_clips_to_bounds_and_guest_clip() {
        // Partially off the right/bottom edge of a 100×40 target.
        let band = clip_run_band(60, 20, 90, 30, 100, 40, None).expect("band on-surface");
        assert_eq!(
            band,
            IRect {
                left: 60,
                top: 20,
                right: 100,
                bottom: 40,
            }
        );
        // ETO_CLIPPED / DrawText rect narrows the band.
        let clipped =
            clip_run_band(8, 8, 90, 30, 200, 100, Some((50, 0, 80, 20))).expect("band inside clip");
        assert_eq!(
            clipped,
            IRect {
                left: 50,
                top: 8,
                right: 80,
                bottom: 20,
            }
        );
    }

    #[test]
    fn text_run_dirty_rect_degenerate_or_off_surface_is_none() {
        // Zero advance → nothing to repaint.
        assert_eq!(clip_run_band(8, 8, 0, 30, 200, 100, None), None);
        // Entirely below the target.
        assert_eq!(clip_run_band(8, 200, 90, 30, 100, 100, None), None);
        // Entirely right of the target.
        assert_eq!(clip_run_band(200, 8, 90, 30, 100, 100, None), None);
        // Clip rect disjoint from the band.
        assert_eq!(
            clip_run_band(8, 8, 90, 30, 200, 100, Some((0, 100, 50, 150))),
            None
        );
    }

    /// Property test: the vectorized row blends are byte-identical to the
    /// scalar reference (original `blend_pixel` + the alpha-0 skip / alpha-255
    /// replace rules) over seeded random pixels, including the boundary alphas
    /// 0 and 255. Pins the AA edges — the blend output must not change.
    #[test]
    fn blend_rows_match_scalar_reference() {
        // The original write_row Surface semantics: alpha 0 leaves the pixel
        // untouched (the old `None` slot), everything else goes through
        // `blend_pixel`.
        let reference = |dst: u32, fg: u32, alpha: u8| -> u32 {
            if alpha == 0 {
                dst
            } else {
                blend_pixel(dst, fg, alpha)
            }
        };
        let mut state = 0x5EED_0DD5_u32;
        let lcg = |s: &mut u32| -> u32 {
            *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *s
        };
        for _ in 0..2000 {
            let n = usize::try_from(lcg(&mut state) % 129).unwrap_or(0);
            let fg = lcg(&mut state) & 0x00FF_FFFF;
            let mut dst: Vec<u32> = (0..n).map(|_| lcg(&mut state) & 0x00FF_FFFF).collect();
            let mut alphas: Vec<u8> = (0..n)
                .map(|_| u8::try_from(lcg(&mut state) & 0xFF).unwrap_or(0))
                .collect();
            // Pin the boundary alphas into every row.
            if let Some(slot) = alphas.first_mut() {
                *slot = 0;
            }
            if let Some(slot) = alphas.get_mut(1) {
                *slot = 255;
            }
            let mut want = dst.clone();
            for (i, &alpha) in alphas.iter().enumerate() {
                want[i] = reference(want[i], fg, alpha);
            }
            blend_row_surface(&mut dst, fg, &alphas);
            assert_eq!(dst, want, "surface blend diverged from scalar reference");
        }
    }

    /// Property test: the DIB row blend (BGRA bytes, alpha forced opaque) is
    /// byte-identical to the original write_row Dib loop over random bytes and
    /// alphas, including the 0/255 boundaries.
    #[test]
    fn dib_blend_rows_match_scalar_reference() {
        let lcg = |s: &mut u32| -> u32 {
            *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *s
        };
        // The original write_row Dib semantics per 4-byte BGRA slot.
        let reference = |slot: &[u8], fg: u32, alpha: u8| -> [u8; 4] {
            if alpha == 0 {
                return [slot[0], slot[1], slot[2], slot[3]];
            }
            let existing = u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]);
            let blended = blend_pixel(existing, fg, alpha);
            let [b, g, r, _] = blended.to_le_bytes();
            [b, g, r, 0xFF]
        };
        let mut state = 0xD1B_5EED_u32;
        for _ in 0..2000 {
            let n = usize::try_from(lcg(&mut state) % 129).unwrap_or(0);
            let fg = lcg(&mut state) & 0x00FF_FFFF;
            let mut buf: Vec<u8> = (0..n.saturating_mul(4))
                .map(|_| u8::try_from(lcg(&mut state) & 0xFF).unwrap_or(0))
                .collect();
            let mut alphas: Vec<u8> = (0..n)
                .map(|_| u8::try_from(lcg(&mut state) & 0xFF).unwrap_or(0))
                .collect();
            if let Some(slot) = alphas.first_mut() {
                *slot = 0;
            }
            if let Some(slot) = alphas.get_mut(1) {
                *slot = 255;
            }
            let mut want = buf.clone();
            for (i, &alpha) in alphas.iter().enumerate() {
                let off = i.saturating_mul(4);
                let out = reference(&want[off..off.saturating_add(4)], fg, alpha);
                want[off..off.saturating_add(4)].copy_from_slice(&out);
            }
            blend_row_dib(&mut buf, fg, &alphas);
            assert_eq!(buf, want, "DIB blend diverged from scalar reference");
        }
    }

    // ── Print-DC text routing (P1b) ──────────────────────────────────────
    // The print arm drives the real handlers — CreateDCW → StartDocW →
    // StartPage → TextOutW — and asserts the rasterized glyphs land in the
    // job's active PageCanvas. Scaffolding mirrors the print.rs test module.

    /// Minimal engine: maps guest pages and a stack with a valid return
    /// address (`return_from_win64_api` pops it).
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, 0x1_0000, RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        // `return_from_win64_api` pops the return address, so RSP drifts 8
        // bytes past STACK_TOP after the first call; reset it every call.
        cpu.write_rsp(STACK_TOP).ok();
    }

    /// Write the 5th/6th stack args at their Win64 shadow-space slots.
    fn write_stack_args(cpu: &mut IcedCpu, fifth: u64, sixth: u64) {
        cpu.mem_write(STACK_TOP + 0x28, &fifth.to_le_bytes())
            .expect("write 5th stack arg");
        cpu.mem_write(STACK_TOP + 0x30, &sixth.to_le_bytes())
            .expect("write 6th stack arg");
    }

    fn write_u64(cpu: &mut IcedCpu, addr: u64, value: u64) {
        cpu.mem_write(addr, &value.to_le_bytes())
            .expect("write u64");
    }

    fn write_utf16(cpu: &mut IcedCpu, addr: u64, s: &str) {
        let mut bytes = Vec::new();
        for unit in s.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        cpu.mem_write(addr, &bytes).expect("write utf16 string");
    }

    fn test_environment() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 0,
        }
    }

    /// Minimal state: the print + text handlers only touch the GDI slot
    /// (lazily initialized by `gdi_state()`).
    fn test_winapi_state() -> WinApiState {
        WinApiState {
            heap_state: HeapState {
                heap: GuestHeap::new(0x2000, 0x10000),
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: crate::FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: ahash::HashMap::default(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: GuestStdinMode::InjectOnly,
                ucrt_files: ahash::HashMap::default(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: ahash::HashMap::default(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: crate::RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: ahash::HashMap::default(),
                environment: Vec::new(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: ahash::HashMap::default(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: ahash::HashMap::default(),
                import_resolver: None,
                get_proc_address_cache: ahash::HashMap::default(),
                next_module_handle: crate::ModuleHandle::from(
                    crate::dll_loader::REAL_MODULE_HANDLE_BASE,
                ),
            },
        }
    }

    fn run(
        ctx: &mut HandlerContext<'_>,
        handler: fn(&mut HandlerContext<'_>) -> anyhow::Result<crate::WinApiHandlerResult>,
    ) -> u64 {
        handler(ctx).expect("handler should succeed").return_value
    }

    /// Create a print DC via the CreateDCW handler and return its handle.
    fn create_print_dc(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
        write_regs(engine, 0, 0, 0, 0); // driver = NULL (any driver → print job)
        run(
            &mut HandlerContext::new(engine, test_environment(), state),
            crate::gdi32::handle_create_dc_w,
        )
    }

    /// StartDocW on `hdc` with a DOCINFOW written at `0x2000` (doc name at
    /// `0x3000`). Returns the handler's success value.
    fn start_doc(engine: &mut IcedCpu, state: &mut WinApiState, hdc: u64) -> u64 {
        cpu_write_i32(engine, 0x2000, 40).expect("DOCINFO.cbSize");
        write_u64(engine, 0x2008, 0x3000);
        write_utf16(engine, 0x3000, "P1b text test");
        write_regs(engine, hdc, 0x2000, 0, 0);
        run(
            &mut HandlerContext::new(engine, test_environment(), state),
            crate::gdi32::handle_start_doc_w,
        )
    }

    fn cpu_write_i32(cpu: &mut IcedCpu, addr: u64, value: i32) -> Result<(), wie_cpu::CpuError> {
        cpu.mem_write(addr, &value.to_le_bytes())
    }

    /// TextOutW on an active print page must rasterize into the job's
    /// PageCanvas: black 16 px glyphs at (200, 300) on the white 2550×3300
    /// page leave non-white ink pixels.
    #[test]
    fn text_out_w_on_print_dc_rasterizes_page_pixels() {
        let mut engine = test_engine();
        let mut state = test_winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        assert_eq!(start_doc(&mut engine, &mut state, hdc), 1);
        write_regs(&mut engine, hdc, 0, 0, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                crate::gdi32::handle_start_page,
            ),
            1
        );

        // TextOutW(hdc, 200, 300, L"Hello print", 11).
        const TEXT_VA: u64 = 0x4000;
        write_utf16(&mut engine, TEXT_VA, "Hello print");
        write_regs(&mut engine, hdc, 200, 300, TEXT_VA);
        write_stack_args(&mut engine, 11, 0); // cch = 11 (WIDE chars)
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_text_out_w,
            ),
            11,
            "TextOutW returns the character count"
        );

        let job = state
            .gdi_state()
            .find_print_job(Hdc::from(hdc))
            .expect("print job");
        let canvas = job.current.as_ref().expect("active page canvas");
        assert_eq!((canvas.width, canvas.height), (2550, 3300));
        // The only drawing is the text, so any non-white pixel is glyph ink.
        let ink = canvas.pixels.iter().filter(|&&p| p != 0x00FF_FFFF).count();
        assert!(
            ink > 20,
            "the page must carry rasterized text pixels (found {ink})"
        );
    }

    /// TextOutW on a print DC before StartPage has no canvas: the handler
    /// must be a silent no-op (returns the char count, page untouched).
    #[test]
    fn text_out_w_before_start_page_is_a_silent_noop() {
        let mut engine = test_engine();
        let mut state = test_winapi_state();
        let hdc = create_print_dc(&mut engine, &mut state);

        assert_eq!(start_doc(&mut engine, &mut state, hdc), 1);
        // No StartPage — `job.current` is None, so the text arm resolves no
        // target and draws nothing.
        const TEXT_VA: u64 = 0x4000;
        write_utf16(&mut engine, TEXT_VA, "Hello print");
        write_regs(&mut engine, hdc, 200, 300, TEXT_VA);
        write_stack_args(&mut engine, 11, 0);
        assert_eq!(
            run(
                &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
                handle_text_out_w,
            ),
            11,
            "TextOutW still returns the character count"
        );

        let job = state
            .gdi_state()
            .find_print_job(Hdc::from(hdc))
            .expect("print job");
        assert!(job.current.is_none(), "no page was started");
        assert!(job.pages.is_empty());
    }
}
