//! Handles `SETUPAPI.dll` + `CFGMGR32.dll` — device setup (string dispatch).
//!
//! No host devices: enumeration APIs return an empty device list.
//! Stateless (handles are opaque), so no `DllStateMap` slot is needed.

use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

/// Dispatch a `SETUPAPI.dll` / `CFGMGR32.dll` export by name.
pub fn dispatch_setupapi(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        _ => Ok(None),
    }
}
