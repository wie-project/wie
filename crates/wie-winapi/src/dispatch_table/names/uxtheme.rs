//! `uxtheme.dll` name → id rows for the dense WinAPI dispatch.
//!
//! Row order mirrors the `WinApiId` enum's `Uxtheme*` variants and
//! matches the pre-split dense table exactly, so each row keeps its original
//! discriminant mapping.

use crate::dispatch_table::WinApiId;

/// `uxtheme.dll` rows in dense `WinApiId` order.
pub(super) const ROWS: &[(&str, &str, WinApiId)] = &[(
    "uxtheme.dll",
    "setwindowtheme",
    WinApiId::UxthemeSetwindowtheme,
)];
