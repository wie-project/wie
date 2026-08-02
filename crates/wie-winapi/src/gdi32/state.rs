use anyhow::{Context, Result};

use crate::gdi32::{FontEngine, FontKey, ResolvedFont, fontdb_weight_for, height_px_from_lf};
use crate::guest_memory::{
    checked_field_address, read_i32 as read_guest_i32, read_u8 as read_guest_u8,
    read_u16 as read_guest_u16, read_u32 as read_guest_u32, read_u64 as read_guest_u64,
    write_i32 as write_guest_i32, write_u16 as write_guest_u16, write_u32 as write_guest_u32,
    write_u64 as write_guest_u64,
};
use crate::guest_string::{
    read_ansi_lossy as read_guest_ansi_lossy, read_utf16_lossy as read_guest_utf16_lossy,
};
use crate::handles::{Hbitmap, Hbrush, Hdc, Hfont, Hpen, Hwnd};
use crate::user32::low_i32;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

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

/// Handles `GDI32.dll!CreateSolidBrush`.
pub fn handle_create_solid_brush(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let color_raw = engine
        .read_rcx()
        .context("failed to read RCX for CreateSolidBrush")?;

    let color = u32::try_from(color_raw & u64::from(u32::MAX))
        .context("CreateSolidBrush color does not fit u32")?;

    let handle = state.gdi_state().alloc_brush(color);

    tracing::debug!(handle = handle.as_u64(), color, "CreateSolidBrush");

    let return_address = engine
        .return_from_win64_api(handle.as_u64())
        .context("failed to return from CreateSolidBrush")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle.as_u64(),
    })
}

/// Handles `GDI32.dll!CreatePen` (object allocated; stroke rendering deferred).
pub fn handle_create_pen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _style = engine
        .read_rcx()
        .context("failed to read RCX for CreatePen")?;
    let _width = engine
        .read_rdx()
        .context("failed to read RDX for CreatePen")?;
    let color_raw = engine
        .read_r8()
        .context("failed to read R8 for CreatePen")?;

    let color = u32::try_from(color_raw & u64::from(u32::MAX))
        .context("CreatePen color does not fit u32")?;

    let handle = state.gdi_state().alloc_pen(color);

    tracing::debug!(handle = handle.as_u64(), color, "CreatePen");

    let return_address = engine
        .return_from_win64_api(handle.as_u64())
        .context("failed to return from CreatePen")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle.as_u64(),
    })
}

/// Handles `GDI32.dll!CreateDIBSection`.
///
/// Allocates a guest pixel buffer from the process heap and returns a fake
/// `HBITMAP`. Pixel contents are zeroed; rendering is not implemented.
pub fn handle_create_dib_section(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _hdc = engine
        .read_rcx()
        .context("failed to read RCX for CreateDIBSection")?;

    let bmi_ptr = engine
        .read_rdx()
        .context("failed to read RDX for CreateDIBSection")?;

    let _usage = engine
        .read_r8()
        .context("failed to read R8 for CreateDIBSection")?;

    let bits_out_ptr = engine
        .read_r9()
        .context("failed to read R9 for CreateDIBSection")?;

    let (width_abs, height_abs, bit_count, height_signed) = if bmi_ptr != 0 {
        // BITMAPINFOHEADER: biWidth@4, biHeight@8, biBitCount@14
        let bi_width = read_guest_i32(engine, checked_field_address(bmi_ptr, 4, "biWidth"))
            .context("failed to read CreateDIBSection biWidth")?;
        let bi_height = read_guest_i32(engine, checked_field_address(bmi_ptr, 8, "biHeight"))
            .context("failed to read CreateDIBSection biHeight")?;
        let bit_count = u32::from(
            read_guest_u16(engine, checked_field_address(bmi_ptr, 14, "biBitCount"))
                .context("failed to read CreateDIBSection biBitCount")?,
        );
        // Preserve the signed height: negative = top-down DIB.
        // Use absolute values only for stride/allocation calculations.
        (
            bi_width.unsigned_abs().max(1),
            bi_height.unsigned_abs().max(1),
            bit_count.max(1),
            bi_height, // keep signed for the record
        )
    } else {
        (16, 16, 32, -240) // default bottom-up 16x16
    };

    let bytes_per_pixel = bit_count.div_ceil(8);
    // DIB rows are DWORD-aligned.
    let stride = width_abs
        .checked_mul(bytes_per_pixel)
        .context("CreateDIBSection stride overflow")?
        .div_ceil(4)
        .checked_mul(4)
        .context("CreateDIBSection stride align overflow")?;
    let image_size = u64::from(stride)
        .checked_mul(u64::from(height_abs))
        .context("CreateDIBSection image size overflow")?;

    let bits_ptr = allocate_gdi_heap_block(engine, state, image_size.max(16));
    if bits_ptr == 0 {
        tracing::warn!(
            width_abs,
            height_abs,
            bit_count,
            image_size,
            "CreateDIBSection heap allocation failed"
        );
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from CreateDIBSection")?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    // Zero the pixel buffer so guest reads are defined.
    if image_size > 0 {
        let zero = vec![0_u8; usize::try_from(image_size).unwrap_or(0)];
        if !zero.is_empty() {
            engine
                .mem_write(bits_ptr, &zero)
                .context("failed to zero CreateDIBSection bits")?;
        }
    }

    if bits_out_ptr != 0 {
        write_guest_u64(engine, bits_out_ptr, bits_ptr)
            .context("failed to write CreateDIBSection *ppvBits")?;
    }

    // Allocate a real bitmap handle and record the DibSection.
    let dib_handle = state.gdi_state().alloc_bitmap_handle();
    // Replace the bump-cursor return with the real handle.
    // (We still call next_gdi_bitmap_handle for the old bump sequence to keep
    // the cursor moving — but the real handle is what we return to the guest.)
    let _old_handle = next_gdi_bitmap_handle(state)?;

    state.gdi_state().dibs.push(DibSection {
        handle: dib_handle,
        // 32-bpp DIBs are bounded well below i32::MAX by guest memory limits;
        // saturate rather than wrap if a pathological header is supplied.
        width: i32::try_from(width_abs).unwrap_or(i32::MAX),
        height: height_signed, // preserved sign
        bit_count: u16::try_from(bit_count).unwrap_or(0),
        stride: i32::try_from(stride).unwrap_or(i32::MAX),
        bits_va: bits_ptr,
        byte_len: image_size,
    });

    tracing::debug!(
        dib_handle = dib_handle.as_u64(),
        bits_ptr,
        width_abs,
        height_signed,
        bit_count,
        image_size,
        "CreateDIBSection"
    );

    let return_address = engine
        .return_from_win64_api(dib_handle.as_u64())
        .context("failed to return from CreateDIBSection")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: dib_handle.as_u64(),
    })
}

/// Handles `GDI32.dll!CreateCompatibleBitmap`.
pub fn handle_create_compatible_bitmap(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _hdc = engine
        .read_rcx()
        .context("failed to read RCX for CreateCompatibleBitmap")?;

    let width = engine
        .read_rdx()
        .context("failed to read RDX for CreateCompatibleBitmap")?;

    let height = engine
        .read_r8()
        .context("failed to read R8 for CreateCompatibleBitmap")?;

    let handle = if width == 0 || height == 0 {
        0
    } else {
        next_gdi_bitmap_handle(state)?
    };

    tracing::debug!(handle, width, height, "CreateCompatibleBitmap");

    let return_address = engine
        .return_from_win64_api(handle)
        .context("failed to return from CreateCompatibleBitmap")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}

fn next_gdi_bitmap_handle(state: &mut WinApiState) -> Result<u64> {
    // Use bump-heap high bits as a cheap monotonic discriminator.
    let live = u64::try_from(state.heap_state.heap.live_count()).unwrap_or(0);
    let index = (state.heap_state.heap.bump_cursor() >> 4).wrapping_add(live);
    let handle = FAKE_GDI_BITMAP_HANDLE_BASE
        .checked_add(index)
        .context("GDI bitmap handle overflow")?;
    Ok(handle)
}

/// Handles `GDI32.dll!CreateFontA`.
pub fn handle_create_font_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_create_font_impl(ctx, "CreateFontA", false)
}

/// Handles `GDI32.dll!CreateFontW`.
pub fn handle_create_font_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_create_font_impl(ctx, "CreateFontW", true)
}

/// Shared `CreateFontA/W` implementation.
///
/// Reads the 14 Win32 arguments (4 register args + 10 stack slots). Keeps the
/// attributes the font engine resolves: `cHeight` → px height, `cWeight` →
/// 400/700, `bItalic`, `iCharSet`, and `pszFaceName`. `lfWidth` is ignored
/// (proportional fonts).
fn handle_create_font_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let height_raw = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;
    let rsp = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {api_name}"))?;
    // Stack args are 8-byte slots; arg 5 (`cWeight`) lives at [rsp+0x28].
    let weight_raw = read_guest_u32(engine, checked_field_address(rsp, 0x28, "cWeight"))
        .with_context(|| format!("failed to read {api_name} cWeight"))?;
    let italic_raw = read_guest_u32(engine, checked_field_address(rsp, 0x30, "bItalic"))
        .with_context(|| format!("failed to read {api_name} bItalic"))?;
    // arg 9 (`iCharSet`) at [rsp+0x48].
    let charset_raw = read_guest_u32(engine, checked_field_address(rsp, 0x48, "iCharSet"))
        .with_context(|| format!("failed to read {api_name} iCharSet"))?;
    let face_name_ptr = read_guest_u64(engine, checked_field_address(rsp, 0x70, "pszFaceName"))
        .with_context(|| format!("failed to read {api_name} pszFaceName"))?;

    let face_name = if wide {
        read_guest_utf16_lossy(engine, face_name_ptr, 64)
    } else {
        read_guest_ansi_lossy(engine, face_name_ptr, 64)
    }
    .with_context(|| format!("failed to read {api_name} face name"))?;

    let height = low_i32(height_raw, api_name)?;
    let weight = fontdb_weight_for(i32::from_le_bytes(weight_raw.to_le_bytes()));
    let italic = italic_raw != 0;
    let charset = u8::try_from(charset_raw & 0xFF).unwrap_or(0);

    let handle = state
        .gdi_state()
        .alloc_font(face_name.clone(), height, weight, italic, charset);

    tracing::debug!(
        handle = handle.as_u64(),
        height,
        weight,
        italic,
        charset,
        face_name,
        api_name
    );

    let return_address = engine
        .return_from_win64_api(handle.as_u64())
        .with_context(|| format!("failed to return from {api_name}"))?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle.as_u64(),
    })
}

/// Handles `GDI32.dll!CreateFontIndirectA`.
pub fn handle_create_font_indirect_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let logfont_ptr = engine
        .read_rcx()
        .context("failed to read RCX for CreateFontIndirectA")?;

    if logfont_ptr == 0 {
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from CreateFontIndirectA")?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    // LOGFONTA layout (Win64):
    // LONG  lfHeight;         offset 0
    // LONG  lfWidth;          offset 4
    // LONG  lfEscapement;     offset 8
    // LONG  lfOrientation;    offset 12
    // LONG  lfWeight;         offset 16
    // BYTE  lfItalic;         offset 20
    // BYTE  lfUnderline;      offset 21
    // BYTE  lfStrikeOut;      offset 22
    // BYTE  lfCharSet;        offset 23
    // BYTE  lfOutPrecision;   offset 24
    // BYTE  lfClipPrecision;  offset 25
    // BYTE  lfQuality;        offset 26
    // BYTE  lfPitchAndFamily; offset 27
    // TCHAR lfFaceName[32];   offset 28
    let height = read_guest_i32(engine, logfont_ptr).context("failed to read LOGFONTA.lfHeight")?;
    let weight = read_guest_i32(engine, checked_field_address(logfont_ptr, 16, "lfWeight"))
        .context("failed to read LOGFONTA.lfWeight")?;
    let italic_and_underline =
        read_guest_u16(engine, checked_field_address(logfont_ptr, 20, "lfItalic"))
            .context("failed to read LOGFONTA.lfItalic")?;
    let charset = read_guest_u8(engine, checked_field_address(logfont_ptr, 23, "lfCharSet"))
        .context("failed to read LOGFONTA.lfCharSet")?;
    let face_name_ptr = checked_field_address(logfont_ptr, 28, "lfFaceName");
    let face_name = read_guest_ansi_lossy(engine, face_name_ptr, 64)
        .context("failed to read LOGFONTA.lfFaceName")?;

    let weight = fontdb_weight_for(weight);
    let italic = italic_and_underline & 0x1 != 0;
    let handle = state
        .gdi_state()
        .alloc_font(face_name.clone(), height, weight, italic, charset);

    tracing::debug!(
        handle = handle.as_u64(),
        height,
        weight,
        italic,
        charset,
        face_name,
        "CreateFontIndirectA"
    );

    let return_address = engine
        .return_from_win64_api(handle.as_u64())
        .context("failed to return from CreateFontIndirectA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle.as_u64(),
    })
}

/// Resolve the font currently selected into `dc_handle` through the font
/// engine, returning the [`FontKey`] and the resolved px metrics.
///
/// A DC with no selected font (or an unknown DC/font) resolves the system
/// default: sans-serif, 16 px, regular. Returns `None` only when the system
/// font lookup itself fails.
pub(crate) fn dc_resolved_font(
    state: &WinApiState,
    dc_handle: u64,
    font_engine: &mut FontEngine,
) -> Option<(FontKey, ResolvedFont)> {
    let gdi = state.try_gdi_state()?;
    let dc = gdi.find_dc(Hdc::from(dc_handle))?;
    let (family, height, weight, italic) = match dc.selected_font {
        Some(font_handle) => {
            let font = gdi.find_font(font_handle)?;
            (font.family.clone(), font.height, font.weight, font.italic)
        }
        None => (String::new(), 0, 400, false),
    };
    let key = FontKey {
        family: family.to_ascii_lowercase(),
        weight,
        italic,
    };
    let resolved = font_engine.resolve(&key, height_px_from_lf(height))?;
    Some((key, resolved))
}

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

fn allocate_gdi_heap_block(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    size: u64,
) -> u64 {
    state.heap_state.heap.alloc_coherent(engine, size)
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

/// What kind of device context a [`DcRecord`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcKind {
    /// DC obtained via GetDC(hwnd) or BeginPaint.
    Window(Hwnd),
    /// DC created via CreateCompatibleDC.
    Memory,
    /// DC obtained via GetDC(NULL) — the screen.
    Screen,
}

/// A live device context (DC).
#[derive(Debug, Clone)]
pub struct DcRecord {
    /// Fake handle for this DC.
    pub handle: Hdc,
    /// What this DC represents.
    pub kind: DcKind,
    /// Handle of the bitmap currently selected into this DC (if any).
    pub selected_bitmap: Option<Hbitmap>,
    /// Handle of the brush currently selected into this DC (if any).
    pub selected_brush: Option<Hbrush>,
    /// Handle of the pen currently selected into this DC (if any).
    pub selected_pen: Option<Hpen>,
    /// Handle of the font currently selected into this DC (if any).
    pub selected_font: Option<Hfont>,
    pub text_color: u32,
    pub bk_color: u32,
    pub bk_mode: u32,
}

/// A DIBSECTION allocated by CreateDIBSection (or a compatible bitmap).
#[derive(Debug, Clone)]
pub struct DibSection {
    /// Fake handle.
    pub handle: Hbitmap,
    /// Pixel width (always positive; stored from the absolute value).
    pub width: i32,
    /// **Signed** height: negative = top-down DIB.
    pub height: i32,
    /// Bits per pixel (1, 4, 8, 16, 24, 32).
    pub bit_count: u16,
    /// Row stride in bytes (aligned to 4).
    pub stride: i32,
    /// Guest virtual address of the pixel buffer.
    pub bits_va: u64,
    /// Size of the pixel buffer in bytes.
    pub byte_len: u64,
}

/// A brush allocated by `CreateSolidBrush` (or a future `CreateBrushIndirect`).
#[derive(Debug, Clone)]
pub struct BrushRecord {
    /// Fake HBRUSH handle.
    pub handle: Hbrush,
    /// 0RGB color (COLORREF-compatible).
    pub color: u32,
}

/// A pen allocated by `CreatePen` (rendering not yet implemented).
#[derive(Debug, Clone)]
pub struct PenRecord {
    /// Fake HPEN handle.
    pub handle: Hpen,
    /// 0RGB color (COLORREF-compatible).
    pub color: u32,
}

/// A font allocated by `CreateFontA/W` or `CreateFontIndirectA`.
///
/// Records the Win32 attributes the font engine resolves: face name, height,
/// weight, italic and charset. Resolution (family → system face, height →
/// px scale) happens lazily in [`FontEngine`]; the metric APIs and the
/// rasterizer share the same resolved font.
#[derive(Debug, Clone)]
pub struct FontRecord {
    /// HFONT handle.
    pub handle: Hfont,
    /// Raw `lfFaceName` ("" = system default / sans-serif).
    pub family: String,
    /// Raw `lfHeight` (px semantics applied at resolve time).
    pub height: i32,
    /// Resolved weight (400 or 700).
    pub weight: u16,
    /// Italic requested.
    pub italic: bool,
    /// `lfCharSet` (reported by `GetTextMetricsA.tmCharSet`).
    pub charset: u8,
}

/// Per-slot GDI state stored in `DllId::Gdi`.
#[derive(Debug, Clone)]
pub struct GdiState {
    /// All allocated DCs.
    pub dcs: Vec<DcRecord>,
    /// All allocated DIB sections.
    pub dibs: Vec<DibSection>,
    /// All allocated brushes.
    pub brushes: Vec<BrushRecord>,
    /// All allocated pens.
    pub pens: Vec<PenRecord>,
    /// All allocated fonts.
    pub fonts: Vec<FontRecord>,
    /// The system-font engine (face + metrics caches for text rendering).
    pub font_engine: super::font_system::FontEngine,
    /// Next handle for DC allocation.
    pub next_dc_handle: u64,
    /// Next handle for bitmap allocation.
    pub next_bitmap_handle: u64,
    /// Next handle for brush allocation.
    pub next_brush_handle: u64,
    /// Next handle for pen allocation.
    pub next_pen_handle: u64,
    /// Next handle for font allocation.
    pub next_font_handle: u64,
}

impl Default for GdiState {
    fn default() -> Self {
        Self {
            dcs: Vec::new(),
            dibs: Vec::new(),
            brushes: Vec::new(),
            pens: Vec::new(),
            fonts: Vec::new(),
            font_engine: super::font_system::FontEngine::default(),
            next_dc_handle: DC_HANDLE_BASE,
            next_bitmap_handle: BITMAP_HANDLE_BASE,
            next_brush_handle: BRUSH_HANDLE_BASE,
            next_pen_handle: PEN_HANDLE_BASE,
            next_font_handle: FONT_HANDLE_BASE,
        }
    }
}

impl GdiState {
    /// Allocate a new DC handle and record.
    pub fn alloc_dc(&mut self, kind: DcKind) -> Hdc {
        let handle = Hdc::from(self.next_dc_handle);
        self.next_dc_handle = self.next_dc_handle.wrapping_add(DC_HANDLE_STRIDE);
        self.dcs.push(DcRecord {
            handle,
            kind,
            selected_bitmap: None,
            selected_brush: None,
            selected_pen: None,
            selected_font: None,
            text_color: 0,
            bk_color: 0x00FF_FFFF, // white
            bk_mode: 2,            // OPAQUE (real GDI default)
        });
        handle
    }

    /// Allocate a new bitmap/DIB handle.
    pub fn alloc_bitmap_handle(&mut self) -> Hbitmap {
        let handle = Hbitmap::from(self.next_bitmap_handle);
        self.next_bitmap_handle = self.next_bitmap_handle.wrapping_add(BITMAP_HANDLE_STRIDE);
        handle
    }

    /// Allocate a new brush handle and record.
    pub fn alloc_brush(&mut self, color: u32) -> Hbrush {
        let handle = Hbrush::from(self.next_brush_handle);
        self.next_brush_handle = self.next_brush_handle.wrapping_add(BRUSH_HANDLE_STRIDE);
        self.brushes.push(BrushRecord { handle, color });
        handle
    }

    /// Allocate a new pen handle and record.
    pub fn alloc_pen(&mut self, color: u32) -> Hpen {
        let handle = Hpen::from(self.next_pen_handle);
        self.next_pen_handle = self.next_pen_handle.wrapping_add(PEN_HANDLE_STRIDE);
        self.pens.push(PenRecord { handle, color });
        handle
    }

    /// Allocate a new font handle and record.
    pub fn alloc_font(
        &mut self,
        family: String,
        height: i32,
        weight: u16,
        italic: bool,
        charset: u8,
    ) -> Hfont {
        let handle = Hfont::from(self.next_font_handle);
        self.next_font_handle = self.next_font_handle.wrapping_add(FONT_HANDLE_STRIDE);
        self.fonts.push(FontRecord {
            handle,
            family,
            height,
            weight,
            italic,
            charset,
        });
        handle
    }

    /// Find a DC by handle.
    pub fn find_dc(&self, handle: Hdc) -> Option<&DcRecord> {
        self.dcs.iter().find(|dc| dc.handle == handle)
    }

    /// Find a mutable DC by handle.
    pub fn find_dc_mut(&mut self, handle: Hdc) -> Option<&mut DcRecord> {
        self.dcs.iter_mut().find(|dc| dc.handle == handle)
    }

    /// Find a DIB section by handle.
    pub fn find_dib(&self, handle: Hbitmap) -> Option<&DibSection> {
        self.dibs.iter().find(|dib| dib.handle == handle)
    }

    /// Find a brush record by handle.
    pub fn find_brush(&self, handle: Hbrush) -> Option<&BrushRecord> {
        self.brushes.iter().find(|brush| brush.handle == handle)
    }

    /// Find a pen record by handle.
    pub fn find_pen(&self, handle: Hpen) -> Option<&PenRecord> {
        self.pens.iter().find(|pen| pen.handle == handle)
    }

    /// Find a font record by handle.
    pub fn find_font(&self, handle: Hfont) -> Option<&FontRecord> {
        self.fonts.iter().find(|font| font.handle == handle)
    }

    /// Remove a DC by handle.
    pub fn remove_dc(&mut self, handle: Hdc) {
        self.dcs.retain(|dc| dc.handle != handle);
    }

    /// Remove a DIB section by handle.
    pub fn remove_dib(&mut self, handle: Hbitmap) {
        self.dibs.retain(|dib| dib.handle != handle);
    }

    /// Remove a brush by handle.
    pub fn remove_brush(&mut self, handle: Hbrush) {
        self.brushes.retain(|brush| brush.handle != handle);
    }

    /// Remove a pen by handle.
    pub fn remove_pen(&mut self, handle: Hpen) {
        self.pens.retain(|pen| pen.handle != handle);
    }

    /// Remove a font by handle.
    pub fn remove_font(&mut self, handle: Hfont) {
        self.fonts.retain(|font| font.handle != handle);
    }
}

/// A GDI object classified from its handle's disjoint base range.
///
/// The four object ranges (`0x6820` bitmaps, `0x6830` brushes, `0x6840` pens,
/// `0x6850` fonts) are the namespace the handle value already lives in — this
/// decodes it into a typed [`GdiObject`] so `SelectObject` / `DeleteObject`
/// need a single match instead of probing every record table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GdiObject {
    /// `HBITMAP` — a bitmap/DIB handle.
    Dib(Hbitmap),
    /// `HBRUSH` — a brush handle.
    Brush(Hbrush),
    /// `HPEN` — a pen handle.
    Pen(Hpen),
    /// `HFONT` — a font handle.
    Font(Hfont),
}

impl GdiObject {
    /// Decode `handle` from its disjoint base range (the `*_HANDLE_BASE`
    /// constants above). DC handles and FAKE-range values classify to `None`.
    ///
    /// Debug-asserts the cross-kind collision invariant: a handle must never
    /// live in two GDI record tables at once — the ranges are disjoint, so a
    /// collision would mean an allocator reused a value across kinds.
    #[must_use]
    pub fn classify(handle: u64, state: &GdiState) -> Option<Self> {
        let classified = match handle & 0xFFFF_0000 {
            BITMAP_HANDLE_BASE => Some(Self::Dib(Hbitmap::from(handle))),
            BRUSH_HANDLE_BASE => Some(Self::Brush(Hbrush::from(handle))),
            PEN_HANDLE_BASE => Some(Self::Pen(Hpen::from(handle))),
            FONT_HANDLE_BASE => Some(Self::Font(Hfont::from(handle))),
            _ => None,
        };
        debug_assert!(
            {
                let dib = u8::from(state.dibs.iter().any(|d| d.handle == Hbitmap::from(handle)));
                let brush = u8::from(
                    state
                        .brushes
                        .iter()
                        .any(|b| b.handle == Hbrush::from(handle)),
                );
                let pen = u8::from(state.pens.iter().any(|p| p.handle == Hpen::from(handle)));
                let font = u8::from(state.fonts.iter().any(|f| f.handle == Hfont::from(handle)));
                dib.saturating_add(brush)
                    .saturating_add(pen)
                    .saturating_add(font)
                    <= 1
            },
            "GDI handle {handle:#x} collides across record tables"
        );
        classified
    }
}

/// Resolve a brush handle to its 0RGB color.
///
/// Recognizes the stock brushes returned by `GetStockObject`; anything else
/// must be a live [`BrushRecord`]. Returns `None` for the NULL_BRUSH (no
/// pixels change) and for unknown handles.
#[must_use]
pub fn brush_color(state: &mut WinApiState, brush_handle: Hbrush) -> Option<u32> {
    match brush_handle.as_u64() {
        STOCK_WHITE_BRUSH_HANDLE => Some(0x00FF_FFFF),
        STOCK_BLACK_BRUSH_HANDLE => Some(0),
        0x0000_0000_6800_5003 => Some(0x0080_8080), // GRAY_BRUSH
        0x0000_0000_6800_5004 => None,              // NULL_BRUSH — no fill
        _ => state
            .gdi_state()
            .find_brush(brush_handle)
            .map(|brush| brush.color),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used)]
mod tests {
    use super::height_px_from_lf;

    #[test]
    fn height_px_mapping_does_not_need_system_fonts() {
        // lfHeight == 0 → the 16 px default.
        assert_eq!(height_px_from_lf(0), 16);
        // Negative = character height in px.
        assert_eq!(height_px_from_lf(-24), 24);
        assert_eq!(height_px_from_lf(-1), 1);
        // Positive = cell height, approximated as the same px count.
        assert_eq!(height_px_from_lf(24), 24);
        assert_eq!(height_px_from_lf(16), 16);
        // Extreme values never collapse to zero.
        assert_eq!(height_px_from_lf(i32::MAX), i32::MAX);
        // |i32::MIN| does not fit i32 — the function falls back to the default.
        assert_eq!(height_px_from_lf(i32::MIN), 16);
    }

    #[test]
    fn font_handle_allocator_is_disjoint_from_other_gdi_bases() {
        let mut gdi = crate::gdi32::GdiState::default();
        let a = gdi.alloc_font(String::new(), 16, 400, false, 0);
        let b = gdi.alloc_font("arial".to_owned(), 24, 700, true, 1);
        assert_ne!(a, b);
        // Font handles live in 0x6850_0000; DC/bitmap/brush/pen bases are
        // 0x6810/0x6820/0x6830/0x6840 — disjoint by construction.
        assert_eq!(a.as_u64() & 0xFFFF_0000, 0x6850_0000);
        assert_eq!(b.as_u64() & 0xFFFF_0000, 0x6850_0000);
        assert_eq!(gdi.fonts.len(), 2);
        assert!(gdi.find_font(a).is_some());
        assert!(gdi.find_font(b).is_some());
        let record = gdi.find_font(b).expect("font b exists");
        assert_eq!(record.weight, 700);
        assert!(record.italic);
        assert_eq!(record.charset, 1);
        gdi.remove_font(a);
        assert!(gdi.find_font(a).is_none());
        assert!(gdi.find_font(b).is_some());
    }

    #[test]
    fn classify_decodes_each_gdi_kind_from_its_base_range() {
        use crate::gdi32::{DcKind, GdiObject};

        let mut gdi = crate::gdi32::GdiState::default();
        // Allocate one object of every kind so the collision assert in
        // `classify` has real tables to check against.
        let bitmap = gdi.alloc_bitmap_handle();
        let brush = gdi.alloc_brush(0x00ff_0000);
        let pen = gdi.alloc_pen(0x0000_ff00);
        let font = gdi.alloc_font("arial".to_owned(), 16, 400, false, 0);

        assert_eq!(
            GdiObject::classify(bitmap.as_u64(), &gdi),
            Some(GdiObject::Dib(bitmap)),
        );
        assert_eq!(
            GdiObject::classify(brush.as_u64(), &gdi),
            Some(GdiObject::Brush(brush)),
        );
        assert_eq!(
            GdiObject::classify(pen.as_u64(), &gdi),
            Some(GdiObject::Pen(pen)),
        );
        assert_eq!(
            GdiObject::classify(font.as_u64(), &gdi),
            Some(GdiObject::Font(font)),
        );

        // A DC handle, the NULL handle, and a FAKE-range stock handle
        // classify to None.
        let dc = gdi.alloc_dc(DcKind::Memory);
        assert_eq!(GdiObject::classify(dc.as_u64(), &gdi), None);
        assert_eq!(GdiObject::classify(0, &gdi), None);
        assert_eq!(GdiObject::classify(0x0000_0000_6800_5001, &gdi), None);
    }
}
