//! Shared spine for the soft (string-dispatched) DLLs whose export names ARE
//! their dispatch table.
//!
//! ADR-0009 wants one place to add a soft API. For a DLL whose every export
//! maps onto a `fn(&mut HandlerContext<'_>) -> Result<WinApiHandlerResult>`,
//! that place is a single row list: `soft_exports!` expands each row into both
//! the dispatch table and the `EXPORTS` census, so a name cannot be dispatched
//! without being advertised — and cannot be advertised without being
//! dispatched. They are the same rows. This is the shape `urlmon` and
//! `opengl32` already had; the eight soft DLLs whose dispatch also accepts
//! names that must stay out of the census (`IDataObject::*` slots, `ordinal N`,
//! `enumfont*` aliases, …) keep a hand-written list instead, with the reason
//! stated at that list.
//!
//! Cold path only. Lookup is a linear scan, which is what every soft dispatcher
//! already did; the dense `WinApiId` table owns the hot APIs.

use crate::{HandlerContext, WinApiHandlerResult};
use anyhow::Result;

/// Handler signature for one row of a `soft_exports!` table.
pub(crate) type SoftHandler = fn(&mut HandlerContext<'_>) -> Result<WinApiHandlerResult>;

/// Resolve `name` (case-insensitive) against a soft dispatch `table` and run
/// its handler.
///
/// `Ok(None)` is the only "not here" answer, and it is exactly what
/// `dispatch_winapi` turns into the unsupported-API bail; a handler error is
/// propagated unchanged, as the per-DLL `match` arms did.
pub(crate) fn dispatch_soft_table(
    ctx: &mut HandlerContext<'_>,
    table: &'static [(&'static str, SoftHandler)],
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    let Some((_, handler)) = table.iter().find(|(export, _)| *export == n.as_str()) else {
        return Ok(None);
    };
    handler(ctx).map(Some)
}

/// Declare a soft export table from one `("census name", handler)` row per
/// export.
///
/// Expands into the two things such a DLL needs — `DISPATCH`, the table
/// `dispatch_soft_table` scans, and `EXPORTS`, the census that
/// `crate::is_winapi_implemented` and `names::SOFT_EXPORT_CENSUS` report — both
/// generated from these rows alone: a row added here is dispatched and
/// advertised, and there is no second list to forget.
macro_rules! soft_exports {
    ($(($name:literal, $handler:path),)*) => {
        /// The dispatch table: `(export, handler)` rows in declaration order.
        const DISPATCH: &[(&str, $crate::soft_dispatch::SoftHandler)] = &[$(($name, $handler),)*];

        /// The export census — generated from the same rows as `DISPATCH`.
        ///
        /// Consumed by [`crate::is_winapi_implemented`] and
        /// `names::SOFT_EXPORT_CENSUS`; `dispatch_table::census_tests` sweeps it
        /// against the dispatch to prove the two agree.
        pub(crate) const EXPORTS: &[&str] = &[$($name,)*];
    };
}

pub(crate) use soft_exports;
