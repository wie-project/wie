//! `advapi32.dll` name → id rows for the dense WinAPI dispatch.
//!
//! Row order mirrors the `WinApiId` enum's `Advapi32*` variants and
//! matches the pre-split dense table exactly, so each row keeps its original
//! discriminant mapping.

use crate::dispatch_table::WinApiId;

/// `advapi32.dll` rows in dense `WinApiId` order.
pub(super) const ROWS: &[(&str, &str, WinApiId)] = &[
    (
        "advapi32.dll",
        "regcreatekeyexa",
        WinApiId::Advapi32Regcreatekeyexa,
    ),
    (
        "advapi32.dll",
        "regopenkeyexa",
        WinApiId::Advapi32Regopenkeyexa,
    ),
    (
        "advapi32.dll",
        "regqueryvalueexa",
        WinApiId::Advapi32Regqueryvalueexa,
    ),
    (
        "advapi32.dll",
        "regqueryvalueexw",
        WinApiId::Advapi32Regqueryvalueexw,
    ),
    (
        "advapi32.dll",
        "regsetvalueexa",
        WinApiId::Advapi32Regsetvalueexa,
    ),
    (
        "advapi32.dll",
        "regsetvalueexw",
        WinApiId::Advapi32Regsetvalueexw,
    ),
    (
        "advapi32.dll",
        "regdeletevaluea",
        WinApiId::Advapi32Regdeletevaluea,
    ),
    ("advapi32.dll", "regclosekey", WinApiId::Advapi32Regclosekey),
    (
        "advapi32.dll",
        "initializesecuritydescriptor",
        WinApiId::Advapi32Initializesecuritydescriptor,
    ),
    (
        "advapi32.dll",
        "setsecuritydescriptordacl",
        WinApiId::Advapi32Setsecuritydescriptordacl,
    ),
    (
        // Rows placed at the table end with the appended variants (412/413),
        // so the id table and the name rows stay in the same order.
        "advapi32.dll",
        "regopenkeya",
        WinApiId::Advapi32Regopenkeya,
    ),
    ("advapi32.dll", "regopenkeyw", WinApiId::Advapi32Regopenkeyw),
];
