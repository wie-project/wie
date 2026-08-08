//! `gdi32.dll` name → id rows for the dense WinAPI dispatch.
//!
//! Row order mirrors the `WinApiId` enum's `Gdi32*` variants and
//! matches the pre-split dense table exactly, so each row keeps its original
//! discriminant mapping.

use crate::dispatch_table::WinApiId;

/// `gdi32.dll` rows in dense `WinApiId` order.
pub(super) const ROWS: &[(&str, &str, WinApiId)] = &[
    ("gdi32.dll", "selectobject", WinApiId::Gdi32Selectobject),
    (
        "gdi32.dll",
        "gettextextentpoint32a",
        WinApiId::Gdi32Gettextextentpoint32a,
    ),
    (
        "gdi32.dll",
        "gettextextentpoint32w",
        WinApiId::Gdi32Gettextextentpoint32w,
    ),
    ("gdi32.dll", "exttextoutw", WinApiId::Gdi32Exttextoutw),
    ("gdi32.dll", "getobjecta", WinApiId::Gdi32Getobjecta),
    (
        "gdi32.dll",
        "createcompatibledc",
        WinApiId::Gdi32Createcompatibledc,
    ),
    (
        "gdi32.dll",
        "createdibsection",
        WinApiId::Gdi32Createdibsection,
    ),
    (
        "gdi32.dll",
        "createcompatiblebitmap",
        WinApiId::Gdi32Createcompatiblebitmap,
    ),
    ("gdi32.dll", "getdevicecaps", WinApiId::Gdi32Getdevicecaps),
    ("gdi32.dll", "createfonta", WinApiId::Gdi32Createfonta),
    ("gdi32.dll", "createfontw", WinApiId::Gdi32Createfontw),
    (
        "gdi32.dll",
        "createfontindirecta",
        WinApiId::Gdi32Createfontindirecta,
    ),
    (
        "gdi32.dll",
        "gettextmetricsa",
        WinApiId::Gdi32Gettextmetricsa,
    ),
    ("gdi32.dll", "settextcolor", WinApiId::Gdi32Settextcolor),
    ("gdi32.dll", "setbkcolor", WinApiId::Gdi32Setbkcolor),
    ("gdi32.dll", "setbkmode", WinApiId::Gdi32Setbkmode),
    ("gdi32.dll", "textouta", WinApiId::Gdi32Textouta),
    ("gdi32.dll", "bitblt", WinApiId::Gdi32Bitblt),
    ("gdi32.dll", "stretchblt", WinApiId::Gdi32Stretchblt),
    ("gdi32.dll", "patblt", WinApiId::Gdi32Patblt),
    ("gdi32.dll", "getpixel", WinApiId::Gdi32Getpixel),
    ("gdi32.dll", "deletedc", WinApiId::Gdi32Deletedc),
    ("gdi32.dll", "deleteobject", WinApiId::Gdi32Deleteobject),
    ("gdi32.dll", "getstockobject", WinApiId::Gdi32Getstockobject),
    (
        "gdi32.dll",
        "createsolidbrush",
        WinApiId::Gdi32Createsolidbrush,
    ),
    ("gdi32.dll", "createpen", WinApiId::Gdi32Createpen),
    ("gdi32.dll", "textoutw", WinApiId::Gdi32Textoutw),
    (
        // Row placed at the table end with the appended variant (414),
        // so the id table and the name rows stay in the same order.
        "gdi32.dll",
        "createfontindirectw",
        WinApiId::Gdi32Createfontindirectw,
    ),
    (
        // Rows placed at the table end with the appended variants (454-463),
        // so the id table and the name rows stay in the same order.
        "gdi32.dll",
        "startdocw",
        WinApiId::Gdi32Startdocw,
    ),
    ("gdi32.dll", "startpage", WinApiId::Gdi32Startpage),
    ("gdi32.dll", "endpage", WinApiId::Gdi32Endpage),
    ("gdi32.dll", "enddoc", WinApiId::Gdi32Enddoc),
    ("gdi32.dll", "abortdoc", WinApiId::Gdi32Abortdoc),
    ("gdi32.dll", "createdcw", WinApiId::Gdi32Createdcw),
    (
        "gdi32.dll",
        "gettextmetricsw",
        WinApiId::Gdi32Gettextmetricsw,
    ),
    ("gdi32.dll", "setmapmode", WinApiId::Gdi32Setmapmode),
    ("gdi32.dll", "rectangle", WinApiId::Gdi32Rectangle),
];
