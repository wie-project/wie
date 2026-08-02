use anyhow::{Context, Result};

use crate::guest_memory::{
    checked_field_address, write_i32 as write_guest_i32, write_u16 as write_guest_u16,
    write_u32 as write_guest_u32, write_u64 as write_guest_u64,
};
use crate::handles::{Hbitmap, Hbrush, Hdc, Hfont, Hpen};
use crate::{HandlerContext, WinApiHandlerResult};

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
        // Win64 BITMAP:
        // LONG   bmType;       offset 0
        // LONG   bmWidth;      offset 4
        // LONG   bmHeight;     offset 8
        // LONG   bmWidthBytes; offset 12
        // WORD   bmPlanes;     offset 16
        // WORD   bmBitsPixel;  offset 18
        // padding              offset 20..23
        // LPVOID bmBits;       offset 24

        write_guest_i32(engine, object_buffer_ptr, 0).context("failed to write BITMAP.bmType")?;

        write_guest_i32(
            engine,
            checked_field_address(object_buffer_ptr, 4, "BITMAP.bmWidth"),
            16,
        )
        .context("failed to write BITMAP.bmWidth")?;

        write_guest_i32(
            engine,
            checked_field_address(object_buffer_ptr, 8, "BITMAP.bmHeight"),
            16,
        )
        .context("failed to write BITMAP.bmHeight")?;

        write_guest_i32(
            engine,
            checked_field_address(object_buffer_ptr, 12, "BITMAP.bmWidthBytes"),
            64,
        )
        .context("failed to write BITMAP.bmWidthBytes")?;

        write_guest_u16(
            engine,
            checked_field_address(object_buffer_ptr, 16, "BITMAP.bmPlanes"),
            1,
        )
        .context("failed to write BITMAP.bmPlanes")?;

        write_guest_u16(
            engine,
            checked_field_address(object_buffer_ptr, 18, "BITMAP.bmBitsPixel"),
            32,
        )
        .context("failed to write BITMAP.bmBitsPixel")?;

        write_guest_u32(
            engine,
            checked_field_address(object_buffer_ptr, 20, "BITMAP padding"),
            0,
        )
        .context("failed to write BITMAP padding")?;

        write_guest_u64(
            engine,
            checked_field_address(object_buffer_ptr, 24, "BITMAP.bmBits"),
            0,
        )
        .context("failed to write BITMAP.bmBits")?;

        BITMAP_STRUCT_SIZE
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetObjectA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Stock object identifiers (wingdi.h).
const STOCK_WHITE_BRUSH: u64 = 0;
const STOCK_BLACK_BRUSH: u64 = 1;
const STOCK_GRAY_BRUSH: u64 = 2;
const STOCK_NULL_BRUSH: u64 = 5;
const STOCK_SYSTEM_FONT: u64 = 13;
const STOCK_DEFAULT_PALETTE: u64 = 15;

/// Handles `GDI32.dll!GetStockObject` — returns predefined stock object handles.
pub fn handle_get_stock_object(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let n_index = engine.read_rcx()? & 0xffff_ffff;
    let handle = match n_index {
        STOCK_WHITE_BRUSH => 0x0000_0000_6800_5001,
        STOCK_BLACK_BRUSH => 0x0000_0000_6800_5002,
        STOCK_GRAY_BRUSH => 0x0000_0000_6800_5003,
        STOCK_NULL_BRUSH => 0x0000_0000_6800_5004,
        STOCK_SYSTEM_FONT => 0x0000_0000_6800_5005,
        STOCK_DEFAULT_PALETTE => 0x0000_0000_6800_5006,
        _ => 0, // NULL for unknown stock objects
    };
    let return_address = engine.return_from_win64_api(handle)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
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

    let replaced = match object_kind {
        Some(GdiObject::Dib(bitmap)) => state
            .gdi_state()
            .find_dc_mut(Hdc::from(dc_handle))
            .map(|dc| dc.selected_bitmap.replace(bitmap).map(Hbitmap::as_u64)),
        Some(GdiObject::Brush(brush)) => state
            .gdi_state()
            .find_dc_mut(Hdc::from(dc_handle))
            .map(|dc| dc.selected_brush.replace(brush).map(Hbrush::as_u64)),
        Some(GdiObject::Pen(pen)) => state
            .gdi_state()
            .find_dc_mut(Hdc::from(dc_handle))
            .map(|dc| dc.selected_pen.replace(pen).map(Hpen::as_u64)),
        Some(GdiObject::Font(font)) => state
            .gdi_state()
            .find_dc_mut(Hdc::from(dc_handle))
            .map(|dc| dc.selected_font.replace(font).map(Hfont::as_u64)),
        None => None,
    };

    if let Some(replaced) = replaced {
        let return_value = replaced.unwrap_or(FAKE_PREVIOUS_GDI_OBJECT_HANDLE);
        let ra = engine
            .return_from_win64_api(return_value)
            .context("failed to return from SelectObject")?;
        return Ok(WinApiHandlerResult {
            return_address: ra,
            return_value,
        });
    }

    // For fonts and unknown DCs/objects, return the fake handle.
    let return_address = engine
        .return_from_win64_api(FAKE_PREVIOUS_GDI_OBJECT_HANDLE)
        .context("failed to return from SelectObject")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_PREVIOUS_GDI_OBJECT_HANDLE,
    })
}

/// Handles `GDI32.dll!GetTextExtentPoint32A`.
pub fn handle_get_text_extent_point_32_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_get_text_extent_point_32(ctx, "GetTextExtentPoint32A", false)
}

/// Handles `GDI32.dll!GetTextExtentPoint32W`.
pub fn handle_get_text_extent_point_32_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_get_text_extent_point_32(ctx, "GetTextExtentPoint32W", true)
}

fn handle_get_text_extent_point_32(
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
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let (width, height) = {
        let resolved = dc_resolved_font(state, device_context_handle, &mut font_engine);
        match resolved {
            Some((key, resolved)) => {
                let chars = crate::gdi32::text::read_text_chars(engine, text_ptr, count, wide)?;
                let mut width = 0_i32;
                for ch in chars {
                    if let Some(ch) = char::from_u32(ch) {
                        width = width.saturating_add(font_engine.char_advance(&resolved, &key, ch));
                    }
                }
                (width, resolved.line_height())
            }
            None => (0, 16),
        }
    };
    state.gdi_state().font_engine = font_engine;

    let width = width.max(0);
    let height = height.max(0);
    let (width_u32, height_u32) = (
        u32::try_from(width).unwrap_or(u32::MAX),
        u32::try_from(height).unwrap_or(u32::MAX),
    );

    if size_ptr != 0 {
        write_guest_u32(engine, size_ptr, width_u32)
            .with_context(|| format!("failed to write SIZE.cx for {api_name}"))?;
        write_guest_u32(
            engine,
            checked_field_address(size_ptr, 4, "SIZE.cy"),
            height_u32,
        )
        .with_context(|| format!("failed to write SIZE.cy for {api_name}"))?;
    }

    let return_value = u64::from(size_ptr != 0);

    let return_address = engine
        .return_from_win64_api(return_value)
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

/// Handles `GDI32.dll!CreateCompatibleDC`.
pub fn handle_create_compatible_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _source_device_context = engine
        .read_rcx()
        .context("failed to read RCX for CreateCompatibleDC")?;

    let dc_handle = state.gdi_state().alloc_dc(DcKind::Memory);

    let return_address = engine
        .return_from_win64_api(dc_handle.as_u64())
        .context("failed to return from CreateCompatibleDC")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: dc_handle.as_u64(),
    })
}

/// Handles `GDI32.dll!GetDeviceCaps`.
///
/// Returns plausible values for a 1920×1080 32-bpp desktop so Lunar Magic's
/// display-mode probes succeed without real GDI.
pub fn handle_get_device_caps(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hdc = engine
        .read_rcx()
        .context("failed to read RCX for GetDeviceCaps")?;

    let index = engine
        .read_rdx()
        .context("failed to read RDX for GetDeviceCaps")?;

    // Common GetDeviceCaps indices from wingdi.h.
    // Identical return values are intentionally merged (clippy match_same_arms).
    let return_value = match index {
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
    };

    tracing::debug!(index, return_value, "GetDeviceCaps");

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetDeviceCaps")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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

    let return_address = engine
        .return_from_win64_api(FAKE_PIXEL_COLOR)
        .context("failed to return from GetPixel")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_PIXEL_COLOR,
    })
}

/// Handles `GDI32.dll!DeleteDC`.
pub fn handle_delete_dc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let device_context_handle = engine
        .read_rcx()
        .context("failed to read RCX for DeleteDC")?;

    let return_value = u64::from(device_context_handle != 0);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from DeleteDC")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from DeleteObject")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
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
