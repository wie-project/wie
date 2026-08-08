//! The run rasterizer: proportional glyph layout, per-row coverage blending,
//! and the effect strokes (split from `mod.rs` — the pixel-level rendering
//! core the text APIs call).

use anyhow::Result;

use super::{OPAQUE_BK_MODE, TextAttrs, TextTarget, char_for_code_point, clip_run_band, round_i32};
use crate::gdi32::{FontEngine, FontKey, IRect, RasterizedGlyph, ResolvedFont};

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
pub(super) fn render_run(
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

    // Effect strokes (GDI paints these AFTER the glyph ink, cutting through
    // the text): a horizontal line per enabled effect in the run's color,
    // spanning the whole visible band. Positions derive from the resolved
    // font metrics like Windows' otmfsStrikeoutPos/otmfsUnderlinePos — strike
    // ~45% of the ascent above the baseline (through the cap height),
    // underline just below it — both ~5% of the em thick.
    if (key.strike_out || key.underline) && span > 0 {
        let stroke_alphas = vec![255_u8; span];
        let stroke_thickness = resolved.height_px.div_euclid(20).max(1);
        let stroke = |target: &mut TextTarget<'_>,
                      row_start: i32,
                      engine: &mut dyn wie_cpu::CpuEngine|
         -> Result<()> {
            for row in row_start..row_start.saturating_add(stroke_thickness) {
                if row < y0 || row >= y1 {
                    continue;
                }
                target.write_row(engine, row, x0, x1, attrs.text_color, &stroke_alphas)?;
            }
            Ok(())
        };
        if key.strike_out {
            let strike_top = baseline.saturating_sub(round_i32(resolved.ascent * 0.45));
            stroke(&mut *target, strike_top, engine)?;
        }
        if key.underline {
            stroke(&mut *target, baseline.saturating_add(1), engine)?;
        }
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
pub(super) fn fill_rect(
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
