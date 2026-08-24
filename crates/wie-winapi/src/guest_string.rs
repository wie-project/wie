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
/// Reads guest memory in 4 KiB slices (one lock acquisition per page) and scans
/// for NUL on the host side — orders of magnitude cheaper than the previous
/// byte-per-lock loop on long paths.
pub(crate) fn read_ansi_bytes(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    if address == 0 || max_bytes == 0 {
        return Ok(Vec::new());
    }

    // Preallocate the common short-string size; long paths grow geometrically.
    let mut bytes: Vec<u8> = Vec::with_capacity(128);
    let mut scratch = [0_u8; 4096];
    let mut remaining = max_bytes;
    let mut cursor = address;

    while remaining > 0 {
        let want = remaining.min(scratch.len());
        let slice = scratch.get_mut(..want).context("ANSI slice bounds")?;
        let got = read_page_slice(engine, cursor, slice)?;
        if got == 0 {
            break;
        }
        let got_slice = slice.get(..got).context("ANSI got_slice bounds")?;
        if let Some(nul) = got_slice.iter().position(|&b| b == 0) {
            let head = got_slice.get(..nul).context("ANSI head bounds")?;
            bytes.extend_from_slice(head);
            return Ok(bytes);
        }
        bytes.extend_from_slice(got_slice);
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
