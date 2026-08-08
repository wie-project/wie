//! Handles `IMM32.dll` — Input Method Manager (string dispatch).
//!
//! IME is a non-goal: every export is a benign no-op that lets apps fall back
//! to classic input. Stateless, so no `DllStateMap` slot is needed.

use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

/// Dispatch an `IMM32.dll` export by name (case-insensitive).
pub fn dispatch_imm32(
    _ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let _ = name;
    Ok(None)
}
