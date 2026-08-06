use anyhow::{Context, Result};

use crate::guest_layout::{Bitmap, Size};
use crate::guest_memory::with_typed_write;
use crate::handles::{Hbitmap, Hbrush, Hdc, Hfont, Hpen};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

mod metrics;
mod objects;
mod records;

// Re-export everything the old single-file `state` module exposed, so
// `crate::gdi32::state::*` and the `pub use state::*` in gdi32/mod.rs keep
// resolving with identical visibilities.
pub use metrics::*;
pub use objects::*;
pub use records::*;

const FAKE_PREVIOUS_GDI_OBJECT_HANDLE: u64 = 0x0000_0000_6800_0001;
const BITMAP_STRUCT_SIZE: u64 = 32;
#[expect(dead_code)]
const FAKE_COMPATIBLE_DC_HANDLE: u64 = 0x0000_0000_6800_0100;
const FAKE_PIXEL_COLOR: u64 = 0x0000_0000_00ff_00ff;
const FAKE_GDI_BITMAP_HANDLE_BASE: u64 = 0x0000_0000_6800_2000;

/// Handle of the stock `WHITE_BRUSH` (0x6800_5001, matches `GetStockObject`).
pub(crate) const STOCK_WHITE_BRUSH_HANDLE: u64 = 0x0000_0000_6800_5001;
/// Handle of the stock `BLACK_BRUSH` (0x6800_5002, matches `GetStockObject`).
pub(crate) const STOCK_BLACK_BRUSH_HANDLE: u64 = 0x0000_0000_6800_5002;
/// Handle of the stock `GRAY_BRUSH`.
pub(crate) const STOCK_GRAY_BRUSH_HANDLE: u64 = 0x0000_0000_6800_5003;
/// Handle of the stock `NULL_BRUSH`.
pub(crate) const STOCK_NULL_BRUSH_HANDLE: u64 = 0x0000_0000_6800_5004;
/// Handle of the stock `LTGRAY_BRUSH`.
pub(crate) const STOCK_LTGRAY_BRUSH_HANDLE: u64 = 0x0000_0000_6800_5007;
/// Handle of the stock `DKGRAY_BRUSH`.
pub(crate) const STOCK_DKGRAY_BRUSH_HANDLE: u64 = 0x0000_0000_6800_5008;
/// Handle of the stock `WHITE_PEN`.
pub(crate) const STOCK_WHITE_PEN_HANDLE: u64 = 0x0000_0000_6800_5009;
/// Handle of the stock `BLACK_PEN`.
pub(crate) const STOCK_BLACK_PEN_HANDLE: u64 = 0x0000_0000_6800_500A;
/// Handle of the stock `NULL_PEN`.
pub(crate) const STOCK_NULL_PEN_HANDLE: u64 = 0x0000_0000_6800_500B;

/// Handles `GDI32.dll!GetObjectA`.
pub fn handle_get_object_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let object_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetObjectA")?;

    let buffer_size = engine
        .read_rdx()
        .context("failed to read RDX for GetObjectA")?;

    let object_buffer_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetObjectA")?;

    let can_write_bitmap =
        object_handle != 0 && object_buffer_ptr != 0 && buffer_size >= BITMAP_STRUCT_SIZE;

    let return_value = if object_buffer_ptr == 0 && object_handle != 0 {
        BITMAP_STRUCT_SIZE
    } else if can_write_bitmap {
        // Win64 BITMAP (32 bytes, layout pinned by the Bitmap const-assert
        // table): LONG bmType @0 … WORD bmPlanes @16, WORD bmBitsPixel @18,
        // pad @20..23, LPVOID bmBits @24. The typed view zero-fills the pad
        // and leaves bmBits NULL, matching the old per-field writes.
        with_typed_write::<Bitmap, _, _>(engine, object_buffer_ptr, |bitmap| {
            bitmap.bm_type = 0;
            bitmap.bm_width = 16;
            bitmap.bm_height = 16;
            bitmap.bm_width_bytes = 64;
            bitmap.bm_planes = 1;
            bitmap.bm_bits_pixel = 32;
            bitmap.bm_bits = 0;
            Ok(())
        })
        .context("failed to write BITMAP")?;

        BITMAP_STRUCT_SIZE
    } else {
        0
    };

    ctx.finish(return_value)
}

/// Stock object identifiers (wingdi.h). Verified against the Windows SDK:
/// WHITE_BRUSH 0, LTGRAY_BRUSH 1, GRAY_BRUSH 2, DKGRAY_BRUSH 3, BLACK_BRUSH 4,
/// NULL_BRUSH 5, WHITE_PEN 6, BLACK_PEN 7, NULL_PEN 8, SYSTEM_FONT 13,
/// DEFAULT_PALETTE 15. (The previous table mislabeled id 1 as BLACK_BRUSH —
/// id 1 is LTGRAY_BRUSH and BLACK_BRUSH is id 4 — which broke any guest that
/// requested `GetStockObject(BLACK_PEN)` = 7 or the true BLACK_BRUSH = 4.)
const STOCK_WHITE_BRUSH: u64 = 0;
const STOCK_LTGRAY_BRUSH: u64 = 1;
const STOCK_GRAY_BRUSH: u64 = 2;
const STOCK_DKGRAY_BRUSH: u64 = 3;
const STOCK_BLACK_BRUSH: u64 = 4;
const STOCK_NULL_BRUSH: u64 = 5;
const STOCK_WHITE_PEN: u64 = 6;
const STOCK_BLACK_PEN: u64 = 7;
const STOCK_NULL_PEN: u64 = 8;
const STOCK_SYSTEM_FONT: u64 = 13;
const STOCK_DEFAULT_PALETTE: u64 = 15;

/// Handles `GDI32.dll!GetStockObject` — returns predefined stock object handles.
pub fn handle_get_stock_object(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let n_index = engine.read_rcx()? & 0xffff_ffff;
    let handle = match n_index {
        STOCK_WHITE_BRUSH => STOCK_WHITE_BRUSH_HANDLE,
        STOCK_LTGRAY_BRUSH => STOCK_LTGRAY_BRUSH_HANDLE,
        STOCK_GRAY_BRUSH => STOCK_GRAY_BRUSH_HANDLE,
        STOCK_DKGRAY_BRUSH => STOCK_DKGRAY_BRUSH_HANDLE,
        STOCK_BLACK_BRUSH => STOCK_BLACK_BRUSH_HANDLE,
        STOCK_NULL_BRUSH => STOCK_NULL_BRUSH_HANDLE,
        STOCK_WHITE_PEN => STOCK_WHITE_PEN_HANDLE,
        STOCK_BLACK_PEN => STOCK_BLACK_PEN_HANDLE,
        STOCK_NULL_PEN => STOCK_NULL_PEN_HANDLE,
        STOCK_SYSTEM_FONT => 0x0000_0000_6800_5005,
        STOCK_DEFAULT_PALETTE => 0x0000_0000_6800_5006,
        _ => 0, // NULL for unknown stock objects
    };
    ctx.finish(handle)
}

/// Handles `GDI32.dll!SelectObject`.
pub fn handle_select_object(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let dc_handle = engine
        .read_rcx()
        .context("failed to read RCX for SelectObject")?;

    let object_handle = engine
        .read_rdx()
        .context("failed to read RDX for SelectObject")?;

    // Scope the object-kind checks so the mutable borrow ends before find_dc_mut.
    let object_kind = GdiObject::classify(object_handle, state.gdi_state());
    // Stock brushes/pens never classify to `GdiObject` (0x6800_500x FAKE
    // range) but must still be recorded so the fill/stroke paths resolve the
    // DC's brush/pen color.
    let stock_kind = stock_select_kind(object_handle);

    let replaced =
        match (stock_kind, object_kind) {
            (Some(StockSelectKind::Brush), _) => state
                .gdi_state()
                .find_dc_mut(Hdc::from(dc_handle))
                .map(|dc| {
                    dc.selected_brush
                        .replace(Hbrush::from(object_handle))
                        .map(Hbrush::as_u64)
                }),
            (Some(StockSelectKind::Pen), _) => state
                .gdi_state()
                .find_dc_mut(Hdc::from(dc_handle))
                .map(|dc| {
                    dc.selected_pen
                        .replace(Hpen::from(object_handle))
                        .map(Hpen::as_u64)
                }),
            (None, Some(GdiObject::Dib(bitmap))) => state
                .gdi_state()
                .find_dc_mut(Hdc::from(dc_handle))
                .map(|dc| dc.selected_bitmap.replace(bitmap).map(Hbitmap::as_u64)),
            (None, Some(GdiObject::Brush(brush))) => state
                .gdi_state()
                .find_dc_mut(Hdc::from(dc_handle))
                .map(|dc| dc.selected_brush.replace(brush).map(Hbrush::as_u64)),
            (None, Some(GdiObject::Pen(pen))) => state
                .gdi_state()
                .find_dc_mut(Hdc::from(dc_handle))
                .map(|dc| dc.selected_pen.replace(pen).map(Hpen::as_u64)),
            (None, Some(GdiObject::Font(font))) => state
                .gdi_state()
                .find_dc_mut(Hdc::from(dc_handle))
                .map(|dc| dc.selected_font.replace(font).map(Hfont::as_u64)),
            (None, None) => None,
        };

    if let Some(replaced) = replaced {
        let return_value = replaced.unwrap_or(FAKE_PREVIOUS_GDI_OBJECT_HANDLE);
        return ctx.finish(return_value);
    }

    // For fonts and unknown DCs/objects, return the fake handle.
    ctx.finish(FAKE_PREVIOUS_GDI_OBJECT_HANDLE)
}

/// Handles `GDI32.dll!GetTextExtentPoint32A`.
pub fn handle_get_text_extent_point_32_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_get_text_extent_point_32_impl(ctx, "GetTextExtentPoint32A", false)
}

/// Handles `GDI32.dll!GetTextExtentPoint32W`.
pub fn handle_get_text_extent_point_32_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_get_text_extent_point_32_impl(ctx, "GetTextExtentPoint32W", true)
}

fn handle_get_text_extent_point_32_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let device_context_handle = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    let text_ptr = engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {api_name}"))?;

    let character_count = engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {api_name}"))?;

    let size_ptr = engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {api_name}"))?;

    let count = u32::try_from(character_count & u64::from(u32::MAX))
        .unwrap_or(0)
        .min(4096);

    // Width = the sum of the per-glyph advances of the resolved font (the
    // same advances the rasterizer uses, including the CJK fallback), so the
    // extent always matches the drawn text. Height = ascent + descent.
    let (width, height) = state.with_font_engine(|state, font_engine| -> Result<(i32, i32)> {
        let resolved = dc_resolved_font(state, device_context_handle, font_engine);
        match resolved {
            Some((key, resolved)) => {
                let chars = crate::gdi32::text::read_text_chars(engine, text_ptr, count, wide)?;
                let mut width = 0_i32;
                for ch in chars {
                    if let Some(ch) = char::from_u32(ch) {
                        width = width.saturating_add(font_engine.char_advance(&resolved, &key, ch));
                    }
                }
                Ok((width, resolved.line_height()))
            }
            None => Ok((0, 16)),
        }
    })?;

    let width = width.max(0);
    let height = height.max(0);

    if size_ptr != 0 {
        // SIZE is LONG cx @0, LONG cy @4 — one typed write (the values are
        // non-negative i32, so the i32 fields carry the exact guest bytes the
        // old u32 writes produced).
        with_typed_write::<Size, _, _>(engine, size_ptr, |size| {
            size.cx = width;
            size.cy = height;
            Ok(())
        })
        .with_context(|| format!("failed to write SIZE for {api_name}"))?;
    }

    let return_value = u64::from(size_ptr != 0);

    ctx.finish(return_value)
}

/// Handles `GDI32.dll!CreateCompatibleDC`.
pub fn handle_create_compatible_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _source_device_context = engine
        .read_rcx()
        .context("failed to read RCX for CreateCompatibleDC")?;

    let dc_handle = state.gdi_state().alloc_dc(DcKind::Memory);

    ctx.finish(dc_handle.as_u64())
}

/// Handles `GDI32.dll!GetDeviceCaps`.
///
/// Returns plausible values for a 1920×1080 32-bpp desktop so Lunar Magic's
/// display-mode probes succeed without real GDI. Print DCs branch FIRST and
/// report the print-job geometry (300 DPI, the paper size in device px) —
/// the canvas always matches whatever this reports.
pub fn handle_get_device_caps(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = engine
        .read_rcx()
        .context("failed to read RCX for GetDeviceCaps")?;

    let index = engine
        .read_rdx()
        .context("failed to read RDX for GetDeviceCaps")?;

    let is_print = matches!(
        state.gdi_state().find_dc(Hdc::from(hdc)).map(|dc| dc.kind),
        Some(DcKind::Print(_))
    );
    let return_value = if is_print {
        print_dc_caps(state, hdc, index)
    } else {
        screen_dc_caps(index)
    };

    tracing::debug!(index, return_value, "GetDeviceCaps");

    ctx.finish(return_value)
}

/// The fake 1920×1080 screen caps table (shared by non-print DCs and the
/// print fallback when a `Print`-typed record has no job — a torn state).
fn screen_dc_caps(index: u64) -> u64 {
    // Common GetDeviceCaps indices from wingdi.h.
    // Identical return values are intentionally merged (clippy match_same_arms).
    match index {
        0 => 0x4000,                         // DRIVERVERSION
        2 | 26 | 112 | 113 | 119 | 121 => 0, // TECHNOLOGY, PDEVICESIZE, offsets, BLTALIGNMENT, COLORMGMTCAPS
        4 => 508,                            // HORZSIZE mm (~20")
        6 => 286,                            // VERTSIZE mm
        8 | 110 | 118 => 1920,               // HORZRES / PHYSICALWIDTH / DESKTOPHORZRES
        10 | 111 | 117 => 1080,              // VERTRES / PHYSICALHEIGHT / DESKTOPVERTRES
        12 => 32,                            // BITSPIXEL
        14 | 16 | 18 | 20 | 22 | 36 => 1,    // PLANES, NUMBRUSHES/PENS/MARKERS/FONTS, CLIPCAPS
        24 => u64::MAX,                      // NUMCOLORS (-1 for >8bpp, sign-extended int)
        28 => 0x1ff,                         // CURVECAPS
        30 => 0xfe,                          // LINECAPS
        32 => 0xff,                          // POLYGONALCAPS
        34 => 0x7007,                        // TEXTCAPS
        38 => 0x7e99,                        // RASTERCAPS
        40 | 42 => 36,                       // ASPECTX / ASPECTY
        44 => 51,                            // ASPECTXY
        88 | 90 => 96,                       // LOGPIXELSX / LOGPIXELSY
        104 => 256,                          // SIZEPALETTE
        106 => 20,                           // NUMRESERVED
        108 => 24,                           // COLORRES
        114 | 115 => 100,                    // SCALINGFACTORX / Y
        116 => 60,                           // VREFRESH
        120 => 3,                            // SHADEBLENDCAPS
        _ => {
            tracing::debug!(index, "GetDeviceCaps unknown index; returning 0");
            0
        }
    }
}

/// The print-device caps for a print DC: 300 DPI, the job's paper geometry,
/// no hardware margins (PHYSICALOFFSET = 0 — a documented P1a deviation).
/// Color/plane/caps values reuse the screen table's.
fn print_dc_caps(state: &mut WinApiState, hdc: u64, index: u64) -> u64 {
    let Some(job) = state.gdi_state().find_print_job(Hdc::from(hdc)) else {
        tracing::debug!(
            index,
            "GetDeviceCaps: print DC without a job; screen fallback"
        );
        return screen_dc_caps(index);
    };
    let (paper_w, paper_h) = job.paper_px;
    let (size_w, size_h) = job.paper_mm;
    match index {
        88 | 90 => u64::from(job.dpi),  // LOGPIXELSX / LOGPIXELSY
        8 | 110 => u64::from(paper_w),  // HORZRES / PHYSICALWIDTH
        10 | 111 => u64::from(paper_h), // VERTRES / PHYSICALHEIGHT
        112 | 113 => 0,                 // PHYSICALOFFSETX / PHYSICALOFFSETY
        4 => u64::from(size_w),         // HORZSIZE mm
        6 => u64::from(size_h),         // VERTSIZE mm
        12 => 32,                       // BITSPIXEL
        14 => 1,                        // PLANES
        24 => u64::MAX,                 // NUMCOLORS
        34 => 0x7007,                   // TEXTCAPS (screen value)
        38 => 0x7e99,                   // RASTERCAPS (screen value)
        _ => {
            tracing::debug!(index, "GetDeviceCaps(print) unknown index; returning 0");
            0
        }
    }
}

/// Handles `GDI32.dll!GetPixel`.
pub fn handle_get_pixel(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _device_context_handle = engine
        .read_rcx()
        .context("failed to read RCX for GetPixel")?;

    let _x = engine
        .read_rdx()
        .context("failed to read RDX for GetPixel")?;

    let _y = engine.read_r8().context("failed to read R8 for GetPixel")?;

    ctx.finish(FAKE_PIXEL_COLOR)
}

/// Handles `GDI32.dll!DeleteDC`.
pub fn handle_delete_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let device_context_handle = engine
        .read_rcx()
        .context("failed to read RCX for DeleteDC")?;

    // Print DCs own a job whose canvases can be ~34 MB each — drop it with
    // the DC so the pages do not leak for the session's lifetime.
    let is_print = matches!(
        state
            .gdi_state()
            .find_dc(Hdc::from(device_context_handle))
            .map(|dc| dc.kind),
        Some(DcKind::Print(_))
    );
    if is_print {
        state
            .gdi_state()
            .remove_print_job(Hdc::from(device_context_handle));
    }
    state
        .gdi_state()
        .remove_dc(Hdc::from(device_context_handle));

    let return_value = u64::from(device_context_handle != 0);

    ctx.finish(return_value)
}

/// Handles `GDI32.dll!DeleteObject`.
pub fn handle_delete_object(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let object_handle = engine
        .read_rcx()
        .context("failed to read RCX for DeleteObject")?;

    // Free known GDI objects; the return value preserves the historical
    // "any non-zero handle succeeds" behavior for unknown handles.
    let object = GdiObject::classify(object_handle, state.gdi_state());
    let existed = object.is_some();
    if existed {
        let gdi = state.gdi_state();
        match object {
            Some(GdiObject::Dib(bitmap)) => gdi.remove_dib(bitmap),
            Some(GdiObject::Brush(brush)) => gdi.remove_brush(brush),
            Some(GdiObject::Pen(pen)) => gdi.remove_pen(pen),
            Some(GdiObject::Font(font)) => gdi.remove_font(font),
            None => {}
        }
    }

    let return_value = u64::from(existed || object_handle != 0);

    ctx.finish(return_value)
}

// --- Phase 2b: Real GDI handle table ---

/// Real (non-FAKE) DC handle base — disjoint from 0x6800_xxxx FAKE range.
const DC_HANDLE_BASE: u64 = 0x0000_0000_6810_0000;
const DC_HANDLE_STRIDE: u64 = 0x10;
/// Real (non-FAKE) bitmap/DIB handle base.
const BITMAP_HANDLE_BASE: u64 = 0x0000_0000_6820_0000;
const BITMAP_HANDLE_STRIDE: u64 = 0x10;
/// Real (non-FAKE) brush handle base.
const BRUSH_HANDLE_BASE: u64 = 0x0000_0000_6830_0000;
const BRUSH_HANDLE_STRIDE: u64 = 0x10;
/// Real (non-FAKE) pen handle base.
const PEN_HANDLE_BASE: u64 = 0x0000_0000_6840_0000;
const PEN_HANDLE_STRIDE: u64 = 0x10;
/// Real (non-FAKE) font handle base.
const FONT_HANDLE_BASE: u64 = 0x0000_0000_6850_0000;
const FONT_HANDLE_STRIDE: u64 = 0x10;
