//! UCRT `_vsnwprintf` / `_vsnprintf`: bounded printf-family format engine.
//!
//! Both exports are string-dispatched like the other UCRT functions. They
//! share one generic format walker parameterised over the output unit (byte
//! for the narrow variant, UTF-16 code unit for the wide variant) and the
//! per-variant guest string reader.
//!
//! Semantics follow the legacy MSVC `_vsn*` pair (not the C99 `vsnprintf`):
//! at most `count` units are written *including* the terminating NUL, the
//! return value is the number of content units written, and -1 is returned
//! with a NUL-terminated truncated buffer when the result does not fit.
//! Real Windows callers rely on this: notepad's `StringCchVPrintfW` wrapper
//! branches on a negative return as "insufficient buffer".
//!
//! Note: NUL-terminating on truncation is a deliberate safe-direction
//! deviation from the MSDN contract, which documents the truncated buffer
//! as *not* NUL-terminated. A caller that consumes the -1 return still gets
//! a usable C string (notepad's `StringCchVPrintfW` path), so the deviation
//! is kept.
//!
//! Supported conversions: `d i u x X p s c` plus the `%%` escape, field
//! width (digits or `*`), precision (digits or `*`, applied to strings and
//! as minimum digits for numerics), and the `-` / `0` flags. The `+`, ` `,
//! and `#` flags and all length modifiers are consumed and ignored; unknown
//! conversions fall through as literals. This is the set notepad's trace
//! needs; `f`/`e`/`g` floating point is deliberately unsupported (YAGNI).

use crate::guest_memory::read_u64;
use crate::{HandlerContext, WinApiHandlerResult};
use anyhow::Result;

use super::{finish, i32_status_to_u64};

/// Absolute ceiling for one formatted result. Real callers pass buffer sizes
/// in the low KBs (notepad's status bar); this only guards hostile formats.
/// Results beyond the cap are reported as truncation (-1).
const MAX_FORMAT_OUTPUT: usize = 1 << 20;

/// A unit of formatted output: byte for the narrow variant, UTF-16 code unit
/// for the wide variant.
trait FmtUnit: Copy + PartialEq {
    /// Build an output unit from an ASCII byte (digits, `%`, spaces).
    fn from_byte(b: u8) -> Self;
    /// Low byte of the unit — ASCII control characters are shared between the
    /// narrow and wide encodings, so the format walker can compare on it.
    fn to_u8(self) -> u8;
    /// Unit produced from a `%c` vararg: the low byte (narrow) or the low
    /// UTF-16 unit (wide).
    fn from_int_low(v: u64) -> Self;
    /// Is this unit the ASCII byte `b`?
    fn is_byte(self, b: u8) -> bool {
        self.to_u8() == b
    }
    /// Numeric value of an ASCII digit unit (0-9), or `None`.
    fn digit(self) -> Option<u8> {
        let lo = self.to_u8();
        if lo.is_ascii_digit() {
            Some(lo - b'0')
        } else {
            None
        }
    }
}

impl FmtUnit for u8 {
    fn from_byte(b: u8) -> Self {
        b
    }
    fn to_u8(self) -> u8 {
        self
    }
    fn from_int_low(v: u64) -> Self {
        u8::try_from(v & 0xff).unwrap_or(0)
    }
}

impl FmtUnit for u16 {
    fn from_byte(b: u8) -> Self {
        u16::from(b)
    }
    fn to_u8(self) -> u8 {
        u8::try_from(self & 0xff).unwrap_or(0)
    }
    fn from_int_low(v: u64) -> Self {
        u16::try_from(v & 0xffff).unwrap_or(0)
    }
}

/// Append `body` (ASCII) as units, with `-`/`0` flags and `*`/digit
/// width/precision applied: precision zero-pads digits, width pads with
/// spaces (left-justified when `left_justify`, else right-justified), and the
/// `0` flag zero-pads the body to the width when no precision was given.
fn emit_numeric<U: FmtUnit>(
    out: &mut Vec<U>,
    width: Option<usize>,
    precision: Option<usize>,
    left_justify: bool,
    zero_pad: bool,
    body: &str,
) {
    // Split the sign off so zero-padding lands between sign and digits.
    let (sign, digits) = match body.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", body),
    };
    // C: `%.0d` of zero prints nothing at all.
    let digits = if digits == "0" && precision == Some(0) {
        ""
    } else {
        digits
    };
    let zero_count = precision
        .map(|p| p.saturating_sub(digits.len()))
        .unwrap_or(0);
    let mut field = String::with_capacity(sign.len() + digits.len() + zero_count);
    field.push_str(sign);
    for _ in 0..zero_count {
        field.push('0');
    }
    field.push_str(digits);
    if let Some(w) = width {
        let pad = w.saturating_sub(field.len());
        if pad > 0 {
            if zero_pad && precision.is_none() && !left_justify {
                // Insert the zeros after the sign (C `%08d` of -42 → "-0000042").
                let zeros = "0".repeat(pad);
                field = format!("{sign}{zeros}{digits}");
            } else if !left_justify {
                field = format!("{}{field}", " ".repeat(pad));
            } else {
                field = format!("{field}{}", " ".repeat(pad));
            }
        }
    }
    out.extend(field.bytes().map(U::from_byte));
}

/// Append `s` as units with optional field-width padding (strings never
/// zero-pad; the `0` flag applies to numerics only).
fn emit_text<U: FmtUnit>(out: &mut Vec<U>, width: Option<usize>, left_justify: bool, s: &[U]) {
    if let Some(w) = width {
        let pad = w.saturating_sub(s.len());
        if pad > 0 && !left_justify {
            for _ in 0..pad {
                out.push(U::from_byte(b' '));
            }
        }
        out.extend_from_slice(s);
        if pad > 0 && left_justify {
            for _ in 0..pad {
                out.push(U::from_byte(b' '));
            }
        }
    } else {
        out.extend_from_slice(s);
    }
}

/// Pull one 8-byte vararg slot from the guest va_list cursor.
#[inline]
fn read_vararg(engine: &mut dyn wie_cpu::CpuEngine, va: &mut u64) -> u64 {
    let v = read_u64(engine, *va).unwrap_or(0);
    *va = va.wrapping_add(8);
    v
}

/// Walk the format string (already read from guest memory, in output units)
/// and append formatted output to `out`, pulling varargs from the guest
/// va_list cursor `va`. `read_string` fetches the guest `%s` argument as a
/// unit vector. Stops once `out` reaches `cap` (bounded formatting) and
/// reports `true` so the caller can apply truncation semantics.
fn format_into<U: FmtUnit>(
    engine: &mut dyn wie_cpu::CpuEngine,
    fmt: &[U],
    va: &mut u64,
    out: &mut Vec<U>,
    cap: usize,
    read_string: &mut dyn FnMut(&mut dyn wie_cpu::CpuEngine, u64) -> Vec<U>,
) -> bool {
    let mut truncated = false;
    let mut i = 0;
    while i < fmt.len() && out.len() < cap {
        if !fmt[i].is_byte(b'%') || i + 1 >= fmt.len() {
            out.push(fmt[i]);
            i += 1;
            if out.len() >= cap {
                truncated = true;
            }
            continue;
        }
        i += 1;
        // Flags: `-` left-justifies, `0` zero-pads numerics; `+` / ` ` / `#`
        // are consumed but have no effect (not needed by the trace so far).
        let mut left_justify = false;
        let mut zero_pad = false;
        while i < fmt.len() {
            if fmt[i].is_byte(b'-') {
                left_justify = true;
                i += 1;
            } else if fmt[i].is_byte(b'0') {
                zero_pad = true;
                i += 1;
            } else if fmt[i].is_byte(b'+') || fmt[i].is_byte(b' ') || fmt[i].is_byte(b'#') {
                i += 1;
            } else {
                break;
            }
        }
        // Field width: digit string or `*` (pulled from the varargs).
        let mut width: Option<usize> = None;
        if i < fmt.len() && fmt[i].is_byte(b'*') {
            width = usize::try_from(read_vararg(engine, va) & 0x7fff_ffff)
                .ok()
                .map(|w| w.min(MAX_FORMAT_OUTPUT));
            i += 1;
        } else {
            let mut w: usize = 0;
            while i < fmt.len() {
                let Some(d) = fmt[i].digit() else {
                    break;
                };
                w = w.saturating_mul(10).saturating_add(usize::from(d));
                i += 1;
            }
            if w > 0 {
                width = Some(w.min(MAX_FORMAT_OUTPUT));
            }
        }
        // Precision: `.` then a digit string or `*`.
        let mut precision: Option<usize> = None;
        if i < fmt.len() && fmt[i].is_byte(b'.') {
            i += 1;
            if i < fmt.len() && fmt[i].is_byte(b'*') {
                precision = usize::try_from(read_vararg(engine, va) & 0x7fff_ffff)
                    .ok()
                    .map(|p| p.min(MAX_FORMAT_OUTPUT));
                i += 1;
            } else {
                let mut p: usize = 0;
                while i < fmt.len() {
                    let Some(d) = fmt[i].digit() else {
                        break;
                    };
                    p = p.saturating_mul(10).saturating_add(usize::from(d));
                    i += 1;
                }
                precision = Some(p.min(MAX_FORMAT_OUTPUT));
            }
        }
        // Length modifiers (h/hh/l/ll/j/z/t/L/I32/I64). Every Win64 vararg
        // occupies one 8-byte slot, but a 32-bit conversion (%d/%u/%x) reads
        // only the low 32 bits of that slot — callers build va_lists over
        // their own stack frames, where a 32-bit store leaves the high bytes
        // stale (RNotepad's StringCchPrintfW va_list read 0x1CD_0000_0001 for
        // a stored column of 1). `wide_arg` tracks the modifiers that demand
        // the full 64-bit value (ll, j, z, t, I64); plain `l` is a 32-bit
        // long on Windows.
        let mut wide_arg = false;
        while i < fmt.len() {
            let b = fmt[i];
            if b.is_byte(b'h') || b.is_byte(b'L') {
                i += 1;
            } else if b.is_byte(b'l') {
                // 'l' alone is 32-bit; 'll' is 64-bit.
                if i + 1 < fmt.len() && fmt[i + 1].is_byte(b'l') {
                    wide_arg = true;
                    i += 2;
                } else {
                    i += 1;
                }
            } else if b.is_byte(b'j') || b.is_byte(b'z') || b.is_byte(b't') {
                wide_arg = true;
                i += 1;
            } else if b.is_byte(b'I') {
                // I32 / I64 (UCRT fixed-width prefix); bare 'I' is consumed.
                if i + 2 < fmt.len() && fmt[i + 1].is_byte(b'6') && fmt[i + 2].is_byte(b'4') {
                    wide_arg = true;
                    i += 3;
                } else if i + 2 < fmt.len() && fmt[i + 1].is_byte(b'3') && fmt[i + 2].is_byte(b'2')
                {
                    i += 3;
                } else {
                    i += 1;
                }
            } else {
                break;
            }
        }
        if i >= fmt.len() {
            // Trailing '%' with nothing left to specify — literal.
            out.push(U::from_byte(b'%'));
            break;
        }
        let spec = fmt[i];
        i += 1;
        match spec.to_u8() {
            b'%' => out.push(U::from_byte(b'%')),
            b'd' | b'i' => {
                let v = read_vararg(engine, va);
                // %d / %i is a 32-bit int: sign-extend the low slot word so
                // stale high bytes never leak into the printed value.
                let signed = if wide_arg {
                    i64::from_ne_bytes(v.to_ne_bytes())
                } else {
                    let low = u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0);
                    i64::from(i32::from_ne_bytes(low.to_ne_bytes()))
                };
                emit_numeric(
                    out,
                    width,
                    precision,
                    left_justify,
                    zero_pad,
                    &signed.to_string(),
                );
            }
            b'u' => {
                let v = read_vararg(engine, va);
                let unsigned = if wide_arg {
                    v
                } else {
                    u64::from(u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0))
                };
                emit_numeric(
                    out,
                    width,
                    precision,
                    left_justify,
                    zero_pad,
                    &unsigned.to_string(),
                );
            }
            b'x' | b'X' => {
                let v = read_vararg(engine, va);
                let hex = if wide_arg {
                    v
                } else {
                    u64::from(u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0))
                };
                let s = if spec.to_u8() == b'X' {
                    format!("{hex:X}")
                } else {
                    format!("{hex:x}")
                };
                emit_numeric(out, width, precision, left_justify, zero_pad, &s);
            }
            b'p' => {
                let v = read_vararg(engine, va);
                // UCRT prints pointers as `0x` + lowercase hex.
                emit_numeric(
                    out,
                    width,
                    precision,
                    left_justify,
                    zero_pad,
                    &format!("0x{v:x}"),
                );
            }
            b's' => {
                let p = read_vararg(engine, va);
                let mut s = read_string(engine, p);
                if let Some(prec) = precision {
                    s.truncate(prec);
                }
                emit_text(out, width, left_justify, &s);
            }
            b'c' => out.push(U::from_int_low(read_vararg(engine, va))),
            _ => {
                // Unknown conversion: emit as literals (legacy CRT fallback).
                out.push(U::from_byte(b'%'));
                out.push(spec);
            }
        }
        if out.len() >= cap {
            truncated = true;
        }
    }
    truncated
}

/// `_vsnwprintf(wchar_t* buf, size_t count, const wchar_t* fmt, va_list)`.
pub(crate) fn handle_vsnwprintf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let buf = engine.read_rcx()?;
    let count_raw = engine.read_rdx()?;
    let fmt_ptr = engine.read_r8()?;
    let va_list = engine.read_r9()?;
    if buf == 0 || fmt_ptr == 0 || count_raw == 0 {
        return finish(engine, i32_status_to_u64(-1));
    }
    let count = usize::try_from(count_raw)
        .unwrap_or(0)
        .min(MAX_FORMAT_OUTPUT);
    let fmt = crate::guest_string::read_utf16_lossy(engine, fmt_ptr, 4096)
        .map(|s| s.encode_utf16().collect::<Vec<u16>>())
        .unwrap_or_default();
    let mut va = va_list;
    let mut out: Vec<u16> = Vec::with_capacity(64);
    let mut read_string = |engine: &mut dyn wie_cpu::CpuEngine, p: u64| -> Vec<u16> {
        crate::guest_string::read_utf16_lossy(engine, p, MAX_FORMAT_OUTPUT)
            .map(|s| s.encode_utf16().collect())
            .unwrap_or_default()
    };
    let truncated = format_into(engine, &fmt, &mut va, &mut out, count, &mut read_string);
    if truncated {
        let keep = count.saturating_sub(1).min(out.len());
        let mut write = out[..keep].to_vec();
        write.push(0);
        crate::guest_string::write_utf16_units(engine, buf, &write)?;
        return finish(engine, i32_status_to_u64(-1));
    }
    out.push(0);
    crate::guest_string::write_utf16_units(engine, buf, &out)?;
    let written = u64::try_from(out.len().saturating_sub(1)).unwrap_or(0);
    finish(engine, written)
}

/// `_vsnprintf(char* buf, size_t count, const char* fmt, va_list)`.
pub(crate) fn handle_vsnprintf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let buf = engine.read_rcx()?;
    let count_raw = engine.read_rdx()?;
    let fmt_ptr = engine.read_r8()?;
    let va_list = engine.read_r9()?;
    if buf == 0 || fmt_ptr == 0 || count_raw == 0 {
        return finish(engine, i32_status_to_u64(-1));
    }
    let count = usize::try_from(count_raw)
        .unwrap_or(0)
        .min(MAX_FORMAT_OUTPUT);
    let fmt = crate::guest_string::read_ansi_bytes(engine, fmt_ptr, 4096).unwrap_or_default();
    let mut va = va_list;
    let mut out: Vec<u8> = Vec::with_capacity(64);
    let mut read_string = |engine: &mut dyn wie_cpu::CpuEngine, p: u64| -> Vec<u8> {
        crate::guest_string::read_ansi_bytes(engine, p, MAX_FORMAT_OUTPUT).unwrap_or_default()
    };
    let truncated = format_into(engine, &fmt, &mut va, &mut out, count, &mut read_string);
    if truncated {
        let keep = count.saturating_sub(1).min(out.len());
        let mut write = out[..keep].to_vec();
        write.push(0);
        crate::guest_memory::write_bytes(engine, buf, &write)?;
        return finish(engine, i32_status_to_u64(-1));
    }
    out.push(0);
    crate::guest_memory::write_bytes(engine, buf, &out)?;
    let written = u64::try_from(out.len().saturating_sub(1)).unwrap_or(0);
    finish(engine, written)
}
