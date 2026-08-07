//! Win32 process environment: `GetEnvironmentVariable`, `SetEnvironmentVariable`,
//! `ExpandEnvironmentStrings`.
//!
//! Backed by [`crate::ProcessState::environment`] rather than the guest-memory
//! block, so updates are visible immediately without invalidating a pointer the
//! guest may still be holding from `GetEnvironmentStringsW`.

use super::{
    Context, HandlerContext, Result, WinApiHandlerResult, low_u32, read_guest_ansi_lossy,
    read_guest_utf16_lossy, ret_u64,
};
use crate::guest_string::{write_ansi_c_string, write_utf16_c_string};

/// `ERROR_ENVVAR_NOT_FOUND`.
const ERROR_ENVVAR_NOT_FOUND: u32 = 203;
/// Longest name or value WIE will read from the guest.
const MAX_ENV_CHARS: usize = 32 * 1024;

/// Look up a variable, matching case-insensitively as Windows does.
fn lookup(ctx: &HandlerContext<'_>, name: &str) -> Option<String> {
    ctx.state
        .process
        .environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// Insert, replace, or (with `None`) delete a variable.
fn assign(ctx: &mut HandlerContext<'_>, name: &str, value: Option<&str>) {
    let existing = ctx
        .state
        .process
        .environment
        .iter()
        .position(|(key, _)| key.eq_ignore_ascii_case(name));
    match (existing, value) {
        (Some(index), Some(value)) => {
            if let Some(slot) = ctx.state.process.environment.get_mut(index) {
                slot.1 = String::from(value);
            }
        }
        (Some(index), None) => {
            ctx.state.process.environment.remove(index);
        }
        (None, Some(value)) => ctx
            .state
            .process
            .environment
            .push((name.to_owned(), value.to_owned())),
        (None, None) => {}
    }
}

/// Shared body of `GetEnvironmentVariableA` / `GetEnvironmentVariableW`.
///
/// Returns the character count written excluding the terminator, or — when the
/// buffer is too small — the size *required* including it. That two-meanings
/// return value is the documented contract and what callers loop on.
fn get_environment_variable(
    ctx: &mut HandlerContext<'_>,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "GetEnvironmentVariableW"
    } else {
        "GetEnvironmentVariableA"
    };
    let name_va = ctx
        .engine
        .read_rcx()
        .context("GetEnvironmentVariable RCX")?;
    let buffer_va = ctx
        .engine
        .read_rdx()
        .context("GetEnvironmentVariable RDX")?;
    let capacity = low_u32(
        ctx.engine.read_r8().context("GetEnvironmentVariable R8")?,
        "GetEnvironmentVariable size",
    )?;

    if name_va == 0 {
        ctx.state.process.last_error = ERROR_ENVVAR_NOT_FOUND;
        return ret_u64(ctx.engine, 0, api);
    }
    let name = if wide {
        read_guest_utf16_lossy(ctx.engine, name_va, MAX_ENV_CHARS)?
    } else {
        read_guest_ansi_lossy(ctx.engine, name_va, MAX_ENV_CHARS)?
    };

    let Some(value) = lookup(ctx, &name) else {
        ctx.state.process.last_error = ERROR_ENVVAR_NOT_FOUND;
        return ret_u64(ctx.engine, 0, api);
    };

    let needed = if wide {
        value.encode_utf16().count()
    } else {
        crate::vfs::encode_acp(&value).len()
    };
    let capacity_usize = usize::try_from(capacity).unwrap_or(0);

    if buffer_va == 0 || capacity_usize <= needed {
        // Buffer too small: report the size including the terminator.
        let required = u64::try_from(needed.saturating_add(1)).unwrap_or(0);
        return ret_u64(ctx.engine, required, api);
    }

    if wide {
        write_utf16_c_string(ctx.engine, buffer_va, capacity_usize, &value)?;
    } else {
        write_ansi_c_string(ctx.engine, buffer_va, capacity_usize, &value)?;
    }
    ctx.state.process.last_error = 0;
    ret_u64(ctx.engine, u64::try_from(needed).unwrap_or(0), api)
}

pub fn handle_get_environment_variable_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    get_environment_variable(ctx, true)
}

pub fn handle_get_environment_variable_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    get_environment_variable(ctx, false)
}

/// Shared body of `SetEnvironmentVariableA` / `SetEnvironmentVariableW`.
///
/// A NULL value deletes the variable, per Microsoft Learn.
fn set_environment_variable(
    ctx: &mut HandlerContext<'_>,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "SetEnvironmentVariableW"
    } else {
        "SetEnvironmentVariableA"
    };
    let name_va = ctx
        .engine
        .read_rcx()
        .context("SetEnvironmentVariable RCX")?;
    let value_va = ctx
        .engine
        .read_rdx()
        .context("SetEnvironmentVariable RDX")?;

    if name_va == 0 {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, api);
    }
    let name = if wide {
        read_guest_utf16_lossy(ctx.engine, name_va, MAX_ENV_CHARS)?
    } else {
        read_guest_ansi_lossy(ctx.engine, name_va, MAX_ENV_CHARS)?
    };
    // A name containing '=' would produce an environment block that cannot be
    // parsed back, so Windows rejects it.
    if name.is_empty() || name.contains('=') {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, api);
    }

    let value = if value_va == 0 {
        None
    } else if wide {
        Some(read_guest_utf16_lossy(ctx.engine, value_va, MAX_ENV_CHARS)?)
    } else {
        Some(read_guest_ansi_lossy(ctx.engine, value_va, MAX_ENV_CHARS)?)
    };

    assign(ctx, &name, value.as_deref());
    ctx.state.process.last_error = 0;
    ret_u64(ctx.engine, 1, api)
}

pub fn handle_set_environment_variable_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    set_environment_variable(ctx, true)
}

pub fn handle_set_environment_variable_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    set_environment_variable(ctx, false)
}

/// Substitute `%NAME%` references in `input`.
///
/// An unmatched `%` and an unknown variable are both left verbatim, which is
/// what Windows does — `50%` in a string survives expansion unchanged.
fn expand(ctx: &HandlerContext<'_>, input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('%') {
        let (before, after_open) = rest.split_at(open);
        out.push_str(before);
        let tail = after_open.get(1..).unwrap_or("");
        if let Some(close) = tail.find('%') {
            let name = tail.get(..close).unwrap_or("");
            if let Some(value) = lookup(ctx, name) {
                out.push_str(&value);
            } else {
                // Unknown name: keep the original `%NAME%` text.
                out.push('%');
                out.push_str(name);
                out.push('%');
            }
            rest = tail.get(close.saturating_add(1)..).unwrap_or("");
        } else {
            // No closing '%': the remainder is literal.
            out.push('%');
            out.push_str(tail);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Shared body of `ExpandEnvironmentStringsA` / `ExpandEnvironmentStringsW`.
///
/// Returns the required size in characters *including* the terminator, whether
/// or not the buffer was large enough.
fn expand_environment_strings(
    ctx: &mut HandlerContext<'_>,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let api = if wide {
        "ExpandEnvironmentStringsW"
    } else {
        "ExpandEnvironmentStringsA"
    };
    let src_va = ctx
        .engine
        .read_rcx()
        .context("ExpandEnvironmentStrings RCX")?;
    let dst_va = ctx
        .engine
        .read_rdx()
        .context("ExpandEnvironmentStrings RDX")?;
    let capacity = low_u32(
        ctx.engine
            .read_r8()
            .context("ExpandEnvironmentStrings R8")?,
        "ExpandEnvironmentStrings size",
    )?;

    if src_va == 0 {
        ctx.state.process.last_error = super::ERROR_INVALID_PARAMETER;
        return ret_u64(ctx.engine, 0, api);
    }
    let source = if wide {
        read_guest_utf16_lossy(ctx.engine, src_va, MAX_ENV_CHARS)?
    } else {
        read_guest_ansi_lossy(ctx.engine, src_va, MAX_ENV_CHARS)?
    };
    let expanded = expand(ctx, &source);

    let chars = if wide {
        expanded.encode_utf16().count()
    } else {
        crate::vfs::encode_acp(&expanded).len()
    };
    let required = chars.saturating_add(1);
    let capacity_usize = usize::try_from(capacity).unwrap_or(0);

    if dst_va != 0 && capacity_usize >= required {
        if wide {
            write_utf16_c_string(ctx.engine, dst_va, capacity_usize, &expanded)?;
        } else {
            write_ansi_c_string(ctx.engine, dst_va, capacity_usize, &expanded)?;
        }
        ctx.state.process.last_error = 0;
    }
    ret_u64(ctx.engine, u64::try_from(required).unwrap_or(0), api)
}

pub fn handle_expand_environment_strings_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    expand_environment_strings(ctx, true)
}

pub fn handle_expand_environment_strings_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    expand_environment_strings(ctx, false)
}
