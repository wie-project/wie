//! Guest string helpers: page-safe guest reads plus UTF-16 / ANSI / UTF-8
//! conversions at the guest–host boundary. Reads stop at 4 KiB page
//! boundaries so an unmapped tail page cannot fail a valid prefix.
//!
//! The ANSI paths use the WHATWG windows-1252 codec (`encoding_rs`) — the
//! single-byte mapping Windows uses for ACP 1252, C1 range (0x80–0x9F)
//! included.

use anyhow::{Context, Result};

/// 4 KiB — the guest page size. Bulk reads stop at the page boundary so an
/// unmapped following page cannot fail a read whose prefix is valid.
const PAGE_SIZE: u64 = 4096;

/// Read up to `len` guest bytes starting at `addr` into `buf`, staying inside the
/// current 4 KiB page. Returns the number of bytes actually copied (≤ `len`).
///
/// `mem_read` already performs one software-permission check and one bulk copy
/// for the whole slice, so a single call per page is all the amortisation the
/// byte-at-a-time loop needed. An earlier version used `host_span` plus
/// `copy_nonoverlapping` here, which did the *same* copy behind an `unsafe`
/// block for no additional benefit.
fn read_page_slice(
    engine: &mut dyn wie_cpu::CpuEngine,
    addr: u64,
    buf: &mut [u8],
) -> Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let page_end = (addr | (PAGE_SIZE - 1)).wrapping_add(1);
    let in_page = page_end
        .saturating_sub(addr)
        .min(u64::try_from(buf.len()).unwrap_or(u64::MAX));
    let take = usize::try_from(in_page).unwrap_or(buf.len()).min(buf.len());
    let dst = buf.get_mut(..take).context("page-slice buf too small")?;

    engine
        .mem_read(addr, dst)
        .context("failed bulk read in page-slice")?;
    Ok(take)
}

/// Decode a host byte slice as an ANSI string (the A-path decode), stopping
/// at the first NUL byte.
///
/// A-strings are UTF-8 in WIE when the guest was compiled with mingw (the
/// toolchain stores string literals as UTF-8); real Windows binaries pass
/// ACP-encoded strings. Bytes that are not valid UTF-8 therefore fall back to
/// a strict Windows-1252 decode — the WHATWG codec, identical to Windows
/// codepage 1252 including the 0x80–0x9F C1 range.
///
/// The NUL stop matters for the fixed-size-buffer callers (a `CHAR[32]` face
/// name): bytes past the terminator are uninitialized garbage. [`read_ansi_lossy`]
/// already strips the terminator before decoding, so the stop is a no-op there.
pub(crate) fn decode_ansi_lossy(bytes: &[u8]) -> String {
    let head = bytes
        .iter()
        .take_while(|&&byte| byte != 0)
        .copied()
        .collect::<Vec<u8>>();
    match std::str::from_utf8(&head) {
        Ok(valid) => valid.to_owned(),
        Err(_) => {
            let (decoded, _, _) = encoding_rs::WINDOWS_1252.decode(&head);
            decoded.into_owned()
        }
    }
}

pub(crate) fn read_ansi_lossy(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_bytes: usize,
) -> Result<String> {
    let bytes = read_ansi_bytes(engine, address, max_bytes)?;
    Ok(decode_ansi_lossy(&bytes))
}

/// Read raw ANSI bytes (NUL-terminated), excluding the terminator.
///
/// Reads guest memory in 4 KiB slices (one lock acquisition per page) straight
/// into the destination buffer, then scans for NUL on the host side — orders of
/// magnitude cheaper than a byte-per-lock loop, which is what every caller of
/// this used to hand-roll.
pub(crate) fn read_ansi_bytes(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    read_ansi_bytes_inner(engine, address, max_bytes, true)
}

/// [`read_ansi_bytes`], but an unreadable guest page ends the string instead of
/// failing the call: whatever prefix was read stays readable.
///
/// For console output sinks only. A truncated line is a better outcome than a
/// handler error, and a line that ends at an unmapped page still has its NUL in
/// the last mapped page, so the truncation does not bite in practice.
pub(crate) fn read_ansi_bytes_best_effort(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    read_ansi_bytes_inner(engine, address, max_bytes, false)
}

/// `fault_is_error == false` stops on a fault instead of propagating it.
fn read_ansi_bytes_inner(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_bytes: usize,
    fault_is_error: bool,
) -> Result<Vec<u8>> {
    if address == 0 || max_bytes == 0 {
        return Ok(Vec::new());
    }

    // Preallocate the common short-string size; long paths grow geometrically.
    // Bytes are read straight into the tail of this buffer — a staging array
    // would add a second copy of every string for no benefit (the page-slice
    // rule only needs the *read* to stop at a page boundary, which it does).
    let mut bytes: Vec<u8> = Vec::with_capacity(max_bytes.min(128));
    let mut remaining = max_bytes;
    let mut cursor = address;

    while remaining > 0 {
        let start = bytes.len();
        let want = remaining.min(PAGE_SIZE as usize);
        // Zero-fill is overwritten by the guest read below; it exists so the
        // buffer's length, not its contents, tracks progress.
        bytes.resize(start + want, 0);
        let tail = bytes.get_mut(start..).context("ANSI tail bounds")?;
        let got = match read_page_slice(engine, cursor, tail) {
            Ok(got) => got,
            Err(_unreadable) if !fault_is_error => {
                bytes.truncate(start);
                break;
            }
            Err(error) => return Err(error),
        };
        if got == 0 {
            bytes.truncate(start);
            break;
        }
        let chunk = bytes.get(start..start + got).context("ANSI chunk bounds")?;
        if let Some(nul) = chunk.iter().position(|&byte| byte == 0) {
            bytes.truncate(start + nul);
            return Ok(bytes);
        }
        bytes.truncate(start + got);
        cursor = cursor.wrapping_add(u64::try_from(got).unwrap_or(0));
        remaining = remaining.saturating_sub(got);
    }

    Ok(bytes)
}

/// Decode a host unit slice as a UTF-16 string (the W-path decode), stopping
/// at the first NUL unit.
///
/// Fixed-size buffers (a `WCHAR[32]` face name) carry uninitialized garbage
/// past the terminator, so the decode must stop at the NUL unit.
/// [`read_utf16_lossy`]'s read loop already stops at the NUL, so the stop is
/// a no-op there.
pub(crate) fn decode_utf16_lossy(units: &[u16]) -> String {
    let head = units
        .iter()
        .take_while(|&&unit| unit != 0)
        .copied()
        .collect::<Vec<u16>>();
    String::from_utf16_lossy(&head)
}

/// Compare two guest UTF-16 strings unit by unit, page-sliced, up to `max_units`
/// and stopping at the first NUL in either.
///
/// Returns the `wcscmp` convention: `0` when equal over the compared prefix,
/// otherwise the sign-carrying difference of the first differing pair. Each
/// side is read in 4 KiB borrowed slices, so a 260-character path comparison
/// costs two lock acquisitions instead of 260 per side.
pub(crate) fn utf16_cmp(
    engine: &mut dyn wie_cpu::CpuEngine,
    a: u64,
    b: u64,
    max_units: usize,
) -> Result<i32> {
    let mut cursor_a = a;
    let mut cursor_b = b;
    let mut remaining = max_units;
    // Odd-address tails: a unit whose halves land in different borrows.
    let mut tail_a: Option<u8> = None;
    let mut tail_b: Option<u8> = None;

    while remaining > 0 {
        let span = page_span(cursor_a, remaining);
        let want = span.min(page_span(cursor_b, remaining)).min(remaining * 2);
        if want == 0 {
            break;
        }
        let (Some(sa), Some(sb)) = (
            engine.host_slice(cursor_a, want),
            engine.host_slice(cursor_b, want),
        ) else {
            // Fall back to a scalar unit so the pair still compares correctly.
            let mut ba = [0_u8; 2];
            let mut bb = [0_u8; 2];
            if engine.mem_read(cursor_a, &mut ba).is_err()
                || engine.mem_read(cursor_b, &mut bb).is_err()
            {
                break;
            }
            let (wa, wb) = (u16::from_le_bytes(ba), u16::from_le_bytes(bb));
            if wa != wb {
                return Ok(i32::from(wa).wrapping_sub(i32::from(wb)));
            }
            if wa == 0 {
                return Ok(0);
            }
            remaining = remaining.saturating_sub(1);
            cursor_a = cursor_a.wrapping_add(2);
            cursor_b = cursor_b.wrapping_add(2);
            tail_a = None;
            tail_b = None;
            continue;
        };
        let mut ia = sa.iter().copied();
        let mut ib = sb.iter().copied();
        if let (Some(la), Some(lb)) = (tail_a.take(), tail_b.take()) {
            let (Some(ha), Some(hb)) = (ia.next(), ib.next()) else {
                break;
            };
            let (wa, wb) = (u16::from_le_bytes([la, ha]), u16::from_le_bytes([lb, hb]));
            if wa != wb {
                return Ok(i32::from(wa).wrapping_sub(i32::from(wb)));
            }
            if wa == 0 {
                return Ok(0);
            }
            remaining = remaining.saturating_sub(1);
            cursor_a = cursor_a.wrapping_add(1);
            cursor_b = cursor_b.wrapping_add(1);
        }
        let rest_a: Vec<u8> = ia.collect();
        let rest_b: Vec<u8> = ib.collect();
        let pairs_a = rest_a.as_chunks::<2>().0;
        let pairs_b = rest_b.as_chunks::<2>().0;
        let mut consumed = 0_usize;
        for (pa, pb) in pairs_a.iter().zip(pairs_b.iter()) {
            let (wa, wb) = (u16::from_le_bytes(*pa), u16::from_le_bytes(*pb));
            if wa != wb {
                return Ok(i32::from(wa).wrapping_sub(i32::from(wb)));
            }
            if wa == 0 {
                return Ok(0);
            }
            consumed = consumed.saturating_add(2);
        }
        remaining = remaining.saturating_sub(consumed / 2);
        if consumed == 0 {
            break;
        }
        if consumed < rest_a.len() {
            tail_a = rest_a.get(consumed).copied();
        }
        if consumed < rest_b.len() {
            tail_b = rest_b.get(consumed).copied();
        }
        let advance = u64::try_from(consumed).unwrap_or(0);
        cursor_a = cursor_a.wrapping_add(advance);
        cursor_b = cursor_b.wrapping_add(advance);
    }

    Ok(0)
}

/// Bytes from `cursor` to the end of its guest page, capped by `max_units * 2`.
fn page_span(cursor: u64, max_units: usize) -> usize {
    let page_end = (cursor | (PAGE_SIZE - 1)).wrapping_add(1);
    let in_page = usize::try_from(page_end.saturating_sub(cursor)).unwrap_or(0);
    in_page.min(max_units.saturating_mul(2))
}

/// Length of a NUL-terminated guest UTF-16 string, in units, without allocating.
///
/// Page-sliced: one lock acquisition per 4 KiB page rather than one per unit.
/// The wide-string handlers used to call `mem_read(.., &[0u8; 2])` per unit,
/// so `wcslen` on a 260-character path paid 260 lock/acquisition/walk cycles.
///
/// Scans the borrowed page as little-endian pairs. An odd guest address means
/// the first unit straddles the borrow, so that one unit is read scalar and the
/// scan resumes one byte later — guest `wchar_t` is 2-aligned in practice, but
/// a handler must not fault on a misaligned pointer it was simply given.
pub(crate) fn utf16_len(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_units: usize,
) -> Result<u64> {
    if address == 0 || max_units == 0 {
        return Ok(0);
    }
    let mut cursor = address;
    let mut remaining = max_units;
    let mut total: u64 = 0;
    // A unit whose two bytes land in different borrows.
    let mut pending_low: Option<u8> = None;

    while remaining > 0 {
        // Clamp to the page: a span that runs past the end of the mapping would
        // fail to resolve even though its in-page prefix is readable.
        let page_end = (cursor | (PAGE_SIZE - 1)).wrapping_add(1);
        let in_page = usize::try_from(page_end.saturating_sub(cursor)).unwrap_or(0);
        let want = remaining
            .saturating_mul(2)
            .min(PAGE_SIZE as usize)
            .min(in_page);
        if want == 0 {
            break;
        }
        let Some(slice) = engine.host_slice(cursor, want) else {
            // Unmapped or protect-denied span: one scalar unit, then re-probe.
            let mut unit = [0_u8; 2];
            if engine.mem_read(cursor, &mut unit).is_err() {
                break;
            }
            if u16::from_le_bytes(unit) == 0 {
                return Ok(total);
            }
            total = total.saturating_add(1);
            remaining = remaining.saturating_sub(1);
            cursor = cursor.wrapping_add(2);
            pending_low = None;
            continue;
        };
        let mut iter = slice.iter().copied();
        if let Some(low) = pending_low.take() {
            let Some(high) = iter.next() else {
                break;
            };
            if u16::from_le_bytes([low, high]) == 0 {
                return Ok(total);
            }
            total = total.saturating_add(1);
            remaining = remaining.saturating_sub(1);
            cursor = cursor.wrapping_add(1);
        }
        let rest: Vec<u8> = iter.collect();
        // Whole pairs only; a trailing odd byte is carried into the next span.
        let pairs = rest.as_chunks::<2>().0;
        let mut units_in_span = 0_usize;
        let mut stopped = false;
        for pair in pairs {
            if u16::from_le_bytes(*pair) == 0 {
                stopped = true;
                break;
            }
            units_in_span = units_in_span.saturating_add(1);
        }
        let consumed_bytes = if stopped {
            units_in_span.saturating_add(1).saturating_mul(2)
        } else {
            pairs.len().saturating_mul(2)
        };
        total = total.saturating_add(u64::try_from(units_in_span).unwrap_or(0));
        if stopped {
            return Ok(total);
        }
        remaining = remaining.saturating_sub(units_in_span);
        if consumed_bytes < rest.len() {
            // Odd tail byte: the high half of the next unit.
            pending_low = rest.get(consumed_bytes).copied();
        }
        let advance = if consumed_bytes == 0 && pending_low.is_some() {
            1
        } else {
            consumed_bytes
        };
        if advance == 0 {
            break;
        }
        cursor = cursor.wrapping_add(u64::try_from(advance).unwrap_or(0));
    }

    Ok(total)
}

/// Read up to `max_units` NUL-terminated UTF-16 units from guest memory.
///
/// Uses 4 KiB page-sliced bulk reads — one lock acquisition per page instead
/// of the one-per-unit loop the KERNEL32 W-string reader used to run. Stops at
/// the first NUL unit or after `max_units` units.
fn read_utf16_units(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_units: usize,
) -> Result<Vec<u16>> {
    if address == 0 || max_units == 0 {
        return Ok(Vec::new());
    }

    // Text-rendering hot path: preallocate the common short-string size so
    // per-call Vec::new growth reallocations disappear for typical UI text.
    let mut units: Vec<u16> = Vec::with_capacity(128);
    let mut scratch = [0_u8; 4096];
    let mut remaining_units = max_units;
    let mut cursor = address;

    while remaining_units > 0 {
        // Page-aligned byte budget, capped by remaining unit count × 2.
        let byte_budget = remaining_units.saturating_mul(2).min(scratch.len() & !1); // keep even so we never split a UTF-16 unit
        if byte_budget == 0 {
            break;
        }
        let slice = scratch
            .get_mut(..byte_budget)
            .context("UTF-16 slice bounds")?;
        let got_bytes = read_page_slice(engine, cursor, slice)?;
        if got_bytes < 2 {
            break;
        }
        // Only whole units this iteration; carry over odd trailing byte next iter.
        let got_pairs = got_bytes & !1;
        let mut done = false;
        for chunk in slice
            .get(..got_pairs)
            .context("UTF-16 got_pairs bounds")?
            .as_chunks::<2>()
            .0
        {
            let lo = *chunk.first().unwrap_or(&0);
            let hi = *chunk.get(1).unwrap_or(&0);
            let unit = u16::from_le_bytes([lo, hi]);
            if unit == 0 {
                done = true;
                break;
            }
            units.push(unit);
            if units.len() >= max_units {
                done = true;
                break;
            }
        }
        if done {
            break;
        }
        cursor = cursor.wrapping_add(u64::try_from(got_pairs).unwrap_or(0));
        // `got_pairs` is always even (`got_bytes & !1`), so `>> 1` is the exact unit count.
        remaining_units = remaining_units.saturating_sub(got_pairs >> 1);
    }

    Ok(units)
}

/// UTF-16 decode mode for [`read_utf16`]: `Strict` fails on a lone surrogate,
/// `Lossy` replaces invalid units with U+FFFD.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Utf16Decode {
    Strict,
    Lossy,
}

/// Read a NUL-terminated UTF-16 string, decoding per `mode`.
///
/// `Strict` is the KERNEL32 W-string reader contract (`from_utf16`); `Lossy`
/// is the [`read_utf16_lossy`] contract (`from_utf16_lossy`). Both share the
/// page-safe bulk read loop — only the decode step differs.
pub(crate) fn read_utf16(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_units: usize,
    mode: Utf16Decode,
) -> Result<String> {
    let units = read_utf16_units(engine, address, max_units)?;
    match mode {
        Utf16Decode::Strict => {
            String::from_utf16(&units).context("wide string is not valid UTF-16")
        }
        Utf16Decode::Lossy => Ok(String::from_utf16_lossy(&units)),
    }
}

pub(crate) fn read_utf16_lossy(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_units: usize,
) -> Result<String> {
    read_utf16(engine, address, max_units, Utf16Decode::Lossy)
}

/// Encode UTF-16 units as their little-endian guest byte layout.
///
/// The single home of the per-unit `to_le_bytes` expansion shared by the
/// UTF-16 writers and the bulk string-output handlers.
pub(crate) fn utf16_units_to_le(units: &[u16]) -> Vec<u8> {
    // Capacity hint only: extend grows if the multiply saturates.
    let mut bytes = Vec::with_capacity(units.len().saturating_mul(std::mem::size_of::<u16>()));
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

pub(crate) fn write_utf16_units(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    units: &[u16],
) -> Result<()> {
    let byte_length = units
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .context("UTF-16 byte length overflow")?;

    let bytes = utf16_units_to_le(units);
    debug_assert_eq!(bytes.len(), byte_length);

    crate::guest_memory::write_bytes(engine, address, &bytes)
        .context("failed to write UTF-16 units to guest memory")
}

/// Writes a NUL-terminated ANSI string into a fixed-size guest buffer.
pub(crate) fn write_fixed_ansi(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    byte_len: usize,
    value: &[u8],
) -> Result<()> {
    let mut bytes = vec![0_u8; byte_len];
    let copy_len = value.len().min(byte_len.saturating_sub(1));

    let destination = bytes
        .get_mut(0..copy_len)
        .context("ANSI fixed string destination out of range")?;

    let source = value
        .get(0..copy_len)
        .context("ANSI fixed string source out of range")?;

    destination.copy_from_slice(source);

    crate::guest_memory::write_bytes(engine, address, &bytes)
        .context("failed to write fixed ANSI string")
}

/// Writes a NUL-terminated UTF-16 string into a fixed-size guest buffer.
pub(crate) fn write_fixed_utf16(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    unit_len: usize,
    value: &str,
) -> Result<()> {
    let byte_len = unit_len
        .checked_mul(2)
        .context("UTF-16 fixed string byte length overflow")?;

    let mut bytes = vec![0_u8; byte_len];

    for (index, unit) in value
        .encode_utf16()
        .take(unit_len.saturating_sub(1))
        .enumerate()
    {
        let offset = index
            .checked_mul(2)
            .context("UTF-16 fixed string offset overflow")?;

        let range_end = offset
            .checked_add(2)
            .context("UTF-16 fixed string range overflow")?;

        let destination = bytes
            .get_mut(offset..range_end)
            .context("UTF-16 fixed string destination out of range")?;

        destination.copy_from_slice(&unit.to_le_bytes());
    }

    crate::guest_memory::write_bytes(engine, address, &bytes)
        .context("failed to write fixed UTF-16 string")
}

/// Encode `text` as Windows-1252 bytes, one byte per char, with Windows'
/// `?` (0x3F) fallback for chars cp1252 cannot represent.
///
/// The WHATWG codec's convenience `encode()` emits a numeric character
/// reference (e.g. `&#128512;`) for unmappable chars — that is the spec's
/// encoder behavior, but it is NOT what Windows `WideCharToMultiByte(CP_ACP)`
/// writes, and the multi-byte expansion would corrupt A-buffer sizes. So the
/// fast path uses the codec only when every char is mappable (ASCII is a
/// zero-copy borrow); otherwise each unmappable char is substituted with '?'
/// first and the codec encodes the result. Output is byte-identical to the
/// Windows ACP write path.
pub(crate) fn encode_cp1252(text: &str) -> Vec<u8> {
    let (encoded, _, had_errors) = encoding_rs::WINDOWS_1252.encode(text);
    if !had_errors {
        return encoded.into_owned();
    }

    let mut scratch = [0_u8; 4];
    let mut replaced = String::with_capacity(text.len());
    for ch in text.chars() {
        let (_, _, char_had_errors) =
            encoding_rs::WINDOWS_1252.encode(ch.encode_utf8(&mut scratch));
        replaced.push(if char_had_errors { '?' } else { ch });
    }
    encoding_rs::WINDOWS_1252.encode(&replaced).0.into_owned()
}

/// Writes a NUL-terminated ANSI string and returns the number of content bytes.
///
/// The text is encoded as Windows-1252 (the ACP, one byte per char) — the
/// same bytes Windows `WideCharToMultiByte(CP_ACP)` writes, unmappable chars
/// becoming `?`. The returned count is therefore the ANSI character count,
/// exactly what Windows reports for A-API writes.
pub(crate) fn write_ansi_c_string(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_characters: usize,
    text: &str,
) -> Result<usize> {
    if address == 0 || max_characters == 0 {
        return Ok(0);
    }

    let content_capacity = max_characters.saturating_sub(1);

    let mut output = encode_cp1252(text);
    output.truncate(content_capacity);

    let copied = output.len();
    output.push(0);

    crate::guest_memory::write_bytes(engine, address, &output)
        .context("failed to write ANSI C string")?;

    Ok(copied)
}

/// Writes a NUL-terminated UTF-16 string and returns the number of content units.
pub(crate) fn write_utf16_c_string(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_characters: usize,
    text: &str,
) -> Result<usize> {
    if address == 0 || max_characters == 0 {
        return Ok(0);
    }

    let content_capacity = max_characters.saturating_sub(1);

    let mut units = text
        .encode_utf16()
        .take(content_capacity)
        .collect::<Vec<_>>();

    let copied = units.len();
    units.push(0);

    write_utf16_units(engine, address, &units).context("failed to write UTF-16 C string")?;

    Ok(copied)
}

// ── The A/W string-argument pair family ──────────────────────────────────
//
// The per-handler-pair A/W glue (read a string argument, write a string out)
// used to repeat the wide-vs-ansi branch at every handler pair. These two
// helpers carry that split ONCE; the A-path asymmetry is preserved exactly —
// the read is UTF-8-first with a cp1252 fallback, the write is cp1252 (one
// byte per char, unmappables → '?'), the W path is lossless UTF-16 both ways.

/// Default cap for a guest string-argument read: bytes for the A path, UTF-16
/// units for the W path. This is the 32 KiB window the text-setting handlers
/// (SetWindowText/SetDlgItemText) use; the page-safe readers stop at the first
/// NUL far earlier for real strings.
const STRING_ARG_MAX: usize = 32_768;

/// Read a NUL-terminated guest string argument: UTF-16 units when `wide` is
/// true, ANSI/UTF-8 bytes otherwise. A NULL pointer (or the empty string)
/// yields `""`. Capped at [`STRING_ARG_MAX`].
pub(crate) fn read_arg_string(
    engine: &mut dyn wie_cpu::CpuEngine,
    ptr: u64,
    wide: bool,
) -> Result<String> {
    if wide {
        read_utf16_lossy(engine, ptr, STRING_ARG_MAX)
    } else {
        read_ansi_lossy(engine, ptr, STRING_ARG_MAX)
    }
}

/// Write `text` into a guest c-string buffer — cp1252 bytes (unmappables →
/// `?`) for the A path, UTF-16 units for the W path — truncating to
/// `cap - 1` content characters plus the terminating NUL. Returns the content
/// count (characters for A, units for W) excluding the NUL, as `u64` — the
/// length semantics both GetWindowTextA/W and the A/W LoadString/DlgItem
/// variants report.
pub(crate) fn write_out_string(
    engine: &mut dyn wie_cpu::CpuEngine,
    ptr: u64,
    cap: u64,
    text: &str,
    wide: bool,
) -> Result<u64> {
    let capacity = usize::try_from(cap).context("guest string capacity does not fit usize")?;
    let copied = if wide {
        write_utf16_c_string(engine, ptr, capacity, text)?
    } else {
        write_ansi_c_string(engine, ptr, capacity, text)?
    };
    u64::try_from(copied).context("guest string length does not fit u64")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    const BUF: u64 = 0x3000;

    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu
    }

    fn read_guest_bytes(engine: &mut IcedCpu, addr: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0_u8; len];
        engine
            .mem_read(addr, &mut bytes)
            .expect("read guest buffer");
        bytes
    }

    /// The Windows-1252 bytes `encode_cp1252` writes for `text` (the A-path
    /// encode: one byte per char, unmappables → '?').
    fn cp1252_bytes(text: &str) -> Vec<u8> {
        encode_cp1252(text)
    }

    // --- WHATWG windows-1252 codec behavior (no engine) ---

    #[test]
    fn windows_1252_c1_range_maps_to_cp1252_glyphs() {
        // The famous ISO-8859-1 divergence: byte 0x80 is U+20AC (€), not a
        // C1 control. The WHATWG windows-1252 codec IS Windows codepage 1252.
        assert_eq!(cp1252_bytes("€"), [0x80]);
        // Latin-1 range identity: é → 0xE9.
        assert_eq!(cp1252_bytes("é"), [0xE9]);
        // Decode mirrors the table: 0x80 → €, lone 0xE9 → é.
        let (decoded, _, _) = encoding_rs::WINDOWS_1252.decode(&[0x80]);
        assert_eq!(decoded, "€");
        let (decoded, _, _) = encoding_rs::WINDOWS_1252.decode(&[0xE9]);
        assert_eq!(decoded, "é");
    }

    #[test]
    fn windows_1252_unmappable_falls_back_to_question() {
        // Windows WideCharToMultiByte(CP_ACP) substitutes '?' (0x3F) for
        // chars absent from cp1252. `encode_cp1252` normalizes the codec's
        // output to the same byte, so the guest-visible behavior matches
        // Windows. NOTE: '—' (U+2014) is NOT unmappable — cp1252 maps it to
        // 0x97, so it keeps its glyph.
        assert_eq!(cp1252_bytes("€—"), [0x80, 0x97], "em dash is mappable");
        assert_eq!(cp1252_bytes("€ā"), [0x80, 0x3F], "ā is not in cp1252");
        assert_eq!(cp1252_bytes("😀"), [0x3F], "astral chars are unmappable");
        // ASCII is byte-identical.
        assert_eq!(cp1252_bytes(r"C:\App\x.txt"), *b"C:\\App\\x.txt");
    }

    #[test]
    fn whatwg_raw_encode_emits_numeric_reference() {
        // The verification behind the '?' substitution: encoding_rs's
        // convenience `encode()` writes the WHATWG numeric character
        // reference for unmappables — NOT Windows' '?'. That multi-byte
        // expansion would corrupt A-buffer sizes, so `encode_cp1252` must
        // substitute. Pinned here so the substitution cannot be "simplified"
        // back into a bare `encode()`.
        assert_eq!(
            encoding_rs::WINDOWS_1252.encode("ā").0.as_ref(),
            b"&#257;",
            "WHATWG encoder writes &#257;, Windows writes '?'"
        );
        assert_eq!(
            encoding_rs::WINDOWS_1252.encode("😀").0.as_ref(),
            b"&#128512;"
        );
    }

    #[test]
    fn windows_1252_count_is_one_byte_per_char() {
        // Windows GetWindowTextLengthA semantics: the ANSI byte count is the
        // CP1252 char count — "café" is 4 chars, not 5 UTF-8 bytes, and an
        // unmappable char still counts as one byte ('?').
        assert_eq!(cp1252_bytes("café").len(), 4);
        assert_eq!(cp1252_bytes("caféā").len(), 5);
        assert_eq!(cp1252_bytes("€—").len(), 2);
    }

    // --- write_ansi_c_string (the A write path) ---

    #[test]
    fn write_ansi_c_string_encodes_cp1252_bytes() {
        // Regression: the write path used to emit raw UTF-8 (C3 A9 for é);
        // Windows ACP 1252 writes one byte (E9).
        let mut engine = test_engine();
        let copied =
            write_ansi_c_string(&mut engine, BUF, 16, "café").expect("write ANSI C string");
        assert_eq!(copied, 4, "4 CP1252 chars, not 5 UTF-8 bytes");
        assert_eq!(
            read_guest_bytes(&mut engine, BUF, 6),
            &[0x63, 0x61, 0x66, 0xE9, 0x00, 0x00],
            "café is one byte per CP1252 char (E9), NUL-terminated"
        );
    }

    #[test]
    fn write_ansi_c_string_c1_range_and_unmappable() {
        let mut engine = test_engine();
        // '€' → 0x80 (C1 range); 'ā' (not in cp1252) → Windows' '?' 0x3F.
        let copied = write_ansi_c_string(&mut engine, BUF, 16, "€ā").expect("write ANSI C string");
        assert_eq!(copied, 2);
        assert_eq!(read_guest_bytes(&mut engine, BUF, 3), &[0x80, 0x3F, 0x00]);
    }

    #[test]
    fn write_ansi_c_string_truncates_and_nul_terminates() {
        let mut engine = test_engine();
        // Capacity 6 → 5 content chars + NUL (ASCII stays byte-identical).
        let copied =
            write_ansi_c_string(&mut engine, BUF, 6, "Hello World").expect("write ANSI C string");
        assert_eq!(copied, 5);
        assert_eq!(read_guest_bytes(&mut engine, BUF, 6), b"Hello\0");
    }

    #[test]
    fn write_ansi_c_string_zero_capacity_writes_nothing() {
        let mut engine = test_engine();
        let copied = write_ansi_c_string(&mut engine, BUF, 0, "x").expect("write ANSI C string");
        assert_eq!(copied, 0);
        assert_eq!(
            read_guest_bytes(&mut engine, BUF, 4),
            &[0, 0, 0, 0],
            "nothing must be written"
        );
    }

    // --- read_ansi_lossy (the A read path) ---

    #[test]
    fn decode_ansi_lossy_stops_at_nul_and_decodes_utf8_then_cp1252() {
        // The shared decode behind read_ansi_lossy and the fixed-size LOGFONT
        // face-name read: bytes past the terminator must not leak into text.
        assert_eq!(decode_ansi_lossy(b"caf\xC3\xA9\0garbage"), "café");
        assert_eq!(decode_ansi_lossy(&[0x63, 0xE9, 0x00, 0xFF]), "cé");
        assert_eq!(decode_ansi_lossy(b"\x80\0"), "€");
        assert_eq!(decode_ansi_lossy(b""), "");
    }

    #[test]
    fn decode_utf16_lossy_stops_at_nul_unit() {
        let mut units = "Segoe".encode_utf16().collect::<Vec<u16>>();
        units.push(0);
        units.push(0xDEAD);
        assert_eq!(decode_utf16_lossy(&units), "Segoe");
        assert_eq!(decode_utf16_lossy(&[]), "");
    }

    #[test]
    fn read_utf16_strict_stops_at_nul_and_roundtrips() {
        // The strict reader (read_wide_string_from_cpu's path) shares the bulk
        // loop with the lossy reader and must stop at the NUL unit.
        let mut engine = test_engine();
        let mut bytes = "Hello"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<u8>>();
        bytes.extend_from_slice(&[0, 0]); // NUL unit terminator
        bytes.push(0xDE);
        bytes.push(0xAD);
        engine
            .mem_write(BUF, &bytes)
            .expect("write guest units + garbage");
        let text = read_utf16(&mut engine, BUF, 64, Utf16Decode::Strict).expect("strict read");
        assert_eq!(text, "Hello");
    }

    #[test]
    fn read_utf16_strict_rejects_lone_surrogate_that_lossy_replaces() {
        // The one behavioral divergence between read_wide_string_from_cpu
        // (Strict) and read_utf16_lossy (Lossy): a lone surrogate must fail
        // the strict read, not silently become U+FFFD.
        let mut engine = test_engine();
        engine
            .mem_write(BUF, &[0x00, 0xD8, 0x00, 0x00])
            .expect("write lone surrogate + NUL");
        let strict = read_utf16(&mut engine, BUF, 64, Utf16Decode::Strict);
        assert!(
            strict.is_err(),
            "lone surrogate must fail the strict decode"
        );
        let lossy = read_utf16(&mut engine, BUF, 64, Utf16Decode::Lossy).expect("lossy decode");
        assert_eq!(lossy, "\u{FFFD}");
    }

    #[test]
    fn read_ansi_lossy_decodes_utf8_literals_first() {
        // mingw-compiled guests store A-string literals as UTF-8; those must
        // decode to the original chars, not CP1252 mojibake.
        let mut engine = test_engine();
        let mut literal = "café".as_bytes().to_vec();
        literal.push(0);
        engine
            .mem_write(BUF, &literal)
            .expect("write guest literal");
        let text = read_ansi_lossy(&mut engine, BUF, 64).expect("read ANSI");
        assert_eq!(text, "café");
    }

    /// The reference `wcscmp`: one `mem_read` per unit per side.
    fn wcscmp_reference(engine: &mut IcedCpu, a: u64, b: u64, max_units: usize) -> i32 {
        for i in 0..max_units {
            let mut ba = [0_u8; 2];
            let mut bb = [0_u8; 2];
            let off = (i as u64) * 2;
            if engine.mem_read(a + off, &mut ba).is_err()
                || engine.mem_read(b + off, &mut bb).is_err()
            {
                return 0;
            }
            let (wa, wb) = (u16::from_le_bytes(ba), u16::from_le_bytes(bb));
            if wa != wb {
                return i32::from(wa).wrapping_sub(i32::from(wb));
            }
            if wa == 0 {
                return 0;
            }
        }
        0
    }

    #[test]
    fn utf16_cmp_matches_the_per_unit_loop() {
        let mut engine = test_engine();
        let (a, b) = (0x3000_u64, 0x3200_u64);

        // Equal strings.
        write_utf16(&mut engine, a, "alpha");
        write_utf16(&mut engine, b, "alpha");
        assert_eq!(utf16_cmp(&mut engine, a, b, 1_000_000).expect("cmp"), 0);

        // Differing at unit 2 ('p' vs 'x'): the sign is 'p' - 'x' < 0.
        write_utf16(&mut engine, b, "alxha");
        assert_eq!(
            utf16_cmp(&mut engine, a, b, 1_000_000).expect("cmp"),
            wcscmp_reference(&mut engine, a, b, 1_000_000)
        );
        assert!(utf16_cmp(&mut engine, a, b, 1_000_000).expect("cmp") < 0);

        // A prefix terminates one side: "ab" vs "abc" compares the NUL pair.
        write_utf16(&mut engine, a, "ab");
        write_utf16(&mut engine, b, "abc");
        assert_eq!(
            utf16_cmp(&mut engine, a, b, 1_000_000).expect("cmp"),
            wcscmp_reference(&mut engine, a, b, 1_000_000)
        );

        // The count stops the compare before the difference: equal prefix.
        write_utf16(&mut engine, a, "aaxx");
        write_utf16(&mut engine, b, "aayy");
        assert_eq!(utf16_cmp(&mut engine, a, b, 2).expect("cmp"), 0);

        // Cross-page: 300 identical units starting 0x100 before a page edge.
        let long = "z".repeat(300);
        write_utf16(&mut engine, 0x3F00, &long);
        write_utf16(&mut engine, 0x7000, &long);
        assert_eq!(
            utf16_cmp(&mut engine, 0x3F00, 0x7000, 1_000_000).expect("cmp"),
            0
        );
        assert_eq!(
            utf16_cmp(&mut engine, 0x3F00, 0x7000, 1_000_000).expect("cmp"),
            wcscmp_reference(&mut engine, 0x3F00, 0x7000, 1_000_000)
        );
    }

    /// The reference `wcslen`: one `mem_read` per unit, exactly what the handler
    /// did before it was page-sliced. The new scan must agree with it everywhere,
    /// including the cases the slicing can get wrong (a unit straddling a page
    /// boundary, an odd start address, a NUL on a page's last byte).
    fn wcslen_reference(engine: &mut IcedCpu, address: u64, max_units: usize) -> u64 {
        let mut i = 0_u64;
        while usize::try_from(i).unwrap_or(usize::MAX) < max_units {
            let mut b = [0_u8; 2];
            if engine.mem_read(address + i * 2, &mut b).is_err() {
                break;
            }
            if u16::from_le_bytes(b) == 0 {
                break;
            }
            i += 1;
        }
        i
    }

    fn write_utf16(engine: &mut IcedCpu, address: u64, text: &str) {
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        engine.mem_write(address, &bytes).expect("write UTF-16");
    }

    #[test]
    fn utf16_len_matches_the_per_unit_loop() {
        let mut engine = test_engine();
        // Even start, short: "hello".
        write_utf16(&mut engine, 0x3000, "hello");
        assert_eq!(
            utf16_len(&mut engine, 0x3000, 1_000_000).expect("len"),
            wcslen_reference(&mut engine, 0x3000, 1_000_000)
        );
        assert_eq!(utf16_len(&mut engine, 0x3000, 1_000_000).expect("len"), 5);

        // Straddling the 0x4000 page boundary: 200 units from 0x3F00 covers
        // 0x3F00..0x40C8, so the scan crosses one page.
        let long = "a".repeat(200);
        write_utf16(&mut engine, 0x3F00, &long);
        assert_eq!(
            utf16_len(&mut engine, 0x3F00, 1_000_000).expect("len"),
            wcslen_reference(&mut engine, 0x3F00, 1_000_000)
        );
        assert_eq!(utf16_len(&mut engine, 0x3F00, 1_000_000).expect("len"), 200);

        // NUL as a page's last unit: 0x4FFF is odd, so the *unit* at 0x4FFE is
        // what must terminate, without touching the next page.
        engine.mem_write(0x4FFE, &[b'z', 0]).expect("write tail");
        assert_eq!(utf16_len(&mut engine, 0x4FFE, 1_000_000).expect("len"), 1);
    }

    /// A misaligned pointer: the unit straddles no page, but the address is odd,
    /// so the pair cannot be read as an aligned `u16`.
    #[test]
    fn utf16_len_handles_an_odd_start_address() {
        let mut engine = test_engine();
        // Bytes: x x h 00 i 00 00 00 → reading from 0x3001 gives "hi".
        engine
            .mem_write(0x3000, &[0xFF, b'h', 0, b'i', 0, 0, 0, 0])
            .expect("write bytes");
        let len = utf16_len(&mut engine, 0x3001, 1_000_000).expect("len");
        assert_eq!(len, 2, "two units at an odd address");
    }

    #[test]
    fn utf16_len_respects_the_cap_and_an_unreadable_tail() {
        let mut engine = test_engine();
        // No NUL anywhere: the cap is the only stop.
        let run = "b".repeat(32);
        write_utf16(&mut engine, 0x3000, &run);
        // Overwrite the terminator so the string has no NUL at all.
        engine
            .mem_write(0x3000 + 64, &[0x41, 0x41])
            .expect("no NUL");
        assert_eq!(utf16_len(&mut engine, 0x3000, 10).expect("len"), 10);
        assert_eq!(utf16_len(&mut engine, 0x3000, 0).expect("len"), 0);
        assert_eq!(utf16_len(&mut engine, 0, 1_000_000).expect("len"), 0);

        // A string that walks off the mapped region (0x1000..0x101000) stops at
        // the boundary instead of faulting: 16 non-zero bytes fill the mapping
        // exactly, so the scan counts 8 units and then cannot read further.
        engine
            .mem_write(0x100FF0, &[0x41_u8; 16])
            .expect("fill to the mapping end");
        let len = utf16_len(&mut engine, 0x100FF0, 100_000).expect("len");
        assert_eq!(len, 8, "units up to the end of the mapping");
    }

    /// The page-sliced reader must return the same bytes the old byte-at-a-time
    /// loop did, including across a page boundary (where the page-slice rule
    /// splits the read) and when the NUL lands exactly on a page's last byte.
    #[test]
    fn read_ansi_bytes_spans_pages_and_stops_at_nul() {
        let mut engine = test_engine();
        // 0x3F00 is 0x100 bytes before the page boundary at 0x4000.
        let start = 0x3F00_u64;
        let mut payload = vec![b'a'; 0x180]; // 0x100 to the boundary, then 0x80 more
        payload.push(0);
        engine.mem_write(start, &payload).expect("write payload");
        let bytes = read_ansi_bytes(&mut engine, start, 4096).expect("read across pages");
        assert_eq!(bytes.len(), 0x180);
        assert!(bytes.iter().all(|&b| b == b'a'));

        // NUL as the very last byte of a page: the read must not touch the next.
        let last = 0x4FFF_u64;
        engine.mem_write(last, &[b'z', 0]).expect("write tail");
        let bytes = read_ansi_bytes(&mut engine, last, 4096).expect("read page tail");
        assert_eq!(bytes, b"z");
    }

    /// `max_bytes` caps an unterminated string without reading past it.
    #[test]
    fn read_ansi_bytes_respects_the_cap() {
        let mut engine = test_engine();
        engine.mem_write(BUF, &[b'q'; 64]).expect("write run");
        let bytes = read_ansi_bytes(&mut engine, BUF, 10).expect("read capped");
        assert_eq!(bytes.len(), 10);

        // Zero cap and null pointer both yield nothing, without a lock.
        assert!(
            read_ansi_bytes(&mut engine, BUF, 0)
                .expect("zero cap")
                .is_empty()
        );
        assert!(
            read_ansi_bytes(&mut engine, 0, 64)
                .expect("null ptr")
                .is_empty()
        );
    }

    /// A string that runs into unmapped memory: the strict reader reports the
    /// fault, the best-effort one returns what it read. Console output uses the
    /// best-effort shape so a truncated line still prints.
    #[test]
    fn best_effort_stops_at_an_unreadable_page() {
        let mut engine = test_engine();
        // Mapped range is 0x1000..0x101000; put 8 bytes ending exactly at the end.
        let start = 0x100FF8_u64;
        engine
            .mem_write(start, &[b'r'; 8])
            .expect("write at mapping end");
        let lenient = read_ansi_bytes_best_effort(&mut engine, start, 4096)
            .expect("best effort must not fail");
        assert_eq!(lenient, b"rrrrrrrr");
        assert!(
            read_ansi_bytes(&mut engine, start, 4096).is_err(),
            "the strict reader still reports the unmapped page"
        );
    }

    #[test]
    fn read_ansi_lossy_falls_back_to_cp1252() {
        // Bytes that are not valid UTF-8 (real Windows binaries pass ACP
        // strings) decode via windows-1252, C1 range included.
        let mut engine = test_engine();
        engine
            .mem_write(BUF, &[0x63, 0x61, 0x66, 0xE9, 0x00])
            .expect("write CP1252 bytes");
        let text = read_ansi_lossy(&mut engine, BUF, 64).expect("read ANSI");
        assert_eq!(text, "café");

        engine
            .mem_write(BUF, &[0x80, 0x00])
            .expect("write CP1252 C1 byte");
        let text = read_ansi_lossy(&mut engine, BUF, 64).expect("read ANSI");
        assert_eq!(text, "€");
    }

    #[test]
    fn read_ansi_lossy_stops_at_nul_and_ascii_roundtrips() {
        let mut engine = test_engine();
        engine
            .mem_write(BUF, b"Hello\0World")
            .expect("write guest bytes");
        let text = read_ansi_lossy(&mut engine, BUF, 64).expect("read ANSI");
        assert_eq!(text, "Hello");
    }

    // --- read_arg_string / write_out_string (the A/W arg pair family) ---

    /// The guest UTF-16LE bytes (NUL-terminated) for `text`.
    fn utf16_c_string_bytes(text: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes
    }

    #[test]
    fn read_arg_string_wide_round_trips_utf16() {
        // The demo W-path pin ("dialog text — ✓" echoes losslessly through
        // SetDlgItemTextW/GetDlgItemTextW): the family's wide read must
        // decode the guest UTF-16 units back to the identical text — em dash
        // and U+2713 included (the A path would degrade ✓ to '?').
        let mut engine = test_engine();
        engine
            .mem_write(BUF, &utf16_c_string_bytes("dialog text — ✓"))
            .expect("write guest units");
        let text = read_arg_string(&mut engine, BUF, true).expect("wide arg read");
        assert_eq!(text, "dialog text — ✓");
    }

    #[test]
    fn read_arg_string_ansi_is_utf8_first_then_cp1252() {
        // The A-path read asymmetry: a mingw UTF-8 literal decodes to the
        // original chars, an ACP 1252 byte string falls back to cp1252.
        let mut engine = test_engine();
        engine
            .mem_write(BUF, b"caf\xC3\xA9\0")
            .expect("write UTF-8 literal");
        let text = read_arg_string(&mut engine, BUF, false).expect("ANSI arg read");
        assert_eq!(text, "café");

        engine
            .mem_write(BUF, &[0x63, 0xE9, 0x00])
            .expect("write CP1252 bytes");
        let text = read_arg_string(&mut engine, BUF, false).expect("ANSI arg read");
        assert_eq!(text, "cé");
    }

    #[test]
    fn read_arg_string_null_ptr_and_nul_stop() {
        let mut engine = test_engine();
        assert_eq!(
            read_arg_string(&mut engine, 0, false).expect("NULL ANSI arg"),
            ""
        );
        assert_eq!(
            read_arg_string(&mut engine, 0, true).expect("NULL wide arg"),
            ""
        );
        engine
            .mem_write(BUF, b"Hello\0World")
            .expect("write guest bytes");
        assert_eq!(
            read_arg_string(&mut engine, BUF, false).expect("NUL-stopped ANSI arg"),
            "Hello"
        );
    }

    #[test]
    fn write_out_string_wide_round_trips_utf16_and_counts_units() {
        let mut engine = test_engine();
        let copied =
            write_out_string(&mut engine, BUF, 16, "dialog text — ✓", true).expect("wide write");
        // 15 BMP units (the em dash and U+2713 are single units), NUL excluded.
        assert_eq!(copied, 15, "UTF-16 unit count excluding the NUL");
        let text = read_utf16_lossy(&mut engine, BUF, 64).expect("read back wide");
        assert_eq!(text, "dialog text — ✓", "W-path round-trip is lossless");
    }

    #[test]
    fn write_out_string_ansi_encodes_cp1252_and_counts() {
        let mut engine = test_engine();
        let copied = write_out_string(&mut engine, BUF, 16, "café", false).expect("ANSI write");
        assert_eq!(copied, 4, "4 CP1252 chars, not 5 UTF-8 bytes");
        assert_eq!(
            read_guest_bytes(&mut engine, BUF, 5),
            &[0x63, 0x61, 0x66, 0xE9, 0x00],
            "é writes one byte (E9), NUL-terminated"
        );
    }

    #[test]
    fn write_out_string_ansi_unmappable_falls_back_to_question() {
        let mut engine = test_engine();
        let copied = write_out_string(&mut engine, BUF, 16, "€ā", false).expect("ANSI write");
        assert_eq!(copied, 2);
        assert_eq!(
            read_guest_bytes(&mut engine, BUF, 3),
            &[0x80, 0x3F, 0x00],
            "€ → 0x80 (C1), ā → Windows' '?'"
        );
    }

    #[test]
    fn write_out_string_truncates_and_nul_terminates() {
        let mut engine = test_engine();
        // Capacity 6 → 5 content chars + NUL (ASCII stays byte-identical).
        let copied =
            write_out_string(&mut engine, BUF, 6, "Hello World", false).expect("ANSI write");
        assert_eq!(copied, 5);
        assert_eq!(read_guest_bytes(&mut engine, BUF, 6), b"Hello\0");

        // Wide truncation counts UTF-16 units: cap 5 → 4 units + NUL.
        let copied =
            write_out_string(&mut engine, BUF, 5, "Hello World", true).expect("wide write");
        assert_eq!(copied, 4);
        assert_eq!(
            read_utf16_lossy(&mut engine, BUF, 64).expect("read back"),
            "Hell"
        );
    }

    #[test]
    fn write_out_string_zero_cap_and_null_ptr_write_nothing() {
        let mut engine = test_engine();
        assert_eq!(
            write_out_string(&mut engine, BUF, 0, "x", false).expect("zero-cap ANSI write"),
            0
        );
        assert_eq!(
            write_out_string(&mut engine, BUF, 0, "x", true).expect("zero-cap wide write"),
            0
        );
        assert_eq!(
            write_out_string(&mut engine, 0, 16, "x", false).expect("NULL-ptr write"),
            0
        );
        assert_eq!(
            read_guest_bytes(&mut engine, BUF, 4),
            &[0, 0, 0, 0],
            "nothing must be written"
        );
    }

    #[test]
    fn read_a_then_write_a_preserves_the_cp1252_contract() {
        // The full A-path asymmetry through the family: a mingw UTF-8 literal
        // reads back as the original chars, then the write side re-encodes
        // them as the cp1252 bytes Windows writes (é → one byte E9).
        let mut engine = test_engine();
        engine
            .mem_write(BUF, b"caf\xC3\xA9\0")
            .expect("write UTF-8 literal");
        let text = read_arg_string(&mut engine, BUF, false).expect("read A");
        assert_eq!(text, "café");

        let copied = write_out_string(&mut engine, BUF + 0x100, 8, &text, false).expect("write A");
        assert_eq!(copied, 4);
        assert_eq!(
            read_guest_bytes(&mut engine, BUF + 0x100, 5),
            &[0x63, 0x61, 0x66, 0xE9, 0x00],
            "UTF-8-first read, cp1252 write"
        );
    }
}
