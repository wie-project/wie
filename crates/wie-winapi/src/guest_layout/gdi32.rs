//! gdi32 lane: `TextMetricA`/`TextMetricW`, `LogFontA`/`LogFontW`,
//! `Rect`, `Size`, `Bitmap`, `BitmapInfoHeader`.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// --- gdi32 lane: TEXTMETRIC / LOGFONT / RECT / SIZE / BITMAP / BITMAPINFOHEADER ---

/// Win64 `TEXTMETRICA` (wingdi.h): 11 `LONG`s @0..43, then 9 `BYTE`s @44..52
/// (`tmFirstChar` … `tmCharSet`), trailing alignment pad @53..55 — 56 bytes,
/// align 4.
///
/// `TEXTMETRICW` is NOT layout-identical to `TEXTMETRICA`: the W variant's four
/// character fields are `WCHAR`s (2 bytes), which shifts the flag bytes and the
/// trailing pad (60 bytes total — see [`TextMetricW`]).
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct TextMetricA {
    pub(crate) height: i32,
    pub(crate) ascent: i32,
    pub(crate) descent: i32,
    pub(crate) internal_leading: i32,
    pub(crate) external_leading: i32,
    pub(crate) avg_char_width: i32,
    pub(crate) max_char_width: i32,
    pub(crate) weight: i32,
    pub(crate) overhang: i32,
    pub(crate) digitized_aspect_x: i32,
    pub(crate) digitized_aspect_y: i32,
    pub(crate) first_char: u8,
    pub(crate) last_char: u8,
    pub(crate) default_char: u8,
    pub(crate) break_char: u8,
    pub(crate) italic: u8,
    pub(crate) underlined: u8,
    pub(crate) struck_out: u8,
    pub(crate) pitch_and_family: u8,
    pub(crate) charset: u8,
    /// Trailing alignment padding (53 payload bytes → 56).
    pub(crate) _pad: [u8; 3],
}

/// Compile-time layout check for [`TextMetricA`].
const _: () = {
    assert!(
        core::mem::size_of::<TextMetricA>() == 56,
        "TEXTMETRICA must be 56 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, height) == 0,
        "tmHeight @0"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, ascent) == 4,
        "tmAscent @4"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, descent) == 8,
        "tmDescent @8"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, internal_leading) == 12,
        "tmInternalLeading @12"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, external_leading) == 16,
        "tmExternalLeading @16"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, avg_char_width) == 20,
        "tmAveCharWidth @20"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, max_char_width) == 24,
        "tmMaxCharWidth @24"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, weight) == 28,
        "tmWeight @28"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, overhang) == 32,
        "tmOverhang @32"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, digitized_aspect_x) == 36,
        "tmDigitizedAspectX @36"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, digitized_aspect_y) == 40,
        "tmDigitizedAspectY @40"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, first_char) == 44,
        "tmFirstChar @44"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, last_char) == 45,
        "tmLastChar @45"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, default_char) == 46,
        "tmDefaultChar @46"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, break_char) == 47,
        "tmBreakChar @47"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, italic) == 48,
        "tmItalic @48"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, underlined) == 49,
        "tmUnderlined @49"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, struck_out) == 50,
        "tmStruckOut @50"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, pitch_and_family) == 51,
        "tmPitchAndFamily @51"
    );
    assert!(
        core::mem::offset_of!(TextMetricA, charset) == 52,
        "tmCharSet @52"
    );
    assert!(core::mem::offset_of!(TextMetricA, _pad) == 53, "pad @53");
};

/// Win64 `TEXTMETRICW` (wingdi.h): 11 `LONG`s @0..43, then 4 `WCHAR`s @44..51,
/// then 5 `BYTE`s @52..56, trailing alignment pad @57..59 — 60 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct TextMetricW {
    pub(crate) height: i32,
    pub(crate) ascent: i32,
    pub(crate) descent: i32,
    pub(crate) internal_leading: i32,
    pub(crate) external_leading: i32,
    pub(crate) avg_char_width: i32,
    pub(crate) max_char_width: i32,
    pub(crate) weight: i32,
    pub(crate) overhang: i32,
    pub(crate) digitized_aspect_x: i32,
    pub(crate) digitized_aspect_y: i32,
    pub(crate) first_char: u16,
    pub(crate) last_char: u16,
    pub(crate) default_char: u16,
    pub(crate) break_char: u16,
    pub(crate) italic: u8,
    pub(crate) underlined: u8,
    pub(crate) struck_out: u8,
    pub(crate) pitch_and_family: u8,
    pub(crate) charset: u8,
    /// Trailing alignment padding (57 payload bytes → 60).
    pub(crate) _pad: [u8; 3],
}

/// Compile-time layout check for [`TextMetricW`].
const _: () = {
    assert!(
        core::mem::size_of::<TextMetricW>() == 60,
        "TEXTMETRICW must be 60 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, height) == 0,
        "tmHeight @0"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, ascent) == 4,
        "tmAscent @4"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, descent) == 8,
        "tmDescent @8"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, internal_leading) == 12,
        "tmInternalLeading @12"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, external_leading) == 16,
        "tmExternalLeading @16"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, avg_char_width) == 20,
        "tmAveCharWidth @20"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, max_char_width) == 24,
        "tmMaxCharWidth @24"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, weight) == 28,
        "tmWeight @28"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, overhang) == 32,
        "tmOverhang @32"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, digitized_aspect_x) == 36,
        "tmDigitizedAspectX @36"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, digitized_aspect_y) == 40,
        "tmDigitizedAspectY @40"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, first_char) == 44,
        "tmFirstChar @44"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, last_char) == 46,
        "tmLastChar @46"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, default_char) == 48,
        "tmDefaultChar @48"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, break_char) == 50,
        "tmBreakChar @50"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, italic) == 52,
        "tmItalic @52"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, underlined) == 53,
        "tmUnderlined @53"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, struck_out) == 54,
        "tmStruckOut @54"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, pitch_and_family) == 55,
        "tmPitchAndFamily @55"
    );
    assert!(
        core::mem::offset_of!(TextMetricW, charset) == 56,
        "tmCharSet @56"
    );
    assert!(core::mem::offset_of!(TextMetricW, _pad) == 57, "pad @57");
};

/// Win64 `LOGFONTA` (wingdi.h): 5 `LONG`s @0..19, 8 `BYTE`s @20..27, then
/// `CHAR lfFaceName[32]` @28 — 60 bytes, align 4. No internal or trailing
/// padding (the byte block fills offsets 20..27 exactly).
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct LogFontA {
    pub(crate) height: i32,
    pub(crate) width: i32,
    pub(crate) escapement: i32,
    pub(crate) orientation: i32,
    pub(crate) weight: i32,
    pub(crate) italic: u8,
    pub(crate) underline: u8,
    pub(crate) strike_out: u8,
    pub(crate) charset: u8,
    pub(crate) out_precision: u8,
    pub(crate) clip_precision: u8,
    pub(crate) quality: u8,
    pub(crate) pitch_and_family: u8,
    pub(crate) face_name: [u8; 32],
}

/// Compile-time layout check for [`LogFontA`].
const _: () = {
    assert!(
        core::mem::size_of::<LogFontA>() == 60,
        "LOGFONTA must be 60 bytes on Win64"
    );
    assert!(core::mem::offset_of!(LogFontA, height) == 0, "lfHeight @0");
    assert!(core::mem::offset_of!(LogFontA, width) == 4, "lfWidth @4");
    assert!(
        core::mem::offset_of!(LogFontA, escapement) == 8,
        "lfEscapement @8"
    );
    assert!(
        core::mem::offset_of!(LogFontA, orientation) == 12,
        "lfOrientation @12"
    );
    assert!(
        core::mem::offset_of!(LogFontA, weight) == 16,
        "lfWeight @16"
    );
    assert!(
        core::mem::offset_of!(LogFontA, italic) == 20,
        "lfItalic @20"
    );
    assert!(
        core::mem::offset_of!(LogFontA, underline) == 21,
        "lfUnderline @21"
    );
    assert!(
        core::mem::offset_of!(LogFontA, strike_out) == 22,
        "lfStrikeOut @22"
    );
    assert!(
        core::mem::offset_of!(LogFontA, charset) == 23,
        "lfCharSet @23"
    );
    assert!(
        core::mem::offset_of!(LogFontA, out_precision) == 24,
        "lfOutPrecision @24"
    );
    assert!(
        core::mem::offset_of!(LogFontA, clip_precision) == 25,
        "lfClipPrecision @25"
    );
    assert!(
        core::mem::offset_of!(LogFontA, quality) == 26,
        "lfQuality @26"
    );
    assert!(
        core::mem::offset_of!(LogFontA, pitch_and_family) == 27,
        "lfPitchAndFamily @27"
    );
    assert!(
        core::mem::offset_of!(LogFontA, face_name) == 28,
        "lfFaceName @28"
    );
};

/// Win64 `LOGFONTW` (wingdi.h): 5 `LONG`s @0..19, 8 `BYTE`s @20..27, then
/// `WCHAR lfFaceName[32]` @28 — 92 bytes, align 4. No padding.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct LogFontW {
    pub(crate) height: i32,
    pub(crate) width: i32,
    pub(crate) escapement: i32,
    pub(crate) orientation: i32,
    pub(crate) weight: i32,
    pub(crate) italic: u8,
    pub(crate) underline: u8,
    pub(crate) strike_out: u8,
    pub(crate) charset: u8,
    pub(crate) out_precision: u8,
    pub(crate) clip_precision: u8,
    pub(crate) quality: u8,
    pub(crate) pitch_and_family: u8,
    pub(crate) face_name: [u16; 32],
}

/// Compile-time layout check for [`LogFontW`].
const _: () = {
    assert!(
        core::mem::size_of::<LogFontW>() == 92,
        "LOGFONTW must be 92 bytes on Win64"
    );
    assert!(core::mem::offset_of!(LogFontW, height) == 0, "lfHeight @0");
    assert!(core::mem::offset_of!(LogFontW, width) == 4, "lfWidth @4");
    assert!(
        core::mem::offset_of!(LogFontW, escapement) == 8,
        "lfEscapement @8"
    );
    assert!(
        core::mem::offset_of!(LogFontW, orientation) == 12,
        "lfOrientation @12"
    );
    assert!(
        core::mem::offset_of!(LogFontW, weight) == 16,
        "lfWeight @16"
    );
    assert!(
        core::mem::offset_of!(LogFontW, italic) == 20,
        "lfItalic @20"
    );
    assert!(
        core::mem::offset_of!(LogFontW, underline) == 21,
        "lfUnderline @21"
    );
    assert!(
        core::mem::offset_of!(LogFontW, strike_out) == 22,
        "lfStrikeOut @22"
    );
    assert!(
        core::mem::offset_of!(LogFontW, charset) == 23,
        "lfCharSet @23"
    );
    assert!(
        core::mem::offset_of!(LogFontW, out_precision) == 24,
        "lfOutPrecision @24"
    );
    assert!(
        core::mem::offset_of!(LogFontW, clip_precision) == 25,
        "lfClipPrecision @25"
    );
    assert!(
        core::mem::offset_of!(LogFontW, quality) == 26,
        "lfQuality @26"
    );
    assert!(
        core::mem::offset_of!(LogFontW, pitch_and_family) == 27,
        "lfPitchAndFamily @27"
    );
    assert!(
        core::mem::offset_of!(LogFontW, face_name) == 28,
        "lfFaceName @28"
    );
};

/// Win64 `RECT` (windef.h): four `LONG`s — 16 bytes, align 4, no padding.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct Rect {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) right: i32,
    pub(crate) bottom: i32,
}

/// Compile-time layout check for [`Rect`].
const _: () = {
    assert!(core::mem::size_of::<Rect>() == 16, "RECT must be 16 bytes");
    assert!(core::mem::offset_of!(Rect, left) == 0, "RECT.left @0");
    assert!(core::mem::offset_of!(Rect, top) == 4, "RECT.top @4");
    assert!(core::mem::offset_of!(Rect, right) == 8, "RECT.right @8");
    assert!(core::mem::offset_of!(Rect, bottom) == 12, "RECT.bottom @12");
};

/// Win64 `SIZE` (windef.h): `LONG cx`, `LONG cy` — 8 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct Size {
    pub(crate) cx: i32,
    pub(crate) cy: i32,
}

/// Compile-time layout check for [`Size`].
const _: () = {
    assert!(core::mem::size_of::<Size>() == 8, "SIZE must be 8 bytes");
    assert!(core::mem::offset_of!(Size, cx) == 0, "SIZE.cx @0");
    assert!(core::mem::offset_of!(Size, cy) == 4, "SIZE.cy @4");
};

/// Win64 `BITMAP` (wingdi.h): 4 `LONG`s @0..15, `WORD bmPlanes` @16,
/// `WORD bmBitsPixel` @18, alignment pad @20..23, `LPVOID bmBits` @24 —
/// 32 bytes, align 8 (the pointer field).
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct Bitmap {
    pub(crate) bm_type: i32,
    pub(crate) bm_width: i32,
    pub(crate) bm_height: i32,
    pub(crate) bm_width_bytes: i32,
    pub(crate) bm_planes: u16,
    pub(crate) bm_bits_pixel: u16,
    /// Alignment padding between the WORD pair and the pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) bm_bits: u64,
}

/// Compile-time layout check for [`Bitmap`].
const _: () = {
    assert!(
        core::mem::size_of::<Bitmap>() == 32,
        "BITMAP must be 32 bytes"
    );
    assert!(core::mem::offset_of!(Bitmap, bm_type) == 0, "bmType @0");
    assert!(core::mem::offset_of!(Bitmap, bm_width) == 4, "bmWidth @4");
    assert!(core::mem::offset_of!(Bitmap, bm_height) == 8, "bmHeight @8");
    assert!(
        core::mem::offset_of!(Bitmap, bm_width_bytes) == 12,
        "bmWidthBytes @12"
    );
    assert!(
        core::mem::offset_of!(Bitmap, bm_planes) == 16,
        "bmPlanes @16"
    );
    assert!(
        core::mem::offset_of!(Bitmap, bm_bits_pixel) == 18,
        "bmBitsPixel @18"
    );
    assert!(core::mem::offset_of!(Bitmap, _pad) == 20, "BITMAP pad @20");
    assert!(core::mem::offset_of!(Bitmap, bm_bits) == 24, "bmBits @24");
};

/// Win64 `BITMAPINFOHEADER` (wingdi.h) — the fixed header of a `BITMAPINFO`:
/// `DWORD biSize` @0, `LONG biWidth` @4, `LONG biHeight` @8, `WORD biPlanes`
/// @12, `WORD biBitCount` @14, `DWORD biCompression` @16, `DWORD biSizeImage`
/// @20, `LONG biXPelsPerMeter` @24, `LONG biYPelsPerMeter` @28, `DWORD
/// biClrUsed` @32, `DWORD biClrImportant` @36 — 40 bytes, align 4, no padding.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct BitmapInfoHeader {
    pub(crate) bi_size: u32,
    pub(crate) bi_width: i32,
    pub(crate) bi_height: i32,
    pub(crate) bi_planes: u16,
    pub(crate) bi_bit_count: u16,
    pub(crate) bi_compression: u32,
    pub(crate) bi_size_image: u32,
    pub(crate) bi_x_pels_per_meter: i32,
    pub(crate) bi_y_pels_per_meter: i32,
    pub(crate) bi_clr_used: u32,
    pub(crate) bi_clr_important: u32,
}

/// Compile-time layout check for [`BitmapInfoHeader`].
const _: () = {
    assert!(
        core::mem::size_of::<BitmapInfoHeader>() == 40,
        "BITMAPINFOHEADER must be 40 bytes"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_size) == 0,
        "biSize @0"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_width) == 4,
        "biWidth @4"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_height) == 8,
        "biHeight @8"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_planes) == 12,
        "biPlanes @12"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_bit_count) == 14,
        "biBitCount @14"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_compression) == 16,
        "biCompression @16"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_size_image) == 20,
        "biSizeImage @20"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_x_pels_per_meter) == 24,
        "biXPelsPerMeter @24"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_y_pels_per_meter) == 28,
        "biYPelsPerMeter @28"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_clr_used) == 32,
        "biClrUsed @32"
    );
    assert!(
        core::mem::offset_of!(BitmapInfoHeader, bi_clr_important) == 36,
        "biClrImportant @36"
    );
};

/// Win64 `ENUMLOGFONTEXW` (wingdi.h): a `LOGFONTW` (92 bytes) followed by
/// `WCHAR elfFullName[LF_FULLFACESIZE=64]`, `WCHAR elfStyle[LF_FACESIZE=32]`
/// and `WCHAR elfScript[LF_FACESIZE=32]` — 348 bytes, align 4, no padding.
///
/// `EnumFontFamiliesExW` passes this to the guest `FONTENUMPROCW` as the
/// `lpelfe` argument; the `LOGFONTW` front is layout-identical to the plain
/// `ENUMLOGFONTW` that `EnumFontFamiliesW`/`EnumFonts` pass, so one payload
/// serves all three enumeration APIs.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct EnumLogFontExW {
    pub(crate) log_font: LogFontW,
    pub(crate) full_name: [u16; 64],
    pub(crate) style: [u16; 32],
    pub(crate) script: [u16; 32],
}

/// Compile-time layout check for [`EnumLogFontExW`].
const _: () = {
    assert!(
        core::mem::size_of::<EnumLogFontExW>() == 348,
        "ENUMLOGFONTEXW must be 348 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(EnumLogFontExW, log_font) == 0,
        "elfLogFont @0"
    );
    assert!(
        core::mem::offset_of!(EnumLogFontExW, full_name) == 92,
        "elfFullName @92"
    );
    assert!(
        core::mem::offset_of!(EnumLogFontExW, style) == 220,
        "elfStyle @220"
    );
    assert!(
        core::mem::offset_of!(EnumLogFontExW, script) == 284,
        "elfScript @284"
    );
};

/// Win64 `ENUMLOGFONTEXA` (wingdi.h): a `LOGFONTA` (60 bytes) followed by
/// `CHAR elfFullName[64]`, `CHAR elfStyle[32]`, `CHAR elfScript[32]` — 188
/// bytes, align 4, no padding.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct EnumLogFontExA {
    pub(crate) log_font: LogFontA,
    pub(crate) full_name: [u8; 64],
    pub(crate) style: [u8; 32],
    pub(crate) script: [u8; 32],
}

/// Compile-time layout check for [`EnumLogFontExA`].
const _: () = {
    assert!(
        core::mem::size_of::<EnumLogFontExA>() == 188,
        "ENUMLOGFONTEXA must be 188 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(EnumLogFontExA, log_font) == 0,
        "elfLogFont @0"
    );
    assert!(
        core::mem::offset_of!(EnumLogFontExA, full_name) == 60,
        "elfFullName @60"
    );
    assert!(
        core::mem::offset_of!(EnumLogFontExA, style) == 124,
        "elfStyle @124"
    );
    assert!(
        core::mem::offset_of!(EnumLogFontExA, script) == 156,
        "elfScript @156"
    );
};

/// Win64 `GLYPHMETRICS` (wingdi.h): `UINT gmBlackBoxX`, `UINT gmBlackBoxY`,
/// `POINT gmptGlyphOrigin` (two `LONG`s), `short gmCellIncX`, `short
/// gmCellIncY` — 20 bytes, align 4, no padding.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct GlyphMetrics {
    pub(crate) black_box_x: u32,
    pub(crate) black_box_y: u32,
    pub(crate) glyph_origin_x: i32,
    pub(crate) glyph_origin_y: i32,
    pub(crate) cell_inc_x: i16,
    pub(crate) cell_inc_y: i16,
}

/// Compile-time layout check for [`GlyphMetrics`].
const _: () = {
    assert!(
        core::mem::size_of::<GlyphMetrics>() == 20,
        "GLYPHMETRICS must be 20 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(GlyphMetrics, black_box_x) == 0,
        "gmBlackBoxX @0"
    );
    assert!(
        core::mem::offset_of!(GlyphMetrics, glyph_origin_x) == 8,
        "gmptGlyphOrigin.x @8"
    );
    assert!(
        core::mem::offset_of!(GlyphMetrics, cell_inc_x) == 16,
        "gmCellIncX @16"
    );
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::guest_memory::{with_typed_read, with_typed_write};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    /// Minimal engine with mapped guest memory for view round-trips.
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu
    }

    /// Read the raw guest bytes at `va` (bypasses the typed views).
    fn raw_bytes(engine: &mut IcedCpu, va: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0_u8; len];
        engine.mem_read(va, &mut bytes).expect("read raw bytes");
        bytes
    }
    // ── gdi32 lane: TEXTMETRIC / LOGFONT / RECT / SIZE / BITMAP ─────────

    const TM_A_VA: u64 = 0x6000;
    const TM_W_VA: u64 = 0x6100;
    const LF_A_VA: u64 = 0x6200;
    const LF_W_VA: u64 = 0x6300;

    #[test]
    fn text_metric_a_write_places_flags_at_44_through_52() {
        let mut engine = test_engine();
        with_typed_write::<TextMetricA, _, _>(&mut engine, TM_A_VA, |tm| {
            tm.height = 16;
            tm.ascent = 12;
            tm.descent = 4;
            tm.weight = 700;
            tm.italic = 1;
            tm.charset = 1;
            Ok(())
        })
        .expect("typed TEXTMETRICA write");
        let bytes = raw_bytes(&mut engine, TM_A_VA, 56);
        assert_eq!(&bytes[0..4], &16_i32.to_le_bytes(), "tmHeight");
        assert_eq!(&bytes[4..8], &12_i32.to_le_bytes(), "tmAscent");
        assert_eq!(&bytes[8..12], &4_i32.to_le_bytes(), "tmDescent");
        assert_eq!(&bytes[28..32], &700_i32.to_le_bytes(), "tmWeight");
        assert_eq!(bytes[48], 1, "tmItalic @48");
        assert_eq!(bytes[52], 1, "tmCharSet @52");
        assert_eq!(bytes[53], 0, "trailing pad @53 zeroed");
        assert_eq!(&bytes[44..48], &[0; 4], "A char fields are BYTEs @44..47");
    }

    #[test]
    fn text_metric_w_write_places_wchars_at_44_and_flags_at_52() {
        let mut engine = test_engine();
        with_typed_write::<TextMetricW, _, _>(&mut engine, TM_W_VA, |tm| {
            tm.height = 16;
            tm.first_char = 0x41;
            tm.italic = 1;
            tm.pitch_and_family = 1;
            tm.charset = 0;
            Ok(())
        })
        .expect("typed TEXTMETRICW write");
        let bytes = raw_bytes(&mut engine, TM_W_VA, 60);
        assert_eq!(&bytes[0..4], &16_i32.to_le_bytes(), "tmHeight");
        assert_eq!(&bytes[44..46], &0x41_u16.to_le_bytes(), "tmFirstChar WCHAR");
        assert_eq!(&bytes[46..52], &[0; 6], "remaining WCHARs zero");
        assert_eq!(bytes[52], 1, "tmItalic @52");
        assert_eq!(bytes[55], 1, "tmPitchAndFamily @55");
        assert_eq!(bytes[56], 0, "tmCharSet @56");
        assert_eq!(&bytes[57..60], &[0; 3], "trailing pad @57..59 zeroed");
    }

    #[test]
    fn text_metric_w_variant_is_four_bytes_larger_than_a() {
        // The SDK verification: TEXTMETRICW (60) vs TEXTMETRICA (56) — the W
        // character fields widen to WCHAR, shifting the flag bytes.
        assert_eq!(core::mem::size_of::<TextMetricA>(), 56);
        assert_eq!(core::mem::size_of::<TextMetricW>(), 60);
    }

    #[test]
    fn log_font_a_read_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let mut bytes = vec![0_u8; 60];
        bytes[0..4].copy_from_slice(&(-13_i32).to_le_bytes()); // lfHeight
        bytes[16..20].copy_from_slice(&700_i32.to_le_bytes()); // lfWeight
        bytes[20] = 1; // lfItalic
        bytes[23] = 1; // lfCharSet
        bytes[27] = 0x01; // lfPitchAndFamily (FIXED_PITCH)
        bytes[28..40].copy_from_slice(b"Courier New\0");
        engine
            .mem_write(LF_A_VA, &bytes)
            .expect("write raw LOGFONTA");
        with_typed_read::<LogFontA, _, _>(&mut engine, LF_A_VA, |lf| {
            assert_eq!(lf.height, -13);
            assert_eq!(lf.weight, 700);
            assert_eq!(lf.italic, 1);
            assert_eq!(lf.charset, 1);
            assert_eq!(lf.pitch_and_family, 0x01);
            assert_eq!(&lf.face_name[..11], b"Courier New", "face name bytes");
            Ok(())
        })
        .expect("typed LOGFONTA read");
    }

    #[test]
    fn log_font_w_read_preserves_utf16_face_name() {
        let mut engine = test_engine();
        let mut bytes = vec![0_u8; 92];
        bytes[0..4].copy_from_slice(&(-16_i32).to_le_bytes()); // lfHeight
        bytes[16..20].copy_from_slice(&400_i32.to_le_bytes()); // lfWeight
        // "Segoe" as UTF-16LE = 10 bytes, plus a NUL unit.
        bytes[28..38].copy_from_slice(b"S\0e\0g\0o\0e\0");
        bytes[38..40].copy_from_slice(&0_u16.to_le_bytes());
        engine
            .mem_write(LF_W_VA, &bytes)
            .expect("write raw LOGFONTW");
        with_typed_read::<LogFontW, _, _>(&mut engine, LF_W_VA, |lf| {
            assert_eq!(lf.height, -16);
            assert_eq!(lf.weight, 400);
            assert_eq!(lf.face_name[0], u16::from(b'S'));
            assert_eq!(lf.face_name[3], u16::from(b'o'));
            assert_eq!(lf.face_name[5], 0, "NUL terminator unit");
            Ok(())
        })
        .expect("typed LOGFONTW read");
    }

    #[test]
    fn rect_and_size_round_trip_through_views() {
        let mut engine = test_engine();
        with_typed_write::<Rect, _, _>(&mut engine, 0x6400, |rect| {
            rect.left = -5;
            rect.top = 2;
            rect.right = 100;
            rect.bottom = 50;
            Ok(())
        })
        .expect("typed RECT write");
        with_typed_read::<Rect, _, _>(&mut engine, 0x6400, |rect| {
            assert_eq!(rect.left, -5);
            assert_eq!(rect.top, 2);
            assert_eq!(rect.right, 100);
            assert_eq!(rect.bottom, 50);
            Ok(())
        })
        .expect("typed RECT read");

        with_typed_write::<Size, _, _>(&mut engine, 0x6500, |size| {
            size.cx = 640;
            size.cy = 480;
            Ok(())
        })
        .expect("typed SIZE write");
        let bytes = raw_bytes(&mut engine, 0x6500, 8);
        assert_eq!(&bytes[0..4], &640_i32.to_le_bytes(), "SIZE.cx");
        assert_eq!(&bytes[4..8], &480_i32.to_le_bytes(), "SIZE.cy");
    }

    #[test]
    fn bitmap_write_zero_fills_the_word_pad_and_keeps_pointer_offset() {
        let mut engine = test_engine();
        with_typed_write::<Bitmap, _, _>(&mut engine, 0x6600, |bitmap| {
            bitmap.bm_width = 16;
            bitmap.bm_height = 16;
            bitmap.bm_width_bytes = 64;
            bitmap.bm_planes = 1;
            bitmap.bm_bits_pixel = 32;
            Ok(())
        })
        .expect("typed BITMAP write");
        let bytes = raw_bytes(&mut engine, 0x6600, 32);
        assert_eq!(&bytes[0..4], &0_i32.to_le_bytes(), "bmType");
        assert_eq!(&bytes[4..8], &16_i32.to_le_bytes(), "bmWidth");
        assert_eq!(&bytes[16..18], &1_u16.to_le_bytes(), "bmPlanes");
        assert_eq!(&bytes[18..20], &32_u16.to_le_bytes(), "bmBitsPixel");
        assert_eq!(&bytes[20..24], &[0; 4], "padding @20..23 zeroed");
        assert_eq!(&bytes[24..32], &0_u64.to_le_bytes(), "bmBits NULL");
    }

    #[test]
    fn bitmap_info_header_read_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let mut bytes = vec![0_u8; 40];
        bytes[0..4].copy_from_slice(&40_u32.to_le_bytes()); // biSize
        bytes[4..8].copy_from_slice(&320_i32.to_le_bytes()); // biWidth
        bytes[8..12].copy_from_slice(&(-200_i32).to_le_bytes()); // biHeight (top-down)
        bytes[14..16].copy_from_slice(&32_u16.to_le_bytes()); // biBitCount
        engine
            .mem_write(0x6700, &bytes)
            .expect("write raw BITMAPINFOHEADER");
        with_typed_read::<BitmapInfoHeader, _, _>(&mut engine, 0x6700, |header| {
            assert_eq!(header.bi_size, 40);
            assert_eq!(header.bi_width, 320);
            assert_eq!(header.bi_height, -200);
            assert_eq!(header.bi_bit_count, 32);
            assert_eq!(header.bi_compression, 0);
            Ok(())
        })
        .expect("typed BITMAPINFOHEADER read");
    }
}
