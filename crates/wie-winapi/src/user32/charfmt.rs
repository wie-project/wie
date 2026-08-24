//! USER32 character + formatting lane (soft-dispatch): `CharUpperW`,
//! `CharPrevExA`, `wsprintfW`, plus the trivial no-op exports
//! (`SetProcessDefaultLayout`, `WinHelpW`) and the `DialogBoxParamW`
//! fallback arm.
//!
//! Routed from `dispatch_user32_extra` (user32/mod.rs) — the string-match
//! fallback the dense `WinApiId` table does not cover. These are the six
//! notepad imports the import census flagged as unhandled; none are hot-path.

use super::{Context, HandlerContext, Result, WinApiHandlerResult, read_guest_utf16_lossy};
use crate::gdi32::{ArgReg, read_arg};
use crate::guest_memory::read_u64;

/// Cap for a `CharUpperW` in-place string read and a `wsprintfW` format read.
const STRING_READ_MAX: usize = 4096;
/// Cap for one `wsprintfW` result. `wsprintfW` has no buffer-length argument
/// (the unsafe variant), so the cap only bounds a hostile format; the returned
/// length is the pre-NUL unit count actually written.
const WSPRINTF_OUTPUT_MAX: usize = 1 << 16;

/// Dispatches the char/format lane of `dispatch_user32_extra`.
pub(crate) fn dispatch_charfmt(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "charupperw" => Ok(Some(handle_char_upper_w(ctx)?)),
        "charprevexa" => Ok(Some(handle_char_prev_ex_a(ctx)?)),
        // DialogBoxParamW's IAT is normally rewritten to the in-guest
        // modal-loop stub (wie-runtime/guest_stubs), whose callee is the dense
        // CreateDialogParamW row — so this arm is only the fallback for a
        // guest that reaches the host stop anyway. CreateDialogParamW and
        // DialogBoxParamW share the same five Win64 arguments, so routing to
        // the existing CreateDialogParamW machinery mirrors the real flow.
        "dialogboxparamw" => Ok(Some(super::handle_create_dialog_param_w(ctx)?)),
        "setprocessdefaultlayout" => Ok(Some(handle_set_process_default_layout(ctx)?)),
        "winhelpw" => Ok(Some(handle_win_help_w(ctx)?)),
        "wsprintfw" => Ok(Some(handle_wsprintf_w(ctx)?)),
        _ => Ok(None),
    }
}

/// Handles `USER32.dll!CharUpperW`.
///
/// Win64 ABI: `rcx` = `lpsz` — a single character when the value is below
/// `0x10000`, else a pointer to a NUL-terminated UTF-16 string. The string
/// form uppercases in place and returns the original pointer. Uppercasing is
/// ASCII-only per code unit (KISS: a length-changing mapping like ß→SS would
/// overflow the guest buffer, and the lossy decode/re-encode round-trips valid
/// BMP text exactly).
pub(crate) fn handle_char_upper_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let value = read_arg(engine, ArgReg::Rcx, "CharUpperW")?;
    if value < 0x1_0000 {
        let unit = u16::try_from(value).unwrap_or(0);
        // ASCII-only uppercase: 'a'..='z' → 'A'..='Z', identity otherwise
        // (`u16` has no to_ascii_uppercase — the u8/char methods do).
        let upper = if (0x61..=0x7A).contains(&unit) {
            unit.wrapping_sub(0x20)
        } else {
            unit
        };
        return ctx.finish(u64::from(upper));
    }
    if value == 0 {
        return ctx.finish(0);
    }
    let text = read_guest_utf16_lossy(engine, value, STRING_READ_MAX)
        .context("failed to read CharUpperW string")?;
    let uppercased: String = text.chars().map(|c| c.to_ascii_uppercase()).collect();
    let mut units: Vec<u16> = uppercased.encode_utf16().collect();
    units.push(0);
    crate::guest_string::write_utf16_units(engine, value, &units)
        .context("failed to write CharUpperW result")?;
    ctx.finish(value)
}

/// Handles `USER32.dll!CharPrevExA`.
///
/// Win64 ABI: `rcx` = `lpszStart`, `rdx` = `lpszCurrent`, `r8` = `dwFlags`.
/// ANSI is one byte per character, so the previous character is one byte back,
/// clamped to never step before the string start. The flags parameter only
/// matters for DBCS lead-byte handling, which is out of scope.
pub(crate) fn handle_char_prev_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let start = read_arg(engine, ArgReg::Rcx, "CharPrevExA")?;
    let current = read_arg(engine, ArgReg::Rdx, "CharPrevExA")?;
    let _flags = read_arg(engine, ArgReg::R8, "CharPrevExA")?;

    let previous = if current > start { current - 1 } else { start };
    ctx.finish(previous)
}

/// Handles `USER32.dll!SetProcessDefaultLayout` — a no-op TRUE.
///
/// The guest layout direction (LTR/RTL) is never observed by the window
/// machinery, so the request is accepted and ignored.
pub(crate) fn handle_set_process_default_layout(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _layout = read_arg(ctx.engine, ArgReg::Rcx, "SetProcessDefaultLayout")?;
    ctx.finish(1)
}

/// Handles `USER32.dll!WinHelpW` — a no-op TRUE.
///
/// The help viewer is not emulated; the call is accepted so the guest keeps
/// running (Windows help is a UX nicety, never load-bearing).
pub(crate) fn handle_win_help_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _hwnd = read_arg(ctx.engine, ArgReg::Rcx, "WinHelpW")?;
    let _help_file = read_arg(ctx.engine, ArgReg::Rdx, "WinHelpW")?;
    let _command = read_arg(ctx.engine, ArgReg::R8, "WinHelpW")?;
    let _data = read_arg(ctx.engine, ArgReg::R9, "WinHelpW")?;
    ctx.finish(1)
}

/// Handles `USER32.dll!wsprintfW` — the wide printf family's unbounded variant.
///
/// Win64 ABI: `rcx` = `lpOut`, `rdx` = `lpFmt`, then the varargs. With only two
/// fixed arguments, the first two varargs ride in R8/R9 and the rest spill to
/// the stack at `RSP+0x28`, `RSP+0x30`, … — the same slot order a va_list
/// built over the caller's frame would give (the ucrt `_vsnwprintf` reader
/// walks that va_list; this walks the inline slots directly).
///
/// Supported conversions: `%s` (a UTF-16 string pointer), `%d`/`%i`/`%u`/`%x`
/// (32-bit values from one 8-byte vararg slot), the `%%` escape, and plain
/// text. Width/precision/flags and `%c`/`%f` are deliberately unsupported
/// (YAGNI — the census target is notepad's status-bar and find-dialog text).
/// Returns the number of characters written, excluding the terminating NUL.
pub(crate) fn handle_wsprintf_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let out_va = read_arg(engine, ArgReg::Rcx, "wsprintfW")?;
    let fmt_va = read_arg(engine, ArgReg::Rdx, "wsprintfW")?;
    if out_va == 0 || fmt_va == 0 {
        return ctx.finish(0);
    }
    let first_vararg = read_arg(engine, ArgReg::R8, "wsprintfW")?;
    let second_vararg = read_arg(engine, ArgReg::R9, "wsprintfW")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for wsprintfW")?;

    let fmt = read_guest_utf16_lossy(engine, fmt_va, STRING_READ_MAX)
        .context("failed to read wsprintfW format")?;

    let mut out: Vec<u16> = Vec::with_capacity(64);
    // Vararg cursor: slot 0/1 are R8/R9, slot 2+ are the caller's stack.
    let mut vararg_slot: u64 = 0;
    let mut next_vararg = |engine: &mut dyn wie_cpu::CpuEngine| -> Result<u64> {
        let value = match vararg_slot {
            0 => first_vararg,
            1 => second_vararg,
            _ => {
                let offset = vararg_slot
                    .saturating_sub(2)
                    .saturating_mul(8)
                    .saturating_add(0x28);
                read_u64(engine, rsp.wrapping_add(offset))?
            }
        };
        vararg_slot = vararg_slot.saturating_add(1);
        Ok(value)
    };

    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if out.len() >= WSPRINTF_OUTPUT_MAX {
            break;
        }
        if c != '%' {
            push_char_capped(&mut out, c);
            continue;
        }
        let Some(spec) = chars.next() else {
            // Trailing '%' with nothing left to specify — literal.
            push_capped(&mut out, "%".encode_utf16());
            break;
        };
        match spec {
            '%' => push_capped(&mut out, "%".encode_utf16()),
            's' => {
                let ptr = next_vararg(engine)?;
                let s = read_guest_utf16_lossy(engine, ptr, STRING_READ_MAX).unwrap_or_default();
                push_capped(&mut out, s.encode_utf16());
            }
            'd' | 'i' => {
                let v = next_vararg(engine)?;
                // %d/%i is a 32-bit int: sign-extend the low slot word so
                // stale high bytes never leak into the printed value (the
                // ucrt walker's rule).
                let low = u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0);
                let signed = i64::from(i32::from_ne_bytes(low.to_ne_bytes()));
                push_capped(&mut out, signed.to_string().encode_utf16());
            }
            'u' => {
                let v = next_vararg(engine)?;
                let unsigned = u64::from(u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0));
                push_capped(&mut out, unsigned.to_string().encode_utf16());
            }
            'x' | 'X' => {
                let v = next_vararg(engine)?;
                let hex = u64::from(u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0));
                let s = if spec == 'X' {
                    format!("{hex:X}")
                } else {
                    format!("{hex:x}")
                };
                push_capped(&mut out, s.encode_utf16());
            }
            _ => {
                // Unknown conversion: emit as literals (legacy CRT fallback).
                push_capped(&mut out, "%".encode_utf16());
                push_char_capped(&mut out, spec);
            }
        }
    }

    let mut written = out;
    written.push(0);
    crate::guest_string::write_utf16_units(engine, out_va, &written)
        .context("failed to write wsprintfW result")?;
    let count = u64::try_from(written.len().saturating_sub(1)).unwrap_or(0);
    ctx.finish(count)
}

/// Append `units` to `out`, stopping at [`WSPRINTF_OUTPUT_MAX`].
fn push_capped(out: &mut Vec<u16>, units: impl Iterator<Item = u16>) {
    for unit in units {
        if out.len() >= WSPRINTF_OUTPUT_MAX {
            break;
        }
        out.push(unit);
    }
}

/// Append one character's UTF-16 units to `out`, stopping at the cap.
///
/// `char::encode_utf16` writes into a caller buffer and returns `&mut [u16]`
/// (it is not an iterator like `str::encode_utf16`), so a two-unit scratch
/// covers BMP and astral characters alike.
fn push_char_capped(out: &mut Vec<u16>, c: char) {
    let mut scratch = [0_u16; 2];
    for unit in c.encode_utf16(&mut scratch).iter().copied() {
        if out.len() >= WSPRINTF_OUTPUT_MAX {
            break;
        }
        out.push(unit);
    }
}
