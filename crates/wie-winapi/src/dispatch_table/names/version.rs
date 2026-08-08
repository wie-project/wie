//! `version.dll` name → id rows for the dense WinAPI dispatch.
//!
//! Row order mirrors the `WinApiId` enum's `Version*` variants (appended at
//! the end, so every row keeps the pre-split discriminant mapping).

use crate::dispatch_table::WinApiId;

/// `version.dll` rows in dense `WinApiId` order.
pub(super) const ROWS: &[(&str, &str, WinApiId)] = &[
    (
        "version.dll",
        "getfileversioninfosizew",
        WinApiId::VersionGetfileversioninfosizew,
    ),
    (
        "version.dll",
        "getfileversioninfosizea",
        WinApiId::VersionGetfileversioninfosizea,
    ),
    (
        "version.dll",
        "getfileversioninfosizeexw",
        WinApiId::VersionGetfileversioninfosizeexw,
    ),
    (
        "version.dll",
        "getfileversioninfosizeexa",
        WinApiId::VersionGetfileversioninfosizeexa,
    ),
    (
        "version.dll",
        "getfileversioninfow",
        WinApiId::VersionGetfileversioninfow,
    ),
    (
        "version.dll",
        "getfileversioninfoa",
        WinApiId::VersionGetfileversioninfoa,
    ),
    (
        "version.dll",
        "getfileversioninfoexw",
        WinApiId::VersionGetfileversioninfoexw,
    ),
    (
        "version.dll",
        "getfileversioninfoexa",
        WinApiId::VersionGetfileversioninfoexa,
    ),
    (
        "version.dll",
        "verqueryvaluew",
        WinApiId::VersionVerqueryvaluew,
    ),
    (
        "version.dll",
        "verqueryvaluea",
        WinApiId::VersionVerqueryvaluea,
    ),
    (
        "version.dll",
        "getfileversioninfobyhandlew",
        WinApiId::VersionGetfileversioninfobyhandlew,
    ),
    (
        "version.dll",
        "getfileversioninfobyhandlea",
        WinApiId::VersionGetfileversioninfobyhandlea,
    ),
    (
        "version.dll",
        "verlanguagenamew",
        WinApiId::VersionVerlanguagenamew,
    ),
    (
        "version.dll",
        "verlanguagenamea",
        WinApiId::VersionVerlanguagenamea,
    ),
    ("version.dll", "verfindfilew", WinApiId::VersionVerfindfilew),
    ("version.dll", "verfindfilea", WinApiId::VersionVerfindfilea),
    (
        "version.dll",
        "verinstallfilew",
        WinApiId::VersionVerinstallfilew,
    ),
    (
        "version.dll",
        "verinstallfilea",
        WinApiId::VersionVerinstallfilea,
    ),
];
