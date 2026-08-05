//! `comctl32.dll` name → id rows for the dense WinAPI dispatch.
//!
//! Row order mirrors the `WinApiId` enum's `Comctl32*` variants and
//! matches the pre-split dense table exactly, so each row keeps its original
//! discriminant mapping.

use crate::dispatch_table::WinApiId;

/// `comctl32.dll` rows in dense `WinApiId` order.
pub(super) const ROWS: &[(&str, &str, WinApiId)] = &[
    (
        "comctl32.dll",
        "dllgetversion",
        WinApiId::Comctl32Dllgetversion,
    ),
    ("comctl32.dll", "ordinal 17", WinApiId::Comctl32Ordinal17),
    (
        "comctl32.dll",
        "initcommoncontrolsex",
        WinApiId::Comctl32Initcommoncontrolsex,
    ),
    (
        "comctl32.dll",
        "imagelist_create",
        WinApiId::Comctl32ImagelistCreate,
    ),
    (
        "comctl32.dll",
        "imagelist_addmasked",
        WinApiId::Comctl32ImagelistAddmasked,
    ),
    (
        "comctl32.dll",
        "imagelist_setbkcolor",
        WinApiId::Comctl32ImagelistSetbkcolor,
    ),
    (
        "comctl32.dll",
        "imagelist_destroy",
        WinApiId::Comctl32ImagelistDestroy,
    ),
    (
        // Rows placed at the table end with the appended variants (418/419),
        // so the id table and the name rows stay in the same order.
        "comctl32.dll",
        "createstatuswindowa",
        WinApiId::Comctl32Createstatuswindowa,
    ),
    (
        "comctl32.dll",
        "createstatuswindoww",
        WinApiId::Comctl32Createstatuswindoww,
    ),
];
