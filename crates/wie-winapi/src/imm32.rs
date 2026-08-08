//! Handles `IMM32.dll` — Input Method Manager (string dispatch).
//!
//! IME is a non-goal: every export is a benign no-op that lets apps fall back
//! to classic input. Stateless, so no `DllId` slot is needed.

use anyhow::{Context, Result};

use crate::{HandlerContext, WinApiHandlerResult};

/// Dispatch an `IMM32.dll` export by name (case-insensitive).
pub fn dispatch_imm32(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "immgetcontext" => Ok(Some(handle_imm_get_context(ctx)?)),
        "immgetopenstatus" => Ok(Some(handle_imm_get_open_status(ctx)?)),
        "immreleasecontext" => Ok(Some(handle_imm_release_context(ctx)?)),
        "immgetcompositionstringw" => Ok(Some(handle_imm_get_composition_string_w(ctx)?)),
        _ => Ok(None),
    }
}

/// `HIMC ImmGetContext(HWND hwnd)` — no IME, so always `NULL`.
fn handle_imm_get_context(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwnd = engine
        .read_rcx()
        .context("failed to read RCX for ImmGetContext")?;
    ctx.finish(0)
}

/// `BOOL ImmGetOpenStatus(HIMC himc)` — the (absent) IME is never open.
fn handle_imm_get_open_status(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _himc = engine
        .read_rcx()
        .context("failed to read RCX for ImmGetOpenStatus")?;
    ctx.finish(0)
}

/// `BOOL ImmReleaseContext(HWND hwnd, HIMC himc)` — releasing a NULL context
/// still succeeds.
fn handle_imm_release_context(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwnd = engine
        .read_rcx()
        .context("failed to read RCX for ImmReleaseContext")?;
    let _himc = engine
        .read_rdx()
        .context("failed to read RDX for ImmReleaseContext")?;
    ctx.finish(1)
}

/// `LONG ImmGetCompositionStringW(HIMC himc, DWORD dwIndex, LPWSTR lpBuf,
/// DWORD dwBufLen)` — no composition, so returns 0 (and touches nothing).
fn handle_imm_get_composition_string_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _himc = engine
        .read_rcx()
        .context("failed to read RCX for ImmGetCompositionStringW")?;
    let _dw_index = engine
        .read_rdx()
        .context("failed to read RDX for ImmGetCompositionStringW")?;
    let _lp_buf = engine
        .read_r8()
        .context("failed to read R8 for ImmGetCompositionStringW")?;
    let _dw_buf_len = engine
        .read_r9()
        .context("failed to read R9 for ImmGetCompositionStringW")?;
    ctx.finish(0)
}
