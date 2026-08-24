//! Handles `IMM32.dll` — Input Method Manager (string dispatch).
//!
//! IME is a non-goal: every export is a benign no-op that lets apps fall back
//! to classic input. Stateless, so no `DllId` slot is needed.

use anyhow::Result;

use crate::gdi32::finish_after_discarding;
use crate::gdi32::{ArgReg, read_arg};
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
        // Phase-3 stub wave: the remaining IME surface reports "no IME".
        "immassociatecontext" => Ok(Some(handle_imm_associate_context(ctx)?)),
        "immgetcandidatelistw" => Ok(Some(handle_imm_get_candidate_list_w(ctx)?)),
        "immgetimefilenamea" => Ok(Some(handle_imm_get_ime_file_name_a(ctx)?)),
        "immnotifyime" => Ok(Some(handle_imm_notify_ime(ctx)?)),
        "immsetcandidatewindow" => Ok(Some(handle_imm_set_false(ctx)?)),
        "immsetcompositionstringw" => Ok(Some(handle_imm_set_false(ctx)?)),
        "immsetcompositionwindow" => Ok(Some(handle_imm_set_false(ctx)?)),
        _ => Ok(None),
    }
}

/// `HIMC ImmGetContext(HWND hwnd)` — no IME, so always `NULL`.
fn handle_imm_get_context(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwnd = read_arg(engine, ArgReg::Rcx, "ImmGetContext")?;
    ctx.finish(0)
}

/// `BOOL ImmGetOpenStatus(HIMC himc)` — the (absent) IME is never open.
fn handle_imm_get_open_status(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _himc = read_arg(engine, ArgReg::Rcx, "ImmGetOpenStatus")?;
    ctx.finish(0)
}

/// `BOOL ImmReleaseContext(HWND hwnd, HIMC himc)` — releasing a NULL context
/// still succeeds.
fn handle_imm_release_context(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwnd = read_arg(engine, ArgReg::Rcx, "ImmReleaseContext")?;
    let _himc = read_arg(engine, ArgReg::Rdx, "ImmReleaseContext")?;
    ctx.finish(1)
}

/// `LONG ImmGetCompositionStringW(HIMC himc, DWORD dwIndex, LPWSTR lpBuf,
/// DWORD dwBufLen)` — no composition, so returns 0 (and touches nothing).
fn handle_imm_get_composition_string_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _himc = read_arg(engine, ArgReg::Rcx, "ImmGetCompositionStringW")?;
    let _dw_index = read_arg(engine, ArgReg::Rdx, "ImmGetCompositionStringW")?;
    let _lp_buf = read_arg(engine, ArgReg::R8, "ImmGetCompositionStringW")?;
    let _dw_buf_len = read_arg(engine, ArgReg::R9, "ImmGetCompositionStringW")?;
    ctx.finish(0)
}

/// `HIMC ImmAssociateContext(HWND hwnd, HIMC himc)` — no IME, so the call
/// succeeds conceptually but returns NULL (no default IME context exists).
fn handle_imm_associate_context(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    finish_after_discarding(ctx, 2, 0)
}

/// `DWORD ImmGetCandidateListW(HIMC, DWORD, LPCANDIDATELIST, DWORD)` — no
/// candidate list; returns 0.
fn handle_imm_get_candidate_list_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    finish_after_discarding(ctx, 4, 0)
}

/// `DWORD ImmGetIMEFileNameA(HIMC, LPSTR, DWORD)` — no IME file; returns 0.
fn handle_imm_get_ime_file_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    finish_after_discarding(ctx, 3, 0)
}

/// `BOOL ImmNotifyIME(HIMC, DWORD, DWORD, DWORD)` — no IME; FALSE.
fn handle_imm_notify_ime(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    finish_after_discarding(ctx, 4, 0)
}

/// Shared `BOOL` FALSE for the `ImmSet*` family — the (absent) IME rejects
/// candidate/composition window changes.
fn handle_imm_set_false(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    finish_after_discarding(ctx, 4, 0)
}
