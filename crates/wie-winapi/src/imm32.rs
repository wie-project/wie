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

/// `HIMC ImmAssociateContext(HWND hwnd, HIMC himc)` — no IME, so the call
/// succeeds conceptually but returns NULL (no default IME context exists).
fn handle_imm_associate_context(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _hwnd = ctx.engine.read_rcx()?;
    let _himc = ctx.engine.read_rdx()?;
    ctx.finish(0)
}

/// `DWORD ImmGetCandidateListW(HIMC, DWORD, LPCANDIDATELIST, DWORD)` — no
/// candidate list; returns 0.
fn handle_imm_get_candidate_list_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _himc = ctx.engine.read_rcx()?;
    let _index = ctx.engine.read_rdx()?;
    let _list = ctx.engine.read_r8()?;
    let _size = ctx.engine.read_r9()?;
    ctx.finish(0)
}

/// `DWORD ImmGetIMEFileNameA(HIMC, LPSTR, DWORD)` — no IME file; returns 0.
fn handle_imm_get_ime_file_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _himc = ctx.engine.read_rcx()?;
    let _name = ctx.engine.read_rdx()?;
    let _size = ctx.engine.read_r8()?;
    ctx.finish(0)
}

/// `BOOL ImmNotifyIME(HIMC, DWORD, DWORD, DWORD)` — no IME; FALSE.
fn handle_imm_notify_ime(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _himc = ctx.engine.read_rcx()?;
    let _action = ctx.engine.read_rdx()?;
    let _index = ctx.engine.read_r8()?;
    let _value = ctx.engine.read_r9()?;
    ctx.finish(0)
}

/// Shared `BOOL` FALSE for the `ImmSet*` family — the (absent) IME rejects
/// candidate/composition window changes.
fn handle_imm_set_false(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _arg0 = ctx.engine.read_rcx()?;
    let _arg1 = ctx.engine.read_rdx()?;
    let _arg2 = ctx.engine.read_r8()?;
    let _arg3 = ctx.engine.read_r9()?;
    ctx.finish(0)
}
