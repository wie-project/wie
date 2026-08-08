//! Handles `MSIMG32.dll` — image operations (AlphaBlend, TransparentBlt,
//! GradientFill). String dispatch. Stateless: operates on existing HDCs
//! through the gdi32 state, so no `DllStateMap` slot is needed.

use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

/// Dispatch a `MSIMG32.dll` export by name (case-insensitive).
pub fn dispatch_msimg32(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        _ => Ok(None),
    }
}
