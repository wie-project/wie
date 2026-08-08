//! Handles `DBGHELP.dll` + `IMAGEHLP.dll` — debug helpers (string dispatch).
//!
//! Minimal: initialization succeeds, symbol lookups fail gracefully.
//! Stateless, so no `DllStateMap` slot is needed.

use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

/// Dispatch a `DBGHELP.dll` / `IMAGEHLP.dll` export by name.
pub fn dispatch_dbghelp(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        _ => Ok(None),
    }
}
