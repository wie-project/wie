//! UCRT string/memory/ctype handlers: `mem*`, `str*`, `wcs*`, the
//! character-class helpers, and the secure-CRT `_s` variants (MSVCR100+).

use crate::guest_memory::read_u64;
use crate::guest_string;
use crate::{HandlerContext, WinApiHandlerResult};
use anyhow::{Context, Result};

use super::{EINVAL, ERANGE, finish, i32_status_to_u64, read_guest_str};
pub(crate) fn handle_memcpy(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let src = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || dest == 0 || src == 0 {
        return finish(engine, dest);
    }
    // `mem_copy` resolves both spans inside wie-cpu and uses memmove
    // semantics, so overlapping ranges are handled correctly rather than
    // being punted to a host bounce buffer. Returns false only when a side
    // is not a single mapped span.
    if engine.mem_copy(dest, src, n_usize) {
        return finish(engine, dest);
    }
    // Fallback: cross-arena or SPC-denied.
    let mut buf = vec![0_u8; n_usize];
    engine.mem_read(src, &mut buf)?;
    engine.mem_write(dest, &buf)?;
    finish(engine, dest)
}

pub(crate) fn handle_memcmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || a == 0 || b == 0 {
        return finish(engine, 0);
    }
    // Fast path: both spans in host-contiguous arenas → direct slice compare.
    // Both slices borrow `&engine`, so they can coexist; the borrow ends
    // before `finish` needs `&mut engine`.
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
        return finish(engine, i32_status_to_u64(result));
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
    finish(engine, i32_status_to_u64(result))
}

pub(crate) fn handle_memset(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let c = engine.read_rdx()? & 0xff;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || dest == 0 {
        return finish(engine, dest);
    }
    let value = u8::try_from(c).unwrap_or(0);
    if engine.mem_fill(dest, value, n_usize) {
        return finish(engine, dest);
    }
    // Fallback: bounce through a host Vec only when the span isn't directly writable.
    let buf = vec![value; n_usize];
    engine.mem_write(dest, &buf)?;
    finish(engine, dest)
}
pub(crate) fn handle_strlen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = engine.read_rcx()?;
    if s == 0 {
        return finish(engine, 0);
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
        // borrow ends before `finish` takes `&mut engine`.
        if let Some(found) = engine
            .host_slice(cursor, span_len)
            .map(|slice| slice.iter().position(|&b| b == 0))
        {
            if let Some(off) = found {
                total = total.saturating_add(u64::try_from(off).unwrap_or(0));
                return finish(engine, total);
            }
            total = total.saturating_add(span_len_u64);
            cursor = cursor.wrapping_add(span_len_u64);
            continue;
        }
        // Fallback (unmapped span / protect denied): scalar byte scan of this page.
        let mut buf = [0_u8; 1];
        engine.mem_read(cursor, &mut buf)?;
        if buf[0] == 0 {
            return finish(engine, total);
        }
        total = total.saturating_add(1);
        cursor = cursor.wrapping_add(1);
    }
    finish(engine, total)
}

pub(crate) fn handle_strncmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 {
        return finish(engine, 0);
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
        return finish(engine, i32_status_to_u64(result));
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
    finish(engine, i32_status_to_u64(result))
}
/// `strtol(s, endptr, base)` — parse string to long.
pub(crate) fn handle_strtol(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_va = engine.read_rcx()?;
    let _endptr = engine.read_rdx()?;
    let base = engine.read_r8()?;
    let s = read_guest_str(engine, s_va, 64)?;
    let val = i64::from_str_radix(s.trim(), u32::try_from(base).unwrap_or(10)).unwrap_or(0);
    finish(engine, val as u64)
}

/// `strtoul(s, endptr, base)` — parse string to unsigned long.
pub(crate) fn handle_strtoul(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_va = engine.read_rcx()?;
    let _endptr = engine.read_rdx()?;
    let base = engine.read_r8()?;
    let s = read_guest_str(engine, s_va, 64)?;
    let val = u64::from_str_radix(s.trim(), u32::try_from(base).unwrap_or(10)).unwrap_or(0);
    finish(engine, val)
}

/// `strtod(s, endptr)` — parse string to double.
pub(crate) fn handle_strtod(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_va = engine.read_rcx()?;
    let _endptr = engine.read_rdx()?;
    let s = read_guest_str(engine, s_va, 64)?;
    let val: f64 = s.trim().parse().unwrap_or(0.0);
    finish(engine, val.to_bits())
}

/// `strtok(s, delim)` — tokenize string (single-threaded, static buffer).
pub(crate) fn handle_strtok(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_va = engine.read_rcx()?;
    let d_va = engine.read_rdx()?;
    static SAVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ptr = if s_va == 0 {
        SAVE.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        s_va
    };
    if ptr == 0 {
        return finish(engine, 0);
    }
    let delim = read_guest_str(engine, d_va, 32).unwrap_or_default();
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
            return finish(engine, start);
        }
        end_off += 1;
    }
    // No more delimiters — return remaining token.
    SAVE.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut b = [0_u8; 1];
    if engine.mem_read(start, &mut b).is_ok() && b[0] != 0 {
        finish(engine, start)
    } else {
        finish(engine, 0)
    }
}
/// Byte budget of one `strstr` operand. The per-byte loop this replaced capped
/// at the same order of magnitude; it exists so a string with no NUL cannot
/// scan forever.
const STRSTR_SCAN_BYTES: usize = 1_000_000;

/// `strstr(haystack, needle)` — pointer to the FIRST occurrence of `needle`
/// in `haystack`, or NULL. Empty needle → haystack.
pub(crate) fn handle_strstr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let haystack = engine.read_rcx()?;
    let needle = engine.read_rdx()?;
    if haystack == 0 || needle == 0 {
        return finish(engine, 0);
    }
    // Both operands come back page-sliced (one bulk read per 4 KiB page, NUL
    // excluded, capacity taken from the byte budget instead of a `push` per
    // byte), so the search runs over host slices. `_best_effort` keeps the
    // per-byte loop's shape: an unreadable page ends the string instead of
    // failing the call.
    let hay = guest_string::read_ansi_bytes_best_effort(engine, haystack, STRSTR_SCAN_BYTES)?;
    let ndl = guest_string::read_ansi_bytes_best_effort(engine, needle, STRSTR_SCAN_BYTES)?;
    if ndl.is_empty() {
        return finish(engine, haystack);
    }
    // `windows` yields nothing when the needle is longer than the haystack.
    if let Some(pos) = hay
        .windows(ndl.len())
        .position(|window| window == ndl.as_slice())
    {
        return finish(
            engine,
            haystack.wrapping_add(u64::try_from(pos).unwrap_or(0)),
        );
    }
    finish(engine, 0)
}
/// ctype helpers: isalpha, isdigit, isalnum, islower, isupper, isspace, toupper, tolower.
pub(crate) fn handle_isalpha(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    finish(engine, u64::from(c.is_ascii_alphabetic()))
}
pub(crate) fn handle_isdigit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    finish(engine, u64::from(c.is_ascii_digit()))
}
pub(crate) fn handle_isalnum(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    finish(engine, u64::from(c.is_ascii_alphanumeric()))
}
pub(crate) fn handle_islower(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    finish(engine, u64::from(c.is_ascii_lowercase()))
}
pub(crate) fn handle_isupper(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    finish(engine, u64::from(c.is_ascii_uppercase()))
}
pub(crate) fn handle_isspace(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    finish(
        engine,
        u64::from(c.is_ascii_whitespace() || c == b'\t' || c == b'\n' || c == b'\r'),
    )
}
/// `iswctype(wc, mask)` — wide-char ctype test with an explicit attribute
/// mask, the MS CRT `corecrt_wctype.h` bit layout: `_UPPER` 0x1, `_LOWER`
/// 0x2, `_DIGIT` 0x4, `_SPACE` 0x8, `_PUNCT` 0x10, `_CONTROL` 0x20,
/// `_BLANK` 0x40, `_HEX` 0x80, `_ALPHA` 0x103 (`0x100|_UPPER|_LOWER`),
/// `_LEADBYTE` 0x8000. Returns nonzero when `wc` carries ANY of the bits in
/// `mask`; `WEOF` (0xFFFF) never matches. RNotepad's whole-word Find calls
/// this (via `_istalnum` → `iswalnum` → `iswctype`) with `_ALPHA|_DIGIT`
/// (0x107) on the characters around a candidate match — before this handler
/// existed the dispatch bailed with "unsupported UCRT export" and the whole
/// session stopped (the "Match whole word crashes the app" report).
pub(crate) fn handle_iswctype(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let wc = u32::try_from(engine.read_rcx()?).unwrap_or(0);
    let mask = u32::try_from(engine.read_rdx()?).unwrap_or(0);
    let matched = wc != 0xFFFF && wc_ctype_attrs(wc) & mask != 0;
    finish(engine, u64::from(matched))
}

/// The MS CRT ctype attribute bits for one wide character (see
/// [`handle_iswctype`]).
///
/// The 0x100 bit alone is the "is a letter" marker; `_UPPER`/`_LOWER` are set
/// only for cased letters, so a caseless letter (`中`) matches an `_ALPHA`
/// mask (0x103) but NOT an `_UPPER`/`_LOWER` mask. Unpaired surrogates and
/// non-characters carry no attributes.
fn wc_ctype_attrs(wc: u32) -> u32 {
    let Some(c) = char::from_u32(wc) else {
        return 0;
    };
    let mut attrs = 0_u32;
    if c.is_uppercase() {
        attrs |= 0x0001; // _UPPER
    }
    if c.is_lowercase() {
        attrs |= 0x0002; // _LOWER
    }
    if c.is_ascii_digit() {
        attrs |= 0x0004; // _DIGIT (ASCII-scoped like the narrow ctype handlers)
    }
    if c.is_whitespace() {
        attrs |= 0x0008; // _SPACE
    }
    // _PUNCT: printable but neither alphanumeric, whitespace, nor control.
    if !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control() {
        attrs |= 0x0010;
    }
    if c.is_control() {
        attrs |= 0x0020; // _CONTROL
    }
    if matches!(c, ' ' | '\t') {
        attrs |= 0x0040; // _BLANK
    }
    if c.is_ascii_hexdigit() {
        attrs |= 0x0080; // _HEX
    }
    if c.is_alphabetic() {
        attrs |= 0x0100; // _ALPHA letter bit (caseless letters included)
    }
    attrs
}
pub(crate) fn handle_toupper(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()?;
    finish(engine, u64::from((c as u8).to_ascii_uppercase()))
}
pub(crate) fn handle_tolower(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()?;
    finish(engine, u64::from((c as u8).to_ascii_lowercase()))
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
        return finish(engine, i32_status_to_u64(r));
    }
    let mut i = 0_u64;
    loop {
        let mut ba = [0_u8; 1];
        let mut bb = [0_u8; 1];
        engine.mem_read(a.wrapping_add(i), &mut ba)?;
        engine.mem_read(b.wrapping_add(i), &mut bb)?;
        if ba[0] != bb[0] {
            let r = i32::from(ba[0]).wrapping_sub(i32::from(bb[0]));
            return finish(engine, i32_status_to_u64(r));
        }
        if ba[0] == 0 {
            return finish(engine, 0);
        }
        i = i.saturating_add(1);
        if i > 1_000_000 {
            return finish(engine, 0);
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
        return finish(engine, dest);
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
    finish(engine, dest)
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
        return finish(engine, i32_status_to_u64(r));
    }
    let r = guest_string::utf16_cmp(engine, a, b, 1_000_000)?;
    finish(engine, i32_status_to_u64(r))
}
// ── Page-sliced wide-string scans ──────────────────────────────────────
//
// Every `wcs*` handler below used to call `mem_read(.., &[0u8; 2])` once per
// unit, so a 260-character path paid 260 lock/acquisition/walk cycles per side.
// The shape is the one `guest_string::utf16_len`/`utf16_cmp` already use: one
// bulk access per 4 KiB guest page, with the scan itself running over host
// slices. `mem_read` (not `host_slice`) carries the page here because it also
// survives a cross-arena span and an odd start address, where a `host_slice`
// borrow would fail and force the scalar fallback the readers had to hand-roll.

/// 4 KiB — the guest page size, and the size of one page-sliced access.
const PAGE_BYTES: u64 = 4096;

/// UTF-16 units per page-sliced chunk (one 4 KiB access).
const CHUNK_UNITS: usize = PAGE_BYTES as usize / std::mem::size_of::<u16>();

/// Bytes from `cursor` to the end of its 4 KiB guest page.
fn page_tail(cursor: u64) -> usize {
    let page_end = (cursor | (PAGE_BYTES - 1)).wrapping_add(1);
    usize::try_from(page_end.saturating_sub(cursor)).unwrap_or(0)
}

/// Why a page-sliced wide read stopped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnitsEnd {
    /// The whole requested unit span was read.
    Span,
    /// A NUL unit was read, and it is the last unit in `out`.
    Nul,
    /// An unreadable guest page ended the read; `out` holds the readable prefix.
    Fault,
}

/// Read up to `max_units` UTF-16 units at `addr` into `out`, one bulk access
/// per guest page, stopping at the first NUL unit (which is kept, so `wcsrchr`
/// can match the terminator and `wcscpy` can copy it).
///
/// A stop that is not the caller's own limit is reported rather than swallowed:
/// `Fault` still leaves the readable prefix in `out`, because the per-unit loops
/// processed that prefix before giving up. `fault_is_error` turns the fault into
/// an error instead — the same split `guest_string`'s ANSI reader makes between
/// `read_ansi_bytes` and `read_ansi_bytes_best_effort`.
fn read_units_at(
    engine: &mut dyn wie_cpu::CpuEngine,
    addr: u64,
    max_units: usize,
    out: &mut Vec<u16>,
    fault_is_error: bool,
) -> Result<UnitsEnd> {
    out.clear();
    let mut buf = [0_u8; PAGE_BYTES as usize];
    let mut cursor = addr;
    let mut remaining = max_units;
    while remaining > 0 {
        // An even `take` keeps the unit offset identical in the next chunk, so a
        // unit never straddles two reads and an odd start address needs no
        // special case: a unit is two consecutive bytes, alignment is not part
        // of the contract.
        let budget = remaining.saturating_mul(2).min(buf.len());
        let take = budget.min(page_tail(cursor)) & !1;
        if take == 0 {
            // Last byte of a page (or a one-byte page remainder): one scalar
            // pair, then re-probe — the same fallback `utf16_len` takes.
            let mut pair = [0_u8; 2];
            if let Err(error) = engine.mem_read(cursor, &mut pair) {
                if fault_is_error {
                    return Err(error).context("unreadable page in a wide-string scan");
                }
                return Ok(UnitsEnd::Fault);
            }
            let unit = u16::from_le_bytes(pair);
            out.push(unit);
            if unit == 0 {
                return Ok(UnitsEnd::Nul);
            }
            remaining = remaining.saturating_sub(1);
            cursor = cursor.wrapping_add(2);
            continue;
        }
        let Some(dst) = buf.get_mut(..take) else {
            return Ok(UnitsEnd::Fault);
        };
        if let Err(error) = engine.mem_read(cursor, dst) {
            if fault_is_error {
                return Err(error).context("unreadable page in a wide-string scan");
            }
            return Ok(UnitsEnd::Fault);
        }
        // `take` is even, so every byte of the chunk lands in a whole unit. The
        // NUL check ends the decode here rather than after a whole page: a 260
        // unit path must not pay for the 2048 units the page could hold.
        for pair in dst.as_chunks::<2>().0 {
            let unit = u16::from_le_bytes(*pair);
            out.push(unit);
            if unit == 0 {
                return Ok(UnitsEnd::Nul);
            }
        }
        remaining = remaining.saturating_sub(take / 2);
        cursor = cursor.wrapping_add(u64::try_from(take).unwrap_or(0));
    }
    Ok(UnitsEnd::Span)
}

/// Read the NUL-terminated wide string at `addr` into `out`, NUL excluded,
/// page-sliced like [`read_units_at`] — which means the terminator is found on
/// the host side, one chunk at a time, instead of by one guest read per unit.
fn read_wide_string_at(
    engine: &mut dyn wie_cpu::CpuEngine,
    addr: u64,
    max_units: usize,
    out: &mut Vec<u16>,
    fault_is_error: bool,
) -> Result<()> {
    out.clear();
    let mut chunk: Vec<u16> = Vec::with_capacity(CHUNK_UNITS.min(max_units));
    let mut cursor = addr;
    let mut remaining = max_units;
    while remaining > 0 {
        let want = remaining.min(CHUNK_UNITS);
        let end = read_units_at(engine, cursor, want, &mut chunk, fault_is_error)?;
        let content = if end == UnitsEnd::Nul {
            chunk.get(..chunk.len().saturating_sub(1)).unwrap_or(&[])
        } else {
            chunk.as_slice()
        };
        out.extend_from_slice(content);
        if end != UnitsEnd::Span || chunk.is_empty() {
            return Ok(());
        }
        remaining = remaining.saturating_sub(chunk.len());
        cursor = cursor.wrapping_add(u64::try_from(chunk.len().saturating_mul(2)).unwrap_or(0));
    }
    Ok(())
}

/// Write `units` at `addr`, one bulk store per guest page.
///
/// Write faults are dropped: the per-unit loops dropped them too (a `wcs*`
/// destination is the caller's promise, not something to report), and a
/// partially writable buffer must keep the prefix that did land.
fn write_units_at(engine: &mut dyn wie_cpu::CpuEngine, mut addr: u64, mut units: &[u16]) {
    let mut buf = [0_u8; PAGE_BYTES as usize];
    while let Some((unit, rest)) = units.split_first() {
        let in_page = page_tail(addr) & !1;
        if in_page < 2 {
            // A unit spanning two pages: one scalar store, then re-probe.
            drop(engine.mem_write(addr, &unit.to_le_bytes()));
            addr = addr.wrapping_add(2);
            units = rest;
            continue;
        }
        let count = (in_page / 2).min(units.len());
        let Some(dst) = buf.get_mut(..count.saturating_mul(2)) else {
            return;
        };
        for (pair, unit) in dst.as_chunks_mut::<2>().0.iter_mut().zip(units) {
            *pair = unit.to_le_bytes();
        }
        drop(engine.mem_write(addr, dst));
        addr = addr.wrapping_add(u64::try_from(count.saturating_mul(2)).unwrap_or(0));
        units = units.get(count..).unwrap_or(&[]);
    }
}

/// Copy the NUL-terminated wide string at `src` over `dest`, terminator
/// included, one page-sliced chunk at a time.
///
/// Streaming rather than materialise-then-store keeps the host copy bounded: the
/// scan cap is 1 M units, which as a host buffer would be a 2 MB allocation for
/// a plain `wcscpy`. It also preserves the forward copy order of the old
/// per-unit loop.
fn copy_wide_string(engine: &mut dyn wie_cpu::CpuEngine, dest: u64, src: u64) -> Result<()> {
    let mut chunk: Vec<u16> = Vec::with_capacity(CHUNK_UNITS);
    let mut read_cursor = src;
    let mut write_cursor = dest;
    let mut remaining = 1_000_000_usize;
    while remaining > 0 {
        let want = remaining.min(CHUNK_UNITS);
        let end = read_units_at(engine, read_cursor, want, &mut chunk, false)?;
        if chunk.is_empty() {
            return Ok(());
        }
        // The chunk already ends at the terminator when there is one: `wcscpy`
        // and `wcscat` write it, so it is copied rather than trimmed.
        write_units_at(engine, write_cursor, &chunk);
        if end != UnitsEnd::Span {
            return Ok(());
        }
        let advance = u64::try_from(chunk.len().saturating_mul(2)).unwrap_or(0);
        read_cursor = read_cursor.wrapping_add(advance);
        write_cursor = write_cursor.wrapping_add(advance);
        remaining = remaining.saturating_sub(chunk.len());
    }
    Ok(())
}

/// `wcsstr(haystack, needle)` — return pointer to first match or NULL.
pub(crate) fn handle_wcsstr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hay = engine.read_rcx()?;
    let needle = engine.read_rdx()?;
    if hay == 0 || needle == 0 {
        return finish(engine, 0);
    }
    // Both operands are read page-sliced; an unreadable page still fails the
    // call, which is what the per-unit loop did (it propagated `mem_read`).
    let mut ndl: Vec<u16> = Vec::with_capacity(CHUNK_UNITS);
    read_wide_string_at(engine, needle, 100_000, &mut ndl, true)?;
    if ndl.is_empty() {
        return finish(engine, hay);
    }
    let mut hay_units: Vec<u16> = Vec::with_capacity(CHUNK_UNITS);
    read_wide_string_at(engine, hay, 1_000_000, &mut hay_units, true)?;
    // `windows` yields nothing when the needle is longer than the haystack.
    if let Some(pos) = hay_units
        .windows(ndl.len())
        .position(|window| window == ndl.as_slice())
    {
        let addr = hay.wrapping_add(u64::try_from(pos).unwrap_or(0).wrapping_mul(2));
        return finish(engine, addr);
    }
    finish(engine, 0)
}
/// `wcslen(s)` — number of wide units before the NUL.
pub(crate) fn handle_wcslen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = engine.read_rcx()?;
    if s == 0 {
        return finish(engine, 0);
    }
    let len = guest_string::utf16_len(engine, s, 1_000_000)?;
    finish(engine, len)
}
/// `wcscpy(dest, src)` — copy the wide string including its NUL; returns `dest`.
pub(crate) fn handle_wcscpy(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let src = engine.read_rdx()?;
    if dest == 0 || src == 0 {
        return finish(engine, dest);
    }
    copy_wide_string(engine, dest, src)?;
    finish(engine, dest)
}
/// `wcscat(dest, src)` — append `src` over `dest`'s terminator; returns `dest`.
pub(crate) fn handle_wcscat(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let src = engine.read_rdx()?;
    if dest == 0 || src == 0 {
        return finish(engine, dest);
    }
    // Locate the end of dest: page-sliced, and an unreadable page ends the scan,
    // exactly as the per-unit walk did.
    let offset = guest_string::utf16_len(engine, dest, 1_000_000)?;
    // Copy src over the terminator.
    copy_wide_string(engine, dest.wrapping_add(offset.wrapping_mul(2)), src)?;
    finish(engine, dest)
}
/// `wcsncmp(a, b, n)` — compare up to `n` wide units.
pub(crate) fn handle_wcsncmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let count = n.min(1_000_000);
    if count == 0 {
        return finish(engine, 0);
    }
    // One lock per 4 KiB page on each side, not one per unit per side.
    let count_us = usize::try_from(count).unwrap_or(0);
    let r = guest_string::utf16_cmp(engine, a, b, count_us)?;
    finish(engine, i32_status_to_u64(r))
}
/// `wcsncpy(dest, src, n)` — copy up to `n` wide units, padding with NULs.
pub(crate) fn handle_wcsncpy(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let src = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let count = usize::try_from(n.min(4096)).unwrap_or(0);
    if dest == 0 || src == 0 || count == 0 {
        return finish(engine, dest);
    }
    // One bounded page-sliced read of the source, then one page-sliced store:
    // the short-source padding is a host `resize`, not a store per unit. An
    // unreadable source page ends the read, and the remainder is padded — which
    // is what the per-unit loop did (a failed read left its `[0u8; 2]` as the
    // written pair and padded the rest).
    let mut out: Vec<u16> = Vec::with_capacity(count);
    read_wide_string_at(engine, src, count, &mut out, false)?;
    out.resize(count, 0);
    write_units_at(engine, dest, &out);
    finish(engine, dest)
}
/// The ASCII fold `_wcsnicmp` applies per unit: codes above 0xFF map to
/// themselves, `A`–`Z` fold to lowercase (the same fold the ctype helpers use,
/// so non-ASCII never folds).
fn fold_ascii(unit: u16) -> u16 {
    u8::try_from(unit)
        .map(|b| u16::from(b.to_ascii_lowercase()))
        .unwrap_or(unit)
}

/// `_wcsnicmp(a, b, n)` — case-insensitive `wcsncmp` (ASCII fold, like the
/// ctype helpers; non-ASCII maps to itself).
pub(crate) fn handle_wcsnicmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let mut remaining = usize::try_from(n.min(1_000_000)).unwrap_or(0);
    if remaining == 0 {
        return finish(engine, 0);
    }
    // Lockstep page-sliced compare: both cursors advance by the same amount, so
    // one bulk read per 4 KiB page per side replaces one read per unit per side
    // while the bail-out on the first differing unit is unchanged.
    let mut chunk_a: Vec<u16> = Vec::with_capacity(CHUNK_UNITS);
    let mut chunk_b: Vec<u16> = Vec::with_capacity(CHUNK_UNITS);
    let mut cursor_a = a;
    let mut cursor_b = b;
    while remaining > 0 {
        let want = remaining.min(CHUNK_UNITS);
        let end_a = read_units_at(engine, cursor_a, want, &mut chunk_a, false)?;
        let end_b = read_units_at(engine, cursor_b, want, &mut chunk_b, false)?;
        for (wa, wb) in chunk_a.iter().zip(chunk_b.iter()) {
            let (la, lb) = (fold_ascii(*wa), fold_ascii(*wb));
            if la != lb {
                let r = i32::from(la).wrapping_sub(i32::from(lb));
                return finish(engine, i32_status_to_u64(r));
            }
            if *wa == 0 {
                return finish(engine, 0);
            }
        }
        let compared = chunk_a.len().min(chunk_b.len());
        if end_a != UnitsEnd::Span || end_b != UnitsEnd::Span {
            // A terminator (already compared above) or an unreadable page ends
            // the compare equal, as the per-unit `break` did.
            return finish(engine, 0);
        }
        remaining = remaining.saturating_sub(compared);
        let advance = u64::try_from(compared.saturating_mul(2)).unwrap_or(0);
        cursor_a = cursor_a.wrapping_add(advance);
        cursor_b = cursor_b.wrapping_add(advance);
    }
    finish(engine, 0)
}
/// `towupper(c)` — uppercase a wide character (ASCII subset, like `toupper`).
pub(crate) fn handle_towupper(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()?;
    let w = u16::try_from(c & 0xffff).unwrap_or(0);
    let upper = u8::try_from(w)
        .map(|b| u16::from(b.to_ascii_uppercase()))
        .unwrap_or(w);
    finish(engine, u64::from(upper))
}
/// `wcsrchr(s, c)` — pointer to the LAST occurrence of wide char `c`, or NULL.
pub(crate) fn handle_wcsrchr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = engine.read_rcx()?;
    let c = u16::try_from(engine.read_rdx()? & 0xffff).unwrap_or(0);
    if s == 0 {
        return finish(engine, 0);
    }
    let mut last: u64 = 0;
    let mut chunk: Vec<u16> = Vec::with_capacity(CHUNK_UNITS);
    let mut cursor = s;
    let mut remaining = 1_000_000_usize;
    while remaining > 0 {
        let want = remaining.min(CHUNK_UNITS);
        let end = read_units_at(engine, cursor, want, &mut chunk, false)?;
        if chunk.is_empty() {
            break;
        }
        // A chunk never runs past a terminator, and the terminator is itself a
        // matchable unit (`wcsrchr(s, 0)` finds it), so the last match in the
        // chunk is the last match in the readable string.
        if let Some(pos) = chunk.iter().rposition(|&unit| unit == c) {
            last = cursor.wrapping_add(u64::try_from(pos).unwrap_or(0).wrapping_mul(2));
        }
        if end != UnitsEnd::Span {
            break;
        }
        remaining = remaining.saturating_sub(chunk.len());
        cursor = cursor.wrapping_add(u64::try_from(chunk.len().saturating_mul(2)).unwrap_or(0));
    }
    finish(engine, last)
}
/// `strchr(s, c)` — pointer to the FIRST occurrence of byte `c`, or NULL.
pub(crate) fn handle_strchr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = engine.read_rcx()?;
    let c = u8::try_from(engine.read_rdx()? & 0xff).unwrap_or(0);
    if s == 0 {
        return finish(engine, 0);
    }
    let mut i = 0_u64;
    loop {
        let mut b = [0_u8; 1];
        if engine.mem_read(s.wrapping_add(i), &mut b).is_err() {
            break;
        }
        if b[0] == c {
            return finish(engine, s.wrapping_add(i));
        }
        if b[0] == 0 {
            break;
        }
        i = i.saturating_add(1);
        if i > 1_000_000 {
            break;
        }
    }
    finish(engine, 0)
}

// ═══════════════════════════════════════════════════════════════════════
// Secure-CRT `_s` variants (MSVCR100+)
// ═══════════════════════════════════════════════════════════════════════
//
// Every handler follows the CRT contract: `errno_t` return (0 on success),
// sizes validated before touching the destination, and the destination
// cleared (at least `dest[0] = 0`) on failure so it can never be misread as
// a valid string.

/// Zero `n` bytes at `va` (secure-CRT failure contract). Failures ignored —
/// the write is best-effort on a buffer the guest already owns.
fn zero_memory(engine: &mut dyn wie_cpu::CpuEngine, va: u64, n: usize) {
    if va != 0 && n > 0 {
        let zeros = vec![0_u8; n];
        drop(engine.mem_write(va, &zeros));
    }
}

/// Write one NUL byte at `dest` (the "empty string on failure" contract).
fn empty_string(engine: &mut dyn wie_cpu::CpuEngine, dest: u64) {
    if dest != 0 {
        drop(engine.mem_write(dest, &[0_u8]));
    }
}

/// Shared core of `memcpy_s` / `memmove_s`: bounds-checked copy with the
/// `errno_t` contract. `engine.mem_copy` already has memmove semantics, so
/// the overlap-undefined `memcpy_s` case is served safely.
fn mem_copy_s_core(
    engine: &mut dyn wie_cpu::CpuEngine,
    dest: u64,
    destsz: u64,
    src: u64,
    count: u64,
) -> Result<WinApiHandlerResult> {
    if dest == 0 || src == 0 {
        zero_memory(engine, dest, usize::try_from(destsz).unwrap_or(0));
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    if count == 0 {
        return finish(engine, 0);
    }
    if count > destsz {
        zero_memory(engine, dest, usize::try_from(destsz).unwrap_or(0));
        return finish(engine, i32_status_to_u64(ERANGE));
    }
    let n = usize::try_from(count).unwrap_or(0);
    if engine.mem_copy(dest, src, n) {
        return finish(engine, 0);
    }
    // Cross-arena or SPC-denied: bounce through a host buffer.
    let mut buf = vec![0_u8; n];
    engine.mem_read(src, &mut buf)?;
    engine.mem_write(dest, &buf)?;
    finish(engine, 0)
}

/// `memcpy_s(dest, destsz, src, count)` — `(dest, destsz, src, count)` order.
pub(crate) fn handle_memcpy_s(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let destsz = engine.read_rdx()?;
    let src = engine.read_r8()?;
    let count = engine.read_r9()?;
    mem_copy_s_core(engine, dest, destsz, src, count)
}

/// `memmove_s(dest, destsz, src, count)` — same order and contract.
pub(crate) fn handle_memmove_s(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let destsz = engine.read_rdx()?;
    let src = engine.read_r8()?;
    let count = engine.read_r9()?;
    mem_copy_s_core(engine, dest, destsz, src, count)
}

/// `memset_s(dest, destsz, value, count)` — bounds-checked fill.
pub(crate) fn handle_memset_s(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let destsz = engine.read_rdx()?;
    let value = u8::try_from(engine.read_r8()? & 0xff).unwrap_or(0);
    let count = engine.read_r9()?;
    if dest == 0 {
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    if count == 0 {
        return finish(engine, 0);
    }
    if count > destsz {
        zero_memory(engine, dest, usize::try_from(destsz).unwrap_or(0));
        return finish(engine, i32_status_to_u64(ERANGE));
    }
    let n = usize::try_from(count).unwrap_or(0);
    if engine.mem_fill(dest, value, n) {
        return finish(engine, 0);
    }
    let buf = vec![value; n];
    engine.mem_write(dest, &buf)?;
    finish(engine, 0)
}

/// `strcpy_s(dest, destsz, src)` — copy a NUL-terminated string into a
/// sized buffer; ERANGE (with `dest[0] = 0`) when the string does not fit.
pub(crate) fn handle_strcpy_s(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let destsz = engine.read_rdx()?;
    let src = engine.read_r8()?;
    if dest == 0 || destsz == 0 {
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    if src == 0 {
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    let cap = usize::try_from(destsz).unwrap_or(0);
    let bytes = crate::guest_string::read_ansi_bytes(engine, src, cap).unwrap_or_default();
    // No NUL found within the budget → the terminator cannot fit.
    if bytes.len() >= cap {
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(ERANGE));
    }
    let mut out = bytes;
    out.push(0);
    engine.mem_write(dest, &out)?;
    finish(engine, 0)
}

/// `strncpy_s(dest, destsz, src, count)` — copy at most `count` chars. A
/// short source is NUL-padded to `count` (strncpy behavior); a source of
/// `count` or more chars is copied exactly (no NUL, no error). `count` must
/// fit in `destsz` or ERANGE is returned with `dest[0] = 0`.
pub(crate) fn handle_strncpy_s(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let destsz = engine.read_rdx()?;
    let src = engine.read_r8()?;
    let count = engine.read_r9()?;
    if dest == 0 || destsz == 0 {
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    if src == 0 {
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    if count == 0 {
        return finish(engine, 0);
    }
    let dest_cap = usize::try_from(destsz).unwrap_or(0);
    let count_us = usize::try_from(count).unwrap_or(0);
    if count_us > dest_cap {
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(ERANGE));
    }
    // Read up to count+1 bytes so a NUL found at exactly `count` is visible
    // (a short source then pads the rest of `count` with NULs).
    let bytes = crate::guest_string::read_ansi_bytes(engine, src, count_us.saturating_add(1))
        .unwrap_or_default();
    if bytes.len() < count_us {
        // Short source: content + NUL, then NUL-pad to `count`.
        let mut out = bytes;
        out.push(0);
        out.resize(count_us, 0);
        engine.mem_write(dest, &out)?;
    } else {
        // Source has at least `count` chars: copy exactly `count` (no NUL).
        let mut out = bytes;
        out.truncate(count_us);
        engine.mem_write(dest, &out)?;
    }
    finish(engine, 0)
}

/// `strcat_s(dest, destsz, src)` — size-checked append.
pub(crate) fn handle_strcat_s(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let destsz = engine.read_rdx()?;
    let src = engine.read_r8()?;
    if dest == 0 || destsz == 0 {
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    if src == 0 {
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    let cap = usize::try_from(destsz).unwrap_or(0);
    let dest_bytes = crate::guest_string::read_ansi_bytes(engine, dest, cap).unwrap_or_default();
    if dest_bytes.len() >= cap {
        // Destination is not NUL-terminated inside the buffer.
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    let src_bytes = crate::guest_string::read_ansi_bytes(engine, src, cap).unwrap_or_default();
    if dest_bytes
        .len()
        .saturating_add(src_bytes.len())
        .saturating_add(1)
        > cap
    {
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(ERANGE));
    }
    let mut out = dest_bytes;
    out.extend_from_slice(&src_bytes);
    out.push(0);
    engine.mem_write(dest, &out)?;
    finish(engine, 0)
}

/// `strncat_s(dest, destsz, src, count)` — size-checked append of at most
/// `count` source chars.
pub(crate) fn handle_strncat_s(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let destsz = engine.read_rdx()?;
    let src = engine.read_r8()?;
    let count = engine.read_r9()?;
    if dest == 0 || destsz == 0 {
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    if src == 0 {
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    let cap = usize::try_from(destsz).unwrap_or(0);
    let dest_bytes = crate::guest_string::read_ansi_bytes(engine, dest, cap).unwrap_or_default();
    if dest_bytes.len() >= cap {
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    let count_us = usize::try_from(count).unwrap_or(0);
    let src_bytes = crate::guest_string::read_ansi_bytes(engine, src, count_us.saturating_add(1))
        .unwrap_or_default();
    let appended = count_us.min(src_bytes.len());
    if dest_bytes.len().saturating_add(appended).saturating_add(1) > cap {
        empty_string(engine, dest);
        return finish(engine, i32_status_to_u64(ERANGE));
    }
    let mut out = dest_bytes;
    out.extend_from_slice(src_bytes.get(..appended).unwrap_or(&[]));
    out.push(0);
    engine.mem_write(dest, &out)?;
    finish(engine, 0)
}

/// `strtok_s(str, delim, context)` — context-pointer tokenizer (also serves
/// `_strtok_s`). The continuation pointer lives in the guest `char**` cell
/// instead of the static buffer `strtok` uses; `*context` is set to NULL when
/// no more tokens exist (the secure contract).
pub(crate) fn handle_strtok_s(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let str_va = engine.read_rcx()?;
    let d_va = engine.read_rdx()?;
    let context_va = engine.read_r8()?;
    if context_va == 0 || d_va == 0 {
        return finish(engine, 0);
    }
    let mut saved_bytes = [0_u8; 8];
    if engine.mem_read(context_va, &mut saved_bytes).is_err() {
        return finish(engine, 0);
    }
    let saved = u64::from_le_bytes(saved_bytes);
    let ptr = if str_va == 0 { saved } else { str_va };
    if ptr == 0 {
        return finish(engine, 0);
    }
    let delim = read_guest_str(engine, d_va, 32).unwrap_or_default();
    // Skip leading delimiters.
    let mut start = ptr;
    loop {
        let mut b = [0_u8; 1];
        if engine.mem_read(start, &mut b).is_err() {
            break;
        }
        if b[0] == 0 {
            break;
        }
        if delim.contains(b[0] as char) {
            start = start.wrapping_add(1);
            continue;
        }
        break;
    }
    // Find the first delimiter after `start`; replace it with NUL.
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
            let next = start.wrapping_add(end_off).wrapping_add(1);
            engine.mem_write(context_va, &next.to_le_bytes())?;
            return finish(engine, start);
        }
        end_off = end_off.saturating_add(1);
    }
    // No more delimiters: the final token; the secure contract NULLs the
    // context so a later call with NULL str stops cleanly.
    engine.mem_write(context_va, &0_u64.to_le_bytes())?;
    let mut b = [0_u8; 1];
    if engine.mem_read(start, &mut b).is_ok() && b[0] != 0 {
        finish(engine, start)
    } else {
        finish(engine, 0)
    }
}

// ── qsort_s (guest-comparator re-entry bridge) ──────────────────────────

/// One in-flight `qsort_s` sort per guest thread.
///
/// The guest comparator is invoked through the re-entry bridge the pthread
/// exports use: the handler overwrites its own return-address slot with its
/// fake VA, jumps to the comparator, and the comparator's `ret` re-enters
/// this export, where the pending record routes to the next step. RSP is
/// restored to the original caller frame before every comparator call so the
/// guest stack never walks up into the caller's locals and `rsp % 16 == 8`
/// alignment holds for each call.
struct QsortPending {
    base: u64,
    count: usize,
    size: usize,
    context: u64,
    compare: u64,
    /// Original caller return address (restored on finish).
    return_va: u64,
    /// Original caller RSP — the return-address slot is at this address.
    return_rsp: u64,
    /// Insertion-sort outer cursor: element `i` is being inserted.
    i: usize,
    /// Right-hand compare index (`i-1` downward; `-1` = front reached).
    j: i64,
    /// Host copy of the element being inserted (written back at its slot).
    key: Vec<u8>,
    /// Guest copy of the same element (the comparator's `elem2` argument).
    key_va: u64,
}

/// Per-thread pending sorts. Keyed by guest TID; a comparator that itself
/// calls `qsort_s` would clobber the outer record (documented limitation).
static QSORT_PENDING: std::sync::LazyLock<std::sync::Mutex<ahash::HashMap<u32, QsortPending>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(ahash::HashMap::default()));

/// `qsort_s(base, count, size, compare, context)` — context-pointer qsort.
///
/// MSDN argument order: `qsort_s(base, num, width, compare, context)` — the
/// comparator comes fourth (R9) and the context fifth (`[rsp+0x28]`). An
/// insertion sort drives one guest comparator call per element-pair via the
/// re-entry bridge.
pub(crate) fn handle_qsort_s(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let tid = state.kernel.threads.current_tid();

    // Re-entry: the guest comparator returned; RAX holds its result.
    let taken = QSORT_PENDING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&tid);
    if let Some(qs) = taken {
        let raw = engine.read_rax()? & u64::from(u32::MAX);
        let cmp = i32::from_ne_bytes(u32::try_from(raw).unwrap_or(0).to_ne_bytes());
        return qsort_advance(engine, state, qs, cmp);
    }

    let base = engine.read_rcx()?;
    let count_raw = engine.read_rdx()?;
    let size_raw = engine.read_r8()?;
    let compare = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let mut ctx_buf = [0_u8; 8];
    engine.mem_read(rsp.wrapping_add(0x28), &mut ctx_buf)?;
    let context = u64::from_le_bytes(ctx_buf);

    let count = usize::try_from(count_raw).unwrap_or(0);
    let size = usize::try_from(size_raw).unwrap_or(0);
    if base == 0 || compare == 0 || (count > 0 && size == 0) {
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    if count <= 1 {
        return finish(engine, 0);
    }
    // First key: element 1 (element 0 is compared against it).
    let key_va = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, u64::try_from(size).unwrap_or(0));
    if key_va == 0 {
        return finish(engine, i32_status_to_u64(EINVAL));
    }
    let mut key = vec![0_u8; size];
    engine.mem_read(qsort_elem_va(base, size, 1), &mut key)?;
    engine.mem_write(key_va, &key)?;
    let return_va = read_u64(engine, rsp).unwrap_or(0);
    let qs = QsortPending {
        base,
        count,
        size,
        context,
        compare,
        return_va,
        return_rsp: rsp,
        i: 1,
        j: 0,
        key,
        key_va,
    };
    qsort_park_and_compare(engine, state, qs)
}

/// Guest address of element `index` (bounds are guaranteed by the caller).
fn qsort_elem_va(base: u64, size: usize, index: usize) -> u64 {
    base.wrapping_add(
        u64::try_from(index)
            .unwrap_or(0)
            .wrapping_mul(u64::try_from(size).unwrap_or(0)),
    )
}

/// Advance one insertion-sort step after the comparator returned `cmp`.
fn qsort_advance(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut crate::WinApiState,
    mut qs: QsortPending,
    cmp: i32,
) -> Result<WinApiHandlerResult> {
    if cmp > 0 {
        // base[j] > key → shift base[j] up one slot and scan left.
        let j = usize::try_from(qs.j).unwrap_or(0);
        let src = qsort_elem_va(qs.base, qs.size, j);
        let dst = qsort_elem_va(qs.base, qs.size, j.saturating_add(1));
        if !engine.mem_copy(dst, src, qs.size) {
            let mut buf = vec![0_u8; qs.size];
            engine.mem_read(src, &mut buf)?;
            engine.mem_write(dst, &buf)?;
        }
        qs.j = qs.j.saturating_sub(1);
        if qs.j < 0 {
            // Front reached: the key goes at base[0].
            engine.mem_write(qs.base, &qs.key)?;
            qsort_next(engine, state, qs)
        } else {
            qsort_park_and_compare(engine, state, qs)
        }
    } else {
        // base[j] <= key → insert the key right after base[j].
        let j = usize::try_from(qs.j).unwrap_or(0);
        engine.mem_write(
            qsort_elem_va(qs.base, qs.size, j.saturating_add(1)),
            &qs.key,
        )?;
        qsort_next(engine, state, qs)
    }
}

/// Move the outer cursor to the next element, or finish the sort.
fn qsort_next(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut crate::WinApiState,
    mut qs: QsortPending,
) -> Result<WinApiHandlerResult> {
    qs.i = qs.i.saturating_add(1);
    if qs.i >= qs.count {
        // Done: release the guest key copy and return to the caller.
        let _ = state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .free_coherent(engine, qs.key_va);
        engine.write_rsp(qs.return_rsp)?;
        engine.mem_write(qs.return_rsp, &qs.return_va.to_le_bytes())?;
        return finish(engine, 0);
    }
    let mut key = vec![0_u8; qs.size];
    engine.mem_read(qsort_elem_va(qs.base, qs.size, qs.i), &mut key)?;
    engine.mem_write(qs.key_va, &key)?;
    qs.key = key;
    qs.j = i64::try_from(qs.i).unwrap_or(0).saturating_sub(1);
    qsort_park_and_compare(engine, state, qs)
}

/// Store the pending record and dispatch the next comparator call.
fn qsort_park_and_compare(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut crate::WinApiState,
    qs: QsortPending,
) -> Result<WinApiHandlerResult> {
    let tid = state.kernel.threads.current_tid();
    {
        let mut guard = QSORT_PENDING.lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(tid, qs);
    }
    // Re-fetch the Copy fields the call setup needs (qs moved into the map).
    let (base, j, size, key_va, context, compare, return_rsp) = {
        let guard = QSORT_PENDING.lock().unwrap_or_else(|p| p.into_inner());
        let q = guard
            .get(&tid)
            .ok_or_else(|| anyhow::anyhow!("qsort_s pending record vanished"))?;
        (
            q.base,
            q.j,
            q.size,
            q.key_va,
            q.context,
            q.compare,
            q.return_rsp,
        )
    };
    // Reset to the caller frame, plant the re-entry, and enter the comparator.
    engine.write_rsp(return_rsp)?;
    let self_va = engine.read_rip()?;
    engine.mem_write(return_rsp, &self_va.to_le_bytes())?;
    engine.write_rcx(context)?;
    let left = qsort_elem_va(base, size, usize::try_from(j).unwrap_or(0));
    engine.write_rdx(left)?;
    engine.write_r8(key_va)?;
    engine.write_rip(compare)?;
    Ok(WinApiHandlerResult {
        return_address: compare,
        return_value: 0,
    })
}
