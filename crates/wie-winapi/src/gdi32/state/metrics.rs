use anyhow::{Context, Result};

use crate::guest_layout::{TextMetricA, TextMetricW};
use crate::guest_memory::with_typed_write;
use crate::handles::Hdc;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

use super::objects::dc_resolved_font;

/// Round an `f32` px metric to an `i32`.
fn round_i32(value: f32) -> i32 {
    value.round() as i32
}

/// The TEXTMETRIC fields both variants compute from the DC's resolved font.
struct TextMetrics {
    height: i32,
    ascent: i32,
    descent: i32,
    internal_leading: i32,
    external_leading: i32,
    avg_width: i32,
    max_width: i32,
    weight: i32,
    italic: bool,
    charset: u8,
}

/// Resolve the DC's selected font into the shared TEXTMETRIC fields.
///
/// Mirrors the A path exactly (same px metrics the rasterizer uses, same
/// fallback for an unresolved font), so `GetTextMetricsA` and `GetTextMetricsW`
/// can never disagree.
fn resolve_text_metrics(state: &mut WinApiState, hdc: u64) -> TextMetrics {
    let (resolved, charset, bold, italic) = state.with_font_engine(|state, font_engine| {
        let resolved = dc_resolved_font(state, hdc, font_engine);
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
    });

    let (height, ascent, descent, internal_leading, external_leading, avg_width, max_width) =
        match resolved {
            Some(resolved) => {
                let ascent = round_i32(resolved.ascent);
                // ab_glyph's descent is negative (below the baseline); GDI
                // reports the positive magnitude.
                let descent = 0_i32.saturating_sub(round_i32(resolved.descent));
                let external = round_i32(resolved.line_gap);
                // tmHeight = tmAscent + tmDescent (the Windows invariant).
                // The external leading is NOT folded in here — line
                // spacing is tmHeight + tmExternalLeading, which equals
                // the font engine's line_height() (ascent + |descent| +
                // gap), the value every line-based API uses.
                let height = ascent.saturating_add(descent);
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

    TextMetrics {
        height,
        ascent,
        descent,
        internal_leading,
        external_leading,
        avg_width,
        max_width,
        weight: if bold { 700 } else { 400 },
        italic,
        charset,
    }
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

    let success = metrics_ptr != 0;
    if success {
        let m = resolve_text_metrics(state, hdc);
        // System fonts are vector (variable-pitch); TMPF_VECTOR = 0x01.
        let pitch_and_family: u8 = 0x01;

        // TEXTMETRICA: 11 LONGs, 9 BYTEs, 3 pad bytes = 56 bytes. The typed
        // view zero-fills first, so the char fields (44..52), overhang, and
        // digitized-aspect fields read as zero exactly like the old
        // per-field writes left them.
        with_typed_write::<TextMetricA, _, _>(engine, metrics_ptr, |tm| {
            tm.height = m.height;
            tm.ascent = m.ascent;
            tm.descent = m.descent;
            tm.internal_leading = m.internal_leading;
            tm.external_leading = m.external_leading;
            tm.avg_char_width = m.avg_width;
            tm.max_char_width = m.max_width;
            tm.weight = m.weight;
            tm.overhang = 0;
            tm.digitized_aspect_x = 0;
            tm.digitized_aspect_y = 0;
            tm.italic = u8::from(m.italic);
            tm.pitch_and_family = pitch_and_family;
            tm.charset = m.charset;
            Ok(())
        })
        .context("failed to write TEXTMETRICA")?;
    }

    let return_value = u64::from(success);
    ctx.finish(return_value)
}

/// Handles `GDI32.dll!GetTextMetricsW`.
///
/// Same metrics as `GetTextMetricsA` in the `TEXTMETRICW` layout — the four
/// leading `WCHAR` character fields (tmFirstChar/tmLastChar/tmDefaultChar/
/// tmBreakChar) widen to 2 bytes and the BYTE flags shift to offset 52.
pub fn handle_get_text_metrics_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for GetTextMetricsW")?;

    let metrics_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetTextMetricsW")?;

    let success = metrics_ptr != 0;
    if success {
        let m = resolve_text_metrics(state, hdc);
        // System fonts are vector (variable-pitch); TMPF_VECTOR = 0x01.
        let pitch_and_family: u8 = 0x01;

        // TEXTMETRICW: 11 LONGs, 4 WCHARs, 5 BYTEs, 3 pad bytes = 60 bytes.
        // The char fields widen to WCHAR (44..51) and the flags shift to
        // 52..56 — the layout verified by the TextMetricW const-assert table.
        with_typed_write::<TextMetricW, _, _>(engine, metrics_ptr, |tm| {
            tm.height = m.height;
            tm.ascent = m.ascent;
            tm.descent = m.descent;
            tm.internal_leading = m.internal_leading;
            tm.external_leading = m.external_leading;
            tm.avg_char_width = m.avg_width;
            tm.max_char_width = m.max_width;
            tm.weight = m.weight;
            tm.overhang = 0;
            tm.digitized_aspect_x = 0;
            tm.digitized_aspect_y = 0;
            tm.italic = u8::from(m.italic);
            tm.pitch_and_family = pitch_and_family;
            tm.charset = m.charset;
            Ok(())
        })
        .context("failed to write TEXTMETRICW")?;
    }

    let return_value = u64::from(success);
    ctx.finish(return_value)
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

    ctx.finish(u64::from(previous))
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

    ctx.finish(u64::from(previous))
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

    ctx.finish(u64::from(previous))
}
