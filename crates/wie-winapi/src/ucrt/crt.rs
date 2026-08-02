//! UCRT CRT startup/teardown: `_initterm`, env/argv slot exports, app type,
//! and the exit family.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::integer_division,
    clippy::cast_sign_loss,
    clippy::items_after_statements,
    clippy::cast_possible_wrap,
    clippy::unnecessary_wraps
)]

use crate::{HandlerContext, WinApiHandlerResult};
use anyhow::{Context, Result};

use super::{
    ACMDLN_PTR_SLOT, ARGC_SLOT, ARGV_PTR_SLOT, COMMODE_SLOT, CRT_GUEST_BASE, ENVIRON_PTR_SLOT,
    FMODE_SLOT, ret,
};
pub(crate) fn handle_set_new_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _mode = engine.read_rcx()?;
    ret(engine, 0)
}

pub(crate) fn handle_p_environ(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // char*** — point at a slot holding NULL (empty environment block list).
    engine.mem_write(ENVIRON_PTR_SLOT, &0_u64.to_le_bytes())?;
    ret(engine, ENVIRON_PTR_SLOT)
}

pub(crate) fn handle_p_acmdln(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Slot is filled at session start (points at GetCommandLineA buffer).
    ret(engine, ACMDLN_PTR_SLOT)
}

pub(crate) fn handle_p_argc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Slot is filled at session start from guest argv.
    ret(engine, ARGC_SLOT)
}

pub(crate) fn handle_p_argv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Slot holds char** filled at session start.
    ret(engine, ARGV_PTR_SLOT)
}

pub(crate) fn handle_p_commode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    engine.mem_write(COMMODE_SLOT, &0_u32.to_le_bytes())?;
    ret(engine, COMMODE_SLOT)
}

pub(crate) fn handle_p_fmode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    engine.mem_write(FMODE_SLOT, &0_u32.to_le_bytes())?;
    ret(engine, FMODE_SLOT)
}
/// Legacy msvcrt `__getmainargs(argc*, argv**, env**, doWildcard, startupinfo*)`.
///
/// Fills caller out-params from the CRT page prepared at session start.
pub(crate) fn handle_getenv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // `getenv(const char* name)` → returns NULL (variable not found).
    // The C++ runtime checks for debug/env flags during startup; returning
    // NULL is safe — no deployment expects these to be set.
    ret(engine, 0)
}

pub(crate) fn handle_getmainargs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let argc_ptr = engine.read_rcx()?;
    let argv_ptr = engine.read_rdx()?;
    let env_ptr = engine.read_r8()?;
    let _do_wildcard = engine.read_r9()?;
    // 5th arg on stack is ignored (startupinfo*).

    if argc_ptr != 0 {
        let mut argc_bytes = [0_u8; 4];
        engine
            .mem_read(ARGC_SLOT, &mut argc_bytes)
            .context("__getmainargs read argc slot")?;
        engine
            .mem_write(argc_ptr, &argc_bytes)
            .context("__getmainargs write *argc")?;
    }
    if argv_ptr != 0 {
        // *argv = char** table (same layout as __p___argv materialization).
        const ARGV_TABLE: u64 = CRT_GUEST_BASE + 0x400;
        engine
            .mem_write(argv_ptr, &ARGV_TABLE.to_le_bytes())
            .context("__getmainargs write *argv")?;
    }
    if env_ptr != 0 {
        // Empty environment: ENVIRON_PTR_SLOT holds a single NULL char* terminator.
        engine
            .mem_write(ENVIRON_PTR_SLOT, &0_u64.to_le_bytes())
            .context("__getmainargs zero env list")?;
        engine
            .mem_write(env_ptr, &ENVIRON_PTR_SLOT.to_le_bytes())
            .context("__getmainargs write *env")?;
    }
    ret(engine, 0)
}
/// `_initterm(first, last)` — call void (*)() for each non-null entry in [first, last).
///
/// v0: **no-op**. Calling guest constructors requires a full call bridge; empty/noncritical
/// `.CRT` sections still allow simple `main` programs. Tighten when a CRT PE needs ctors.
pub(crate) fn handle_initterm(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _first = engine.read_rcx()?;
    let _last = engine.read_rdx()?;
    ret(engine, 0)
}

/// `_initterm_e` — same as `_initterm` but entries return `int`; non-zero aborts.
/// v0: no-op success (return 0).
pub(crate) fn handle_initterm_e(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _first = engine.read_rcx()?;
    let _last = engine.read_rdx()?;
    ret(engine, 0)
}

pub(crate) fn handle_configure_narrow_argv(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _mode = engine.read_rcx()?;
    ret(engine, 0)
}
pub(crate) fn handle_initialize_narrow_environment(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    ret(engine, 0)
}

pub(crate) fn handle_crt_atexit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _fn = engine.read_rcx()?;
    ret(engine, 0)
}

pub(crate) fn handle_set_app_type(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _t = engine.read_rcx()?;
    ret(engine, 0)
}
pub(crate) fn handle_cexit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    ret(engine, 0)
}
pub(crate) fn handle_exit_like(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Should be intercepted via exit_process trait; if not, still return.
    let code = engine.read_rcx()?;
    ret(engine, code)
}

pub(crate) fn handle_abort(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    ret(engine, 3)
}
/// `_onexit` / `__dllonexit` — accept callback, return it (success).
pub(crate) fn handle_onexit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let func = engine.read_rcx()?;
    // Return the function pointer to indicate registration success (MSVC CRT contract).
    ret(engine, func)
}
