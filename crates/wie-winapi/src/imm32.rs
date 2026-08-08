//! Handles `IMM32.dll` — Input Method Manager (string dispatch).
//!
//! IME is a non-goal: every export is a benign no-op that lets apps fall back
//! to classic input. Stateless, so no `DllStateMap` slot is needed.

use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

/// Dispatch an `IMM32.dll` export by name (case-insensitive).
pub fn dispatch_imm32(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        _ => Ok(None),
    }
}
