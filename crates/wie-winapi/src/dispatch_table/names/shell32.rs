//! `shell32.dll` name → id rows for the dense WinAPI dispatch.
//!
//! Row order mirrors the `WinApiId` enum's `Shell32*` variants and
//! matches the pre-split dense table exactly, so each row keeps its original
//! discriminant mapping.

use crate::dispatch_table::WinApiId;

/// `shell32.dll` rows in dense `WinApiId` order.
pub(super) const ROWS: &[(&str, &str, WinApiId)] = &[
    (
        // Row placed at the table end with the appended variant (417),
        // so the id table and the name rows stay in the same order.
        "shell32.dll",
        "dragacceptfiles",
        WinApiId::Shell32Dragacceptfiles,
    ),
    (
        // Rows placed at the table end with the appended variants (434-437),
        // so the id table and the name rows stay in the same order.
        "shell32.dll",
        "dragqueryfilew",
        WinApiId::Shell32Dragqueryfilew,
    ),
    (
        "shell32.dll",
        "dragqueryfilea",
        WinApiId::Shell32Dragqueryfilea,
    ),
    (
        "shell32.dll",
        "dragquerypoint",
        WinApiId::Shell32Dragquerypoint,
    ),
    ("shell32.dll", "dragfinish", WinApiId::Shell32Dragfinish),
    (
        // Rows placed at the table end with the appended variants (445/446),
        // so the id table and the name rows stay in the same order.
        "shell32.dll",
        "shellaboutw",
        WinApiId::Shell32Shellaboutw,
    ),
    (
        "shell32.dll",
        "shellexecutew",
        WinApiId::Shell32Shellexecutew,
    ),
];
