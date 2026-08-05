//! `winmm.dll` name → id rows for the dense WinAPI dispatch.
//!
//! Row order mirrors the `WinApiId` enum's `Winmm*` variants and
//! matches the pre-split dense table exactly, so each row keeps its original
//! discriminant mapping.

use crate::dispatch_table::WinApiId;

/// `winmm.dll` rows in dense `WinApiId` order.
pub(super) const ROWS: &[(&str, &str, WinApiId)] =
    &[("winmm.dll", "timegettime", WinApiId::WinmmTimegettime)];
