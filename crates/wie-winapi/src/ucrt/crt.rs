//! UCRT CRT startup/teardown: `_initterm`, env/argv slot exports, app type,
//! and the exit family.

use crate::{HandlerContext, WinApiHandlerResult};
use anyhow::{Context, Result};

use super::{
    ACMDLN_PTR_SLOT, ARGC_SLOT, ARGV_PTR_SLOT, COMMODE_SLOT, CRT_GUEST_BASE, ENVIRON_PTR_SLOT,
    FMODE_SLOT, MAX_GUEST_STR, NARROW_ARGV_TABLE, WARGV_PTR_SLOT, WENVIRON_PTR_SLOT, finish,
    read_guest_str,
};

/// Cap on the number of environment/argv entries materialized into the guest
/// (a hostile host environment cannot bloat the CRT page without bound).
const MAX_CRT_ENTRIES: usize = 4096;

pub(crate) fn handle_set_new_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _mode = engine.read_rcx()?;
    finish(engine, 0)
}

pub(crate) fn handle_p_environ(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // char*** — point at a slot holding NULL (empty environment block list).
    engine.mem_write(ENVIRON_PTR_SLOT, &0_u64.to_le_bytes())?;
    finish(engine, ENVIRON_PTR_SLOT)
}

pub(crate) fn handle_p_acmdln(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Slot is filled at session start (points at GetCommandLineA buffer).
    finish(engine, ACMDLN_PTR_SLOT)
}

pub(crate) fn handle_p_argc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Slot is filled at session start from guest argv.
    finish(engine, ARGC_SLOT)
}

pub(crate) fn handle_p_argv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Slot holds char** filled at session start.
    finish(engine, ARGV_PTR_SLOT)
}

pub(crate) fn handle_p_commode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    engine.mem_write(COMMODE_SLOT, &0_u32.to_le_bytes())?;
    finish(engine, COMMODE_SLOT)
}

pub(crate) fn handle_p_fmode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    engine.mem_write(FMODE_SLOT, &0_u32.to_le_bytes())?;
    finish(engine, FMODE_SLOT)
}
/// Legacy msvcrt `__getmainargs(argc*, argv**, env**, doWildcard, startupinfo*)`.
///
/// Fills caller out-params from the CRT page prepared at session start.
pub(crate) fn handle_getenv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // `getenv(const char* name)` → returns NULL (variable not found).
    // The C++ runtime checks for debug/env flags during startup; returning
    // NULL is safe — no deployment expects these to be set.
    finish(engine, 0)
}

pub(crate) fn handle_getmainargs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let argc_va = engine.read_rcx()?;
    let argv_va = engine.read_rdx()?;
    let env_va = engine.read_r8()?;
    let _do_wildcard = engine.read_r9()?;
    // 5th arg on stack is ignored (startupinfo*).

    if argc_va != 0 {
        let mut argc_bytes = [0_u8; 4];
        engine
            .mem_read(ARGC_SLOT, &mut argc_bytes)
            .context("__getmainargs read argc slot")?;
        engine
            .mem_write(argc_va, &argc_bytes)
            .context("__getmainargs write *argc")?;
    }
    if argv_va != 0 {
        // *argv = char** table (same layout as __p___argv materialization).
        const ARGV_TABLE: u64 = CRT_GUEST_BASE + 0x400;
        engine
            .mem_write(argv_va, &ARGV_TABLE.to_le_bytes())
            .context("__getmainargs write *argv")?;
    }
    if env_va != 0 {
        // Empty environment: ENVIRON_PTR_SLOT holds a single NULL char* terminator.
        engine
            .mem_write(ENVIRON_PTR_SLOT, &0_u64.to_le_bytes())
            .context("__getmainargs zero env list")?;
        engine
            .mem_write(env_va, &ENVIRON_PTR_SLOT.to_le_bytes())
            .context("__getmainargs write *env")?;
    }
    finish(engine, 0)
}
/// `_initterm(first, last)` — call void (*)() for each non-null entry in [first, last).
///
/// v0: **no-op**. Calling guest constructors requires a full call bridge; empty/noncritical
/// `.CRT` sections still allow simple `main` programs. Tighten when a CRT PE needs ctors.
pub(crate) fn handle_initterm(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _first = engine.read_rcx()?;
    let _last = engine.read_rdx()?;
    finish(engine, 0)
}

/// `_initterm_e` — same as `_initterm` but entries return `int`; non-zero aborts.
/// v0: no-op success (return 0).
pub(crate) fn handle_initterm_e(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _first = engine.read_rcx()?;
    let _last = engine.read_rdx()?;
    finish(engine, 0)
}

pub(crate) fn handle_configure_narrow_argv(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _mode = engine.read_rcx()?;
    finish(engine, 0)
}
pub(crate) fn handle_initialize_narrow_environment(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    finish(engine, 0)
}

/// UCRT `_initialize_wide_environment` — sets up the wide environment.
///
/// WIE serves `GetEnvironmentStringsW` from host state, so the guest-visible
/// wide environment stays empty; the no-op keeps UCRT startup moving.
pub(crate) fn handle_initialize_wide_environment(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    finish(engine, 0)
}

/// UCRT `_configure_wide_argv(mode)` — selects argv parsing mode.
///
/// Argument vectors are materialized host-side; mode is accepted and ignored,
/// matching the narrow-argv twin.
pub(crate) fn handle_configure_wide_argv(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _mode = engine.read_rcx()?;
    finish(engine, 0)
}

/// UCRT `_fpreset` — restore the FPU control word to its default.
///
/// The guest x87 state is not exposed to hosts, so there is nothing to reset.
pub(crate) fn handle_fpreset(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    finish(engine, 0)
}

pub(crate) fn handle_p_wenviron(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    // Materialize the wide env table once; later calls return the same stable
    // address (real UCRT keeps a static `__wenviron`).
    let mut slot = [0_u8; 8];
    engine
        .mem_read(WENVIRON_PTR_SLOT, &mut slot)
        .context("__p__wenviron read slot")?;
    if u64::from_le_bytes(slot) == 0 {
        materialize_wide_env(engine, state)?;
    }
    finish(engine, WENVIRON_PTR_SLOT)
}

/// Build the guest `wchar_t**` table behind `__p__wenviron` from the host
/// environment (`std::env::vars_os()`), mirroring the `__p___wargv`
/// materialization: one guest-heap block holds `(n+1)` pointer entries
/// followed by the UTF-16 string bodies, NULL-terminated.
///
/// The table pointer cannot live in a process-global `OnceLock`: the
/// allocation needs the per-session guest heap (`&mut WinApiState`), so a
/// static cannot reach it. The guest slot (`WENVIRON_PTR_SLOT`) is the
/// per-session once-guard instead — it starts zeroed and is filled exactly
/// once, keeping the cross-cutting rule that immutable process-global
/// config does not live in `WinApiState`. Heap exhaustion degrades to an
/// empty environment rather than failing the CRT call.
fn materialize_wide_env(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut crate::WinApiState,
) -> Result<()> {
    // Host env vars → L"KEY=VALUE" strings (lossy for non-UTF-8 hosts).
    let vars: Vec<String> = std::env::vars_os()
        .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
        .take(MAX_CRT_ENTRIES)
        .collect();

    let table_len = (vars.len() + 1).saturating_mul(8); // entries + NULL terminator
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(vars.len());
    let mut body_total: usize = 0;
    for var in &vars {
        let mut bytes = Vec::with_capacity(
            var.encode_utf16()
                .count()
                .saturating_mul(2)
                .saturating_add(2),
        );
        for unit in var.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        body_total = body_total.saturating_add(bytes.len());
        bodies.push(bytes);
    }
    // Even an empty host env yields the 8-byte NULL terminator, so the
    // block is never zero-sized.
    let total = u64::try_from(table_len.saturating_add(body_total)).unwrap_or(0);
    let base = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, total);
    if base == 0 {
        return Ok(()); // OOM — slot stays NULL; guest sees an empty env.
    }
    let mut cursor = base.wrapping_add(u64::try_from(table_len).unwrap_or(0));
    let mut entries: Vec<u64> = Vec::with_capacity(bodies.len());
    for body in &bodies {
        entries.push(cursor);
        engine.mem_write(cursor, body)?;
        cursor = cursor.wrapping_add(u64::try_from(body.len()).unwrap_or(0));
    }
    let mut table = Vec::with_capacity(table_len);
    for e in &entries {
        table.extend_from_slice(&e.to_le_bytes());
    }
    table.extend_from_slice(&0_u64.to_le_bytes());
    engine.mem_write(base, &table)?;
    engine
        .mem_write(WENVIRON_PTR_SLOT, &base.to_le_bytes())
        .context("__p__wenviron write slot")?;
    Ok(())
}

pub(crate) fn handle_p_wargv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    // Materialize the wide argv table once; later calls return the same stable
    // address (real UCRT keeps a static `__wargv`).
    let mut slot = [0_u8; 8];
    engine
        .mem_read(WARGV_PTR_SLOT, &mut slot)
        .context("__p___wargv read slot")?;
    if u64::from_le_bytes(slot) == 0 {
        materialize_wide_argv(engine, state)?;
    }
    finish(engine, WARGV_PTR_SLOT)
}

/// Build the guest `wchar_t**` table behind `__p___wargv` from the narrow argv
/// the runtime session materialized on the CRT page.
///
/// One guest-heap block holds `(n+1)` pointer entries followed by the UTF-16
/// string bodies. Heap exhaustion degrades to an empty argv rather than failing
/// the CRT startup call.
fn materialize_wide_argv(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut crate::WinApiState,
) -> Result<()> {
    let mut argc_bytes = [0_u8; 4];
    engine
        .mem_read(ARGC_SLOT, &mut argc_bytes)
        .context("__p___wargv read argc")?;
    let argc = u32::from_le_bytes(argc_bytes);
    let count = usize::try_from(argc).unwrap_or(0).min(MAX_CRT_ENTRIES);

    let mut args: Vec<String> = Vec::with_capacity(count.min(64));
    for i in 0..count {
        let mut ptr_bytes = [0_u8; 8];
        let slot = NARROW_ARGV_TABLE.wrapping_add(u64::try_from(i).unwrap_or(0).wrapping_mul(8));
        engine
            .mem_read(slot, &mut ptr_bytes)
            .context("__p___wargv read argv pointer")?;
        let ptr = u64::from_le_bytes(ptr_bytes);
        if ptr == 0 {
            break;
        }
        args.push(read_guest_str(engine, ptr, MAX_GUEST_STR)?);
    }

    let table_len = (args.len() + 1).saturating_mul(8); // entries + NULL terminator
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(args.len());
    let mut body_total: usize = 0;
    for arg in &args {
        let mut bytes = Vec::with_capacity(
            arg.encode_utf16()
                .count()
                .saturating_mul(2)
                .saturating_add(2),
        );
        for unit in arg.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        body_total = body_total.saturating_add(bytes.len());
        bodies.push(bytes);
    }
    let total = u64::try_from(table_len.saturating_add(body_total)).unwrap_or(0);
    if total == 0 {
        return Ok(());
    }
    let base = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, total);
    if base == 0 {
        return Ok(()); // OOM — slot stays NULL; guest sees an empty argv.
    }
    let mut cursor = base.wrapping_add(u64::try_from(table_len).unwrap_or(0));
    let mut entries: Vec<u64> = Vec::with_capacity(bodies.len());
    for body in &bodies {
        entries.push(cursor);
        engine.mem_write(cursor, body)?;
        cursor = cursor.wrapping_add(u64::try_from(body.len()).unwrap_or(0));
    }
    let mut table = Vec::with_capacity(table_len);
    for e in &entries {
        table.extend_from_slice(&e.to_le_bytes());
    }
    table.extend_from_slice(&0_u64.to_le_bytes());
    engine.mem_write(base, &table)?;
    engine
        .mem_write(WARGV_PTR_SLOT, &base.to_le_bytes())
        .context("__p___wargv write slot")?;
    Ok(())
}

pub(crate) fn handle_crt_atexit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _fn = engine.read_rcx()?;
    finish(engine, 0)
}

pub(crate) fn handle_set_app_type(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _t = engine.read_rcx()?;
    finish(engine, 0)
}
pub(crate) fn handle_cexit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    finish(engine, 0)
}
pub(crate) fn handle_exit_like(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Should be intercepted via exit_process trait; if not, still return.
    let code = engine.read_rcx()?;
    finish(engine, code)
}

pub(crate) fn handle_abort(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    finish(engine, 3)
}
/// `_onexit` / `__dllonexit` — accept callback, return it (success).
pub(crate) fn handle_onexit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let func = engine.read_rcx()?;
    // Return the function pointer to indicate registration success (MSVC CRT contract).
    finish(engine, func)
}
