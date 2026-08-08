//! Handles `DBGHELP.dll` + `IMAGEHLP.dll` — debug helpers (string dispatch).
//!
//! Minimal: initialization succeeds, symbol lookups fail gracefully.
//! Stateless, so no `DllStateMap` slot is needed.

use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

/// Dispatch a `DBGHELP.dll` / `IMAGEHLP.dll` export by name.
pub fn dispatch_dbghelp(
    _ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let _ = name;
    Ok(None)
}
