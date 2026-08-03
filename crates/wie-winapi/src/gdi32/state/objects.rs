use anyhow::{Context, Result};

use crate::gdi32::{FontEngine, FontKey, ResolvedFont, fontdb_weight_for, height_px_from_lf};
use crate::guest_memory::{
    checked_field_address, read_i32 as read_guest_i32, read_u8 as read_guest_u8,
    read_u16 as read_guest_u16, read_u32 as read_guest_u32, read_u64 as read_guest_u64,
    write_u64 as write_guest_u64,
};
use crate::guest_string::{
    read_ansi_lossy as read_guest_ansi_lossy, read_utf16_lossy as read_guest_utf16_lossy,
};
use crate::handles::Hdc;
use crate::user32::low_i32;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

use super::FAKE_GDI_BITMAP_HANDLE_BASE;
use super::records::{DibSection, allocate_gdi_heap_block};

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
    handle_create_font_indirect_impl(ctx, "CreateFontIndirectA", false)
}

/// Handles `GDI32.dll!CreateFontIndirectW`.
pub fn handle_create_font_indirect_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_create_font_indirect_impl(ctx, "CreateFontIndirectW", true)
}

/// Shared `CreateFontIndirectA/W` implementation.
///
/// Reads the guest LOGFONT (identical layout for both variants; only
/// `lfFaceName` differs — `char[32]` for A, `wchar_t[32]` for W, both at
/// offset 28). The A/W split mirrors `CreateFontA/W`.
fn handle_create_font_indirect_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let logfont_ptr = engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {api_name}"))?;

    if logfont_ptr == 0 {
        let return_address = engine
            .return_from_win64_api(0)
            .with_context(|| format!("failed to return from {api_name}"))?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    // LOGFONT layout (Win64):
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
    // TCHAR lfFaceName[32];   offset 28 (char[32] for A, wchar_t[32] for W)
    let height = read_guest_i32(engine, logfont_ptr)
        .with_context(|| format!("failed to read {api_name}.lfHeight"))?;
    let weight = read_guest_i32(engine, checked_field_address(logfont_ptr, 16, "lfWeight"))
        .with_context(|| format!("failed to read {api_name}.lfWeight"))?;
    let italic_and_underline =
        read_guest_u16(engine, checked_field_address(logfont_ptr, 20, "lfItalic"))
            .with_context(|| format!("failed to read {api_name}.lfItalic"))?;
    let charset = read_guest_u8(engine, checked_field_address(logfont_ptr, 23, "lfCharSet"))
        .with_context(|| format!("failed to read {api_name}.lfCharSet"))?;
    let face_name_ptr = checked_field_address(logfont_ptr, 28, "lfFaceName");
    let face_name = if wide {
        read_guest_utf16_lossy(engine, face_name_ptr, 32)
    } else {
        read_guest_ansi_lossy(engine, face_name_ptr, 64)
    }
    .with_context(|| format!("failed to read {api_name}.lfFaceName"))?;

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
