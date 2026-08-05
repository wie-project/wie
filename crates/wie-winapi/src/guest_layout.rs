//! Win64 struct layouts that cross the guest boundary, verified at compile
//! time against mingw-verified constants.
//!
//! Every struct here derives zerocopy's [`KnownLayout`], [`Immutable`],
//! [`FromBytes`], and [`IntoBytes`] on a `#[repr(C)]` definition. Deliberately
//! **not** [`Unaligned`]: `u16`/`u32`/`u64`/`usize` fields are not `Unaligned`,
//! so no Win32 struct can derive it. Alignment is instead resolved at runtime
//! by the staging fallback in `crate::guest_memory` (an odd guest VA stages
//! into an aligned host buffer instead of faulting).
//!
//! [`FromBytes`] permits padding gaps — the Win64 `MSG` has one at offset 12 —
//! which is exactly what these layouts need (bytemuck-style `Pod` would reject
//! the type outright).

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Win64 `MSG` (winuser.h): `HWND hwnd` @0, `UINT message` @8, [pad @12],
/// `WPARAM wParam` @16, `LPARAM lParam` @24, `DWORD time` @32, `POINT pt` @36
/// (`x` @36, `y` @40), `DWORD lPrivate` @44 — 48 bytes, align 8.
///
/// The `_pad` field is explicit because zerocopy 0.8's `IntoBytes` derive
/// rejects implicit padding; the zero-fill write path keeps it zeroed, exactly
/// as the old per-field handler cleared it.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct Msg {
    pub(crate) hwnd: u64,
    pub(crate) message: u32,
    /// Win64 alignment padding between `message` and `wParam`.
    pub(crate) _pad: [u8; 4],
    pub(crate) wparam: u64,
    pub(crate) lparam: u64,
    pub(crate) time: u32,
    pub(crate) pt_x: i32,
    pub(crate) pt_y: i32,
    /// Private field (winuser.h `lPrivate`); zero-filled like the padding.
    pub(crate) l_private: u32,
}

/// Compile-time layout check for [`Msg`]: any field reorder or wrong width
/// breaks the build instead of corrupting guest memory at runtime.
const _: () = {
    assert!(
        core::mem::size_of::<Msg>() == 48,
        "MSG must be 48 bytes on Win64"
    );
    assert!(core::mem::offset_of!(Msg, hwnd) == 0, "MSG.hwnd @0");
    assert!(core::mem::offset_of!(Msg, message) == 8, "MSG.message @8");
    assert!(core::mem::offset_of!(Msg, _pad) == 12, "MSG padding @12");
    assert!(core::mem::offset_of!(Msg, wparam) == 16, "MSG.wParam @16");
    assert!(core::mem::offset_of!(Msg, lparam) == 24, "MSG.lParam @24");
    assert!(core::mem::offset_of!(Msg, time) == 32, "MSG.time @32");
    assert!(core::mem::offset_of!(Msg, pt_x) == 36, "MSG.pt.x @36");
    assert!(core::mem::offset_of!(Msg, pt_y) == 40, "MSG.pt.y @40");
    assert!(
        core::mem::offset_of!(Msg, l_private) == 44,
        "MSG.lPrivate @44"
    );
};

/// Win64 `WINDOWPLACEMENT` (winuser.h, Vista+ — `rcDevice` was removed):
/// `UINT length` @0, `UINT flags` @4, `UINT showCmd` @8, `POINT ptMinPosition`
/// @12 (`x` @12, `y` @16), `POINT ptMaxPosition` @20 (`x` @20, `y` @24), `RECT
/// rcNormalPosition` @28 (`left` @28, `top` @32, `right` @36, `bottom` @40) —
/// 44 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WindowPlacement {
    pub(crate) length: u32,
    pub(crate) flags: u32,
    pub(crate) show_cmd: u32,
    pub(crate) pt_min_x: i32,
    pub(crate) pt_min_y: i32,
    pub(crate) pt_max_x: i32,
    pub(crate) pt_max_y: i32,
    pub(crate) rc_left: i32,
    pub(crate) rc_top: i32,
    pub(crate) rc_right: i32,
    pub(crate) rc_bottom: i32,
}

/// Compile-time layout check for [`WindowPlacement`]. The 44-byte size matches
/// `user32::window::geom::WINDOWPLACEMENT_LENGTH`, which both placement
/// handlers agree on.
const _: () = {
    assert!(
        core::mem::size_of::<WindowPlacement>() == 44,
        "WINDOWPLACEMENT must be 44 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, length) == 0,
        "length @0"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, flags) == 4,
        "flags @4"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, show_cmd) == 8,
        "showCmd @8"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, pt_min_x) == 12,
        "ptMinPosition.x @12"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, pt_min_y) == 16,
        "ptMinPosition.y @16"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, pt_max_x) == 20,
        "ptMaxPosition.x @20"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, pt_max_y) == 24,
        "ptMaxPosition.y @24"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, rc_left) == 28,
        "rcNormalPosition.left @28"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, rc_top) == 32,
        "rcNormalPosition.top @32"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, rc_right) == 36,
        "rcNormalPosition.right @36"
    );
    assert!(
        core::mem::offset_of!(WindowPlacement, rc_bottom) == 40,
        "rcNormalPosition.bottom @40"
    );
};

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

// --- Z1 print lane: DOCINFO / CHOOSEFONT / PRINTDLG / DEVMODE / DEVNAMES /
// --- PAGESETUP -----------------------------------------------------------
//

// `LogFontW` above (the gdi32 lane) is the shared LOGFONTW: the ChooseFontW
// write-back (comdlg32.rs) reads/writes it through the typed views, so the
// dialog-owned fields (`height`, `underline`, `strike_out`, `face_name`)
// change and every other field is preserved by the whole-struct restore.

/// Win64 `DOCINFOW` (wingdi.h): `int cbSize` @0, [pad @4], `LPCWSTR
/// lpszDocName` @8, `LPCWSTR lpszOutput` @16, `LPCWSTR lpszDatatype` @24,
/// `DWORD fwType` @32, [trailing pad @36] — 40 bytes, align 8.
///
/// The explicit `_pad`/`_pad_tail` fields exist because zerocopy 0.8's
/// `IntoBytes` derive rejects implicit padding (interior AND trailing); the
/// zero-fill write path keeps them zeroed like the old per-field handler did.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DocInfoW {
    pub(crate) cb_size: i32,
    /// Win64 alignment padding between `cbSize` and the first pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) lpsz_doc_name: u64,
    pub(crate) lpsz_output: u64,
    pub(crate) lpsz_datatype: u64,
    pub(crate) fw_type: u32,
    /// Trailing padding to the struct's 8-byte alignment.
    pub(crate) _pad_tail: [u8; 4],
}

/// Compile-time layout check for [`DocInfoW`].
const _: () = {
    assert!(
        core::mem::size_of::<DocInfoW>() == 40,
        "DOCINFOW must be 40 bytes on Win64"
    );
    assert!(core::mem::offset_of!(DocInfoW, cb_size) == 0, "cbSize @0");
    assert!(core::mem::offset_of!(DocInfoW, _pad) == 4, "DOCINFO pad @4");
    assert!(
        core::mem::offset_of!(DocInfoW, lpsz_doc_name) == 8,
        "lpszDocName @8"
    );
    assert!(
        core::mem::offset_of!(DocInfoW, lpsz_output) == 16,
        "lpszOutput @16"
    );
    assert!(
        core::mem::offset_of!(DocInfoW, lpsz_datatype) == 24,
        "lpszDatatype @24"
    );
    assert!(core::mem::offset_of!(DocInfoW, fw_type) == 32, "fwType @32");
    assert!(
        core::mem::offset_of!(DocInfoW, _pad_tail) == 36,
        "DOCINFO trailing pad @36"
    );
};

/// Win64 `CHOOSEFONTW` (commdlg.h): `DWORD lStructSize` @0, [pad @4], `HWND
/// hwndOwner` @8, `HDC hDC` @16, `LPLOGFONTW lpLogFont` @24, `INT iPointSize`
/// @32, `DWORD Flags` @36, `COLORREF rgbColors` @40, [pad @44], `LPARAM
/// lCustData` @48, `LPCFHOOKPROC lpfnHook` @56, `LPCWSTR lpTemplateName` @64,
/// `HINSTANCE hInstance` @72, `LPWSTR lpszStyle` @80, `WORD nFontType` @88,
/// [SDK alignment WORD @90], `INT nSizeMin` @92, `INT nSizeMax` @96,
/// [trailing pad @100] — 104 bytes, align 8.
///
/// The `_alignment_pad` word is the Windows SDK's `___MISSING_ALIGNMENT__`
/// member (real layout, not a mingw quirk). The `_pad`/`_pad_tail` fields
/// follow the `IntoBytes` explicit-padding rule; the ChooseFontW write-back
/// restores the whole struct from a read view, so the pads carry the guest's
/// original bytes either way.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct ChooseFontW {
    pub(crate) l_struct_size: u32,
    /// Win64 alignment padding before the first pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) hwnd_owner: u64,
    pub(crate) h_dc: u64,
    pub(crate) lp_log_font: u64,
    pub(crate) i_point_size: i32,
    pub(crate) flags: u32,
    pub(crate) rgb_colors: u32,
    /// Win64 alignment padding before `lCustData`.
    pub(crate) _pad2: [u8; 4],
    pub(crate) l_cust_data: u64,
    pub(crate) lpfn_hook: u64,
    pub(crate) lp_template_name: u64,
    pub(crate) h_instance: u64,
    pub(crate) lpsz_style: u64,
    pub(crate) n_font_type: u16,
    /// The SDK's `___MISSING_ALIGNMENT__` word (commdlg.h) — real padding.
    pub(crate) _alignment_pad: u16,
    pub(crate) n_size_min: i32,
    pub(crate) n_size_max: i32,
    /// Trailing padding to the struct's 8-byte alignment.
    pub(crate) _pad_tail: [u8; 4],
}

/// Compile-time layout check for [`ChooseFontW`].
const _: () = {
    assert!(
        core::mem::size_of::<ChooseFontW>() == 104,
        "CHOOSEFONTW must be 104 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, l_struct_size) == 0,
        "lStructSize @0"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, _pad) == 4,
        "CHOOSEFONT pad @4"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, hwnd_owner) == 8,
        "hwndOwner @8"
    );
    assert!(core::mem::offset_of!(ChooseFontW, h_dc) == 16, "hDC @16");
    assert!(
        core::mem::offset_of!(ChooseFontW, lp_log_font) == 24,
        "lpLogFont @24"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, i_point_size) == 32,
        "iPointSize @32"
    );
    assert!(core::mem::offset_of!(ChooseFontW, flags) == 36, "Flags @36");
    assert!(
        core::mem::offset_of!(ChooseFontW, rgb_colors) == 40,
        "rgbColors @40"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, _pad2) == 44,
        "CHOOSEFONT pad @44"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, l_cust_data) == 48,
        "lCustData @48"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, lpfn_hook) == 56,
        "lpfnHook @56"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, lp_template_name) == 64,
        "lpTemplateName @64"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, h_instance) == 72,
        "hInstance @72"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, lpsz_style) == 80,
        "lpszStyle @80"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, n_font_type) == 88,
        "nFontType @88"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, _alignment_pad) == 90,
        "___MISSING_ALIGNMENT__ @90"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, n_size_min) == 92,
        "nSizeMin @92"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, n_size_max) == 96,
        "nSizeMax @96"
    );
    assert!(
        core::mem::offset_of!(ChooseFontW, _pad_tail) == 100,
        "CHOOSEFONT trailing pad @100"
    );
};

/// Win64 `PRINTDLGW` (commdlg.h): `DWORD lStructSize` @0, [pad @4], `HWND
/// hwndOwner` @8, `HGLOBAL hDevMode` @16, `HGLOBAL hDevNames` @24, `HDC hDC`
/// @32, `DWORD Flags` @40, `WORD nFromPage` @44, `nToPage` @46, `nMinPage`
/// @48, `nMaxPage` @50, `nCopies` @52, [pad @54], `HINSTANCE hInstance` @56,
/// `LPARAM lCustData` @64, `LPPRINTHOOKPROC lpfnPrintHook` @72,
/// `LPSETUPHOOKPROC lpfnSetupHook` @80, `LPCWSTR lpPrintTemplateName` @88,
/// `lpSetupTemplateName` @96, `HGLOBAL hPrintTemplate` @104,
/// `hSetupTemplate` @112 — 120 bytes, align 8.
///
/// NOTE: this is the mingw-verified size (120); the L6 design brief's "176"
/// was miscounted. The const table pins what the cross-compiler reports, so
/// a drift in either direction breaks the build.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct PrintDlgW {
    pub(crate) l_struct_size: u32,
    /// Win64 alignment padding before the first pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) hwnd_owner: u64,
    pub(crate) h_dev_mode: u64,
    pub(crate) h_dev_names: u64,
    pub(crate) h_dc: u64,
    pub(crate) flags: u32,
    pub(crate) n_from_page: u16,
    pub(crate) n_to_page: u16,
    pub(crate) n_min_page: u16,
    pub(crate) n_max_page: u16,
    pub(crate) n_copies: u16,
    /// Win64 alignment padding before `hInstance`.
    pub(crate) _pad2: [u8; 2],
    pub(crate) h_instance: u64,
    pub(crate) l_cust_data: u64,
    pub(crate) lpfn_print_hook: u64,
    pub(crate) lpfn_setup_hook: u64,
    pub(crate) lp_print_template_name: u64,
    pub(crate) lp_setup_template_name: u64,
    pub(crate) h_print_template: u64,
    pub(crate) h_setup_template: u64,
}

/// Compile-time layout check for [`PrintDlgW`].
const _: () = {
    assert!(
        core::mem::size_of::<PrintDlgW>() == 120,
        "PRINTDLGW must be 120 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, l_struct_size) == 0,
        "lStructSize @0"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, _pad) == 4,
        "PRINTDLG pad @4"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, hwnd_owner) == 8,
        "hwndOwner @8"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, h_dev_mode) == 16,
        "hDevMode @16"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, h_dev_names) == 24,
        "hDevNames @24"
    );
    assert!(core::mem::offset_of!(PrintDlgW, h_dc) == 32, "hDC @32");
    assert!(core::mem::offset_of!(PrintDlgW, flags) == 40, "Flags @40");
    assert!(
        core::mem::offset_of!(PrintDlgW, n_from_page) == 44,
        "nFromPage @44"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, n_to_page) == 46,
        "nToPage @46"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, n_min_page) == 48,
        "nMinPage @48"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, n_max_page) == 50,
        "nMaxPage @50"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, n_copies) == 52,
        "nCopies @52"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, _pad2) == 54,
        "PRINTDLG pad @54"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, h_instance) == 56,
        "hInstance @56"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, l_cust_data) == 64,
        "lCustData @64"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, lpfn_print_hook) == 72,
        "lpfnPrintHook @72"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, lpfn_setup_hook) == 80,
        "lpfnSetupHook @80"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, lp_print_template_name) == 88,
        "lpPrintTemplateName @88"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, lp_setup_template_name) == 96,
        "lpSetupTemplateName @96"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, h_print_template) == 104,
        "hPrintTemplate @104"
    );
    assert!(
        core::mem::offset_of!(PrintDlgW, h_setup_template) == 112,
        "hSetupTemplate @112"
    );
};

/// Win64 `DEVMODEW` (wingdi.h): `WCHAR dmDeviceName[32]` @0, `WORD
/// dmSpecVersion` @64, `dmDriverVersion` @66, `dmSize` @68, `dmDriverExtra`
/// @70, `DWORD dmFields` @72, then the printer branch of the anonymous union
/// (`SHORT dmOrientation` @76, `dmPaperSize` @78, `dmPaperLength` @80,
/// `dmPaperWidth` @82, `dmScale` @84, `dmCopies` @86, `dmDefaultSource` @88,
/// `dmPrintQuality` @90), `dmColor` @92, `dmDuplex` @94, `dmYResolution` @96,
/// `dmTTOption` @98, `dmCollate` @100, `WCHAR dmFormName[32]` @102, `WORD
/// dmLogPixels` @166, `DWORD dmBitsPerPel` @168, `dmPelsWidth` @172,
/// `dmPelsHeight` @176, `dmDisplayFlags`/`dmNup` @180, `dmDisplayFrequency`
/// @184, `dmICMMethod` @188, `dmICMIntent` @192, `dmMediaType` @196,
/// `dmDitherType` @200, `dmReserved1` @204, `dmReserved2` @208,
/// `dmPanningWidth` @212, `dmPanningHeight` @216 — 220 bytes, align 4, no
/// padding.
///
/// The anonymous union is modelled with its PRINTER branch (the display
/// branch — `dmPosition`/`dmDisplayOrientation`/`dmDisplayFixedOutput` —
/// shares the same 16 bytes @76 and is never read by the print dialogs).
/// Every field is `u16`/`i16`/`u32`/`i32` and every offset is naturally
/// aligned, so no explicit `_pad` is needed and the `IntoBytes` derive is
/// clean.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DevModeW {
    pub(crate) dm_device_name: [u16; 32],
    pub(crate) dm_spec_version: u16,
    pub(crate) dm_driver_version: u16,
    pub(crate) dm_size: u16,
    pub(crate) dm_driver_extra: u16,
    pub(crate) dm_fields: u32,
    pub(crate) dm_orientation: i16,
    pub(crate) dm_paper_size: i16,
    pub(crate) dm_paper_length: i16,
    pub(crate) dm_paper_width: i16,
    pub(crate) dm_scale: i16,
    pub(crate) dm_copies: i16,
    pub(crate) dm_default_source: i16,
    pub(crate) dm_print_quality: i16,
    pub(crate) dm_color: i16,
    pub(crate) dm_duplex: i16,
    pub(crate) dm_y_resolution: i16,
    pub(crate) dm_ttoption: i16,
    pub(crate) dm_collate: i16,
    pub(crate) dm_form_name: [u16; 32],
    pub(crate) dm_log_pixels: u16,
    pub(crate) dm_bits_per_pel: u32,
    pub(crate) dm_pels_width: u32,
    pub(crate) dm_pels_height: u32,
    pub(crate) dm_display_flags: u32,
    pub(crate) dm_display_frequency: u32,
    pub(crate) dm_icm_method: u32,
    pub(crate) dm_icm_intent: u32,
    pub(crate) dm_media_type: u32,
    pub(crate) dm_dither_type: u32,
    pub(crate) dm_reserved1: u32,
    pub(crate) dm_reserved2: u32,
    pub(crate) dm_panning_width: u32,
    pub(crate) dm_panning_height: u32,
}

/// Compile-time layout check for [`DevModeW`].
const _: () = {
    assert!(
        core::mem::size_of::<DevModeW>() == 220,
        "DEVMODEW must be 220 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_device_name) == 0,
        "dmDeviceName @0"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_spec_version) == 64,
        "dmSpecVersion @64"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_driver_version) == 66,
        "dmDriverVersion @66"
    );
    assert!(core::mem::offset_of!(DevModeW, dm_size) == 68, "dmSize @68");
    assert!(
        core::mem::offset_of!(DevModeW, dm_driver_extra) == 70,
        "dmDriverExtra @70"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_fields) == 72,
        "dmFields @72"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_orientation) == 76,
        "dmOrientation @76"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_paper_size) == 78,
        "dmPaperSize @78"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_paper_length) == 80,
        "dmPaperLength @80"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_paper_width) == 82,
        "dmPaperWidth @82"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_scale) == 84,
        "dmScale @84"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_copies) == 86,
        "dmCopies @86"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_default_source) == 88,
        "dmDefaultSource @88"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_print_quality) == 90,
        "dmPrintQuality @90"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_color) == 92,
        "dmColor @92"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_duplex) == 94,
        "dmDuplex @94"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_y_resolution) == 96,
        "dmYResolution @96"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_ttoption) == 98,
        "dmTTOption @98"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_collate) == 100,
        "dmCollate @100"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_form_name) == 102,
        "dmFormName @102"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_log_pixels) == 166,
        "dmLogPixels @166"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_bits_per_pel) == 168,
        "dmBitsPerPel @168"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_pels_width) == 172,
        "dmPelsWidth @172"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_pels_height) == 176,
        "dmPelsHeight @176"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_display_flags) == 180,
        "dmDisplayFlags @180"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_display_frequency) == 184,
        "dmDisplayFrequency @184"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_icm_method) == 188,
        "dmICMMethod @188"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_icm_intent) == 192,
        "dmICMIntent @192"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_media_type) == 196,
        "dmMediaType @196"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_dither_type) == 200,
        "dmDitherType @200"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_reserved1) == 204,
        "dmReserved1 @204"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_reserved2) == 208,
        "dmReserved2 @208"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_panning_width) == 212,
        "dmPanningWidth @212"
    );
    assert!(
        core::mem::offset_of!(DevModeW, dm_panning_height) == 216,
        "dmPanningHeight @216"
    );
};

/// Win64 `DEVNAMES` (commdlg.h): `WORD wDriverOffset` @0, `wDeviceOffset` @2,
/// `wOutputOffset` @4, `wDefault` @6 — 8 bytes, align 2, no padding.
///
/// The driver/device/output strings follow the 8-byte header (the offsets are
/// relative to the start of the struct); P2 resolves them via the guest string
/// readers once it owns the handlers.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct DevNames {
    pub(crate) w_driver_offset: u16,
    pub(crate) w_device_offset: u16,
    pub(crate) w_output_offset: u16,
    pub(crate) w_default: u16,
}

/// Compile-time layout check for [`DevNames`].
const _: () = {
    assert!(
        core::mem::size_of::<DevNames>() == 8,
        "DEVNAMES must be 8 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(DevNames, w_driver_offset) == 0,
        "wDriverOffset @0"
    );
    assert!(
        core::mem::offset_of!(DevNames, w_device_offset) == 2,
        "wDeviceOffset @2"
    );
    assert!(
        core::mem::offset_of!(DevNames, w_output_offset) == 4,
        "wOutputOffset @4"
    );
    assert!(
        core::mem::offset_of!(DevNames, w_default) == 6,
        "wDefault @6"
    );
};

/// Win64 `PAGESETUPDLGW` (commdlg.h): `DWORD lStructSize` @0, [pad @4], `HWND
/// hwndOwner` @8, `HGLOBAL hDevMode` @16, `hDevNames` @24, `DWORD Flags` @32,
/// `POINT ptPaperSize` @36 (`x` @36, `y` @40), `RECT rtMinMargin` @44
/// (`left`/`top`/`right`/`bottom` @44..56), `RECT rtMargin` @60
/// (`left`/`top`/`right`/`bottom` @60..72), [pad @76], `HINSTANCE hInstance`
/// @80, `LPARAM lCustData` @88, `LPPAGESETUPHOOK lpfnPageSetupHook` @96,
/// `LPPAGEPAINTHOOK lpfnPagePaintHook` @104, `LPCWSTR lpPageSetupTemplateName`
/// @112, `HGLOBAL hPageSetupTemplate` @120 — 128 bytes, align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct PageSetupDlgW {
    pub(crate) l_struct_size: u32,
    /// Win64 alignment padding before the first pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) hwnd_owner: u64,
    pub(crate) h_dev_mode: u64,
    pub(crate) h_dev_names: u64,
    pub(crate) flags: u32,
    pub(crate) pt_paper_size_x: i32,
    pub(crate) pt_paper_size_y: i32,
    pub(crate) rt_min_margin_left: i32,
    pub(crate) rt_min_margin_top: i32,
    pub(crate) rt_min_margin_right: i32,
    pub(crate) rt_min_margin_bottom: i32,
    pub(crate) rt_margin_left: i32,
    pub(crate) rt_margin_top: i32,
    pub(crate) rt_margin_right: i32,
    pub(crate) rt_margin_bottom: i32,
    /// Win64 alignment padding before `hInstance`.
    pub(crate) _pad2: [u8; 4],
    pub(crate) h_instance: u64,
    pub(crate) l_cust_data: u64,
    pub(crate) lpfn_page_setup_hook: u64,
    pub(crate) lpfn_page_paint_hook: u64,
    pub(crate) lp_page_setup_template_name: u64,
    pub(crate) h_page_setup_template: u64,
}

/// Compile-time layout check for [`PageSetupDlgW`].
const _: () = {
    assert!(
        core::mem::size_of::<PageSetupDlgW>() == 128,
        "PAGESETUPDLGW must be 128 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, l_struct_size) == 0,
        "lStructSize @0"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, _pad) == 4,
        "PAGESETUP pad @4"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, hwnd_owner) == 8,
        "hwndOwner @8"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, h_dev_mode) == 16,
        "hDevMode @16"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, h_dev_names) == 24,
        "hDevNames @24"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, flags) == 32,
        "Flags @32"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, pt_paper_size_x) == 36,
        "ptPaperSize.x @36"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, pt_paper_size_y) == 40,
        "ptPaperSize.y @40"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, rt_min_margin_left) == 44,
        "rtMinMargin.left @44"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, rt_min_margin_top) == 48,
        "rtMinMargin.top @48"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, rt_min_margin_right) == 52,
        "rtMinMargin.right @52"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, rt_min_margin_bottom) == 56,
        "rtMinMargin.bottom @56"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, rt_margin_left) == 60,
        "rtMargin.left @60"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, rt_margin_top) == 64,
        "rtMargin.top @64"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, rt_margin_right) == 68,
        "rtMargin.right @68"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, rt_margin_bottom) == 72,
        "rtMargin.bottom @72"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, _pad2) == 76,
        "PAGESETUP pad @76"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, h_instance) == 80,
        "hInstance @80"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, l_cust_data) == 88,
        "lCustData @88"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, lpfn_page_setup_hook) == 96,
        "lpfnPageSetupHook @96"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, lpfn_page_paint_hook) == 104,
        "lpfnPagePaintHook @104"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, lp_page_setup_template_name) == 112,
        "lpPageSetupTemplateName @112"
    );
    assert!(
        core::mem::offset_of!(PageSetupDlgW, h_page_setup_template) == 120,
        "hPageSetupTemplate @120"
    );
};

// --- user32 lane: WNDCLASSEX/WNDCLASS, CREATESTRUCT, RECT/POINT, MENUITEMINFO, TRACKMOUSEEVENT ---

/// Win64 `WNDCLASSEXW`/`WNDCLASSEXA` (winuser.h): `UINT cbSize` @0x00,
/// `UINT style` @0x04, `WNDPROC lpfnWndProc` @0x08, `INT cbClsExtra` @0x10,
/// `INT cbWndExtra` @0x14, `HINSTANCE hInstance` @0x18, `HICON hIcon` @0x20,
/// `HCURSOR hCursor` @0x28, `HBRUSH hbrBackground` @0x30, `LPCWSTR
/// lpszMenuName` @0x38, `LPCWSTR lpszClassName` @0x40, `HICON hIconSm` @0x48 —
/// 80 bytes, align 8.
///
/// The A and W variants share this layout; only the pointed-to strings
/// differ, so one struct serves both `RegisterClassExA` and `RegisterClassExW`.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WndClassEx {
    pub(crate) cb_size: u32,
    pub(crate) style: u32,
    pub(crate) window_proc: u64,
    pub(crate) cb_cls_extra: i32,
    pub(crate) cb_wnd_extra: i32,
    pub(crate) instance_handle: u64,
    pub(crate) icon_handle: u64,
    pub(crate) cursor_handle: u64,
    pub(crate) background_brush: u64,
    pub(crate) menu_name: u64,
    pub(crate) class_name_ptr: u64,
    pub(crate) small_icon_handle: u64,
}

/// Compile-time layout check for [`WndClassEx`]: the offsets mirror the
/// constants the per-field `RegisterClassEx*` handlers pinned (including the
/// +0x38 `lpszMenuName` this repo's class-menu history hinges on).
const _: () = {
    assert!(
        core::mem::size_of::<WndClassEx>() == 80,
        "WNDCLASSEX must be 80 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, cb_size) == 0x00,
        "cbSize @0x00"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, style) == 0x04,
        "style @0x04"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, window_proc) == 0x08,
        "lpfnWndProc @0x08"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, cb_cls_extra) == 0x10,
        "cbClsExtra @0x10"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, cb_wnd_extra) == 0x14,
        "cbWndExtra @0x14"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, instance_handle) == 0x18,
        "hInstance @0x18"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, icon_handle) == 0x20,
        "hIcon @0x20"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, cursor_handle) == 0x28,
        "hCursor @0x28"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, background_brush) == 0x30,
        "hbrBackground @0x30"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, menu_name) == 0x38,
        "lpszMenuName @0x38"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, class_name_ptr) == 0x40,
        "lpszClassName @0x40"
    );
    assert!(
        core::mem::offset_of!(WndClassEx, small_icon_handle) == 0x48,
        "hIconSm @0x48"
    );
};

/// Win64 `WNDCLASSW`/`WNDCLASSA` (winuser.h) — the non-Ex variant (no
/// `cbSize`, no `hIconSm`): `UINT style` @0x00, [pad] @0x04, `WNDPROC
/// lpfnWndProc` @0x08, `INT cbClsExtra` @0x10, `INT cbWndExtra` @0x14,
/// `HINSTANCE hInstance` @0x18, `HICON hIcon` @0x20, `HCURSOR hCursor` @0x28,
/// `HBRUSH hbrBackground` @0x30, `LPCWSTR lpszMenuName` @0x38, `LPCWSTR
/// lpszClassName` @0x40 — 72 bytes, align 8.
///
/// (The 40-byte size sometimes quoted for WNDCLASS is the Win32 layout —
/// 4-byte pointers; the assert table below pins the Win64 72-byte layout the
/// handlers have always read.)
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WndClass {
    pub(crate) style: u32,
    /// Win64 alignment padding between `style` and the `lpfnWndProc` pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) window_proc: u64,
    pub(crate) cb_cls_extra: i32,
    pub(crate) cb_wnd_extra: i32,
    pub(crate) instance_handle: u64,
    pub(crate) icon_handle: u64,
    pub(crate) cursor_handle: u64,
    pub(crate) background_brush: u64,
    pub(crate) menu_name: u64,
    pub(crate) class_name_ptr: u64,
}

/// Compile-time layout check for [`WndClass`].
const _: () = {
    assert!(
        core::mem::size_of::<WndClass>() == 72,
        "WNDCLASS must be 72 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(WndClass, style) == 0x00,
        "style @0x00"
    );
    assert!(
        core::mem::offset_of!(WndClass, window_proc) == 0x08,
        "lpfnWndProc @0x08"
    );
    assert!(
        core::mem::offset_of!(WndClass, cb_cls_extra) == 0x10,
        "cbClsExtra @0x10"
    );
    assert!(
        core::mem::offset_of!(WndClass, cb_wnd_extra) == 0x14,
        "cbWndExtra @0x14"
    );
    assert!(
        core::mem::offset_of!(WndClass, instance_handle) == 0x18,
        "hInstance @0x18"
    );
    assert!(
        core::mem::offset_of!(WndClass, icon_handle) == 0x20,
        "hIcon @0x20"
    );
    assert!(
        core::mem::offset_of!(WndClass, cursor_handle) == 0x28,
        "hCursor @0x28"
    );
    assert!(
        core::mem::offset_of!(WndClass, background_brush) == 0x30,
        "hbrBackground @0x30"
    );
    assert!(
        core::mem::offset_of!(WndClass, menu_name) == 0x38,
        "lpszMenuName @0x38"
    );
    assert!(
        core::mem::offset_of!(WndClass, class_name_ptr) == 0x40,
        "lpszClassName @0x40"
    );
};

/// Win64 `CREATESTRUCTW`/`CREATESTRUCTA` (winuser.h), the `WM_CREATE` lParam:
/// `LPVOID lpCreateParams` @0x00, `HINSTANCE hInstance` @0x08, `HMENU hMenu`
/// @0x10, `HWND hwndParent` @0x18, `int cy` @0x20, `int cx` @0x24, `int y`
/// @0x28, `int x` @0x2C, `LONG style` @0x30, [pad] @0x34, `LPCWSTR lpszName`
/// @0x38, `LPCWSTR lpszClass` @0x40, `DWORD dwExStyle` @0x48, [pad] @0x4C —
/// 80 bytes, align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct CreateStruct {
    pub(crate) create_params: u64,
    pub(crate) instance_handle: u64,
    pub(crate) menu_handle: u64,
    pub(crate) parent_handle: u64,
    pub(crate) cy: i32,
    pub(crate) cx: i32,
    pub(crate) y: i32,
    pub(crate) x: i32,
    pub(crate) style: u32,
    /// Win64 alignment padding between `style` and the `lpszName` pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) name_ptr: u64,
    pub(crate) class_ptr: u64,
    pub(crate) extended_style: u32,
    /// Trailing alignment padding (76 payload bytes → 80).
    pub(crate) _pad_end: [u8; 4],
}

/// Compile-time layout check for [`CreateStruct`].
const _: () = {
    assert!(
        core::mem::size_of::<CreateStruct>() == 0x50,
        "CREATESTRUCT must be 0x50 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, create_params) == 0x00,
        "lpCreateParams @0x00"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, instance_handle) == 0x08,
        "hInstance @0x08"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, menu_handle) == 0x10,
        "hMenu @0x10"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, parent_handle) == 0x18,
        "hwndParent @0x18"
    );
    assert!(core::mem::offset_of!(CreateStruct, cy) == 0x20, "cy @0x20");
    assert!(core::mem::offset_of!(CreateStruct, cx) == 0x24, "cx @0x24");
    assert!(core::mem::offset_of!(CreateStruct, y) == 0x28, "y @0x28");
    assert!(core::mem::offset_of!(CreateStruct, x) == 0x2C, "x @0x2C");
    assert!(
        core::mem::offset_of!(CreateStruct, style) == 0x30,
        "style @0x30"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, name_ptr) == 0x38,
        "lpszName @0x38"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, class_ptr) == 0x40,
        "lpszClass @0x40"
    );
    assert!(
        core::mem::offset_of!(CreateStruct, extended_style) == 0x48,
        "dwExStyle @0x48"
    );
};

/// Win64 `RECT` (windef.h): `LONG left` @0x00, `LONG top` @0x04, `LONG right`
/// @0x08, `LONG bottom` @0x0C — 16 bytes, align 4. Named `WinRect` (not
/// `Rect`) so it cannot collide with the gdi32 lane's `Rect`.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WinRect {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) right: i32,
    pub(crate) bottom: i32,
}

/// Compile-time layout check for [`WinRect`].
const _: () = {
    assert!(
        core::mem::size_of::<WinRect>() == 16,
        "RECT must be 16 bytes on Win64"
    );
    assert!(core::mem::offset_of!(WinRect, left) == 0, "left @0");
    assert!(core::mem::offset_of!(WinRect, top) == 4, "top @4");
    assert!(core::mem::offset_of!(WinRect, right) == 8, "right @8");
    assert!(core::mem::offset_of!(WinRect, bottom) == 12, "bottom @12");
};

/// Win64 `POINT` (windef.h): `LONG x` @0x00, `LONG y` @0x04 — 8 bytes,
/// align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct WinPoint {
    pub(crate) x: i32,
    pub(crate) y: i32,
}

/// Compile-time layout check for [`WinPoint`].
const _: () = {
    assert!(
        core::mem::size_of::<WinPoint>() == 8,
        "POINT must be 8 bytes on Win64"
    );
    assert!(core::mem::offset_of!(WinPoint, x) == 0, "x @0");
    assert!(core::mem::offset_of!(WinPoint, y) == 4, "y @4");
};

/// Win64 `MENUITEMINFO` (winuser.h): `UINT cbSize` @0x00, `UINT fMask` @0x04,
/// `UINT fType` @0x08, `UINT fState` @0x0C, `UINT wID` @0x10, [pad] @0x14,
/// `HMENU hSubMenu` @0x18, `HBITMAP hbmpChecked` @0x20, `HBITMAP
/// hbmpUnchecked` @0x28, `ULONG_PTR dwItemData` @0x30, `LPTSTR dwTypeData`
/// @0x38, `UINT cch` @0x40, [pad] @0x44, `HBITMAP hbmpItem` @0x48 — 80 bytes,
/// align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct MenuItemInfo {
    pub(crate) cb_size: u32,
    pub(crate) f_mask: u32,
    pub(crate) f_type: u32,
    pub(crate) f_state: u32,
    pub(crate) w_id: u32,
    /// Win64 alignment padding between `wID` and the `hSubMenu` pointer.
    pub(crate) _pad: [u8; 4],
    pub(crate) submenu_handle: u64,
    pub(crate) checked_bitmap: u64,
    pub(crate) unchecked_bitmap: u64,
    pub(crate) item_data: u64,
    pub(crate) type_data_ptr: u64,
    pub(crate) cch: u32,
    /// Win64 alignment padding between `cch` and the `hbmpItem` pointer.
    pub(crate) _pad_after_cch: [u8; 4],
    pub(crate) item_bitmap: u64,
}

/// Compile-time layout check for [`MenuItemInfo`].
const _: () = {
    assert!(
        core::mem::size_of::<MenuItemInfo>() == 80,
        "MENUITEMINFO must be 80 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(MenuItemInfo, cb_size) == 0,
        "cbSize @0"
    );
    assert!(core::mem::offset_of!(MenuItemInfo, f_mask) == 4, "fMask @4");
    assert!(core::mem::offset_of!(MenuItemInfo, f_type) == 8, "fType @8");
    assert!(
        core::mem::offset_of!(MenuItemInfo, f_state) == 12,
        "fState @12"
    );
    assert!(core::mem::offset_of!(MenuItemInfo, w_id) == 16, "wID @16");
    assert!(
        core::mem::offset_of!(MenuItemInfo, submenu_handle) == 24,
        "hSubMenu @24"
    );
    assert!(
        core::mem::offset_of!(MenuItemInfo, type_data_ptr) == 56,
        "dwTypeData @56"
    );
    assert!(core::mem::offset_of!(MenuItemInfo, cch) == 64, "cch @64");
    assert!(
        core::mem::offset_of!(MenuItemInfo, item_bitmap) == 72,
        "hbmpItem @72"
    );
};

/// Win64 `TRACKMOUSEEVENT` (winuser.h): `DWORD cbSize` @0x00, `DWORD dwFlags`
/// @0x04, `HWND hwndTrack` @0x08, `DWORD dwHoverTime` @0x10, [pad] @0x14 —
/// 24 bytes, align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct TrackMouseEvent {
    pub(crate) cb_size: u32,
    pub(crate) flags: u32,
    pub(crate) track_window_handle: u64,
    pub(crate) hover_time: u32,
    /// Trailing alignment padding (20 payload bytes → 24).
    pub(crate) _pad: [u8; 4],
}

/// Compile-time layout check for [`TrackMouseEvent`].
const _: () = {
    assert!(
        core::mem::size_of::<TrackMouseEvent>() == 24,
        "TRACKMOUSEEVENT must be 24 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(TrackMouseEvent, cb_size) == 0,
        "cbSize @0"
    );
    assert!(
        core::mem::offset_of!(TrackMouseEvent, flags) == 4,
        "dwFlags @4"
    );
    assert!(
        core::mem::offset_of!(TrackMouseEvent, track_window_handle) == 8,
        "hwndTrack @8"
    );
    assert!(
        core::mem::offset_of!(TrackMouseEvent, hover_time) == 16,
        "dwHoverTime @16"
    );
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::guest_memory::{with_typed_read, with_typed_write};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    const MSG_VA: u64 = 0x4000;

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

    #[test]
    fn msg_write_view_zero_fills_padding_and_unset_fields() {
        let mut engine = test_engine();
        with_typed_write::<Msg, _, _>(&mut engine, MSG_VA, |msg| {
            // Deliberately leave wparam/lparam/time/pt/l_private unset: the
            // view starts zeroed (GetStartupInfo semantics).
            msg.hwnd = 0x1122_3344_5566_7788;
            msg.message = 0x0100;
            Ok(())
        })
        .expect("typed write");
        let bytes = raw_bytes(&mut engine, MSG_VA, 48);
        assert_eq!(&bytes[0..8], &0x1122_3344_5566_7788_u64.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x0100_u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &[0, 0, 0, 0], "Win64 MSG padding");
        assert_eq!(&bytes[16..48], &[0; 32], "unset fields + lPrivate");
    }

    #[test]
    fn msg_write_view_matches_hand_written_byte_pattern() {
        let mut engine = test_engine();
        with_typed_write::<Msg, _, _>(&mut engine, MSG_VA, |msg| {
            msg.hwnd = 0x1111_2222_3333_4444;
            msg.message = 0x0F;
            msg.wparam = 0xAAAA_BBBB_CCCC_DDDD;
            msg.lparam = 0xDEAD_BEEF_CAFE_F00D;
            msg.time = 0x1234_5678;
            msg.pt_x = -7;
            msg.pt_y = 99;
            msg.l_private = 0;
            Ok(())
        })
        .expect("typed write");
        let bytes = raw_bytes(&mut engine, MSG_VA, 48);
        let mut expected = vec![0_u8; 48];
        expected[0..8].copy_from_slice(&0x1111_2222_3333_4444_u64.to_le_bytes());
        expected[8..12].copy_from_slice(&0x0F_u32.to_le_bytes());
        // bytes 12..16: alignment padding, zero.
        expected[16..24].copy_from_slice(&0xAAAA_BBBB_CCCC_DDDD_u64.to_le_bytes());
        expected[24..32].copy_from_slice(&0xDEAD_BEEF_CAFE_F00D_u64.to_le_bytes());
        expected[32..36].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
        expected[36..40].copy_from_slice(&(-7_i32).to_le_bytes());
        expected[40..44].copy_from_slice(&99_i32.to_le_bytes());
        // bytes 44..48: lPrivate, zero.
        assert_eq!(bytes, expected, "MSG byte pattern drift");
    }

    #[test]
    fn msg_read_view_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        // Write a raw MSG with nonzero padding: the read view must preserve
        // guest bytes exactly (reads do not zero-fill).
        let mut bytes = vec![0_u8; 48];
        bytes[0..8].copy_from_slice(&7_u64.to_le_bytes());
        bytes[8..12].copy_from_slice(&0xABCD_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // padding
        bytes[16..24].copy_from_slice(&0x0102_0304_0506_0708_u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&0xF0E0_D0C0_B0A0_9080_u64.to_le_bytes());
        bytes[32..36].copy_from_slice(&0x0101_0101_u32.to_le_bytes());
        bytes[36..40].copy_from_slice(&(-1_i32).to_le_bytes());
        bytes[40..44].copy_from_slice(&(-2_i32).to_le_bytes());
        bytes[44..48].copy_from_slice(&0x77_u32.to_le_bytes());
        engine
            .mem_write(MSG_VA, &bytes)
            .expect("write raw MSG bytes");

        with_typed_read::<Msg, _, _>(&mut engine, MSG_VA, |view| {
            assert_eq!(view.hwnd, 7);
            assert_eq!(view.message, 0xABCD);
            assert_eq!(view.wparam, 0x0102_0304_0506_0708);
            assert_eq!(view.lparam, 0xF0E0_D0C0_B0A0_9080);
            assert_eq!(view.time, 0x0101_0101);
            assert_eq!(view.pt_x, -1);
            assert_eq!(view.pt_y, -2);
            assert_eq!(view.l_private, 0x77);
            Ok(())
        })
        .expect("typed read");
    }

    #[test]
    fn msg_misaligned_guest_va_stages_instead_of_erroring() {
        // An odd address cannot be borrowed in place (align 8): the helper
        // must stage into an aligned host buffer and produce identical bytes.
        let mut engine = test_engine();
        let odd_va = MSG_VA + 1;
        with_typed_write::<Msg, _, _>(&mut engine, odd_va, |msg| {
            msg.hwnd = 0x1234_5678_9ABC_DEF0;
            msg.message = 0x111;
            msg.wparam = 5;
            msg.lparam = 6;
            msg.time = 7;
            msg.pt_x = 8;
            msg.pt_y = 9;
            Ok(())
        })
        .expect("staged typed write");
        let bytes = raw_bytes(&mut engine, odd_va, 48);
        assert_eq!(&bytes[0..8], &0x1234_5678_9ABC_DEF0_u64.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x111_u32.to_le_bytes());
        assert_eq!(&bytes[16..24], &5_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &6_u64.to_le_bytes());
        assert_eq!(&bytes[32..36], &7_u32.to_le_bytes());
        assert_eq!(&bytes[36..40], &8_i32.to_le_bytes());
        assert_eq!(&bytes[40..44], &9_i32.to_le_bytes());
        assert_eq!(&bytes[12..16], &[0; 4], "padding stays zero when staged");
        assert_eq!(&bytes[44..48], &[0; 4], "lPrivate stays zero when staged");
    }

    #[test]
    fn window_placement_write_zero_fills_points_and_flags() {
        let mut engine = test_engine();
        let va = 0x5000_u64;
        with_typed_write::<WindowPlacement, _, _>(&mut engine, va, |placement| {
            placement.length = 44;
            placement.show_cmd = 1;
            placement.rc_left = 10;
            placement.rc_top = 20;
            placement.rc_right = 210;
            placement.rc_bottom = 120;
            Ok(())
        })
        .expect("typed write");
        let bytes = raw_bytes(&mut engine, va, 44);
        assert_eq!(&bytes[0..4], &44_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "flags must be zero");
        assert_eq!(&bytes[8..12], &1_u32.to_le_bytes());
        assert_eq!(&bytes[12..28], &[0; 16], "min/max positions zero");
        assert_eq!(&bytes[28..32], &10_i32.to_le_bytes());
        assert_eq!(&bytes[32..36], &20_i32.to_le_bytes());
        assert_eq!(&bytes[36..40], &210_i32.to_le_bytes());
        assert_eq!(&bytes[40..44], &120_i32.to_le_bytes());
    }

    #[test]
    fn window_placement_read_view_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x5000_u64;
        let mut bytes = vec![0_u8; 44];
        bytes[0..4].copy_from_slice(&44_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&3_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&1_i32.to_le_bytes());
        bytes[16..20].copy_from_slice(&2_i32.to_le_bytes());
        bytes[20..24].copy_from_slice(&3_i32.to_le_bytes());
        bytes[24..28].copy_from_slice(&4_i32.to_le_bytes());
        bytes[28..32].copy_from_slice(&100_i32.to_le_bytes());
        bytes[32..36].copy_from_slice(&200_i32.to_le_bytes());
        bytes[36..40].copy_from_slice(&300_i32.to_le_bytes());
        bytes[40..44].copy_from_slice(&400_i32.to_le_bytes());
        engine.mem_write(va, &bytes).expect("write raw placement");

        with_typed_read::<WindowPlacement, _, _>(&mut engine, va, |view| {
            assert_eq!(view.length, 44);
            assert_eq!(view.flags, 0);
            assert_eq!(view.show_cmd, 3);
            assert_eq!(view.pt_min_x, 1);
            assert_eq!(view.pt_min_y, 2);
            assert_eq!(view.pt_max_x, 3);
            assert_eq!(view.pt_max_y, 4);
            assert_eq!(view.rc_left, 100);
            assert_eq!(view.rc_top, 200);
            assert_eq!(view.rc_right, 300);
            assert_eq!(view.rc_bottom, 400);
            Ok(())
        })
        .expect("typed read");
    }

    #[test]
    fn window_placement_misaligned_stage_read_preserves_bytes() {
        let mut engine = test_engine();
        let odd_va = 0x5000_u64 + 1;
        let mut bytes = vec![0_u8; 44];
        bytes[0..4].copy_from_slice(&44_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&(-5_i32).to_le_bytes());
        engine
            .mem_write(odd_va, &bytes)
            .expect("write raw placement at odd address");
        with_typed_read::<WindowPlacement, _, _>(&mut engine, odd_va, |view| {
            assert_eq!(view.length, 44);
            assert_eq!(view.show_cmd, 1);
            assert_eq!(view.rc_left, -5);
            assert_eq!(view.rc_bottom, 0);
            Ok(())
        })
        .expect("staged typed read");
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
    // ── Z1: the print-lane structs ─────────────────────────────────────

    #[test]
    fn doc_info_w_round_trip_pins_padding_and_pointer_offsets() {
        let mut engine = test_engine();
        let va = 0x7100_u64;
        with_typed_write::<DocInfoW, _, _>(&mut engine, va, |docinfo| {
            docinfo.cb_size = 40;
            docinfo.lpsz_doc_name = 0x1111_2222_3333_4444;
            docinfo.lpsz_output = 0x5555_6666_7777_8888;
            docinfo.lpsz_datatype = 0xAAAA_BBBB_CCCC_DDDD;
            docinfo.fw_type = 0x00AB_CDEF;
            Ok(())
        })
        .expect("typed DOCINFOW write");
        let bytes = raw_bytes(&mut engine, va, 40);
        assert_eq!(&bytes[0..4], &40_i32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "cbSize→pointer alignment pad");
        assert_eq!(&bytes[8..16], &0x1111_2222_3333_4444_u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x5555_6666_7777_8888_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &0xAAAA_BBBB_CCCC_DDDD_u64.to_le_bytes());
        assert_eq!(&bytes[32..36], &0x00AB_CDEF_u32.to_le_bytes());
        assert_eq!(&bytes[36..40], &[0; 4], "DOCINFOW trailing pad");

        with_typed_read::<DocInfoW, _, _>(&mut engine, va, |docinfo| {
            assert_eq!(docinfo.cb_size, 40);
            assert_eq!(docinfo.lpsz_doc_name, 0x1111_2222_3333_4444);
            assert_eq!(docinfo.lpsz_output, 0x5555_6666_7777_8888);
            assert_eq!(docinfo.lpsz_datatype, 0xAAAA_BBBB_CCCC_DDDD);
            assert_eq!(docinfo.fw_type, 0x00AB_CDEF);
            Ok(())
        })
        .expect("typed DOCINFOW read");
    }

    #[test]
    fn choose_font_w_round_trip_pins_sdk_alignment_word() {
        let mut engine = test_engine();
        let va = 0x7200_u64;
        with_typed_write::<ChooseFontW, _, _>(&mut engine, va, |cf| {
            cf.l_struct_size = 104;
            cf.hwnd_owner = 0x0102_0304_0506_0708;
            cf.h_dc = 0x1112_1314_1516_1718;
            cf.lp_log_font = 0x2122_2324_2526_2728;
            cf.i_point_size = 120;
            cf.flags = 0x1 | 0x40 | 0x100;
            cf.rgb_colors = 0x00_30_50;
            cf.l_cust_data = 0x3132_3334_3536_3738;
            cf.lpfn_hook = 0x4142_4344_4546_4748;
            cf.lp_template_name = 0x5152_5354_5556_5758;
            cf.h_instance = 0x6162_6364_6566_6768;
            cf.lpsz_style = 0x7172_7374_7576_7778;
            cf.n_font_type = 0x8001;
            cf.n_size_min = 8;
            cf.n_size_max = 72;
            Ok(())
        })
        .expect("typed CHOOSEFONTW write");
        let bytes = raw_bytes(&mut engine, va, 104);
        assert_eq!(&bytes[0..4], &104_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "lStructSize→hwndOwner pad");
        assert_eq!(&bytes[8..16], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x1112_1314_1516_1718_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &0x2122_2324_2526_2728_u64.to_le_bytes());
        assert_eq!(&bytes[32..36], &120_i32.to_le_bytes(), "iPointSize");
        assert_eq!(&bytes[36..40], &(0x1_u32 | 0x40 | 0x100).to_le_bytes());
        assert_eq!(&bytes[40..44], &0x00_30_50_u32.to_le_bytes());
        assert_eq!(&bytes[44..48], &[0; 4], "rgbColors→lCustData pad");
        assert_eq!(&bytes[48..56], &0x3132_3334_3536_3738_u64.to_le_bytes());
        assert_eq!(&bytes[56..64], &0x4142_4344_4546_4748_u64.to_le_bytes());
        assert_eq!(&bytes[64..72], &0x5152_5354_5556_5758_u64.to_le_bytes());
        assert_eq!(&bytes[72..80], &0x6162_6364_6566_6768_u64.to_le_bytes());
        assert_eq!(&bytes[80..88], &0x7172_7374_7576_7778_u64.to_le_bytes());
        assert_eq!(&bytes[88..90], &0x8001_u16.to_le_bytes(), "nFontType");
        assert_eq!(
            &bytes[90..92],
            &[0, 0],
            "___MISSING_ALIGNMENT__ word stays zero"
        );
        assert_eq!(&bytes[92..96], &8_i32.to_le_bytes(), "nSizeMin");
        assert_eq!(&bytes[96..100], &72_i32.to_le_bytes(), "nSizeMax");
        assert_eq!(&bytes[100..104], &[0; 4], "CHOOSEFONTW trailing pad");

        with_typed_read::<ChooseFontW, _, _>(&mut engine, va, |cf| {
            assert_eq!(cf.l_struct_size, 104);
            assert_eq!(cf.hwnd_owner, 0x0102_0304_0506_0708);
            assert_eq!(cf.h_dc, 0x1112_1314_1516_1718);
            assert_eq!(cf.lp_log_font, 0x2122_2324_2526_2728);
            assert_eq!(cf.i_point_size, 120);
            assert_eq!(cf.flags, 0x1 | 0x40 | 0x100);
            assert_eq!(cf.rgb_colors, 0x00_30_50);
            assert_eq!(cf.l_cust_data, 0x3132_3334_3536_3738);
            assert_eq!(cf.lpfn_hook, 0x4142_4344_4546_4748);
            assert_eq!(cf.lp_template_name, 0x5152_5354_5556_5758);
            assert_eq!(cf.h_instance, 0x6162_6364_6566_6768);
            assert_eq!(cf.lpsz_style, 0x7172_7374_7576_7778);
            assert_eq!(cf.n_font_type, 0x8001);
            assert_eq!(cf.n_size_min, 8);
            assert_eq!(cf.n_size_max, 72);
            Ok(())
        })
        .expect("typed CHOOSEFONTW read");
    }

    #[test]
    fn print_dlg_w_round_trip_pins_word_and_pointer_offsets() {
        let mut engine = test_engine();
        let va = 0x7300_u64;
        with_typed_write::<PrintDlgW, _, _>(&mut engine, va, |pd| {
            pd.l_struct_size = 120;
            pd.hwnd_owner = 0x1111_1111_2222_2222;
            pd.h_dev_mode = 0x3333_3333_4444_4444;
            pd.h_dev_names = 0x5555_5555_6666_6666;
            pd.h_dc = 0x7777_7777_8888_8888;
            pd.flags = 0x2 | 0x8;
            pd.n_from_page = 1;
            pd.n_to_page = 2;
            pd.n_min_page = 1;
            pd.n_max_page = 999;
            pd.n_copies = 1;
            pd.h_instance = 0x9999_9999_AAAA_AAAA;
            pd.l_cust_data = 0xBBBB_BBBB_CCCC_CCCC;
            pd.lpfn_print_hook = 0xDDDD_DDDD_EEEE_EEEE;
            pd.lpfn_setup_hook = 0xFFFF_FFFF_0000_0001;
            pd.lp_print_template_name = 0x0000_0002_0000_0003;
            pd.lp_setup_template_name = 0x0000_0004_0000_0005;
            pd.h_print_template = 0x0000_0006_0000_0007;
            pd.h_setup_template = 0x0000_0008_0000_0009;
            Ok(())
        })
        .expect("typed PRINTDLGW write");
        let bytes = raw_bytes(&mut engine, va, 120);
        assert_eq!(&bytes[0..4], &120_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "lStructSize→hwndOwner pad");
        assert_eq!(&bytes[8..16], &0x1111_1111_2222_2222_u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x3333_3333_4444_4444_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &0x5555_5555_6666_6666_u64.to_le_bytes());
        assert_eq!(&bytes[32..40], &0x7777_7777_8888_8888_u64.to_le_bytes());
        assert_eq!(&bytes[40..44], &(0x2_u32 | 0x8).to_le_bytes());
        assert_eq!(&bytes[44..46], &1_u16.to_le_bytes(), "nFromPage");
        assert_eq!(&bytes[46..48], &2_u16.to_le_bytes(), "nToPage");
        assert_eq!(&bytes[48..50], &1_u16.to_le_bytes(), "nMinPage");
        assert_eq!(&bytes[50..52], &999_u16.to_le_bytes(), "nMaxPage");
        assert_eq!(&bytes[52..54], &1_u16.to_le_bytes(), "nCopies");
        assert_eq!(&bytes[54..56], &[0; 2], "nCopies→hInstance pad");
        assert_eq!(&bytes[56..64], &0x9999_9999_AAAA_AAAA_u64.to_le_bytes());
        assert_eq!(&bytes[64..72], &0xBBBB_BBBB_CCCC_CCCC_u64.to_le_bytes());
        assert_eq!(&bytes[72..80], &0xDDDD_DDDD_EEEE_EEEE_u64.to_le_bytes());
        assert_eq!(&bytes[80..88], &0xFFFF_FFFF_0000_0001_u64.to_le_bytes());
        assert_eq!(
            &bytes[88..96],
            &0x0000_0002_0000_0003_u64.to_le_bytes(),
            "lpPrintTemplateName"
        );
        assert_eq!(
            &bytes[96..104],
            &0x0000_0004_0000_0005_u64.to_le_bytes(),
            "lpSetupTemplateName"
        );
        assert_eq!(
            &bytes[104..112],
            &0x0000_0006_0000_0007_u64.to_le_bytes(),
            "hPrintTemplate"
        );
        assert_eq!(
            &bytes[112..120],
            &0x0000_0008_0000_0009_u64.to_le_bytes(),
            "hSetupTemplate"
        );

        with_typed_read::<PrintDlgW, _, _>(&mut engine, va, |pd| {
            assert_eq!(pd.l_struct_size, 120);
            assert_eq!(pd.hwnd_owner, 0x1111_1111_2222_2222);
            assert_eq!(pd.h_dev_mode, 0x3333_3333_4444_4444);
            assert_eq!(pd.h_dev_names, 0x5555_5555_6666_6666);
            assert_eq!(pd.h_dc, 0x7777_7777_8888_8888);
            assert_eq!(pd.flags, 0x2 | 0x8);
            assert_eq!(pd.n_from_page, 1);
            assert_eq!(pd.n_to_page, 2);
            assert_eq!(pd.n_min_page, 1);
            assert_eq!(pd.n_max_page, 999);
            assert_eq!(pd.n_copies, 1);
            assert_eq!(pd.h_instance, 0x9999_9999_AAAA_AAAA);
            assert_eq!(pd.l_cust_data, 0xBBBB_BBBB_CCCC_CCCC);
            assert_eq!(pd.lpfn_print_hook, 0xDDDD_DDDD_EEEE_EEEE);
            assert_eq!(pd.lpfn_setup_hook, 0xFFFF_FFFF_0000_0001);
            assert_eq!(pd.lp_print_template_name, 0x0000_0002_0000_0003);
            assert_eq!(pd.lp_setup_template_name, 0x0000_0004_0000_0005);
            assert_eq!(pd.h_print_template, 0x0000_0006_0000_0007);
            assert_eq!(pd.h_setup_template, 0x0000_0008_0000_0009);
            Ok(())
        })
        .expect("typed PRINTDLGW read");
    }

    #[test]
    fn dev_mode_w_round_trip_pins_the_mingw_layout() {
        let mut engine = test_engine();
        let va = 0x7400_u64;
        with_typed_write::<DevModeW, _, _>(&mut engine, va, |dm| {
            // dmDeviceName[0..2] = "HP"; rest zeroed by the view.
            dm.dm_device_name[0] = b'H' as u16;
            dm.dm_device_name[1] = b'P' as u16;
            dm.dm_spec_version = 0x0401;
            dm.dm_driver_version = 0x0400;
            dm.dm_size = 220;
            dm.dm_driver_extra = 0;
            dm.dm_fields = 0x0001 | 0x0002 | 0x0010;
            dm.dm_orientation = 1; // DMORIENT_PORTRAIT
            dm.dm_paper_size = 9; // DMPAPER_LETTER
            dm.dm_paper_length = 2794; // 1/10 mm
            dm.dm_paper_width = 2159;
            dm.dm_scale = 100;
            dm.dm_copies = 1;
            dm.dm_default_source = 7; // DMBIN_AUTO
            dm.dm_print_quality = 600;
            dm.dm_color = 2; // DMCOLOR_COLOR
            dm.dm_log_pixels = 300;
            Ok(())
        })
        .expect("typed DEVMODEW write");
        let bytes = raw_bytes(&mut engine, va, 220);
        assert_eq!(&bytes[0..4], b"H\0P\0", "dmDeviceName @0 (UTF-16)");
        assert_eq!(&bytes[64..66], &0x0401_u16.to_le_bytes(), "dmSpecVersion");
        assert_eq!(&bytes[68..70], &220_u16.to_le_bytes(), "dmSize");
        assert_eq!(
            &bytes[72..76],
            &(0x0001_u32 | 0x0002 | 0x0010).to_le_bytes(),
            "dmFields"
        );
        assert_eq!(&bytes[76..78], &1_i16.to_le_bytes(), "dmOrientation @76");
        assert_eq!(&bytes[80..82], &2794_i16.to_le_bytes(), "dmPaperLength");
        assert_eq!(&bytes[82..84], &2159_i16.to_le_bytes(), "dmPaperWidth");
        assert_eq!(&bytes[86..88], &1_i16.to_le_bytes(), "dmCopies");
        assert_eq!(&bytes[92..94], &2_i16.to_le_bytes(), "dmColor");
        assert_eq!(&bytes[166..168], &300_u16.to_le_bytes(), "dmLogPixels @166");
        // Fields the view left unset (form name, ICM block, …) stay zero.
        assert_eq!(&bytes[102..166], &[0; 64], "dmFormName zeroed");
        assert_eq!(&bytes[216..220], &[0; 4], "dmPanningHeight zeroed");

        with_typed_read::<DevModeW, _, _>(&mut engine, va, |dm| {
            assert_eq!(dm.dm_device_name[0], b'H' as u16);
            assert_eq!(dm.dm_device_name[1], b'P' as u16);
            assert_eq!(dm.dm_spec_version, 0x0401);
            assert_eq!(dm.dm_driver_version, 0x0400);
            assert_eq!(dm.dm_size, 220);
            assert_eq!(dm.dm_fields, 0x0001 | 0x0002 | 0x0010);
            assert_eq!(dm.dm_orientation, 1);
            assert_eq!(dm.dm_paper_size, 9);
            assert_eq!(dm.dm_paper_length, 2794);
            assert_eq!(dm.dm_paper_width, 2159);
            assert_eq!(dm.dm_scale, 100);
            assert_eq!(dm.dm_copies, 1);
            assert_eq!(dm.dm_default_source, 7);
            assert_eq!(dm.dm_print_quality, 600);
            assert_eq!(dm.dm_color, 2);
            assert_eq!(dm.dm_log_pixels, 300);
            Ok(())
        })
        .expect("typed DEVMODEW read");
    }

    #[test]
    fn dev_names_round_trip_pins_four_word_offsets() {
        let mut engine = test_engine();
        let va = 0x7500_u64;
        with_typed_write::<DevNames, _, _>(&mut engine, va, |names| {
            names.w_driver_offset = 8;
            names.w_device_offset = 16;
            names.w_output_offset = 24;
            names.w_default = 32;
            Ok(())
        })
        .expect("typed DEVNAMES write");
        let bytes = raw_bytes(&mut engine, va, 8);
        assert_eq!(&bytes[0..2], &8_u16.to_le_bytes(), "wDriverOffset @0");
        assert_eq!(&bytes[2..4], &16_u16.to_le_bytes(), "wDeviceOffset @2");
        assert_eq!(&bytes[4..6], &24_u16.to_le_bytes(), "wOutputOffset @4");
        assert_eq!(&bytes[6..8], &32_u16.to_le_bytes(), "wDefault @6");

        with_typed_read::<DevNames, _, _>(&mut engine, va, |names| {
            assert_eq!(names.w_driver_offset, 8);
            assert_eq!(names.w_device_offset, 16);
            assert_eq!(names.w_output_offset, 24);
            assert_eq!(names.w_default, 32);
            Ok(())
        })
        .expect("typed DEVNAMES read");
    }

    #[test]
    fn page_setup_dlg_w_round_trip_pins_margins_and_pointers() {
        let mut engine = test_engine();
        let va = 0x7600_u64;
        with_typed_write::<PageSetupDlgW, _, _>(&mut engine, va, |psd| {
            psd.l_struct_size = 128;
            psd.hwnd_owner = 0x0101_0101_0101_0101;
            psd.h_dev_mode = 0x0202_0202_0202_0202;
            psd.h_dev_names = 0x0303_0303_0303_0303;
            psd.flags = 0x2; // PSD_MARGINS
            psd.pt_paper_size_x = 8500; // hundredths of inches
            psd.pt_paper_size_y = 11000;
            psd.rt_min_margin_left = 250;
            psd.rt_min_margin_top = 250;
            psd.rt_min_margin_right = 250;
            psd.rt_min_margin_bottom = 250;
            psd.rt_margin_left = 1000;
            psd.rt_margin_top = 1000;
            psd.rt_margin_right = 1000;
            psd.rt_margin_bottom = 1000;
            psd.h_instance = 0x0404_0404_0404_0404;
            psd.l_cust_data = 0x0505_0505_0505_0505;
            psd.lpfn_page_setup_hook = 0x0606_0606_0606_0606;
            psd.lpfn_page_paint_hook = 0x0707_0707_0707_0707;
            psd.lp_page_setup_template_name = 0x0808_0808_0808_0808;
            psd.h_page_setup_template = 0x0909_0909_0909_0909;
            Ok(())
        })
        .expect("typed PAGESETUPDLGW write");
        let bytes = raw_bytes(&mut engine, va, 128);
        assert_eq!(&bytes[0..4], &128_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "lStructSize→hwndOwner pad");
        assert_eq!(&bytes[8..16], &0x0101_0101_0101_0101_u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x0202_0202_0202_0202_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &0x0303_0303_0303_0303_u64.to_le_bytes());
        assert_eq!(&bytes[32..36], &0x2_u32.to_le_bytes(), "Flags");
        assert_eq!(&bytes[36..40], &8500_i32.to_le_bytes(), "ptPaperSize.x");
        assert_eq!(&bytes[40..44], &11000_i32.to_le_bytes(), "ptPaperSize.y");
        assert_eq!(&bytes[44..48], &250_i32.to_le_bytes(), "rtMinMargin.left");
        assert_eq!(&bytes[48..52], &250_i32.to_le_bytes(), "rtMinMargin.top");
        assert_eq!(&bytes[52..56], &250_i32.to_le_bytes(), "rtMinMargin.right");
        assert_eq!(&bytes[56..60], &250_i32.to_le_bytes(), "rtMinMargin.bottom");
        assert_eq!(&bytes[60..64], &1000_i32.to_le_bytes(), "rtMargin.left");
        assert_eq!(&bytes[64..68], &1000_i32.to_le_bytes(), "rtMargin.top");
        assert_eq!(&bytes[68..72], &1000_i32.to_le_bytes(), "rtMargin.right");
        assert_eq!(&bytes[72..76], &1000_i32.to_le_bytes(), "rtMargin.bottom");
        assert_eq!(&bytes[76..80], &[0; 4], "rtMargin→hInstance pad");
        assert_eq!(&bytes[80..88], &0x0404_0404_0404_0404_u64.to_le_bytes());
        assert_eq!(&bytes[88..96], &0x0505_0505_0505_0505_u64.to_le_bytes());
        assert_eq!(
            &bytes[96..104],
            &0x0606_0606_0606_0606_u64.to_le_bytes(),
            "lpfnPageSetupHook"
        );
        assert_eq!(
            &bytes[104..112],
            &0x0707_0707_0707_0707_u64.to_le_bytes(),
            "lpfnPagePaintHook"
        );
        assert_eq!(
            &bytes[112..120],
            &0x0808_0808_0808_0808_u64.to_le_bytes(),
            "lpPageSetupTemplateName"
        );
        assert_eq!(
            &bytes[120..128],
            &0x0909_0909_0909_0909_u64.to_le_bytes(),
            "hPageSetupTemplate"
        );

        with_typed_read::<PageSetupDlgW, _, _>(&mut engine, va, |psd| {
            assert_eq!(psd.l_struct_size, 128);
            assert_eq!(psd.hwnd_owner, 0x0101_0101_0101_0101);
            assert_eq!(psd.h_dev_mode, 0x0202_0202_0202_0202);
            assert_eq!(psd.h_dev_names, 0x0303_0303_0303_0303);
            assert_eq!(psd.flags, 0x2);
            assert_eq!(psd.pt_paper_size_x, 8500);
            assert_eq!(psd.pt_paper_size_y, 11000);
            assert_eq!(psd.rt_min_margin_left, 250);
            assert_eq!(psd.rt_margin_left, 1000);
            assert_eq!(psd.rt_margin_bottom, 1000);
            assert_eq!(psd.h_instance, 0x0404_0404_0404_0404);
            assert_eq!(psd.l_cust_data, 0x0505_0505_0505_0505);
            assert_eq!(psd.lpfn_page_setup_hook, 0x0606_0606_0606_0606);
            assert_eq!(psd.lpfn_page_paint_hook, 0x0707_0707_0707_0707);
            assert_eq!(psd.lp_page_setup_template_name, 0x0808_0808_0808_0808);
            assert_eq!(psd.h_page_setup_template, 0x0909_0909_0909_0909);
            Ok(())
        })
        .expect("typed PAGESETUPDLGW read");
    }

    #[test]
    fn wnd_class_ex_write_places_every_field_at_the_pinned_offsets() {
        let mut engine = test_engine();
        let va = 0x7000_u64;
        with_typed_write::<WndClassEx, _, _>(&mut engine, va, |wc| {
            wc.cb_size = 80;
            wc.style = 0x0000_0002;
            wc.window_proc = 0x1111_2222_3333_4444;
            wc.cb_cls_extra = 3;
            wc.cb_wnd_extra = 4;
            wc.instance_handle = 0x7000_0000_0000_1000;
            wc.icon_handle = 0x6600_0001;
            wc.cursor_handle = 0x6600_0002;
            wc.background_brush = 0x6600_0507;
            wc.menu_name = 0x201;
            wc.class_name_ptr = 0x5000;
            wc.small_icon_handle = 0x6600_0003;
            Ok(())
        })
        .expect("typed WNDCLASSEXW write");
        let bytes = raw_bytes(&mut engine, va, 80);
        assert_eq!(&bytes[0..4], &80_u32.to_le_bytes(), "cbSize");
        assert_eq!(&bytes[4..8], &0x0000_0002_u32.to_le_bytes(), "style");
        assert_eq!(&bytes[8..16], &0x1111_2222_3333_4444_u64.to_le_bytes());
        assert_eq!(&bytes[16..20], &3_i32.to_le_bytes(), "cbClsExtra");
        assert_eq!(&bytes[20..24], &4_i32.to_le_bytes(), "cbWndExtra");
        assert_eq!(&bytes[24..32], &0x7000_0000_0000_1000_u64.to_le_bytes());
        assert_eq!(&bytes[32..40], &0x6600_0001_u64.to_le_bytes(), "hIcon");
        assert_eq!(&bytes[40..48], &0x6600_0002_u64.to_le_bytes(), "hCursor");
        assert_eq!(
            &bytes[48..56],
            &0x6600_0507_u64.to_le_bytes(),
            "hbrBackground"
        );
        assert_eq!(
            &bytes[56..64],
            &0x201_u64.to_le_bytes(),
            "lpszMenuName @0x38"
        );
        assert_eq!(&bytes[64..72], &0x5000_u64.to_le_bytes(), "lpszClassName");
        assert_eq!(&bytes[72..80], &0x6600_0003_u64.to_le_bytes(), "hIconSm");
    }

    #[test]
    fn wnd_class_read_preserves_guest_padding_bytes() {
        // WNDCLASS has 4 implicit pad bytes between style and lpfnWndProc.
        // A read view must surface the guest's bytes unchanged.
        let mut engine = test_engine();
        let va = 0x7100_u64;
        let mut bytes = vec![0_u8; 72];
        bytes[0..4].copy_from_slice(&0x0000_0001_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // padding
        bytes[8..16].copy_from_slice(&0x0808_0808_0808_0808_u64.to_le_bytes());
        bytes[0x40..0x48].copy_from_slice(&0x5000_u64.to_le_bytes());
        engine.mem_write(va, &bytes).expect("write raw WNDCLASS");
        with_typed_read::<WndClass, _, _>(&mut engine, va, |wc| {
            assert_eq!(wc.style, 1);
            assert_eq!(wc.window_proc, 0x0808_0808_0808_0808);
            assert_eq!(wc.class_name_ptr, 0x5000);
            Ok(())
        })
        .expect("typed WNDCLASS read");
    }

    #[test]
    fn wnd_class_write_zero_fills_padding_between_style_and_wndproc() {
        let mut engine = test_engine();
        let va = 0x7200_u64;
        with_typed_write::<WndClass, _, _>(&mut engine, va, |wc| {
            wc.style = 7;
            wc.window_proc = 0x1234_5678_9ABC_DEF0;
            wc.class_name_ptr = 0x6000;
            Ok(())
        })
        .expect("typed WNDCLASS write");
        let bytes = raw_bytes(&mut engine, va, 72);
        assert_eq!(&bytes[0..4], &7_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "style→lpfnWndProc padding zeroed");
        assert_eq!(&bytes[8..16], &0x1234_5678_9ABC_DEF0_u64.to_le_bytes());
        assert_eq!(&bytes[64..72], &0x6000_u64.to_le_bytes());
    }

    #[test]
    fn create_struct_write_matches_hand_written_byte_pattern() {
        let mut engine = test_engine();
        let va = 0x7300_u64;
        with_typed_write::<CreateStruct, _, _>(&mut engine, va, |cs| {
            cs.create_params = 0xAAAA;
            cs.instance_handle = 0xBBBB;
            cs.menu_handle = 0xCCCC;
            cs.parent_handle = 0xDDDD;
            cs.cy = 480;
            cs.cx = 640;
            cs.y = 20;
            cs.x = 10;
            cs.style = 0x10CF0000;
            cs.name_ptr = 0x5000;
            cs.class_ptr = 0x5100;
            cs.extended_style = 0x0000_0100;
            Ok(())
        })
        .expect("typed CREATESTRUCT write");
        let bytes = raw_bytes(&mut engine, va, 0x50);
        let mut expected = vec![0_u8; 0x50];
        expected[0x00..0x08].copy_from_slice(&0xAAAA_u64.to_le_bytes());
        expected[0x08..0x10].copy_from_slice(&0xBBBB_u64.to_le_bytes());
        expected[0x10..0x18].copy_from_slice(&0xCCCC_u64.to_le_bytes());
        expected[0x18..0x20].copy_from_slice(&0xDDDD_u64.to_le_bytes());
        expected[0x20..0x24].copy_from_slice(&480_i32.to_le_bytes());
        expected[0x24..0x28].copy_from_slice(&640_i32.to_le_bytes());
        expected[0x28..0x2C].copy_from_slice(&20_i32.to_le_bytes());
        expected[0x2C..0x30].copy_from_slice(&10_i32.to_le_bytes());
        expected[0x30..0x34].copy_from_slice(&0x10CF0000_u32.to_le_bytes());
        // bytes 0x34..0x38: style→lpszName alignment padding, zero.
        expected[0x38..0x40].copy_from_slice(&0x5000_u64.to_le_bytes());
        expected[0x40..0x48].copy_from_slice(&0x5100_u64.to_le_bytes());
        expected[0x48..0x4C].copy_from_slice(&0x0000_0100_u32.to_le_bytes());
        // bytes 0x4C..0x50: trailing alignment padding, zero.
        assert_eq!(bytes, expected, "CREATESTRUCT byte pattern drift");
    }

    #[test]
    fn rect_and_point_read_write_round_trip_and_stage() {
        let mut engine = test_engine();
        // WinRect write/read round trip at an aligned address.
        with_typed_write::<WinRect, _, _>(&mut engine, 0x7400, |rect| {
            rect.left = -5;
            rect.top = 10;
            rect.right = 320;
            rect.bottom = 200;
            Ok(())
        })
        .expect("typed RECT write");
        with_typed_read::<WinRect, _, _>(&mut engine, 0x7400, |rect| {
            assert_eq!(
                (rect.left, rect.top, rect.right, rect.bottom),
                (-5, 10, 320, 200)
            );
            Ok(())
        })
        .expect("typed RECT read");

        // WinPoint write at an odd (misaligned) VA: the staging fallback must
        // produce byte-identical output.
        with_typed_write::<WinPoint, _, _>(&mut engine, 0x7501, |point| {
            point.x = 42;
            point.y = -7;
            Ok(())
        })
        .expect("staged typed POINT write");
        let bytes = raw_bytes(&mut engine, 0x7501, 8);
        assert_eq!(&bytes[0..4], &42_i32.to_le_bytes());
        assert_eq!(&bytes[4..8], &(-7_i32).to_le_bytes());
    }

    #[test]
    fn menu_item_info_write_preserves_pads_and_all_fields() {
        let mut engine = test_engine();
        let va = 0x7600_u64;
        with_typed_write::<MenuItemInfo, _, _>(&mut engine, va, |info| {
            info.cb_size = 80;
            info.f_mask = 0x20;
            info.f_type = 0;
            info.f_state = 0;
            info.w_id = 0x101;
            info.type_data_ptr = 0x6000;
            info.cch = 12;
            info.item_bitmap = 0x6600_0001;
            Ok(())
        })
        .expect("typed MENUITEMINFO write");
        let bytes = raw_bytes(&mut engine, va, 80);
        assert_eq!(&bytes[0..4], &80_u32.to_le_bytes(), "cbSize");
        assert_eq!(&bytes[4..8], &0x20_u32.to_le_bytes(), "fMask");
        assert_eq!(&bytes[16..20], &0x101_u32.to_le_bytes(), "wID");
        assert_eq!(&bytes[20..24], &[0; 4], "wID→hSubMenu padding");
        assert_eq!(&bytes[24..32], &0_u64.to_le_bytes(), "hSubMenu");
        assert_eq!(&bytes[56..64], &0x6000_u64.to_le_bytes(), "dwTypeData");
        assert_eq!(&bytes[64..68], &12_u32.to_le_bytes(), "cch");
        assert_eq!(&bytes[68..72], &[0; 4], "cch→hbmpItem padding");
        assert_eq!(&bytes[72..80], &0x6600_0001_u64.to_le_bytes(), "hbmpItem");
    }

    #[test]
    fn track_mouse_event_read_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let va = 0x7700_u64;
        let mut bytes = vec![0_u8; 24];
        bytes[0..4].copy_from_slice(&24_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&2_u32.to_le_bytes()); // TME_LEAVE
        bytes[8..16].copy_from_slice(&0x1234_5678_9ABC_DEF0_u64.to_le_bytes());
        bytes[16..20].copy_from_slice(&500_u32.to_le_bytes());
        engine
            .mem_write(va, &bytes)
            .expect("write raw TRACKMOUSEEVENT");
        with_typed_read::<TrackMouseEvent, _, _>(&mut engine, va, |tme| {
            assert_eq!(tme.cb_size, 24);
            assert_eq!(tme.flags, 2);
            assert_eq!(tme.track_window_handle, 0x1234_5678_9ABC_DEF0);
            assert_eq!(tme.hover_time, 500);
            Ok(())
        })
        .expect("typed TRACKMOUSEEVENT read");
    }
}

// --- kernel32 lane: WIN32_FIND_DATA header / STARTUPINFO / BY_HANDLE_FILE_INFORMATION / ---
// --- WIN32_FILE_ATTRIBUTE_DATA / SYSTEMTIME (sizes verified against mingw-w64 14.0.0) ---

/// Win64 `WIN32_FIND_DATA{A,W}` common header (minwinbase.h): `DWORD
/// dwFileAttributes` @0, `FILETIME ftCreationTime` @4 (`dwLowDateTime` @4,
/// `dwHighDateTime` @8), `ftLastAccessTime` @12, `ftLastWriteTime` @20,
/// `nFileSizeHigh` @28, `nFileSizeLow` @32, `dwReserved0` @36, `dwReserved1`
/// @40 — 44 bytes, align 4.
///
/// The A and W variants share this header exactly; they differ only in the
/// trailing name fields (`WCHAR cFileName[260]` @44 for W, `CHAR
/// cFileName[260]` @44 for A), which the callers write with the existing
/// string-write path. `FILETIME` is two `DWORD`s, not a `u64`, so the time
/// fields are split low/high to keep the struct `u32`-aligned throughout.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct FindDataHeader {
    pub(crate) dw_file_attributes: u32,
    pub(crate) ft_creation_time_low: u32,
    pub(crate) ft_creation_time_high: u32,
    pub(crate) ft_last_access_time_low: u32,
    pub(crate) ft_last_access_time_high: u32,
    pub(crate) ft_last_write_time_low: u32,
    pub(crate) ft_last_write_time_high: u32,
    pub(crate) n_file_size_high: u32,
    pub(crate) n_file_size_low: u32,
    pub(crate) dw_reserved0: u32,
    pub(crate) dw_reserved1: u32,
}

/// Offset of `cFileName` in both variants (`0x2C`).
pub(crate) const FIND_DATA_FILE_NAME_OFFSET: u64 = 44;
/// Offset of `cAlternateFileName` in the W variant (`0x234`: 44 + 260×2).
pub(crate) const FIND_DATA_W_ALT_NAME_OFFSET: u64 = 564;
/// Offset of `cAlternateFileName` in the A variant (44 + 260).
pub(crate) const FIND_DATA_A_ALT_NAME_OFFSET: u64 = 304;

/// Compile-time layout check for [`FindDataHeader`] plus the full-struct name
/// offsets. The W struct is 592 bytes (44 + 520 + 28); the A struct is 318
/// payload bytes (44 + 260 + 14) rounded to 320 by its 4-byte alignment.
const _: () = {
    assert!(
        core::mem::size_of::<FindDataHeader>() == 44,
        "WIN32_FIND_DATA header must be 44 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, dw_file_attributes) == 0,
        "dwFileAttributes @0"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, ft_creation_time_low) == 4,
        "ftCreationTime.low @4"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, ft_creation_time_high) == 8,
        "ftCreationTime.high @8"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, ft_last_access_time_low) == 12,
        "ftLastAccessTime.low @12"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, ft_last_write_time_low) == 20,
        "ftLastWriteTime.low @20"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, n_file_size_high) == 28,
        "nFileSizeHigh @28"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, n_file_size_low) == 32,
        "nFileSizeLow @32"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, dw_reserved0) == 36,
        "dwReserved0 @36"
    );
    assert!(
        core::mem::offset_of!(FindDataHeader, dw_reserved1) == 40,
        "dwReserved1 @40"
    );
    assert!(FIND_DATA_FILE_NAME_OFFSET == 44, "cFileName @44 (0x2C)");
    assert!(
        FIND_DATA_W_ALT_NAME_OFFSET == 564,
        "W cAlternateFileName @564 (0x234)"
    );
    assert!(
        FIND_DATA_A_ALT_NAME_OFFSET == 304,
        "A cAlternateFileName @304"
    );
};

/// Win64 `STARTUPINFOW` / `STARTUPINFOA` (winbase.h) — layout-identical on
/// Win64 because the ANSI variant's character pointers are still 8 bytes:
/// `DWORD cb` @0, [pad @4], `LPWSTR lpReserved` @8, `lpDesktop` @16,
/// `lpTitle` @24, `DWORD dwX` @32, `dwY` @36, `dwXSize` @40, `dwYSize` @44,
/// `dwXCountChars` @48, `dwYCountChars` @52, `dwFillAttribute` @56,
/// `dwFlags` @60, `WORD wShowWindow` @64, `WORD cbReserved2` @66, [pad @68],
/// `LPBYTE lpReserved2` @72, `HANDLE hStdInput` @80, `hStdOutput` @88,
/// `hStdError` @96 — 104 bytes, align 8.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct StartupInfo {
    pub(crate) cb: u32,
    /// Win64 alignment padding between `cb` and `lpReserved`.
    pub(crate) _pad0: [u8; 4],
    pub(crate) lp_reserved: u64,
    pub(crate) lp_desktop: u64,
    pub(crate) lp_title: u64,
    pub(crate) dw_x: u32,
    pub(crate) dw_y: u32,
    pub(crate) dw_x_size: u32,
    pub(crate) dw_y_size: u32,
    pub(crate) dw_x_count_chars: u32,
    pub(crate) dw_y_count_chars: u32,
    pub(crate) dw_fill_attribute: u32,
    pub(crate) dw_flags: u32,
    pub(crate) w_show_window: u16,
    pub(crate) cb_reserved2: u16,
    /// Win64 alignment padding between `cbReserved2` and `lpReserved2`.
    pub(crate) _pad1: [u8; 4],
    pub(crate) lp_reserved2: u64,
    pub(crate) h_std_input: u64,
    pub(crate) h_std_output: u64,
    pub(crate) h_std_error: u64,
}

/// Compile-time layout check for [`StartupInfo`]: `cb = 104` matches the
/// documented `STARTUPINFOW`/`STARTUPINFOA` size.
const _: () = {
    assert!(
        core::mem::size_of::<StartupInfo>() == 104,
        "STARTUPINFO must be 104 bytes on Win64"
    );
    assert!(core::mem::offset_of!(StartupInfo, cb) == 0, "cb @0");
    assert!(
        core::mem::offset_of!(StartupInfo, lp_reserved) == 8,
        "lpReserved @8"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, lp_desktop) == 16,
        "lpDesktop @16"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, lp_title) == 24,
        "lpTitle @24"
    );
    assert!(core::mem::offset_of!(StartupInfo, dw_x) == 32, "dwX @32");
    assert!(core::mem::offset_of!(StartupInfo, dw_y) == 36, "dwY @36");
    assert!(
        core::mem::offset_of!(StartupInfo, dw_x_size) == 40,
        "dwXSize @40"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_y_size) == 44,
        "dwYSize @44"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_x_count_chars) == 48,
        "dwXCountChars @48"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_y_count_chars) == 52,
        "dwYCountChars @52"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_fill_attribute) == 56,
        "dwFillAttribute @56"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, dw_flags) == 60,
        "dwFlags @60"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, w_show_window) == 64,
        "wShowWindow @64"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, cb_reserved2) == 66,
        "cbReserved2 @66"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, lp_reserved2) == 72,
        "lpReserved2 @72"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, h_std_input) == 80,
        "hStdInput @80"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, h_std_output) == 88,
        "hStdOutput @88"
    );
    assert!(
        core::mem::offset_of!(StartupInfo, h_std_error) == 96,
        "hStdError @96"
    );
};

/// Win64 `BY_HANDLE_FILE_INFORMATION` (fileapi.h, `GetFileInformationByHandle`):
/// `DWORD dwFileAttributes` @0, `FILETIME ftCreationTime` @4,
/// `ftLastAccessTime` @12, `ftLastWriteTime` @20, `DWORD dwVolumeSerialNumber`
/// @28, `nFileSizeHigh` @32, `nFileSizeLow` @36, `nNumberOfLinks` @40,
/// `nFileIndexHigh` @44, `nFileIndexLow` @48 — 52 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct ByHandleFileInformation {
    pub(crate) dw_file_attributes: u32,
    pub(crate) ft_creation_time_low: u32,
    pub(crate) ft_creation_time_high: u32,
    pub(crate) ft_last_access_time_low: u32,
    pub(crate) ft_last_access_time_high: u32,
    pub(crate) ft_last_write_time_low: u32,
    pub(crate) ft_last_write_time_high: u32,
    pub(crate) dw_volume_serial_number: u32,
    pub(crate) n_file_size_high: u32,
    pub(crate) n_file_size_low: u32,
    pub(crate) n_number_of_links: u32,
    pub(crate) n_file_index_high: u32,
    pub(crate) n_file_index_low: u32,
}

/// Compile-time layout check for [`ByHandleFileInformation`].
const _: () = {
    assert!(
        core::mem::size_of::<ByHandleFileInformation>() == 52,
        "BY_HANDLE_FILE_INFORMATION must be 52 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, dw_file_attributes) == 0,
        "dwFileAttributes @0"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, ft_creation_time_low) == 4,
        "ftCreationTime.low @4"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, ft_last_access_time_low) == 12,
        "ftLastAccessTime.low @12"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, ft_last_write_time_low) == 20,
        "ftLastWriteTime.low @20"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, dw_volume_serial_number) == 28,
        "dwVolumeSerialNumber @28"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_file_size_high) == 32,
        "nFileSizeHigh @32"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_file_size_low) == 36,
        "nFileSizeLow @36"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_number_of_links) == 40,
        "nNumberOfLinks @40"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_file_index_high) == 44,
        "nFileIndexHigh @44"
    );
    assert!(
        core::mem::offset_of!(ByHandleFileInformation, n_file_index_low) == 48,
        "nFileIndexLow @48"
    );
};

/// Win64 `WIN32_FILE_ATTRIBUTE_DATA` (fileapi.h, `GetFileAttributesEx`):
/// `DWORD dwFileAttributes` @0, `FILETIME ftCreationTime` @4,
/// `ftLastAccessTime` @12, `ftLastWriteTime` @20, `nFileSizeHigh` @28,
/// `nFileSizeLow` @32 — 36 bytes, align 4.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct FileAttributeData {
    pub(crate) dw_file_attributes: u32,
    pub(crate) ft_creation_time_low: u32,
    pub(crate) ft_creation_time_high: u32,
    pub(crate) ft_last_access_time_low: u32,
    pub(crate) ft_last_access_time_high: u32,
    pub(crate) ft_last_write_time_low: u32,
    pub(crate) ft_last_write_time_high: u32,
    pub(crate) n_file_size_high: u32,
    pub(crate) n_file_size_low: u32,
}

/// Compile-time layout check for [`FileAttributeData`].
const _: () = {
    assert!(
        core::mem::size_of::<FileAttributeData>() == 36,
        "WIN32_FILE_ATTRIBUTE_DATA must be 36 bytes on Win64"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, dw_file_attributes) == 0,
        "dwFileAttributes @0"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, ft_creation_time_low) == 4,
        "ftCreationTime.low @4"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, ft_last_access_time_low) == 12,
        "ftLastAccessTime.low @12"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, ft_last_write_time_low) == 20,
        "ftLastWriteTime.low @20"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, n_file_size_high) == 28,
        "nFileSizeHigh @28"
    );
    assert!(
        core::mem::offset_of!(FileAttributeData, n_file_size_low) == 32,
        "nFileSizeLow @32"
    );
};

/// Win64 `SYSTEMTIME` (minwinbase.h): eight `WORD` fields, `wYear` @0 through
/// `wMilliseconds` @14 — 16 bytes, align 2.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct SystemTime {
    pub(crate) w_year: u16,
    pub(crate) w_month: u16,
    pub(crate) w_day_of_week: u16,
    pub(crate) w_day: u16,
    pub(crate) w_hour: u16,
    pub(crate) w_minute: u16,
    pub(crate) w_second: u16,
    pub(crate) w_milliseconds: u16,
}

/// Compile-time layout check for [`SystemTime`].
const _: () = {
    assert!(
        core::mem::size_of::<SystemTime>() == 16,
        "SYSTEMTIME must be 16 bytes on Win64"
    );
    assert!(core::mem::offset_of!(SystemTime, w_year) == 0, "wYear @0");
    assert!(core::mem::offset_of!(SystemTime, w_month) == 2, "wMonth @2");
    assert!(
        core::mem::offset_of!(SystemTime, w_day_of_week) == 4,
        "wDayOfWeek @4"
    );
    assert!(core::mem::offset_of!(SystemTime, w_day) == 6, "wDay @6");
    assert!(core::mem::offset_of!(SystemTime, w_hour) == 8, "wHour @8");
    assert!(
        core::mem::offset_of!(SystemTime, w_minute) == 10,
        "wMinute @10"
    );
    assert!(
        core::mem::offset_of!(SystemTime, w_second) == 12,
        "wSecond @12"
    );
    assert!(
        core::mem::offset_of!(SystemTime, w_milliseconds) == 14,
        "wMilliseconds @14"
    );
};

// --- comdlg32 lane: OPENFILENAME / FINDREPLACE (the dialog structs) --------

/// Win64 `OPENFILENAMEW` (commdlg.h): `DWORD lStructSize` @0, [pad @4], `HWND
/// hwndOwner` @8, `HINSTANCE hInstance` @16, `LPCWSTR lpstrFilter` @24, `LPWSTR
/// lpstrCustomFilter` @32, `DWORD nMaxCustFilter` @40, `nFilterIndex` @44,
/// `LPWSTR lpstrFile` @48, `DWORD nMaxFile` @56, [pad @60], `LPWSTR
/// lpstrFileTitle` @64, `DWORD nMaxFileTitle` @72, [pad @76], `LPCWSTR
/// lpstrInitialDir` @80, `lpstrTitle` @88, `DWORD Flags` @96, `WORD
/// nFileOffset` @100, `nFileExtension` @102, `LPCWSTR lpstrDefExt` @104,
/// `LPARAM lCustData` @112, `LPOFNHOOKPROC lpfnHook` @120, `LPCWSTR
/// lpTemplateName` @128, `void* pvReserved` @136, `DWORD dwReserved` @144,
/// `FlagsEx` @148 — 152 bytes, align 8.
///
/// Sizes and offsets verified against mingw-w64 14.0.0 `commdlg.h`, whose
/// `OPENFILENAMEW` carries the Vista+ reserved tail (`pvReserved` /
/// `dwReserved` / `FlagsEx`). The A-variant (`OPENFILENAMEA`) has the
/// identical layout — the strings are ANSI but every offset matches — so the
/// host reads both through this one type. The `_pad` fields follow the
/// `IntoBytes` explicit-padding rule; the read-modify-write path restores the
/// guest's original pad bytes.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct OpenFileName {
    pub(crate) l_struct_size: u32,
    /// Win64 alignment padding before `hwndOwner`.
    pub(crate) _pad: [u8; 4],
    pub(crate) hwnd_owner: u64,
    pub(crate) h_instance: u64,
    pub(crate) lpstr_filter: u64,
    pub(crate) lpstr_custom_filter: u64,
    pub(crate) n_max_cust_filter: u32,
    pub(crate) n_filter_index: u32,
    pub(crate) lpstr_file: u64,
    pub(crate) n_max_file: u32,
    /// Win64 alignment padding before `lpstrFileTitle`.
    pub(crate) _pad2: [u8; 4],
    pub(crate) lpstr_file_title: u64,
    pub(crate) n_max_file_title: u32,
    /// Win64 alignment padding before `lpstrInitialDir`.
    pub(crate) _pad3: [u8; 4],
    pub(crate) lpstr_initial_dir: u64,
    pub(crate) lpstr_title: u64,
    pub(crate) flags: u32,
    pub(crate) n_file_offset: u16,
    pub(crate) n_file_extension: u16,
    pub(crate) lpstr_def_ext: u64,
    pub(crate) l_cust_data: u64,
    pub(crate) lpfn_hook: u64,
    pub(crate) lp_template_name: u64,
    /// Vista+ reserved tail (mingw-w64 commdlg.h) — read/written whole so the
    /// guest's `pvReserved`/`dwReserved`/`FlagsEx` survive the write-back.
    pub(crate) pv_reserved: u64,
    pub(crate) dw_reserved: u32,
    pub(crate) flags_ex: u32,
}

/// Compile-time layout check for [`OpenFileName`].
const _: () = {
    assert!(
        core::mem::size_of::<OpenFileName>() == 152,
        "OPENFILENAMEW must be 152 bytes on Win64 (mingw-w64 14.0.0)"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, l_struct_size) == 0,
        "lStructSize @0"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, _pad) == 4,
        "OPENFILENAME pad @4"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, hwnd_owner) == 8,
        "hwndOwner @8"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, h_instance) == 16,
        "hInstance @16"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_filter) == 24,
        "lpstrFilter @24"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_custom_filter) == 32,
        "lpstrCustomFilter @32"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_max_cust_filter) == 40,
        "nMaxCustFilter @40"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_filter_index) == 44,
        "nFilterIndex @44"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_file) == 48,
        "lpstrFile @48"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_max_file) == 56,
        "nMaxFile @56"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, _pad2) == 60,
        "OPENFILENAME pad @60"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_file_title) == 64,
        "lpstrFileTitle @64"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_max_file_title) == 72,
        "nMaxFileTitle @72"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, _pad3) == 76,
        "OPENFILENAME pad @76"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_initial_dir) == 80,
        "lpstrInitialDir @80"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_title) == 88,
        "lpstrTitle @88"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, flags) == 96,
        "Flags @96"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_file_offset) == 100,
        "nFileOffset @100"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, n_file_extension) == 102,
        "nFileExtension @102"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpstr_def_ext) == 104,
        "lpstrDefExt @104"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, l_cust_data) == 112,
        "lCustData @112"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lpfn_hook) == 120,
        "lpfnHook @120"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, lp_template_name) == 128,
        "lpTemplateName @128"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, pv_reserved) == 136,
        "pvReserved @136"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, dw_reserved) == 144,
        "dwReserved @144"
    );
    assert!(
        core::mem::offset_of!(OpenFileName, flags_ex) == 148,
        "FlagsEx @148"
    );
};

/// Win64 `FINDREPLACEW` (commdlg.h): `DWORD lStructSize` @0, [pad @4], `HWND
/// hwndOwner` @8, `HINSTANCE hInstance` @16, `DWORD Flags` @24, [pad @28],
/// `LPWSTR lpstrFindWhat` @32, `lpstrReplaceWith` @40, `WORD wFindWhatLen`
/// @48, `wReplaceWithLen` @50, [pad @52], `LPARAM lCustData` @56,
/// `LPFRHOOKPROC lpfnHook` @64, `LPCWSTR lpTemplateName` @72 — 80 bytes,
/// align 8 (mingw-w64 14.0.0 commdlg.h; the structure is UNICODE regardless
/// of the A/W suffix of the creating API, so FindTextA/ReplaceTextA still use
/// this type).
///
/// The `_pad` fields follow the `IntoBytes` explicit-padding rule; the
/// read-modify-write paths (the `Flags` write-backs on submit/close) restore
/// the guest's original pad bytes.
#[derive(Debug, Clone, Copy, KnownLayout, Immutable, FromBytes, IntoBytes)]
#[repr(C)]
pub(crate) struct FindReplace {
    pub(crate) l_struct_size: u32,
    /// Win64 alignment padding before `hwndOwner`.
    pub(crate) _pad: [u8; 4],
    pub(crate) hwnd_owner: u64,
    pub(crate) h_instance: u64,
    pub(crate) flags: u32,
    /// Win64 alignment padding before the first string pointer.
    pub(crate) _pad2: [u8; 4],
    pub(crate) lpstr_find_what: u64,
    pub(crate) lpstr_replace_with: u64,
    pub(crate) w_find_what_len: u16,
    pub(crate) w_replace_with_len: u16,
    /// Win64 alignment padding before `lCustData`.
    pub(crate) _pad3: [u8; 4],
    pub(crate) l_cust_data: u64,
    pub(crate) lpfn_hook: u64,
    pub(crate) lp_template_name: u64,
}

/// Compile-time layout check for [`FindReplace`].
const _: () = {
    assert!(
        core::mem::size_of::<FindReplace>() == 80,
        "FINDREPLACEW must be 80 bytes on Win64 (mingw-w64 14.0.0)"
    );
    assert!(
        core::mem::offset_of!(FindReplace, l_struct_size) == 0,
        "lStructSize @0"
    );
    assert!(
        core::mem::offset_of!(FindReplace, _pad) == 4,
        "FINDREPLACE pad @4"
    );
    assert!(
        core::mem::offset_of!(FindReplace, hwnd_owner) == 8,
        "hwndOwner @8"
    );
    assert!(
        core::mem::offset_of!(FindReplace, h_instance) == 16,
        "hInstance @16"
    );
    assert!(core::mem::offset_of!(FindReplace, flags) == 24, "Flags @24");
    assert!(
        core::mem::offset_of!(FindReplace, _pad2) == 28,
        "FINDREPLACE pad @28"
    );
    assert!(
        core::mem::offset_of!(FindReplace, lpstr_find_what) == 32,
        "lpstrFindWhat @32"
    );
    assert!(
        core::mem::offset_of!(FindReplace, lpstr_replace_with) == 40,
        "lpstrReplaceWith @40"
    );
    assert!(
        core::mem::offset_of!(FindReplace, w_find_what_len) == 48,
        "wFindWhatLen @48"
    );
    assert!(
        core::mem::offset_of!(FindReplace, w_replace_with_len) == 50,
        "wReplaceWithLen @50"
    );
    assert!(
        core::mem::offset_of!(FindReplace, _pad3) == 52,
        "FINDREPLACE pad @52"
    );
    assert!(
        core::mem::offset_of!(FindReplace, l_cust_data) == 56,
        "lCustData @56"
    );
    assert!(
        core::mem::offset_of!(FindReplace, lpfn_hook) == 64,
        "lpfnHook @64"
    );
    assert!(
        core::mem::offset_of!(FindReplace, lp_template_name) == 72,
        "lpTemplateName @72"
    );
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod kernel32_lane_tests {
    use super::*;
    use crate::guest_memory::{with_typed_read, with_typed_write};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    /// Minimal engine with mapped guest memory for view round-trips. Each
    /// test builds a fresh engine, so VAs may repeat across tests.
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

    /// The `FIXED_SYSTEM_FILETIME` constant (kernel32/mod.rs) split into the
    /// (low, high) `DWORD` pair the layouts carry.
    fn fixed_filetime_parts() -> (u32, u32) {
        const FT: u64 = 133_485_408_000_000_000;
        (
            u32::try_from(FT & 0xffff_ffff).unwrap_or(0),
            u32::try_from(FT >> 32).unwrap_or(0),
        )
    }

    #[test]
    fn find_data_header_write_zero_fills_reserved_fields() {
        let mut engine = test_engine();
        let (ft_low, ft_high) = fixed_filetime_parts();
        with_typed_write::<FindDataHeader, _, _>(&mut engine, 0x9000, |header| {
            header.dw_file_attributes = 0x20;
            header.ft_creation_time_low = ft_low;
            header.ft_creation_time_high = ft_high;
            header.ft_last_access_time_low = ft_low;
            header.ft_last_access_time_high = ft_high;
            header.ft_last_write_time_low = ft_low;
            header.ft_last_write_time_high = ft_high;
            header.n_file_size_high = 1;
            header.n_file_size_low = 2;
            // dwReserved0 / dwReserved1 deliberately left unset: the view
            // starts zeroed (the hand-built header zeroed them explicitly).
            Ok(())
        })
        .expect("typed header write");
        let bytes = raw_bytes(&mut engine, 0x9000, 44);
        assert_eq!(&bytes[0..4], &0x20_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &ft_low.to_le_bytes());
        assert_eq!(&bytes[8..12], &ft_high.to_le_bytes());
        assert_eq!(&bytes[12..16], &ft_low.to_le_bytes(), "access time.low");
        assert_eq!(&bytes[16..20], &ft_high.to_le_bytes(), "access time.high");
        assert_eq!(&bytes[20..24], &ft_low.to_le_bytes(), "write time.low");
        assert_eq!(&bytes[24..28], &ft_high.to_le_bytes(), "write time.high");
        assert_eq!(&bytes[28..32], &1_u32.to_le_bytes());
        assert_eq!(&bytes[32..36], &2_u32.to_le_bytes());
        assert_eq!(&bytes[36..44], &[0; 8], "dwReserved0/1 zero-filled");
    }

    #[test]
    fn find_data_header_read_view_matches_raw_guest_bytes() {
        let mut engine = test_engine();
        let mut bytes = vec![0_u8; 44];
        bytes[0..4].copy_from_slice(&0x10_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&7_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&8_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&9_u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&11_u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&12_u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&0xDEAD_BEEF_u32.to_le_bytes());
        engine
            .mem_write(0x9000, &bytes)
            .expect("write raw header bytes");
        with_typed_read::<FindDataHeader, _, _>(&mut engine, 0x9000, |header| {
            assert_eq!(header.dw_file_attributes, 0x10);
            assert_eq!(header.ft_creation_time_low, 7);
            assert_eq!(header.ft_creation_time_high, 8);
            assert_eq!(header.ft_last_access_time_low, 9);
            assert_eq!(header.ft_last_access_time_high, 0);
            assert_eq!(header.ft_last_write_time_low, 0);
            assert_eq!(header.n_file_size_high, 11);
            assert_eq!(header.n_file_size_low, 12);
            assert_eq!(header.dw_reserved0, 0);
            assert_eq!(header.dw_reserved1, 0xDEAD_BEEF);
            Ok(())
        })
        .expect("typed header read");
    }

    #[test]
    fn find_data_w_full_round_trip_includes_names() {
        // Full 592-byte WIN32_FIND_DATAW: the 44-byte header via the typed
        // view, the name via the existing string-write path. Pre-fill with
        // 0xAA so untouched tail bytes are observable (the old code only
        // wrote the name + NUL and one alternate-name NUL, leaving the rest
        // as caller garbage — this test pins that semantics).
        let mut engine = test_engine();
        engine
            .mem_write(0x9100, &[0xAA_u8; 592])
            .expect("prefill WIN32_FIND_DATAW");
        crate::kernel32::file_io::write_find_data_w(
            &mut engine,
            0x9100,
            "test.txt",
            0x20,
            0x1_0000_0042,
        )
        .expect("write_find_data_w");
        let bytes = raw_bytes(&mut engine, 0x9100, 592);
        assert_eq!(&bytes[0..4], &0x20_u32.to_le_bytes(), "dwFileAttributes");
        let (ft_low, ft_high) = fixed_filetime_parts();
        assert_eq!(&bytes[4..8], &ft_low.to_le_bytes(), "ftCreationTime.low");
        assert_eq!(&bytes[8..12], &ft_high.to_le_bytes(), "ftCreationTime.high");
        assert_eq!(&bytes[12..20], &bytes[4..12], "access mirrors creation");
        assert_eq!(&bytes[20..28], &bytes[4..12], "write mirrors creation");
        assert_eq!(&bytes[28..32], &1_u32.to_le_bytes(), "nFileSizeHigh");
        assert_eq!(&bytes[32..36], &0x42_u32.to_le_bytes(), "nFileSizeLow");
        assert_eq!(&bytes[36..44], &[0; 8], "dwReserved0/1");
        let mut name = Vec::new();
        for unit in "test.txt".encode_utf16() {
            name.extend_from_slice(&unit.to_le_bytes());
        }
        name.extend_from_slice(&0_u16.to_le_bytes());
        assert_eq!(&bytes[44..44 + name.len()], &name, "cFileName");
        let tail_len = 564 - 44 - name.len();
        assert_eq!(
            &bytes[44 + name.len()..564],
            &vec![0xAA_u8; tail_len],
            "cFileName tail untouched (old semantics)"
        );
        assert_eq!(&bytes[564..566], &[0, 0], "cAlternateFileName NUL");
        assert_eq!(&bytes[566..592], &[0xAA_u8; 26], "alternate tail untouched");
    }

    #[test]
    fn startup_info_write_matches_get_startup_info_semantics() {
        // Mirror of the state/tests.rs GetStartupInfoW assertions: cb = 104,
        // dwFlags = 0, wShowWindow = 1, everything else zeroed.
        let mut engine = test_engine();
        engine
            .mem_write(0x9200, &[0xAA_u8; 104])
            .expect("prefill STARTUPINFO");
        with_typed_write::<StartupInfo, _, _>(&mut engine, 0x9200, |info| {
            info.cb = 104;
            info.dw_flags = 0;
            info.w_show_window = 1;
            Ok(())
        })
        .expect("typed STARTUPINFO write");
        let bytes = raw_bytes(&mut engine, 0x9200, 104);
        assert_eq!(&bytes[0..4], &104_u32.to_le_bytes());
        assert_eq!(&bytes[64..66], &1_u16.to_le_bytes());
        let mut nonzero_offsets: Vec<usize> = Vec::new();
        for (offset, &byte) in bytes.iter().enumerate() {
            if byte != 0 {
                nonzero_offsets.push(offset);
            }
        }
        assert_eq!(
            nonzero_offsets,
            vec![0, 64],
            "only cb and wShowWindow may be nonzero; the rest must be zeroed"
        );
    }

    #[test]
    fn by_handle_file_information_write_matches_hand_written_pattern() {
        let mut engine = test_engine();
        let (ft_low, ft_high) = fixed_filetime_parts();
        with_typed_write::<ByHandleFileInformation, _, _>(&mut engine, 0x9300, |info| {
            info.dw_file_attributes = 0x20;
            info.ft_creation_time_low = ft_low;
            info.ft_creation_time_high = ft_high;
            info.ft_last_access_time_low = ft_low;
            info.ft_last_access_time_high = ft_high;
            info.ft_last_write_time_low = ft_low;
            info.ft_last_write_time_high = ft_high;
            info.dw_volume_serial_number = 0x1234_abcd;
            info.n_file_size_high = 0x12;
            info.n_file_size_low = 0x3456_7890;
            info.n_number_of_links = 1;
            info.n_file_index_high = 0;
            info.n_file_index_low = 1;
            Ok(())
        })
        .expect("typed BY_HANDLE write");
        let bytes = raw_bytes(&mut engine, 0x9300, 52);
        let mut expected = vec![0_u8; 52];
        expected[0..4].copy_from_slice(&0x20_u32.to_le_bytes());
        expected[4..12].copy_from_slice(&133_485_408_000_000_000_u64.to_le_bytes());
        expected[12..20].copy_from_slice(&133_485_408_000_000_000_u64.to_le_bytes());
        expected[20..28].copy_from_slice(&133_485_408_000_000_000_u64.to_le_bytes());
        expected[28..32].copy_from_slice(&0x1234_abcd_u32.to_le_bytes());
        expected[32..36].copy_from_slice(&0x12_u32.to_le_bytes());
        expected[36..40].copy_from_slice(&0x3456_7890_u32.to_le_bytes());
        expected[40..44].copy_from_slice(&1_u32.to_le_bytes());
        expected[44..48].copy_from_slice(&0_u32.to_le_bytes());
        expected[48..52].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(bytes, expected, "BY_HANDLE_FILE_INFORMATION byte drift");
    }

    #[test]
    fn file_attribute_data_write_and_read_back() {
        let mut engine = test_engine();
        let (ft_low, ft_high) = fixed_filetime_parts();
        with_typed_write::<FileAttributeData, _, _>(&mut engine, 0x9400, |data| {
            data.dw_file_attributes = 0x10;
            data.ft_creation_time_low = ft_low;
            data.ft_creation_time_high = ft_high;
            data.ft_last_access_time_low = ft_low;
            data.ft_last_access_time_high = ft_high;
            data.ft_last_write_time_low = ft_low;
            data.ft_last_write_time_high = ft_high;
            data.n_file_size_high = 0;
            data.n_file_size_low = 1234;
            Ok(())
        })
        .expect("typed WIN32_FILE_ATTRIBUTE_DATA write");
        let bytes = raw_bytes(&mut engine, 0x9400, 36);
        assert_eq!(&bytes[0..4], &0x10_u32.to_le_bytes());
        assert_eq!(&bytes[4..12], &133_485_408_000_000_000_u64.to_le_bytes());
        assert_eq!(&bytes[12..20], &133_485_408_000_000_000_u64.to_le_bytes());
        assert_eq!(&bytes[20..28], &133_485_408_000_000_000_u64.to_le_bytes());
        assert_eq!(&bytes[28..32], &0_u32.to_le_bytes());
        assert_eq!(&bytes[32..36], &1234_u32.to_le_bytes());

        with_typed_read::<FileAttributeData, _, _>(&mut engine, 0x9400, |data| {
            assert_eq!(data.dw_file_attributes, 0x10);
            assert_eq!(data.ft_creation_time_low, ft_low);
            assert_eq!(data.ft_creation_time_high, ft_high);
            assert_eq!(data.ft_last_write_time_high, ft_high);
            assert_eq!(data.n_file_size_low, 1234);
            Ok(())
        })
        .expect("typed WIN32_FILE_ATTRIBUTE_DATA read");
    }

    #[test]
    fn system_time_write_and_misaligned_stage_match_raw() {
        let mut engine = test_engine();
        with_typed_write::<SystemTime, _, _>(&mut engine, 0x9500, |st| {
            st.w_year = 2026;
            st.w_month = 7;
            st.w_day_of_week = 4;
            st.w_day = 9;
            st.w_hour = 12;
            st.w_minute = 30;
            st.w_second = 45;
            st.w_milliseconds = 100;
            Ok(())
        })
        .expect("typed SYSTEMTIME write");
        let bytes = raw_bytes(&mut engine, 0x9500, 16);
        assert_eq!(&bytes[0..2], &2026_u16.to_le_bytes());
        assert_eq!(&bytes[2..4], &7_u16.to_le_bytes());
        assert_eq!(&bytes[4..6], &4_u16.to_le_bytes());
        assert_eq!(&bytes[6..8], &9_u16.to_le_bytes());
        assert_eq!(&bytes[8..10], &12_u16.to_le_bytes());
        assert_eq!(&bytes[10..12], &30_u16.to_le_bytes());
        assert_eq!(&bytes[12..14], &45_u16.to_le_bytes());
        assert_eq!(&bytes[14..16], &100_u16.to_le_bytes());

        // Odd guest address: the helper must stage into an aligned host
        // buffer and produce identical bytes.
        let mut staged_engine = test_engine();
        with_typed_write::<SystemTime, _, _>(&mut staged_engine, 0x9501, |st| {
            st.w_year = 2026;
            st.w_month = 7;
            st.w_day_of_week = 4;
            st.w_day = 9;
            st.w_hour = 12;
            st.w_minute = 30;
            st.w_second = 45;
            st.w_milliseconds = 100;
            Ok(())
        })
        .expect("staged SYSTEMTIME write");
        let staged = raw_bytes(&mut staged_engine, 0x9501, 16);
        assert_eq!(staged, bytes, "staged write must match aligned write");
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod comdlg32_lane_tests {
    use super::*;
    use crate::guest_memory::{
        read_typed_copy, with_typed_read, with_typed_write, write_typed_copy,
    };
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    /// Minimal engine with mapped guest memory for view round-trips. Each
    /// test builds a fresh engine, so VAs may repeat across tests.
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

    #[test]
    fn open_file_name_round_trip_pins_all_offsets_and_pads() {
        let mut engine = test_engine();
        let va = 0xA100_u64;
        with_typed_write::<OpenFileName, _, _>(&mut engine, va, |ofn| {
            ofn.l_struct_size = 152;
            ofn.hwnd_owner = 0x0102_0304_0506_0708;
            ofn.h_instance = 0x1112_1314_1516_1718;
            ofn.lpstr_filter = 0x2122_2324_2526_2728;
            ofn.lpstr_custom_filter = 0x3132_3334_3536_3738;
            ofn.n_max_cust_filter = 0x1111_2222;
            ofn.n_filter_index = 1;
            ofn.lpstr_file = 0x4142_4344_4546_4748;
            ofn.n_max_file = 260;
            ofn.lpstr_file_title = 0x5152_5354_5556_5758;
            ofn.n_max_file_title = 64;
            ofn.lpstr_initial_dir = 0x6162_6364_6566_6768;
            ofn.lpstr_title = 0x7172_7374_7576_7778;
            ofn.flags = 0x0000_0008;
            ofn.n_file_offset = 9;
            ofn.n_file_extension = 15;
            ofn.lpstr_def_ext = 0x8182_8384_8586_8788;
            ofn.l_cust_data = 0x9192_9394_9596_9798;
            ofn.lpfn_hook = 0xA1A2_A3A4_A5A6_A7A8;
            ofn.lp_template_name = 0xB1B2_B3B4_B5B6_B7B8;
            ofn.pv_reserved = 0xC1C2_C3C4_C5C6_C7C8;
            ofn.dw_reserved = 0xD1D2_D3D4;
            ofn.flags_ex = 0xE1E2_E3E4;
            Ok(())
        })
        .expect("typed OPENFILENAME write");
        let bytes = raw_bytes(&mut engine, va, 152);
        assert_eq!(&bytes[0..4], &152_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "lStructSize→hwndOwner pad");
        assert_eq!(&bytes[8..16], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x1112_1314_1516_1718_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &0x2122_2324_2526_2728_u64.to_le_bytes());
        assert_eq!(&bytes[32..40], &0x3132_3334_3536_3738_u64.to_le_bytes());
        assert_eq!(
            &bytes[40..44],
            &0x1111_2222_u32.to_le_bytes(),
            "nMaxCustFilter"
        );
        assert_eq!(&bytes[44..48], &1_u32.to_le_bytes(), "nFilterIndex");
        assert_eq!(&bytes[48..56], &0x4142_4344_4546_4748_u64.to_le_bytes());
        assert_eq!(&bytes[56..60], &260_u32.to_le_bytes(), "nMaxFile");
        assert_eq!(&bytes[60..64], &[0; 4], "nMaxFile→lpstrFileTitle pad");
        assert_eq!(&bytes[64..72], &0x5152_5354_5556_5758_u64.to_le_bytes());
        assert_eq!(&bytes[72..76], &64_u32.to_le_bytes(), "nMaxFileTitle");
        assert_eq!(&bytes[76..80], &[0; 4], "nMaxFileTitle→initialDir pad");
        assert_eq!(&bytes[80..88], &0x6162_6364_6566_6768_u64.to_le_bytes());
        assert_eq!(&bytes[88..96], &0x7172_7374_7576_7778_u64.to_le_bytes());
        assert_eq!(&bytes[96..100], &0x0000_0008_u32.to_le_bytes(), "Flags");
        assert_eq!(&bytes[100..102], &9_u16.to_le_bytes(), "nFileOffset");
        assert_eq!(&bytes[102..104], &15_u16.to_le_bytes(), "nFileExtension");
        assert_eq!(&bytes[104..112], &0x8182_8384_8586_8788_u64.to_le_bytes());
        assert_eq!(&bytes[112..120], &0x9192_9394_9596_9798_u64.to_le_bytes());
        assert_eq!(&bytes[120..128], &0xA1A2_A3A4_A5A6_A7A8_u64.to_le_bytes());
        assert_eq!(&bytes[128..136], &0xB1B2_B3B4_B5B6_B7B8_u64.to_le_bytes());
        assert_eq!(
            &bytes[136..144],
            &0xC1C2_C3C4_C5C6_C7C8_u64.to_le_bytes(),
            "pvReserved"
        );
        assert_eq!(
            &bytes[144..148],
            &0xD1D2_D3D4_u32.to_le_bytes(),
            "dwReserved"
        );
        assert_eq!(&bytes[148..152], &0xE1E2_E3E4_u32.to_le_bytes(), "FlagsEx");

        with_typed_read::<OpenFileName, _, _>(&mut engine, va, |ofn| {
            assert_eq!(ofn.l_struct_size, 152);
            assert_eq!(ofn.hwnd_owner, 0x0102_0304_0506_0708);
            assert_eq!(ofn.h_instance, 0x1112_1314_1516_1718);
            assert_eq!(ofn.lpstr_filter, 0x2122_2324_2526_2728);
            assert_eq!(ofn.lpstr_custom_filter, 0x3132_3334_3536_3738);
            assert_eq!(ofn.n_max_cust_filter, 0x1111_2222);
            assert_eq!(ofn.n_filter_index, 1);
            assert_eq!(ofn.lpstr_file, 0x4142_4344_4546_4748);
            assert_eq!(ofn.n_max_file, 260);
            assert_eq!(ofn.lpstr_file_title, 0x5152_5354_5556_5758);
            assert_eq!(ofn.n_max_file_title, 64);
            assert_eq!(ofn.lpstr_initial_dir, 0x6162_6364_6566_6768);
            assert_eq!(ofn.lpstr_title, 0x7172_7374_7576_7778);
            assert_eq!(ofn.flags, 0x0000_0008);
            assert_eq!(ofn.n_file_offset, 9);
            assert_eq!(ofn.n_file_extension, 15);
            assert_eq!(ofn.lpstr_def_ext, 0x8182_8384_8586_8788);
            assert_eq!(ofn.l_cust_data, 0x9192_9394_9596_9798);
            assert_eq!(ofn.lpfn_hook, 0xA1A2_A3A4_A5A6_A7A8);
            assert_eq!(ofn.lp_template_name, 0xB1B2_B3B4_B5B6_B7B8);
            assert_eq!(ofn.pv_reserved, 0xC1C2_C3C4_C5C6_C7C8);
            assert_eq!(ofn.dw_reserved, 0xD1D2_D3D4);
            assert_eq!(ofn.flags_ex, 0xE1E2_E3E4);
            Ok(())
        })
        .expect("typed OPENFILENAME read");
    }

    #[test]
    fn find_replace_round_trip_pins_word_and_pointer_offsets() {
        let mut engine = test_engine();
        let va = 0xA200_u64;
        with_typed_write::<FindReplace, _, _>(&mut engine, va, |fr| {
            fr.l_struct_size = 80;
            fr.hwnd_owner = 0x0102_0304_0506_0708;
            fr.h_instance = 0x1112_1314_1516_1718;
            fr.flags = 0x1 | 0x4 | 0x40;
            fr.lpstr_find_what = 0x2122_2324_2526_2728;
            fr.lpstr_replace_with = 0x3132_3334_3536_3738;
            fr.w_find_what_len = 260;
            fr.w_replace_with_len = 260;
            fr.l_cust_data = 0x4142_4344_4546_4748;
            fr.lpfn_hook = 0x5152_5354_5556_5758;
            fr.lp_template_name = 0x6162_6364_6566_6768;
            Ok(())
        })
        .expect("typed FINDREPLACE write");
        let bytes = raw_bytes(&mut engine, va, 80);
        assert_eq!(&bytes[0..4], &80_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4], "lStructSize→hwndOwner pad");
        assert_eq!(&bytes[8..16], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x1112_1314_1516_1718_u64.to_le_bytes());
        assert_eq!(
            &bytes[24..28],
            &(0x1_u32 | 0x4 | 0x40).to_le_bytes(),
            "Flags"
        );
        assert_eq!(&bytes[28..32], &[0; 4], "Flags→lpstrFindWhat pad");
        assert_eq!(&bytes[32..40], &0x2122_2324_2526_2728_u64.to_le_bytes());
        assert_eq!(&bytes[40..48], &0x3132_3334_3536_3738_u64.to_le_bytes());
        assert_eq!(&bytes[48..50], &260_u16.to_le_bytes(), "wFindWhatLen");
        assert_eq!(&bytes[50..52], &260_u16.to_le_bytes(), "wReplaceWithLen");
        assert_eq!(&bytes[52..56], &[0; 4], "lens→lCustData pad");
        assert_eq!(&bytes[56..64], &0x4142_4344_4546_4748_u64.to_le_bytes());
        assert_eq!(&bytes[64..72], &0x5152_5354_5556_5758_u64.to_le_bytes());
        assert_eq!(&bytes[72..80], &0x6162_6364_6566_6768_u64.to_le_bytes());

        with_typed_read::<FindReplace, _, _>(&mut engine, va, |fr| {
            assert_eq!(fr.l_struct_size, 80);
            assert_eq!(fr.hwnd_owner, 0x0102_0304_0506_0708);
            assert_eq!(fr.h_instance, 0x1112_1314_1516_1718);
            assert_eq!(fr.flags, 0x1 | 0x4 | 0x40);
            assert_eq!(fr.lpstr_find_what, 0x2122_2324_2526_2728);
            assert_eq!(fr.lpstr_replace_with, 0x3132_3334_3536_3738);
            assert_eq!(fr.w_find_what_len, 260);
            assert_eq!(fr.w_replace_with_len, 260);
            assert_eq!(fr.l_cust_data, 0x4142_4344_4546_4748);
            assert_eq!(fr.lpfn_hook, 0x5152_5354_5556_5758);
            assert_eq!(fr.lp_template_name, 0x6162_6364_6566_6768);
            Ok(())
        })
        .expect("typed FINDREPLACE read");
    }

    /// The `write_selected_path` pattern: snapshot → edit → whole-struct
    /// write-back must leave every other field (and pad) byte-identical —
    /// the OPENFILENAME write-back must not clobber the guest's untouched
    /// fields or the reserved tail.
    #[test]
    fn open_file_name_read_modify_write_preserves_untouched_fields() {
        let mut engine = test_engine();
        let va = 0xA300_u64;
        // Seed the guest struct with raw bytes, including nonzero pads and a
        // nonzero reserved tail.
        let mut bytes = vec![0_u8; 152];
        bytes[0..4].copy_from_slice(&152_u32.to_le_bytes());
        bytes[8..16].copy_from_slice(&0x00AA_0001_u64.to_le_bytes()); // hwndOwner
        bytes[48..56].copy_from_slice(&0x6000_u64.to_le_bytes()); // lpstrFile
        bytes[56..60].copy_from_slice(&260_u32.to_le_bytes()); // nMaxFile
        bytes[60..64].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // pad (nonzero)
        bytes[96..100].copy_from_slice(&0x0000_0008_u32.to_le_bytes()); // Flags
        bytes[136..144].copy_from_slice(&0xDEAD_BEEF_CAFE_F00D_u64.to_le_bytes());
        engine
            .mem_write(va, &bytes)
            .expect("write raw OPENFILENAME");

        let mut ofn = read_typed_copy::<OpenFileName>(&mut engine, va).expect("typed read");
        ofn.n_file_offset = 9;
        ofn.n_file_extension = 15;
        write_typed_copy(&mut engine, va, ofn).expect("typed write-back");
        let after = raw_bytes(&mut engine, va, 152);
        assert_eq!(&after[0..4], &152_u32.to_le_bytes());
        assert_eq!(&after[8..16], &0x00AA_0001_u64.to_le_bytes());
        assert_eq!(&after[60..64], &[0xAA, 0xBB, 0xCC, 0xDD], "pad preserved");
        assert_eq!(
            &after[96..100],
            &0x0000_0008_u32.to_le_bytes(),
            "Flags preserved"
        );
        assert_eq!(&after[100..102], &9_u16.to_le_bytes(), "nFileOffset");
        assert_eq!(&after[102..104], &15_u16.to_le_bytes(), "nFileExtension");
        assert_eq!(
            &after[136..144],
            &0xDEAD_BEEF_CAFE_F00D_u64.to_le_bytes(),
            "pvReserved preserved"
        );
    }
}
