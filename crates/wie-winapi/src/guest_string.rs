use anyhow::{Context, Result};

/// 4 KiB — the guest page size. Bulk reads probe one page at a time so a single
/// `host_span` gives up to 4096 bytes of scan for one memory-lock acquisition.
const PAGE_SIZE: u64 = 4096;

/// Read up to `len` guest bytes starting at `addr` into `buf`, staying inside the
/// current 4 KiB page. Returns the number of bytes actually copied (≤ `len`).
///
/// Uses [`CpuEngine::host_span`] to acquire the guest memory lock once and copy
/// directly, avoiding a per-byte lock / page-walk round-trip. Falls back to
/// scalar `mem_read` when the span is unavailable (unmapped, protect denied,
/// executable page on write, generation raced).
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

    if let Some(host) = engine.host_span(addr, take, false) {
        // SAFETY: host_span validated `[addr, addr+take)` maps into one arena
        // with read permission; the pointer is valid until the next mutation of
        // guest memory (we perform no such mutation before the copy).
        #[allow(unsafe_code)]
        unsafe {
            std::ptr::copy_nonoverlapping(host, dst.as_mut_ptr(), take);
        }
        return Ok(take);
    }

    engine
        .mem_read(addr, dst)
        .context("failed bulk read in page-slice fallback")?;
    Ok(take)
}

pub(crate) fn read_ansi_lossy(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_bytes: usize,
) -> Result<String> {
    let bytes = read_ansi_bytes(engine, address, max_bytes)?;
    // Prefer ACP-1252 for path/ANSI APIs (GetACP); UTF-8 lossy was too aggressive.
    Ok(crate::vfs::decode_acp(&bytes))
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

    let mut bytes: Vec<u8> = Vec::new();
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

pub(crate) fn read_utf16_lossy(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_units: usize,
) -> Result<String> {
    if address == 0 || max_units == 0 {
        return Ok(String::new());
    }

    let mut units: Vec<u16> = Vec::new();
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
            .chunks_exact(2)
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

    Ok(String::from_utf16_lossy(&units))
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

    let mut bytes = Vec::with_capacity(byte_length);

    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }

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

/// Writes a NUL-terminated ANSI string and returns the number of content bytes.
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

    let mut output = text
        .as_bytes()
        .iter()
        .copied()
        .take(content_capacity)
        .collect::<Vec<_>>();

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
