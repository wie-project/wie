use anyhow::{Context, Result};

use crate::guest_memory::{checked_field_address, write_i32 as write_guest_i32};
use crate::handles::Hdc;
use crate::{HandlerContext, WinApiHandlerResult};

use super::objects::dc_resolved_font;

/// Round an `f32` px metric to an `i32`.
#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
fn round_i32(value: f32) -> i32 {
    value.round() as i32
}

/// Handles `GDI32.dll!GetTextMetricsA`.
///
/// Fills a `TEXTMETRICA` from the resolved system font (px metrics shared
/// with the rasterizer and `GetTextExtentPoint32`).
pub fn handle_get_text_metrics_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for GetTextMetricsA")?;

    let metrics_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetTextMetricsA")?;

    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let (resolved, charset, bold, italic) = {
        let resolved = dc_resolved_font(state, hdc, &mut font_engine);
        match resolved {
            Some((key, resolved)) => {
                let charset = state
                    .try_gdi_state()
                    .and_then(|gdi| gdi.find_dc(Hdc::from(hdc)))
                    .and_then(|dc| dc.selected_font)
                    .and_then(|font_handle| {
                        state
                            .try_gdi_state()
                            .and_then(|gdi| gdi.find_font(font_handle))
                    })
                    .map_or(0, |font| font.charset);
                (Some(resolved), charset, key.weight >= 600, key.italic)
            }
            None => (None, 0, false, false),
        }
    };
    state.gdi_state().font_engine = font_engine;

    let success = metrics_ptr != 0;
    if success {
        let (height, ascent, descent, internal_leading, external_leading, avg_width, max_width) =
            match resolved {
                Some(resolved) => {
                    let height = resolved.line_height();
                    let ascent = round_i32(resolved.ascent);
                    // ab_glyph's descent is negative (below the baseline); GDI
                    // reports the positive magnitude.
                    let descent = 0_i32.saturating_sub(round_i32(resolved.descent));
                    let external = round_i32(resolved.line_gap);
                    // Internal leading: the part of the line height above the
                    // em square (zero for fonts whose typo span fits the em).
                    let internal =
                        round_i32((resolved.ascent - resolved.descent - resolved.scale).max(0.0));
                    (
                        height,
                        ascent,
                        descent,
                        internal,
                        external,
                        resolved.avg_advance,
                        resolved.max_advance,
                    )
                }
                None => (16, 12, 3, 0, 0, 8, 8),
            };
        let weight = if bold { 700 } else { 400 };
        // System fonts are vector (variable-pitch); TMPF_VECTOR = 0x01.
        let pitch_and_family: u8 = 0x01;

        // TEXTMETRICA layout (all LONG / BYTE fields packed):
        // tmHeight 0, tmAscent 4, tmDescent 8, tmInternalLeading 12,
        // tmExternalLeading 16, tmAveCharWidth 20, tmMaxCharWidth 24,
        // tmWeight 28, tmOverhang 32, tmDigitizedAspectX 36,
        // tmDigitizedAspectY 40, tmFirstChar 44, tmLastChar 45,
        // tmDefaultChar 46, tmBreakChar 47, tmItalic 48, tmUnderlined 49,
        // tmStruckOut 50, tmPitchAndFamily 51, tmCharSet 52
        write_guest_i32(engine, metrics_ptr, height)?; // tmHeight
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 4, "tmAscent"),
            ascent,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 8, "tmDescent"),
            descent,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 12, "tmInternalLeading"),
            internal_leading,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 16, "tmExternalLeading"),
            external_leading,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 20, "tmAveCharWidth"),
            avg_width,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 24, "tmMaxCharWidth"),
            max_width,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 28, "tmWeight"),
            weight,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 32, "tmOverhang"),
            0,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 36, "tmDigitizedAspectX"),
            0,
        )?;
        write_guest_i32(
            engine,
            checked_field_address(metrics_ptr, 40, "tmDigitizedAspectY"),
            0,
        )?;
        // BYTE fields at end
        engine.mem_write(checked_field_address(metrics_ptr, 44, "tmFirstChar"), &[0])?;
        engine.mem_write(checked_field_address(metrics_ptr, 45, "tmLastChar"), &[0])?;
        engine.mem_write(
            checked_field_address(metrics_ptr, 46, "tmDefaultChar"),
            &[0],
        )?;
        engine.mem_write(checked_field_address(metrics_ptr, 47, "tmBreakChar"), &[0])?;
        engine.mem_write(
            checked_field_address(metrics_ptr, 48, "tmItalic"),
            &[u8::from(italic)],
        )?;
        engine.mem_write(checked_field_address(metrics_ptr, 49, "tmUnderlined"), &[0])?;
        engine.mem_write(checked_field_address(metrics_ptr, 50, "tmStruckOut"), &[0])?;
        engine.mem_write(
            checked_field_address(metrics_ptr, 51, "tmPitchAndFamily"),
            &[pitch_and_family],
        )?;
        engine.mem_write(
            checked_field_address(metrics_ptr, 52, "tmCharSet"),
            &[charset],
        )?;
    }

    let return_value = u64::from(success);
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetTextMetricsA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `GDI32.dll!SetTextColor` (stores on the DC, returns previous color).
pub fn handle_set_text_color(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for SetTextColor")?;
    let color_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetTextColor")?;

    let color = u32::try_from(color_raw & u64::from(u32::MAX))
        .context("SetTextColor color does not fit u32")?;

    let previous = state
        .gdi_state()
        .find_dc_mut(Hdc::from(hdc))
        .map_or(0, |dc| std::mem::replace(&mut dc.text_color, color));

    let return_address = engine
        .return_from_win64_api(u64::from(previous))
        .context("failed to return from SetTextColor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(previous),
    })
}

/// Handles `GDI32.dll!SetBkColor` (stores on the DC, returns previous color).
pub fn handle_set_bk_color(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for SetBkColor")?;
    let color_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetBkColor")?;

    let color = u32::try_from(color_raw & u64::from(u32::MAX))
        .context("SetBkColor color does not fit u32")?;

    let previous = state
        .gdi_state()
        .find_dc_mut(Hdc::from(hdc))
        .map_or(0x00ff_ffff, |dc| std::mem::replace(&mut dc.bk_color, color));

    let return_address = engine
        .return_from_win64_api(u64::from(previous))
        .context("failed to return from SetBkColor")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(previous),
    })
}

/// Handles `GDI32.dll!SetBkMode` (stores on the DC, returns previous mode).
///
/// `TRANSPARENT = 1`, `OPAQUE = 2` (wingdi.h).
pub fn handle_set_bk_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for SetBkMode")?;
    let mode_raw = engine
        .read_rdx()
        .context("failed to read RDX for SetBkMode")?;

    let mode =
        u32::try_from(mode_raw & u64::from(u32::MAX)).context("SetBkMode mode does not fit u32")?;

    let previous = state
        .gdi_state()
        .find_dc_mut(Hdc::from(hdc))
        .map_or(2, |dc| std::mem::replace(&mut dc.bk_mode, mode));

    let return_address = engine
        .return_from_win64_api(u64::from(previous))
        .context("failed to return from SetBkMode")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(previous),
    })
}
