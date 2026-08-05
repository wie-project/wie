//! `comdlg32.dll` name → id rows for the dense WinAPI dispatch.
//!
//! Row order mirrors the `WinApiId` enum's `Comdlg32*` variants and
//! matches the pre-split dense table exactly, so each row keeps its original
//! discriminant mapping.

use crate::dispatch_table::WinApiId;

/// `comdlg32.dll` rows in dense `WinApiId` order.
pub(super) const ROWS: &[(&str, &str, WinApiId)] = &[
    (
        "comdlg32.dll",
        "getopenfilenamea",
        WinApiId::Comdlg32Getopenfilenamea,
    ),
    (
        "comdlg32.dll",
        "getopenfilenamew",
        WinApiId::Comdlg32Getopenfilenamew,
    ),
    (
        "comdlg32.dll",
        "getsavefilenamea",
        WinApiId::Comdlg32Getsavefilenamea,
    ),
    (
        "comdlg32.dll",
        "getsavefilenamew",
        WinApiId::Comdlg32Getsavefilenamew,
    ),
    (
        "comdlg32.dll",
        "commdlgextendederror",
        WinApiId::Comdlg32Commdlgextendederror,
    ),
    (
        "comdlg32.dll",
        "choosecolora",
        WinApiId::Comdlg32Choosecolora,
    ),
    (
        // Rows placed at the table end with the appended variants (420/421),
        // so the id table and the name rows stay in the same order.
        "comdlg32.dll",
        "getfiletitlea",
        WinApiId::Comdlg32Getfiletitlea,
    ),
    (
        "comdlg32.dll",
        "getfiletitlew",
        WinApiId::Comdlg32Getfiletitlew,
    ),
    (
        // Rows placed at the table end with the appended variants (449/450),
        // so the id table and the name rows stay in the same order.
        "comdlg32.dll",
        "findtextw",
        WinApiId::Comdlg32Findtextw,
    ),
    (
        "comdlg32.dll",
        "replacetextw",
        WinApiId::Comdlg32Replacetextw,
    ),
    (
        // Rows placed at the table end with the appended variants (451/452/453),
        // so the id table and the name rows stay in the same order.
        "comdlg32.dll",
        "choosefontw",
        WinApiId::Comdlg32Choosefontw,
    ),
    ("comdlg32.dll", "printdlgw", WinApiId::Comdlg32Printdlgw),
    (
        "comdlg32.dll",
        "pagesetupdlgw",
        WinApiId::Comdlg32Pagesetupdlgw,
    ),
];
