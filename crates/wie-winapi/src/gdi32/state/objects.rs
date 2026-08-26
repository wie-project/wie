use anyhow::{Context, Result};

use crate::gdi32::{
    ArgReg, FontEngine, FontKey, ResolvedFont, fontdb_weight_for, height_px_from_lf, read_arg,
};
use crate::guest_layout::{BitmapInfoHeader, LogFontA, LogFontW};
use crate::guest_memory::{
    checked_address, read_u32, read_u64, with_typed_read, write_u64 as write_guest_u64,
};
use crate::guest_string::{
    decode_ansi_lossy, decode_utf16_lossy, read_ansi_lossy as read_guest_ansi_lossy,
    read_utf16_lossy as read_guest_utf16_lossy,
};
use crate::handles::Hdc;
use crate::user32::low_i32;
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

use super::FAKE_GDI_BITMAP_HANDLE_BASE;
use super::records::{DibSection, allocate_gdi_heap_block};
use crate::kernel32::low_u32;

/// Handles `GDI32.dll!CreateSolidBrush`.
pub fn handle_create_solid_brush(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let color_raw = read_arg(engine, ArgReg::Rcx, "CreateSolidBrush")?;

    let color = low_u32(color_raw, "CreateSolidBrush color")?;

    let handle = state.gdi_state().alloc_brush(color);

    tracing::debug!(handle = handle.as_u64(), color, "CreateSolidBrush");

    ctx.finish(handle.as_u64())
}

/// Handles `GDI32.dll!CreatePen` (object allocated; stroke rendering deferred).
pub fn handle_create_pen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _style = read_arg(engine, ArgReg::Rcx, "CreatePen")?;
    let _width = read_arg(engine, ArgReg::Rdx, "CreatePen")?;
    let color_raw = read_arg(engine, ArgReg::R8, "CreatePen")?;

    let color = low_u32(color_raw, "CreatePen color")?;

    let handle = state.gdi_state().alloc_pen(color);

    tracing::debug!(handle = handle.as_u64(), color, "CreatePen");

    ctx.finish(handle.as_u64())
}

/// Handles `GDI32.dll!CreateDIBSection`.
///
/// Allocates a guest pixel buffer from the process heap and returns a fake
/// `HBITMAP`. Pixel contents are zeroed; rendering is not implemented.
pub fn handle_create_dib_section(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _hdc = read_arg(engine, ArgReg::Rcx, "CreateDIBSection")?;

    let bmi_va = read_arg(engine, ArgReg::Rdx, "CreateDIBSection")?;

    let _usage = read_arg(engine, ArgReg::R8, "CreateDIBSection")?;

    let bits_out_va = read_arg(engine, ArgReg::R9, "CreateDIBSection")?;

    let (width_abs, height_abs, bit_count, height_signed) = if bmi_va != 0 {
        // BITMAPINFOHEADER (the fixed 40-byte header of a BITMAPINFO):
        // biWidth@4, biHeight@8, biBitCount@14 — one typed read. The layout is
        // pinned by the BitmapInfoHeader const-assert table.
        let (bi_width, bi_height, bit_count) =
            with_typed_read::<BitmapInfoHeader, _, _>(engine, bmi_va, |header| {
                Ok((
                    header.bi_width,
                    header.bi_height,
                    u32::from(header.bi_bit_count),
                ))
            })
            .context("failed to read CreateDIBSection BITMAPINFOHEADER")?;
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

    let bits_va = allocate_gdi_heap_block(engine, state, image_size.max(16));
    if bits_va == 0 {
        tracing::warn!(
            width_abs,
            height_abs,
            bit_count,
            image_size,
            "CreateDIBSection heap allocation failed"
        );
        return ctx.finish(0);
    }

    // Zero the pixel buffer so guest reads are defined.
    if image_size > 0 {
        let zero = vec![0_u8; usize::try_from(image_size).unwrap_or(0)];
        if !zero.is_empty() {
            engine
                .mem_write(bits_va, &zero)
                .context("failed to zero CreateDIBSection bits")?;
        }
    }

    if bits_out_va != 0 {
        write_guest_u64(engine, bits_out_va, bits_va)
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
        bits_va,
        byte_len: image_size,
    });

    tracing::debug!(
        dib_handle = dib_handle.as_u64(),
        bits_va,
        width_abs,
        height_signed,
        bit_count,
        image_size,
        "CreateDIBSection"
    );

    ctx.finish(dib_handle.as_u64())
}

/// Handles `GDI32.dll!CreateCompatibleBitmap`.
pub fn handle_create_compatible_bitmap(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _hdc = read_arg(engine, ArgReg::Rcx, "CreateCompatibleBitmap")?;

    let width = read_arg(engine, ArgReg::Rdx, "CreateCompatibleBitmap")?;

    let height = read_arg(engine, ArgReg::R8, "CreateCompatibleBitmap")?;

    let handle = if width == 0 || height == 0 {
        0
    } else {
        next_gdi_bitmap_handle(state)?
    };

    tracing::debug!(handle, width, height, "CreateCompatibleBitmap");

    ctx.finish(handle)
}

pub(crate) fn next_gdi_bitmap_handle(state: &mut WinApiState) -> Result<u64> {
    // Use bump-heap high bits as a cheap monotonic discriminator.
    let live = u64::try_from(
        state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live_count(),
    )
    .unwrap_or(0);
    let index = (state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .bump_cursor()
        >> 4)
        .wrapping_add(live);
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
    let height_raw = read_arg(engine, ArgReg::Rcx, api_name)?;
    let rsp = engine
        .read_rsp()
        .with_context(|| format!("failed to read RSP for {api_name}"))?;
    // Stack args are 8-byte slots; arg 5 (`cWeight`) lives at [rsp+0x28].
    let weight_raw = read_u32(engine, checked_address(rsp, 0x28, "cWeight"))
        .with_context(|| format!("failed to read {api_name} cWeight"))?;
    let italic_raw = read_u32(engine, checked_address(rsp, 0x30, "bItalic"))
        .with_context(|| format!("failed to read {api_name} bItalic"))?;
    // arg 9 (`iCharSet`) at [rsp+0x48].
    let charset_raw = read_u32(engine, checked_address(rsp, 0x48, "iCharSet"))
        .with_context(|| format!("failed to read {api_name} iCharSet"))?;
    // arg 13 (`iPitchAndFamily`) at [rsp+0x68]; low byte is the pitch hint.
    let pitch_and_family = read_u32(engine, checked_address(rsp, 0x68, "iPitchAndFamily"))
        .with_context(|| format!("failed to read {api_name} iPitchAndFamily"))?;
    let face_name_va = read_u64(engine, checked_address(rsp, 0x70, "pszFaceName"))
        .with_context(|| format!("failed to read {api_name} pszFaceName"))?;

    let face_name = if wide {
        read_guest_utf16_lossy(engine, face_name_va, 64)
    } else {
        read_guest_ansi_lossy(engine, face_name_va, 64)
    }
    .with_context(|| format!("failed to read {api_name} face name"))?;

    let height = low_i32(height_raw, api_name)?;
    let weight = fontdb_weight_for(i32::from_le_bytes(weight_raw.to_le_bytes()));
    let italic = italic_raw != 0;
    let charset = u8::try_from(charset_raw & 0xFF).unwrap_or(0);
    let pitch = u8::try_from(pitch_and_family & 0xFF).unwrap_or(0);

    let handle = state
        .gdi_state()
        .alloc_font(face_name.clone(), height, weight, italic, charset);
    state.gdi_state().set_font_pitch(handle, pitch);

    tracing::debug!(
        handle = handle.as_u64(),
        height,
        weight,
        italic,
        charset,
        pitch,
        face_name,
        api_name
    );

    ctx.finish(handle.as_u64())
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
/// Reads the guest LOGFONT in one typed view. `LOGFONTA` (60 bytes) and
/// `LOGFONTW` (92 bytes) share the header layout — `lfHeight` @0 …
/// `lfPitchAndFamily` @27 — and differ only in `lfFaceName` at offset 28
/// (`char[32]` for A, `wchar_t[32]` for W). The face name is decoded from the
/// struct's inline bytes, so the guest memory is touched once.
fn handle_create_font_indirect_impl(
    ctx: &mut HandlerContext<'_>,
    api_name: &str,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let logfont_va = read_arg(engine, ArgReg::Rcx, api_name)?;

    if logfont_va == 0 {
        return ctx.finish(0);
    }

    let (height, weight, italic, charset, pitch, underline, strike_out, face_name) = if wide {
        with_typed_read::<LogFontW, _, _>(engine, logfont_va, |lf| {
            Ok((
                lf.height,
                lf.weight,
                lf.italic != 0,
                lf.charset,
                lf.pitch_and_family,
                lf.underline != 0,
                lf.strike_out != 0,
                decode_utf16_lossy(&lf.face_name),
            ))
        })
    } else {
        with_typed_read::<LogFontA, _, _>(engine, logfont_va, |lf| {
            Ok((
                lf.height,
                lf.weight,
                lf.italic != 0,
                lf.charset,
                lf.pitch_and_family,
                lf.underline != 0,
                lf.strike_out != 0,
                decode_ansi_lossy(&lf.face_name),
            ))
        })
    }
    .with_context(|| format!("failed to read {api_name} LOGFONT"))?;

    let weight = fontdb_weight_for(weight);
    let handle = state
        .gdi_state()
        .alloc_font(face_name.clone(), height, weight, italic, charset);
    state.gdi_state().set_font_pitch(handle, pitch);
    state
        .gdi_state()
        .set_font_effects(handle, strike_out, underline);

    tracing::debug!(
        handle = handle.as_u64(),
        height,
        weight,
        italic,
        strike_out,
        underline,
        charset,
        pitch,
        face_name,
        api_name
    );

    ctx.finish(handle.as_u64())
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
    let font_handle = gdi.find_dc(Hdc::from(dc_handle))?.selected_font;
    resolve_stored_font(state, font_handle, font_engine)
}

/// Resolve the HFONT a window's `WM_SETFONT` stored (task 2.7) through the
/// font engine, returning the [`FontKey`] and the resolved px metrics.
///
/// A window with no stored font — or with an HFONT that is not in the GDI
/// font table — resolves the system default (sans-serif, 16 px, regular),
/// matching what the control paint paths drew before `WM_SETFONT` existed.
/// Returns `None` only when the system font lookup itself fails.
pub(crate) fn window_font_resolution(
    state: &WinApiState,
    hwnd: u64,
    font_engine: &mut FontEngine,
) -> Option<(FontKey, ResolvedFont)> {
    let stored = state
        .try_window_state()?
        .windows
        .iter()
        .find(|window| window.handle == crate::handles::Hwnd::from(hwnd))
        .map_or(crate::handles::Hfont::NULL, |window| window.font_handle);
    // A window with no stored font — or with an HFONT that is not in the GDI
    // font table (NULL / never set / stale / foreign) — resolves the system
    // default: sans-serif, 16 px, regular. A bad handle must never fail the
    // whole control paint.
    let (family, height, weight, italic, fixed_pitch, strike_out, underline) = state
        .try_gdi_state()
        .and_then(|gdi| gdi.find_font(stored))
        .map_or(
            (String::new(), 0, 400, false, false, false, false),
            |font| {
                (
                    font.family.clone(),
                    font.height,
                    font.weight,
                    font.italic,
                    font.pitch & 0x01 != 0,
                    font.strike_out,
                    font.underline,
                )
            },
        );
    let key = FontKey {
        family: family.to_ascii_lowercase(),
        weight,
        italic,
        fixed_pitch,
        strike_out,
        underline,
    };
    let resolved = font_engine.resolve(&key, height_px_from_lf(height))?;
    Some((key, resolved))
}

/// Resolve a window's stored font through [`window_font_resolution`], falling
/// back to the system default (sans-serif, 16 px, regular) when the stored
/// font cannot be resolved. Returns `None` only when the default lookup itself
/// fails — the resolution every control paint/measure/hit-test path needs, so
/// the per-site `match` + `FontKey::default()` fallback is folded here.
pub(crate) fn window_font_resolution_or_default(
    state: &WinApiState,
    hwnd: u64,
    font_engine: &mut FontEngine,
) -> Option<(FontKey, ResolvedFont)> {
    match window_font_resolution(state, hwnd, font_engine) {
        Some(key_and_resolved) => Some(key_and_resolved),
        None => {
            let default_key = FontKey::default();
            font_engine
                .resolve(&default_key, 16)
                .map(|resolved| (default_key, resolved))
        }
    }
}

/// Resolve an optional stored HFONT (or the system default when `None`) into
/// a [`FontKey`] + resolved px metrics. An HFONT absent from the GDI font
/// table fails the lookup (`None`) — the per-window path turns that into the
/// default fallback; the DC path surfaces it as an unresolved font. Shared by
/// the DC selected-font path and the per-window `WM_SETFONT` path.
fn resolve_stored_font(
    state: &WinApiState,
    font_handle: Option<crate::handles::Hfont>,
    font_engine: &mut FontEngine,
) -> Option<(FontKey, ResolvedFont)> {
    let gdi = state.try_gdi_state()?;
    let (family, height, weight, italic, fixed_pitch, strike_out, underline) = match font_handle {
        Some(font_handle) => {
            let font = gdi.find_font(font_handle)?;
            (
                font.family.clone(),
                font.height,
                font.weight,
                font.italic,
                font.pitch & 0x01 != 0,
                font.strike_out,
                font.underline,
            )
        }
        None => (String::new(), 0, 400, false, false, false, false),
    };
    let key = FontKey {
        family: family.to_ascii_lowercase(),
        weight,
        italic,
        fixed_pitch,
        strike_out,
        underline,
    };
    let resolved = font_engine.resolve(&key, height_px_from_lf(height))?;
    Some((key, resolved))
}
