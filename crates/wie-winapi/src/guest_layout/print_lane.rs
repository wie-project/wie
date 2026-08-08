//! Z1 print lane: `DocInfoW`, `ChooseFontW`, `PrintDlgW`, `DevModeW`,
//! `DevNames`, `PageSetupDlgW` — the print-pipeline dialog structs.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// --- Z1 print lane: DOCINFO / CHOOSEFONT / PRINTDLG / DEVMODE / DEVNAMES /
// --- PAGESETUP -----------------------------------------------------------
//

// `LogFontW` (crate::guest_layout::gdi32) is the shared LOGFONTW: the
// ChooseFontW write-back (comdlg32.rs) reads/writes it through the typed
// views, so the
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
}
