//! UCRT string/memory/ctype handlers: `mem*`, `str*`, `wcs*`, and the
//! character-class helpers.

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
use anyhow::Result;

use super::{i32_status_to_u64, read_guest_str, ret};
pub(crate) fn handle_memcpy(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let src = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || dest == 0 || src == 0 {
        return ret(engine, dest);
    }
    // `mem_copy` resolves both spans inside wie-cpu and uses memmove
    // semantics, so overlapping ranges are handled correctly rather than
    // being punted to a host bounce buffer. Returns false only when a side
    // is not a single mapped span.
    if engine.mem_copy(dest, src, n_usize) {
        return ret(engine, dest);
    }
    // Fallback: cross-arena or SPC-denied.
    let mut buf = vec![0_u8; n_usize];
    engine.mem_read(src, &mut buf)?;
    engine.mem_write(dest, &buf)?;
    ret(engine, dest)
}

pub(crate) fn handle_memcmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || a == 0 || b == 0 {
        return ret(engine, 0);
    }
    // Fast path: both spans in host-contiguous arenas → direct slice compare.
    // Both slices borrow `&engine`, so they can coexist; the borrow ends
    // before `ret` needs `&mut engine`.
    let direct = match (engine.host_slice(a, n_usize), engine.host_slice(b, n_usize)) {
        (Some(sa), Some(sb)) => {
            let mut result: i32 = 0;
            for (xa, xb) in sa.iter().zip(sb.iter()) {
                if xa != xb {
                    result = i32::from(*xa).wrapping_sub(i32::from(*xb));
                    break;
                }
            }
            Some(result)
        }
        _ => None,
    };
    if let Some(result) = direct {
        return ret(engine, i32_status_to_u64(result));
    }
    let mut ba = vec![0_u8; n_usize];
    let mut bb = vec![0_u8; n_usize];
    engine.mem_read(a, &mut ba)?;
    engine.mem_read(b, &mut bb)?;
    let mut result: i32 = 0;
    for (xa, xb) in ba.iter().zip(bb.iter()) {
        if xa != xb {
            result = i32::from(*xa).wrapping_sub(i32::from(*xb));
            break;
        }
    }
    ret(engine, i32_status_to_u64(result))
}

pub(crate) fn handle_memset(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let c = engine.read_rdx()? & 0xff;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || dest == 0 {
        return ret(engine, dest);
    }
    let value = u8::try_from(c).unwrap_or(0);
    if engine.mem_fill(dest, value, n_usize) {
        return ret(engine, dest);
    }
    // Fallback: bounce through a host Vec only when the span isn't directly writable.
    let buf = vec![value; n_usize];
    engine.mem_write(dest, &buf)?;
    ret(engine, dest)
}
pub(crate) fn handle_strlen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = engine.read_rcx()?;
    if s == 0 {
        return ret(engine, 0);
    }
    // Scan up to one guest page per host_span call; keeps a single memory-lock
    // acquisition covering ~4 KiB of scan instead of one per byte.
    const PAGE: u64 = 4096;
    const CAP: u64 = 1_000_000;
    let mut cursor = s;
    let mut total: u64 = 0;
    while total < CAP {
        let page_end = (cursor | (PAGE - 1)).wrapping_add(1);
        let remaining = CAP.saturating_sub(total);
        let span_len_u64 = page_end.saturating_sub(cursor).min(remaining);
        let span_len = usize::try_from(span_len_u64).unwrap_or(0);
        if span_len == 0 {
            break;
        }
        // Scan the page in place through a borrowed slice — no copy, and the
        // borrow ends before `ret` takes `&mut engine`.
        if let Some(found) = engine
            .host_slice(cursor, span_len)
            .map(|slice| slice.iter().position(|&b| b == 0))
        {
            if let Some(off) = found {
                total = total.saturating_add(u64::try_from(off).unwrap_or(0));
                return ret(engine, total);
            }
            total = total.saturating_add(span_len_u64);
            cursor = cursor.wrapping_add(span_len_u64);
            continue;
        }
        // Fallback (unmapped span / protect denied): scalar byte scan of this page.
        let mut buf = [0_u8; 1];
        engine.mem_read(cursor, &mut buf)?;
        if buf[0] == 0 {
            return ret(engine, total);
        }
        total = total.saturating_add(1);
        cursor = cursor.wrapping_add(1);
    }
    ret(engine, total)
}

pub(crate) fn handle_strncmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 {
        return ret(engine, 0);
    }
    // Try both spans as one contiguous host slice each; fall back to scalar.
    let direct = match (engine.host_slice(a, n_usize), engine.host_slice(b, n_usize)) {
        (Some(sa), Some(sb)) => {
            let mut result: i32 = 0;
            for (&ca, &cb) in sa.iter().zip(sb.iter()) {
                if ca != cb {
                    result = i32::from(ca).wrapping_sub(i32::from(cb));
                    break;
                }
                if ca == 0 {
                    break;
                }
            }
            Some(result)
        }
        _ => None,
    };
    if let Some(result) = direct {
        return ret(engine, i32_status_to_u64(result));
    }
    let mut result: i32 = 0;
    for i in 0..n_usize {
        let mut ba = [0_u8; 1];
        let mut bb = [0_u8; 1];
        engine.mem_read(a.wrapping_add(u64::try_from(i).unwrap_or(0)), &mut ba)?;
        engine.mem_read(b.wrapping_add(u64::try_from(i).unwrap_or(0)), &mut bb)?;
        if ba[0] != bb[0] {
            result = i32::from(ba[0]).wrapping_sub(i32::from(bb[0]));
            break;
        }
        if ba[0] == 0 {
            break;
        }
    }
    ret(engine, i32_status_to_u64(result))
}
/// `strtol(s, endptr, base)` — parse string to long.
pub(crate) fn handle_strtol(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_ptr = engine.read_rcx()?;
    let _endptr = engine.read_rdx()?;
    let base = engine.read_r8()?;
    let s = read_guest_str(engine, s_ptr, 64)?;
    let val = i64::from_str_radix(s.trim(), u32::try_from(base).unwrap_or(10)).unwrap_or(0);
    ret(engine, val as u64)
}

/// `strtoul(s, endptr, base)` — parse string to unsigned long.
pub(crate) fn handle_strtoul(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_ptr = engine.read_rcx()?;
    let _endptr = engine.read_rdx()?;
    let base = engine.read_r8()?;
    let s = read_guest_str(engine, s_ptr, 64)?;
    let val = u64::from_str_radix(s.trim(), u32::try_from(base).unwrap_or(10)).unwrap_or(0);
    ret(engine, val)
}

/// `strtod(s, endptr)` — parse string to double.
pub(crate) fn handle_strtod(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_ptr = engine.read_rcx()?;
    let _endptr = engine.read_rdx()?;
    let s = read_guest_str(engine, s_ptr, 64)?;
    let val: f64 = s.trim().parse().unwrap_or(0.0);
    ret(engine, val.to_bits())
}

/// `strtok(s, delim)` — tokenize string (single-threaded, static buffer).
pub(crate) fn handle_strtok(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_ptr = engine.read_rcx()?;
    let d_ptr = engine.read_rdx()?;
    static SAVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ptr = if s_ptr == 0 {
        SAVE.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        s_ptr
    };
    if ptr == 0 {
        return ret(engine, 0);
    }
    let delim = read_guest_str(engine, d_ptr, 32).unwrap_or_default();
    // Skip leading delimiters.
    let start = {
        let mut p = ptr;
        loop {
            let mut b = [0_u8; 1];
            if engine.mem_read(p, &mut b).is_err() {
                break;
            }
            if b[0] == 0 {
                break;
            }
            if delim.contains(b[0] as char) {
                p = p.wrapping_add(1);
                continue;
            }
            break;
        }
        p
    };
    // Find first delimiter after start.
    let mut end_off = 0_u64;
    loop {
        let mut b = [0_u8; 1];
        if engine
            .mem_read(start.wrapping_add(end_off), &mut b)
            .is_err()
        {
            break;
        }
        if b[0] == 0 {
            break;
        }
        if delim.contains(b[0] as char) {
            let nul = [0_u8];
            drop(engine.mem_write(start.wrapping_add(end_off), &nul));
            SAVE.store(
                start.wrapping_add(end_off).wrapping_add(1),
                std::sync::atomic::Ordering::Relaxed,
            );
            return ret(engine, start);
        }
        end_off += 1;
    }
    // No more delimiters — return remaining token.
    SAVE.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut b = [0_u8; 1];
    if engine.mem_read(start, &mut b).is_ok() && b[0] != 0 {
        ret(engine, start)
    } else {
        ret(engine, 0)
    }
}
/// ctype helpers: isalpha, isdigit, isalnum, islower, isupper, isspace, toupper, tolower.
pub(crate) fn handle_isalpha(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_alphabetic()))
}
pub(crate) fn handle_isdigit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_digit()))
}
pub(crate) fn handle_isalnum(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_alphanumeric()))
}
pub(crate) fn handle_islower(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_lowercase()))
}
pub(crate) fn handle_isupper(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_uppercase()))
}
pub(crate) fn handle_isspace(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(
        engine,
        u64::from(c.is_ascii_whitespace() || c == b'\t' || c == b'\n' || c == b'\r'),
    )
}
pub(crate) fn handle_toupper(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()?;
    ret(engine, u64::from((c as u8).to_ascii_uppercase()))
}
pub(crate) fn handle_tolower(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()?;
    ret(engine, u64::from((c as u8).to_ascii_lowercase()))
}
/// `strcmp(a, b)`.
pub(crate) fn handle_strcmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    if a == 0 || b == 0 {
        let r = match (a, b) {
            (0, 0) => 0_i32,
            (0, _) => -1,
            _ => 1,
        };
        return ret(engine, i32_status_to_u64(r));
    }
    let mut i = 0_u64;
    loop {
        let mut ba = [0_u8; 1];
        let mut bb = [0_u8; 1];
        engine.mem_read(a.wrapping_add(i), &mut ba)?;
        engine.mem_read(b.wrapping_add(i), &mut bb)?;
        if ba[0] != bb[0] {
            let r = i32::from(ba[0]).wrapping_sub(i32::from(bb[0]));
            return ret(engine, i32_status_to_u64(r));
        }
        if ba[0] == 0 {
            return ret(engine, 0);
        }
        i = i.saturating_add(1);
        if i > 1_000_000 {
            return ret(engine, 0);
        }
    }
}
/// `strncpy(dest, src, n)` — copy at most `n` chars from `src` to `dest`.
/// Returns `dest`.
pub(crate) fn handle_strncpy(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let src = engine.read_rdx()?;
    let n = engine.read_r8()?;
    if dest == 0 || src == 0 || n == 0 {
        return ret(engine, dest);
    }
    let cap = usize::try_from(n).unwrap_or(0).min(4096);
    // Read src bytes (up to n, looking for null terminator).
    let mut src_bytes = Vec::with_capacity(cap);
    for i in 0..cap {
        let mut byte = [0_u8; 1];
        if engine
            .mem_read(src.wrapping_add(u64::try_from(i).unwrap_or(0)), &mut byte)
            .is_err()
        {
            break;
        }
        src_bytes.push(byte[0]);
        if byte[0] == 0 {
            break;
        }
    }
    // Write to dest, padding with zeros if src is shorter than n.
    let write_len = src_bytes.len().min(cap);
    let mut buf = vec![0_u8; cap];
    buf[..write_len].copy_from_slice(&src_bytes[..write_len]);
    drop(engine.mem_write(dest, &buf));
    ret(engine, dest)
}
/// `wcscmp(a, b)`.
pub(crate) fn handle_wcscmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    if a == 0 || b == 0 {
        let r = match (a, b) {
            (0, 0) => 0_i32,
            (0, _) => -1,
            _ => 1,
        };
        return ret(engine, i32_status_to_u64(r));
    }
    let mut i = 0_u64;
    loop {
        let mut ba = [0_u8; 2];
        let mut bb = [0_u8; 2];
        let off = i.wrapping_mul(2);
        engine.mem_read(a.wrapping_add(off), &mut ba)?;
        engine.mem_read(b.wrapping_add(off), &mut bb)?;
        let wa = u16::from_le_bytes(ba);
        let wb = u16::from_le_bytes(bb);
        if wa != wb {
            let r = i32::from(wa).wrapping_sub(i32::from(wb));
            return ret(engine, i32_status_to_u64(r));
        }
        if wa == 0 {
            return ret(engine, 0);
        }
        i = i.saturating_add(1);
        if i > 1_000_000 {
            return ret(engine, 0);
        }
    }
}
/// `wcsstr(haystack, needle)` — return pointer to first match or NULL.
pub(crate) fn handle_wcsstr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hay = engine.read_rcx()?;
    let needle = engine.read_rdx()?;
    if hay == 0 || needle == 0 {
        return ret(engine, 0);
    }
    // Read needle
    let mut ndl = Vec::new();
    let mut i = 0_u64;
    loop {
        let mut b = [0_u8; 2];
        engine.mem_read(needle.wrapping_add(i.wrapping_mul(2)), &mut b)?;
        let w = u16::from_le_bytes(b);
        if w == 0 {
            break;
        }
        ndl.push(w);
        i = i.saturating_add(1);
        if i > 100_000 {
            break;
        }
    }
    if ndl.is_empty() {
        return ret(engine, hay);
    }
    // Scan haystack
    let mut hay_units = Vec::new();
    i = 0;
    loop {
        let mut b = [0_u8; 2];
        engine.mem_read(hay.wrapping_add(i.wrapping_mul(2)), &mut b)?;
        let w = u16::from_le_bytes(b);
        if w == 0 {
            break;
        }
        hay_units.push(w);
        i = i.saturating_add(1);
        if i > 1_000_000 {
            break;
        }
    }
    if let Some(pos) = hay_units
        .windows(ndl.len())
        .position(|w| w == ndl.as_slice())
    {
        let addr = hay.wrapping_add(u64::try_from(pos).unwrap_or(0).wrapping_mul(2));
        return ret(engine, addr);
    }
    ret(engine, 0)
}
