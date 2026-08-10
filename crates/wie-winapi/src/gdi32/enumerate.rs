//! Font enumeration + glyph outline + font-resource lanes (soft-dispatch).
//!
//! `EnumFontFamiliesExW/A`, `EnumFontFamiliesW/A` and `EnumFonts` enumerate
//! the host fontdb database through a guest `FONTENUMPROC` callback, using the
//! runtime's full-iteration callback bridge
//! ([`WinApiControlSignal::EnumerationCallbackRequested`]). `GetGlyphOutlineW/A`
//! rasterizes a glyph (GGO_BITMAP). `AddFontResourceW/A` /
//! `RemoveFontResourceW/A` load/unload a font file into a process-wide
//! added-font database.
//!
//! Routed from `dispatch_gdi32_extra` (gdi32/mod.rs) — the string-match
//! fallback the dense `WinApiId` table does not cover.

use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

use crate::guest_layout::{
    EnumLogFontExA, EnumLogFontExW, GlyphMetrics, LogFontA, LogFontW, TextMetricA, TextMetricW,
};
use crate::guest_memory::{
    checked_address, read_typed_copy, read_u32, read_u64, with_typed_write, write_bytes,
};
use crate::guest_string::{
    decode_ansi_lossy, decode_utf16_lossy, encode_cp1252, read_ansi_lossy as read_guest_ansi_lossy,
    read_utf16_lossy as read_guest_utf16_lossy,
};
use crate::{
    GuestCallbackRequest, HandlerContext, OuterReturn, WinApiControlSignal, WinApiHandlerResult,
    WinApiState,
};

use super::font_system::{FontKey, ResolvedFont, system_family_names};
use super::state::dc_resolved_font;

/// `TRUETYPE_FONTTYPE` — the `FontType` reported for every fontdb face.
const TRUETYPE_FONTTYPE: u32 = 4;
/// `DEFAULT_CHARSET` — matches every face regardless of script.
const DEFAULT_CHARSET: u8 = 1;
/// `GGO_BITMAP` — the glyph-outline format this lane implements.
const GGO_BITMAP: u64 = 1;
/// `GGO_NATIVE` — the TrueType-outline format (not implemented; honest error).
const GGO_NATIVE: u64 = 2;
/// `GDI_ERROR` — the 32-bit `-1` failure sentinel.
const GDI_ERROR: u64 = 0xFFFF_FFFF;

const ENUMLOGFONTEXW_SIZE: usize = 348;
const ENUMLOGFONTEXA_SIZE: usize = 188;
const TEXTMETRICW_SIZE: usize = 60;
const TEXTMETRICA_SIZE: usize = 56;

/// The TEXTMETRIC fields both variants compute from a resolved font.
#[derive(Clone)]
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

/// One enumerated family: the LOGFONT to hand the guest plus its metrics.
#[derive(Clone)]
struct EnumItem {
    log_font: LogFontW,
    full_name: String,
    style: String,
    script: String,
    metrics: TextMetrics,
}

/// Host-side state for one in-flight font enumeration.
struct EnumerationState {
    items: Vec<EnumItem>,
    index: usize,
    buffer_va: u64,
    callback_address: u64,
    lparam: u64,
    unicode: bool,
}

/// Active enumerations keyed by the id carried in
/// [`WinApiControlSignal::EnumerationCallbackRequested`]. The runtime advances
/// the index via [`advance_enumeration`] on each non-zero callback return.
static ENUMERATIONS: Mutex<Vec<(u64, EnumerationState)>> = Mutex::new(Vec::new());
static NEXT_ENUM_ID: Mutex<u64> = Mutex::new(1);

/// Fonts added by `AddFontResource*`, kept separate from the read-only system
/// database (which is a `OnceLock`). `Database::new` is not `const`, so the
/// mutex lives behind a `OnceLock`.
static ADDED_FONTS: OnceLock<Mutex<fontdb::Database>> = OnceLock::new();

fn added_fonts() -> &'static Mutex<fontdb::Database> {
    ADDED_FONTS.get_or_init(|| Mutex::new(fontdb::Database::new()))
}

fn round_i32(value: f32) -> i32 {
    value.round() as i32
}

/// Compute the shared TEXTMETRIC fields from a resolved font — mirrors
/// `gdi32::state::metrics::resolve_text_metrics` so enumeration metrics agree
/// with `GetTextMetrics`.
fn text_metrics_from_resolved(resolved: &ResolvedFont, charset: u8) -> TextMetrics {
    let ascent = round_i32(resolved.ascent);
    let descent = 0_i32.saturating_sub(round_i32(resolved.descent));
    let external = round_i32(resolved.line_gap);
    let height = ascent.saturating_add(descent);
    let internal = round_i32((resolved.ascent - resolved.descent - resolved.scale).max(0.0));
    TextMetrics {
        height,
        ascent,
        descent,
        internal_leading: internal,
        external_leading: external,
        avg_width: resolved.avg_advance,
        max_width: resolved.max_advance,
        weight: 400,
        italic: false,
        charset,
    }
}

/// Fill a fixed `[u16; N]` from `s`, NUL-terminated and truncated to fit.
fn write_utf16_into<const N: usize>(dst: &mut [u16; N], s: &str) {
    for (slot, unit) in dst
        .iter_mut()
        .zip(s.encode_utf16().take(N.saturating_sub(1)))
    {
        *slot = unit;
    }
}

/// Fill a fixed `[u8; N]` from `s` (Windows-1252), NUL-terminated/truncated.
fn write_ansi_into<const N: usize>(dst: &mut [u8; N], s: &str) {
    let bytes = encode_cp1252(s);
    for (slot, byte) in dst.iter_mut().zip(bytes.iter().take(N.saturating_sub(1))) {
        *slot = *byte;
    }
}

/// Build a `LOGFONTW` for an enumerated family.
fn make_logfont(family: &str, charset: u8) -> LogFontW {
    let mut lf = LogFontW {
        height: 0,
        width: 0,
        escapement: 0,
        orientation: 0,
        weight: 400,
        italic: 0,
        underline: 0,
        strike_out: 0,
        charset,
        out_precision: 0,
        clip_precision: 0,
        quality: 0,
        pitch_and_family: 0,
        face_name: [0; 32],
    };
    write_utf16_into(&mut lf.face_name, family);
    lf
}

/// Convert a `LOGFONTA` to `LOGFONTW` (the enumeration engine works in W).
fn log_font_w_from_a(lf: &LogFontA) -> LogFontW {
    let mut out = LogFontW {
        height: lf.height,
        width: lf.width,
        escapement: lf.escapement,
        orientation: lf.orientation,
        weight: lf.weight,
        italic: lf.italic,
        underline: lf.underline,
        strike_out: lf.strike_out,
        charset: lf.charset,
        out_precision: lf.out_precision,
        clip_precision: lf.clip_precision,
        quality: lf.quality,
        pitch_and_family: lf.pitch_and_family,
        face_name: [0; 32],
    };
    let name = decode_ansi_lossy(&lf.face_name);
    write_utf16_into(&mut out.face_name, &name);
    out
}

/// Build the enumerated-family item list from the host font database.
///
/// `filter` (when `Some`) restricts to families whose name contains it
/// (case-insensitive); an empty filter enumerates every family. `charset` is
/// the requested `lfCharSet` (DEFAULT_CHARSET matches all).
fn build_items(
    state: &mut WinApiState,
    filter: Option<&str>,
    charset: u8,
) -> Result<Vec<EnumItem>> {
    let families = system_family_names();
    state.with_font_engine(|_state, font_engine| {
        let mut items = Vec::new();
        for family in families {
            if let Some(f) = filter
                && !family.to_lowercase().contains(&f.to_lowercase())
            {
                continue;
            }
            let key = FontKey {
                family: family.clone(),
                weight: 400,
                ..Default::default()
            };
            if let Some(resolved) = font_engine.resolve(&key, 16) {
                let metrics = text_metrics_from_resolved(&resolved, charset);
                items.push(EnumItem {
                    log_font: make_logfont(&family, charset),
                    full_name: family.clone(),
                    style: "Regular".to_string(),
                    script: String::new(),
                    metrics,
                });
            }
        }
        Ok(items)
    })
}

/// Register a new enumeration and return its id.
fn register_enumeration(
    items: Vec<EnumItem>,
    buffer_va: u64,
    callback_address: u64,
    lparam: u64,
    unicode: bool,
) -> u64 {
    let mut next = NEXT_ENUM_ID
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let id = *next;
    *next = next.wrapping_add(1);
    drop(next);
    let mut enums = ENUMERATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    enums.push((
        id,
        EnumerationState {
            items,
            index: 0,
            buffer_va,
            callback_address,
            lparam,
            unicode,
        },
    ));
    id
}

/// Build the `GuestCallbackRequest` for the current item at `buffer_va`.
fn make_request(buffer_va: u64, callback: u64, lparam: u64, unicode: bool) -> GuestCallbackRequest {
    let logfont_size = if unicode {
        ENUMLOGFONTEXW_SIZE
    } else {
        ENUMLOGFONTEXA_SIZE
    };
    GuestCallbackRequest {
        callback_address: callback,
        // FONTENUMPROC ABI: RCX = lpelfe, RDX = lpntme, R8 = FontType, R9 = lParam.
        window_handle: buffer_va,
        message: TRUETYPE_FONTTYPE,
        word_parameter: buffer_va + u64::try_from(logfont_size).unwrap_or(0),
        long_parameter: lparam,
        unicode,
        outer_return: OuterReturn::Passthrough,
    }
}

/// Write one item (ENUMLOGFONTEX + TEXTMETRIC) into the guest buffer.
fn write_item(
    engine: &mut dyn wie_cpu::CpuEngine,
    buffer_va: u64,
    item: &EnumItem,
    unicode: bool,
) -> Result<()> {
    if unicode {
        with_typed_write::<EnumLogFontExW, _, _>(engine, buffer_va, |e| {
            e.log_font = item.log_font;
            write_utf16_into(&mut e.full_name, &item.full_name);
            write_utf16_into(&mut e.style, &item.style);
            write_utf16_into(&mut e.script, &item.script);
            Ok(())
        })
        .context("failed to write ENUMLOGFONTEXW")?;
        let metrics_va = buffer_va + u64::try_from(ENUMLOGFONTEXW_SIZE).unwrap_or(0);
        with_typed_write::<TextMetricW, _, _>(engine, metrics_va, |tm| {
            fill_text_metric_w(tm, &item.metrics);
            Ok(())
        })
        .context("failed to write TEXTMETRICW")?;
    } else {
        with_typed_write::<EnumLogFontExA, _, _>(engine, buffer_va, |e| {
            e.log_font = log_font_a_from_w(&item.log_font);
            write_ansi_into(&mut e.full_name, &item.full_name);
            write_ansi_into(&mut e.style, &item.style);
            write_ansi_into(&mut e.script, &item.script);
            Ok(())
        })
        .context("failed to write ENUMLOGFONTEXA")?;
        let metrics_va = buffer_va + u64::try_from(ENUMLOGFONTEXA_SIZE).unwrap_or(0);
        with_typed_write::<TextMetricA, _, _>(engine, metrics_va, |tm| {
            fill_text_metric_a(tm, &item.metrics);
            Ok(())
        })
        .context("failed to write TEXTMETRICA")?;
    }
    Ok(())
}

fn log_font_a_from_w(lf: &LogFontW) -> LogFontA {
    let mut out = LogFontA {
        height: lf.height,
        width: lf.width,
        escapement: lf.escapement,
        orientation: lf.orientation,
        weight: lf.weight,
        italic: lf.italic,
        underline: lf.underline,
        strike_out: lf.strike_out,
        charset: lf.charset,
        out_precision: lf.out_precision,
        clip_precision: lf.clip_precision,
        quality: lf.quality,
        pitch_and_family: lf.pitch_and_family,
        face_name: [0; 32],
    };
    let name = decode_utf16_lossy(&lf.face_name);
    write_ansi_into(&mut out.face_name, &name);
    out
}

fn fill_text_metric_w(tm: &mut TextMetricW, m: &TextMetrics) {
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
    tm.pitch_and_family = 0x01; // TMPF_VECTOR
    tm.charset = m.charset;
}

fn fill_text_metric_a(tm: &mut TextMetricA, m: &TextMetrics) {
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
    tm.pitch_and_family = 0x01; // TMPF_VECTOR
    tm.charset = m.charset;
}

/// Shared enumeration driver: build the item list, register it, write the
/// first item, and request the first guest callback.
fn start_enumeration(
    ctx: &mut HandlerContext<'_>,
    filter: Option<String>,
    charset: u8,
    callback: u64,
    lparam: u64,
    unicode: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    if callback == 0 {
        return ctx.finish(0);
    }

    let items = build_items(state, filter.as_deref(), charset)?;
    let Some(first) = items.first().cloned() else {
        // No matching faces: enumeration trivially completed.
        return ctx.finish(1);
    };

    let item_size = if unicode {
        ENUMLOGFONTEXW_SIZE + TEXTMETRICW_SIZE
    } else {
        ENUMLOGFONTEXA_SIZE + TEXTMETRICA_SIZE
    };
    let buffer_va = state
        .heap_state
        .heap
        .alloc_coherent(engine, u64::try_from(item_size).unwrap_or(0));
    if buffer_va == 0 {
        return ctx.finish(0);
    }

    let id = register_enumeration(items, buffer_va, callback, lparam, unicode);
    write_item(engine, buffer_va, &first, unicode)?;

    Err(WinApiControlSignal::EnumerationCallbackRequested {
        request: make_request(buffer_va, callback, lparam, unicode),
        enumeration_id: id,
    }
    .into())
}

/// Advance the enumeration index to the next item, returning it (or `None`
/// when the enumeration is complete). Pure — the caller writes the item to
/// guest memory and issues the next callback.
fn advance_index(e: &mut EnumerationState) -> Option<&EnumItem> {
    e.index = e.index.saturating_add(1);
    e.items.get(e.index)
}

/// Advance a font enumeration to the next item (called by the runtime on each
/// non-zero callback return). Returns `Some(next_request)` to re-enter the
/// callback, or `None` when the enumeration is complete.
pub fn advance_enumeration(
    engine: &mut dyn wie_cpu::CpuEngine,
    _state: &mut WinApiState,
    enumeration_id: u64,
) -> Result<Option<GuestCallbackRequest>> {
    let mut enums = ENUMERATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((_, e)) = enums.iter_mut().find(|(id, _)| *id == enumeration_id) else {
        return Ok(None);
    };
    // Copy the per-item fields out before the mutable borrow from
    // `advance_index` (which borrows `*e`) so `write_item` can use them.
    let buffer_va = e.buffer_va;
    let callback = e.callback_address;
    let lparam = e.lparam;
    let unicode = e.unicode;
    let Some(item) = advance_index(e) else {
        // Enumeration complete: drop the state.
        let _ = e;
        enums.retain(|(id, _)| *id != enumeration_id);
        return Ok(None);
    };
    write_item(engine, buffer_va, item, unicode)?;
    Ok(Some(make_request(buffer_va, callback, lparam, unicode)))
}

/// Handles `GDI32.dll!EnumFontFamiliesExW`.
pub(crate) fn handle_enum_font_families_ex_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let logfont_va = engine
        .read_rdx()
        .context("failed to read RDX for EnumFontFamiliesExW")?;
    let callback = engine
        .read_r8()
        .context("failed to read R8 for EnumFontFamiliesExW")?;
    let lparam = engine
        .read_r9()
        .context("failed to read R9 for EnumFontFamiliesExW")?;
    let lf = read_typed_copy::<LogFontW>(engine, logfont_va)
        .context("failed to read LOGFONTW for EnumFontFamiliesExW")?;
    let filter = family_filter(&lf);
    start_enumeration(ctx, filter, lf.charset, callback, lparam, true)
}

/// Handles `GDI32.dll!EnumFontFamiliesExA`.
pub(crate) fn handle_enum_font_families_ex_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let logfont_va = engine
        .read_rdx()
        .context("failed to read RDX for EnumFontFamiliesExA")?;
    let callback = engine
        .read_r8()
        .context("failed to read R8 for EnumFontFamiliesExA")?;
    let lparam = engine
        .read_r9()
        .context("failed to read R9 for EnumFontFamiliesExA")?;
    let lf_a = read_typed_copy::<LogFontA>(engine, logfont_va)
        .context("failed to read LOGFONTA for EnumFontFamiliesExA")?;
    let lf = log_font_w_from_a(&lf_a);
    let filter = family_filter(&lf);
    start_enumeration(ctx, filter, lf.charset, callback, lparam, false)
}

/// Handles `GDI32.dll!EnumFontFamiliesW` (same ABI as the Ex variant; the
/// `ENUMLOGFONTEXW` payload's `LOGFONTW` front is layout-identical to the
/// `ENUMLOGFONTW` this API passes).
pub(crate) fn handle_enum_font_families_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_enum_font_families_ex_w(ctx)
}

/// Handles `GDI32.dll!EnumFontFamiliesA`.
pub(crate) fn handle_enum_font_families_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_enum_font_families_ex_a(ctx)
}

/// Handles `GDI32.dll!EnumFontsW` — `rdx` is a face-name string (or NULL for
/// all families).
pub(crate) fn handle_enum_fonts_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let face_va = engine
        .read_rdx()
        .context("failed to read RDX for EnumFontsW")?;
    let callback = engine
        .read_r8()
        .context("failed to read R8 for EnumFontsW")?;
    let lparam = engine
        .read_r9()
        .context("failed to read R9 for EnumFontsW")?;
    let filter = if face_va == 0 {
        None
    } else {
        Some(read_guest_utf16_lossy(engine, face_va, 256)?)
    };
    start_enumeration(ctx, filter, DEFAULT_CHARSET, callback, lparam, true)
}

/// Handles `GDI32.dll!EnumFontsA`.
pub(crate) fn handle_enum_fonts_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let face_va = engine
        .read_rdx()
        .context("failed to read RDX for EnumFontsA")?;
    let callback = engine
        .read_r8()
        .context("failed to read R8 for EnumFontsA")?;
    let lparam = engine
        .read_r9()
        .context("failed to read R9 for EnumFontsA")?;
    let filter = if face_va == 0 {
        None
    } else {
        Some(read_guest_ansi_lossy(engine, face_va, 256)?)
    };
    start_enumeration(ctx, filter, DEFAULT_CHARSET, callback, lparam, false)
}

/// The family filter from a LOGFONT: `None` when `lfFaceName` is empty (all
/// families), else the face name.
fn family_filter(lf: &LogFontW) -> Option<String> {
    let name = decode_utf16_lossy(&lf.face_name);
    if name.is_empty() { None } else { Some(name) }
}

/// Pack a coverage glyph into a bottom-up 1bpp bitmap (rows padded to 4 bytes).
fn build_glyph_bitmap(glyph: &super::font_system::RasterizedGlyph) -> Vec<u8> {
    let width = usize::try_from(glyph.width).unwrap_or(0);
    let height = usize::try_from(glyph.height).unwrap_or(0);
    let row_bytes = width.div_ceil(8).div_ceil(4).saturating_mul(4);
    let mut bitmap = vec![0_u8; row_bytes.saturating_mul(height)];
    for y in 0..height {
        // DIBs are bottom-up: the first memory row is the glyph's bottom row.
        let src_row = height.saturating_sub(1).saturating_sub(y);
        for x in 0..width {
            let src_index = src_row.saturating_mul(width).saturating_add(x);
            let alpha = glyph.coverage.get(src_index).copied().unwrap_or(0);
            if alpha >= 128 {
                let byte_index = y.saturating_mul(row_bytes).saturating_add(x / 8);
                let bit = 7 - (x % 8);
                if let Some(byte) = bitmap.get_mut(byte_index) {
                    *byte |= 1_u8 << bit;
                }
            }
        }
    }
    bitmap
}

/// Shared `GetGlyphOutline` body (GGO_BITMAP).
fn get_glyph_outline(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for GetGlyphOutline")?;
    let uchar = engine
        .read_rdx()
        .context("failed to read RDX for GetGlyphOutline")?;
    let fu_format = engine
        .read_r8()
        .context("failed to read R8 for GetGlyphOutline")?;
    let lpgm = engine
        .read_r9()
        .context("failed to read R9 for GetGlyphOutline")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for GetGlyphOutline")?;
    let cb_buffer = read_u32(engine, checked_address(rsp, 0x28, "cbBuffer"))?;
    let lpv_buffer = read_u64(engine, checked_address(rsp, 0x30, "lpvBuffer"))?;

    // GGO_NATIVE (TrueType outline) is not implemented — fail honestly rather
    // than fake success. Only GGO_BITMAP is supported.
    if fu_format == GGO_NATIVE || fu_format != GGO_BITMAP {
        return ctx.finish(GDI_ERROR);
    }

    let ch = char::from_u32(u32::try_from(uchar).unwrap_or(0)).unwrap_or('?');
    let glyph = state.with_font_engine(|state, font_engine| {
        let resolved = dc_resolved_font(state, hdc, font_engine);
        resolved.as_ref().and_then(|(_, r)| r.glyph_outline(ch))
    });
    let Some(glyph) = glyph else {
        return ctx.finish(GDI_ERROR);
    };

    let bitmap = build_glyph_bitmap(&glyph);
    let metrics_size = u64::try_from(std::mem::size_of::<GlyphMetrics>()).unwrap_or(0);
    let total = metrics_size.saturating_add(u64::try_from(bitmap.len()).unwrap_or(0));

    if lpgm != 0 {
        with_typed_write::<GlyphMetrics, _, _>(engine, lpgm, |gm| {
            gm.black_box_x = u32::try_from(glyph.width).unwrap_or(0);
            gm.black_box_y = u32::try_from(glyph.height).unwrap_or(0);
            gm.glyph_origin_x = glyph.left;
            gm.glyph_origin_y = glyph.top;
            gm.cell_inc_x = i16::try_from(glyph.advance).unwrap_or(0);
            gm.cell_inc_y = 0;
            Ok(())
        })
        .context("failed to write GLYPHMETRICS")?;
    }

    if lpv_buffer != 0 {
        if u64::from(cb_buffer) < total {
            return ctx.finish(GDI_ERROR);
        }
        write_bytes(engine, lpv_buffer, &bitmap).context("failed to write glyph bitmap")?;
    }

    // With a NULL buffer this is the required size; with a buffer it is the
    // number of bytes written.
    ctx.finish(total)
}

/// Handles `GDI32.dll!GetGlyphOutlineW`.
pub(crate) fn handle_get_glyph_outline_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    get_glyph_outline(ctx)
}

/// Handles `GDI32.dll!GetGlyphOutlineA`.
pub(crate) fn handle_get_glyph_outline_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    get_glyph_outline(ctx)
}

/// Load a font file (bottle-resolved) into the added-font database.
fn add_font_resource(state: &mut WinApiState, guest_path: &str) -> bool {
    let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, guest_path) else {
        return false;
    };
    let mut db = added_fonts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    db.load_font_file(&map.host).is_ok()
}

/// Remove every face loaded from `guest_path` from the added-font database.
fn remove_font_resource(state: &mut WinApiState, guest_path: &str) -> bool {
    let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, guest_path) else {
        return false;
    };
    let mut db = added_fonts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ids: Vec<fontdb::ID> = db
        .faces()
        .filter(|face| matches!(&face.source, fontdb::Source::File(p) if *p == map.host))
        .map(|face| face.id)
        .collect();
    if ids.is_empty() {
        return false;
    }
    for id in ids {
        db.remove_face(id);
    }
    true
}

/// Handles `GDI32.dll!AddFontResourceW`.
pub(crate) fn handle_add_font_resource_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_va = engine
        .read_rcx()
        .context("failed to read RCX for AddFontResourceW")?;
    let guest_path = read_guest_utf16_lossy(engine, path_va, 1024)?;
    let ok = add_font_resource(state, &guest_path);
    ctx.finish(u64::from(ok))
}

/// Handles `GDI32.dll!AddFontResourceA`.
pub(crate) fn handle_add_font_resource_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_va = engine
        .read_rcx()
        .context("failed to read RCX for AddFontResourceA")?;
    let guest_path = read_guest_ansi_lossy(engine, path_va, 1024)?;
    let ok = add_font_resource(state, &guest_path);
    ctx.finish(u64::from(ok))
}

/// Handles `GDI32.dll!RemoveFontResourceW`.
pub(crate) fn handle_remove_font_resource_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_va = engine
        .read_rcx()
        .context("failed to read RCX for RemoveFontResourceW")?;
    let guest_path = read_guest_utf16_lossy(engine, path_va, 1024)?;
    let ok = remove_font_resource(state, &guest_path);
    ctx.finish(u64::from(ok))
}

/// Handles `GDI32.dll!RemoveFontResourceA`.
pub(crate) fn handle_remove_font_resource_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let path_va = engine
        .read_rcx()
        .context("failed to read RCX for RemoveFontResourceA")?;
    let guest_path = read_guest_ansi_lossy(engine, path_va, 1024)?;
    let ok = remove_font_resource(state, &guest_path);
    ctx.finish(u64::from(ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_item(name: &str) -> EnumItem {
        EnumItem {
            log_font: make_logfont(name, DEFAULT_CHARSET),
            full_name: name.to_string(),
            style: "Regular".to_string(),
            script: String::new(),
            metrics: TextMetrics {
                height: 16,
                ascent: 12,
                descent: 4,
                internal_leading: 0,
                external_leading: 0,
                avg_width: 8,
                max_width: 8,
                weight: 400,
                italic: false,
                charset: DEFAULT_CHARSET,
            },
        }
    }

    /// Register 3 items and advance through them: two `Some` continuations
    /// then `None` — the full-iteration contract the runtime relies on.
    #[test]
    fn enumeration_advances_through_all_items_then_completes() {
        let items = vec![dummy_item("A"), dummy_item("B"), dummy_item("C")];
        let id = register_enumeration(items, 0x5000, 0x9000, 0x1234, true);

        let mut enums = ENUMERATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, e) = enums
            .iter_mut()
            .find(|(eid, _)| *eid == id)
            .expect("registered enumeration present");

        // First advance -> item B.
        let next = advance_index(e).expect("item B present");
        assert_eq!(next.full_name, "B");
        // Second advance -> item C.
        let next = advance_index(e).expect("item C present");
        assert_eq!(next.full_name, "C");
        // Third advance -> enumeration complete.
        assert!(
            advance_index(e).is_none(),
            "must complete after the last item"
        );
    }

    #[test]
    fn glyph_bitmap_is_bottom_up_and_padded() {
        // A 3x2 glyph: top row = left/right set, bottom row = middle set.
        let glyph = super::super::font_system::RasterizedGlyph {
            advance: 4,
            left: 0,
            top: 0,
            width: 3,
            height: 2,
            coverage: vec![255, 0, 255, 0, 255, 0],
        };
        let bitmap = build_glyph_bitmap(&glyph);
        // row_bytes = ceil(3/8)=1, padded to 4. 2 rows => 8 bytes.
        assert_eq!(bitmap.len(), 8);
        // Bottom-up: memory row 0 = glyph bottom row (0,255,0) => bit 6 set.
        assert_eq!(bitmap[0], 0b0100_0000);
        // Memory row 1 = glyph top row (255,0,255) => bits 7 and 5 set.
        assert_eq!(bitmap[4], 0b1010_0000);
    }

    #[test]
    fn utf16_fill_truncates_and_terminates() {
        let mut buf = [0_u16; 4];
        write_utf16_into(&mut buf, "abcdef");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }
}
