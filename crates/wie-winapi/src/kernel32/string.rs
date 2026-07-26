use super::{
    ANSI_CODE_PAGE, C1_ALPHA, C1_BLANK, C1_CNTRL, C1_DIGIT, C1_LOWER, C1_PUNCT, C1_SPACE, C1_UPPER,
    C1_XDIGIT, CT_CTYPE1, Context, OEM_CODE_PAGE, Result, WinApiHandlerResult, checked_address,
    checked_field_address, low_u32_to_i32, read_guest_u16, read_guest_u64, write_guest_u16,
    write_guest_u32,
};

/// Handles `KERNEL32.dll!lstrlenW`.
pub fn handle_lstrlen_w(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let s = engine
        .read_rcx()
        .context("failed to read RCX for lstrlenW")?;
    let return_value = if s == 0 {
        0_u64
    } else {
        let mut len = 0_u64;
        loop {
            let mut buf = [0_u8; 2];
            engine.mem_read(s.wrapping_add(len.saturating_mul(2)), &mut buf)?;
            if u16::from_le_bytes(buf) == 0 {
                break;
            }
            len = len.saturating_add(1);
            if len > 1_000_000 {
                break;
            }
        }
        len
    };
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from lstrlenW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!lstrcpyW` — copy wide string; returns dest.
pub fn handle_lstrcpy_w(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let dest = engine
        .read_rcx()
        .context("failed to read RCX for lstrcpyW")?;
    let src = engine
        .read_rdx()
        .context("failed to read RDX for lstrcpyW")?;
    if dest != 0 && src != 0 {
        let mut offset = 0_u64;
        loop {
            let mut buf = [0_u8; 2];
            engine.mem_read(src.wrapping_add(offset), &mut buf)?;
            engine.mem_write(dest.wrapping_add(offset), &buf)?;
            if u16::from_le_bytes(buf) == 0 {
                break;
            }
            offset = offset.saturating_add(2);
            if offset > 2_000_000 {
                break;
            }
        }
    }
    let return_address = engine
        .return_from_win64_api(dest)
        .context("failed to return from lstrcpyW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: dest,
    })
}
/// Handles `KERNEL32.dll!lstrcatW` — append wide string; returns dest.
pub fn handle_lstrcat_w(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let dest = engine
        .read_rcx()
        .context("failed to read RCX for lstrcatW")?;
    let src = engine
        .read_rdx()
        .context("failed to read RDX for lstrcatW")?;
    if dest != 0 && src != 0 {
        // Find end of dest.
        let mut dest_end = 0_u64;
        loop {
            let mut buf = [0_u8; 2];
            engine.mem_read(dest.wrapping_add(dest_end), &mut buf)?;
            if u16::from_le_bytes(buf) == 0 {
                break;
            }
            dest_end = dest_end.saturating_add(2);
            if dest_end > 2_000_000 {
                break;
            }
        }
        let mut offset = 0_u64;
        loop {
            let mut buf = [0_u8; 2];
            engine.mem_read(src.wrapping_add(offset), &mut buf)?;
            engine.mem_write(dest.wrapping_add(dest_end).wrapping_add(offset), &buf)?;
            if u16::from_le_bytes(buf) == 0 {
                break;
            }
            offset = offset.saturating_add(2);
            if offset > 2_000_000 {
                break;
            }
        }
    }
    let return_address = engine
        .return_from_win64_api(dest)
        .context("failed to return from lstrcatW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: dest,
    })
}
pub(crate) fn read_utf16_units(
    engine: &mut dyn wie_cpu::CpuEngine,
    wide_ptr: u64,
    wide_len_raw: u64,
) -> Result<Vec<u16>> {
    if wide_ptr == 0 {
        return Ok(Vec::new());
    }

    let wide_len_i32 = low_u32_to_i32(wide_len_raw, "WideCharToMultiByte cchWideChar")?;

    if wide_len_i32 == -1 {
        read_null_terminated_utf16_units(engine, wide_ptr)
    } else {
        let wide_len =
            usize::try_from(wide_len_i32).context("negative UTF-16 length is not supported")?;

        read_fixed_utf16_units(engine, wide_ptr, wide_len)
    }
}
pub(crate) fn read_null_terminated_utf16_units(
    engine: &mut dyn wie_cpu::CpuEngine,
    wide_ptr: u64,
) -> Result<Vec<u16>> {
    const MAX_UNITS: usize = 32_768;

    let mut units = Vec::new();

    for index in 0..MAX_UNITS {
        let offset = u64::try_from(index)
            .context("UTF-16 index does not fit u64")?
            .checked_mul(2)
            .context("UTF-16 offset overflow")?;

        let address = checked_address(wide_ptr, offset, "UTF-16 NUL scan")?;
        let unit = read_guest_u16(engine, address)?;

        units.push(unit);

        if unit == 0 {
            return Ok(units);
        }
    }

    anyhow::bail!("unterminated UTF-16 string")
}
pub(crate) fn read_fixed_utf16_units(
    engine: &mut dyn wie_cpu::CpuEngine,
    wide_ptr: u64,
    wide_len: usize,
) -> Result<Vec<u16>> {
    let mut units = Vec::with_capacity(wide_len);

    for index in 0..wide_len {
        let offset = u64::try_from(index)
            .context("UTF-16 index does not fit u64")?
            .checked_mul(2)
            .context("UTF-16 offset overflow")?;

        let address = checked_address(wide_ptr, offset, "fixed UTF-16 read")?;
        units.push(read_guest_u16(engine, address)?);
    }

    Ok(units)
}
pub(crate) fn classify_ctype1(unit: u16) -> u16 {
    let Some(ch) = char::from_u32(u32::from(unit)) else {
        return 0;
    };

    if ch == '\0' {
        return C1_CNTRL;
    }

    let mut flags = 0_u16;

    if ch.is_uppercase() {
        flags |= C1_UPPER | C1_ALPHA;
    }

    if ch.is_lowercase() {
        flags |= C1_LOWER | C1_ALPHA;
    }

    if ch.is_alphabetic() && (flags & C1_ALPHA) == 0 {
        flags |= C1_ALPHA;
    }

    if ch.is_ascii_digit() {
        flags |= C1_DIGIT;
    }

    if ch.is_ascii_hexdigit() {
        flags |= C1_XDIGIT;
    }

    if ch.is_whitespace() {
        flags |= C1_SPACE;
    }

    if ch == ' ' || ch == '\t' {
        flags |= C1_BLANK;
    }

    if ch.is_control() {
        flags |= C1_CNTRL;
    }

    if ch.is_ascii_punctuation() {
        flags |= C1_PUNCT;
    }

    flags
}
pub(crate) fn read_multibyte_bytes(
    engine: &mut dyn wie_cpu::CpuEngine,
    input_ptr: u64,
    input_len_raw: u64,
) -> Result<Vec<u8>> {
    if input_ptr == 0 {
        return Ok(Vec::new());
    }

    let input_len_i32 = low_u32_to_i32(input_len_raw, "MultiByteToWideChar cbMultiByte")?;

    if input_len_i32 == -1 {
        read_null_terminated_bytes(engine, input_ptr)
    } else {
        let input_len =
            usize::try_from(input_len_i32).context("negative multibyte length is not supported")?;

        read_fixed_bytes(engine, input_ptr, input_len)
    }
}
pub(crate) fn read_null_terminated_bytes(
    engine: &mut dyn wie_cpu::CpuEngine,
    input_ptr: u64,
) -> Result<Vec<u8>> {
    const MAX_BYTES: usize = 32_768;

    let mut bytes = Vec::new();

    for index in 0..MAX_BYTES {
        let offset = u64::try_from(index).context("byte index does not fit u64")?;
        let address = checked_address(input_ptr, offset, "multibyte NUL scan")?;

        let mut byte = [0_u8; 1];
        engine
            .mem_read(address, &mut byte)
            .context("failed to read multibyte byte")?;

        bytes.push(byte[0]);

        if byte[0] == 0 {
            return Ok(bytes);
        }
    }

    anyhow::bail!("unterminated multibyte string")
}
pub(crate) fn read_fixed_bytes(
    engine: &mut dyn wie_cpu::CpuEngine,
    input_ptr: u64,
    input_len: usize,
) -> Result<Vec<u8>> {
    let mut bytes = vec![0_u8; input_len];

    engine
        .mem_read(input_ptr, &mut bytes)
        .context("failed to read fixed multibyte bytes")?;

    Ok(bytes)
}
/// Handles `KERNEL32.dll!WideCharToMultiByte`.
pub fn handle_wide_char_to_multi_byte(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let code_page = engine
        .read_rcx()
        .context("failed to read RCX for WideCharToMultiByte")?;

    let _flags = engine
        .read_rdx()
        .context("failed to read RDX for WideCharToMultiByte")?;

    let wide_ptr = engine
        .read_r8()
        .context("failed to read R8 for WideCharToMultiByte")?;

    let wide_len_raw = engine
        .read_r9()
        .context("failed to read R9 for WideCharToMultiByte")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for WideCharToMultiByte")?;

    let out_ptr_address = checked_address(rsp, 0x28, "WideCharToMultiByte lpMultiByteStr")?;
    let out_len_address = checked_address(rsp, 0x30, "WideCharToMultiByte cbMultiByte")?;

    let out_ptr = read_guest_u64(engine, out_ptr_address)?;
    let out_len = read_guest_u64(engine, out_len_address)?;

    let units = read_utf16_units(engine, wide_ptr, wide_len_raw)?;
    let cp = u32::try_from(code_page & 0xffff_ffff).unwrap_or(crate::vfs::CP_ACP);
    let bytes = crate::vfs::wide_to_multibyte(cp, &units)
        .ok_or_else(|| anyhow::anyhow!("WideCharToMultiByte invalid UTF-16"))?;

    let required_size =
        u64::try_from(bytes.len()).context("WideCharToMultiByte result length does not fit u64")?;

    let return_value = if out_ptr == 0 || out_len == 0 {
        required_size
    } else {
        let out_len_usize = usize::try_from(out_len)
            .context("WideCharToMultiByte output size does not fit usize")?;

        if out_len_usize < bytes.len() {
            0
        } else {
            engine
                .mem_write(out_ptr, &bytes)
                .context("failed to write WideCharToMultiByte output")?;

            required_size
        }
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from WideCharToMultiByte")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetACP`.
pub fn handle_get_acp(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(ANSI_CODE_PAGE)
        .context("failed to return from GetACP")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: ANSI_CODE_PAGE,
    })
}
/// Handles `KERNEL32.dll!GetOEMCP`.
pub fn handle_get_oem_cp(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(OEM_CODE_PAGE)
        .context("failed to return from GetOEMCP")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: OEM_CODE_PAGE,
    })
}
/// Handles `KERNEL32.dll!GetCPInfo`.
pub fn handle_get_cp_info(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _code_page = engine
        .read_rcx()
        .context("failed to read RCX for GetCPInfo")?;

    let cp_info_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetCPInfo")?;

    if cp_info_ptr != 0 {
        let max_char_size_address = checked_field_address(cp_info_ptr, 0, "MaxCharSize")?;
        let default_char_address = checked_field_address(cp_info_ptr, 4, "DefaultChar")?;
        let lead_byte_address = checked_field_address(cp_info_ptr, 6, "LeadByte")?;

        write_guest_u32(engine, max_char_size_address, 1)?;
        engine
            .mem_write(default_char_address, &[b'?', 0])
            .context("failed to write CPINFO DefaultChar")?;
        engine
            .mem_write(lead_byte_address, &[0_u8; 12])
            .context("failed to write CPINFO LeadByte")?;
    }

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from GetCPInfo")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!IsValidCodePage`.
pub fn handle_is_valid_code_page(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let code_page = engine
        .read_rcx()
        .context("failed to read RCX for IsValidCodePage")?;

    let return_value = match code_page {
        0 | 437 | 1252 | 1200 | 65001 => 1,
        _ => 0,
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from IsValidCodePage")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetStringTypeW`.
pub fn handle_get_string_type_w(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let info_type = engine
        .read_rcx()
        .context("failed to read RCX for GetStringTypeW")?;

    let source_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetStringTypeW")?;

    let source_len_raw = engine
        .read_r8()
        .context("failed to read R8 for GetStringTypeW")?;

    let char_type_ptr = engine
        .read_r9()
        .context("failed to read R9 for GetStringTypeW")?;

    let return_value = if source_ptr == 0 || char_type_ptr == 0 {
        0
    } else {
        let units = read_utf16_units(engine, source_ptr, source_len_raw)?;

        for (index, unit) in units.iter().enumerate() {
            let index_u64 =
                u64::try_from(index).context("GetStringTypeW index does not fit u64")?;
            let offset = index_u64
                .checked_mul(2)
                .context("GetStringTypeW output offset overflow")?;
            let output_address = checked_address(char_type_ptr, offset, "GetStringTypeW output")?;

            let flags = if info_type == CT_CTYPE1 {
                classify_ctype1(*unit)
            } else {
                0
            };

            write_guest_u16(engine, output_address, flags)?;
        }

        1
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetStringTypeW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!MultiByteToWideChar`.
pub fn handle_multi_byte_to_wide_char(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let code_page = engine.read_rcx()?;
    let _flags = engine.read_rdx()?;
    let input_ptr = engine.read_r8()?;
    let input_len_raw = engine.read_r9()?;

    let rsp = engine.read_rsp()?;
    let output_ptr = read_guest_u64(
        engine,
        checked_address(rsp, 0x28, "MultiByteToWideChar lpWideCharStr")?,
    )?;
    let output_len = read_guest_u64(
        engine,
        checked_address(rsp, 0x30, "MultiByteToWideChar cchWideChar")?,
    )?;

    let input_bytes = read_multibyte_bytes(engine, input_ptr, input_len_raw)?;
    let cp = u32::try_from(code_page & 0xffff_ffff).unwrap_or(0);
    let units = crate::vfs::multibyte_to_wide(cp, &input_bytes);

    let required_units =
        u64::try_from(units.len()).context("MultiByteToWideChar unit length does not fit u64")?;

    let return_value = if output_ptr == 0 || output_len == 0 {
        required_units
    } else {
        let output_len_usize = usize::try_from(output_len)
            .context("MultiByteToWideChar output size does not fit usize")?;
        if output_len_usize < units.len() {
            0
        } else {
            // Bulk LE write without per-unit extend_from_slice.
            let mut output_bytes = vec![0_u8; units.len().saturating_mul(2)];
            for (i, unit) in units.iter().enumerate() {
                let o = i.saturating_mul(2);
                let end = o.saturating_add(2);
                if let Some(dst) = output_bytes.get_mut(o..end) {
                    dst.copy_from_slice(&unit.to_le_bytes());
                }
            }
            engine
                .mem_write(output_ptr, &output_bytes)
                .context("failed to write MultiByteToWideChar output")?;
            required_units
        }
    };

    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!LCMapStringW`.
pub fn handle_lc_map_string_w(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _locale = engine
        .read_rcx()
        .context("failed to read RCX for LCMapStringW")?;

    let _map_flags = engine
        .read_rdx()
        .context("failed to read RDX for LCMapStringW")?;

    let source_ptr = engine
        .read_r8()
        .context("failed to read R8 for LCMapStringW")?;

    let source_len_raw = engine
        .read_r9()
        .context("failed to read R9 for LCMapStringW")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for LCMapStringW")?;

    let dest_ptr_address = checked_address(rsp, 0x28, "LCMapStringW lpDestStr")?;
    let dest_len_address = checked_address(rsp, 0x30, "LCMapStringW cchDest")?;

    let dest_ptr = read_guest_u64(engine, dest_ptr_address)?;
    let dest_len = read_guest_u64(engine, dest_len_address)?;

    let source_units = read_utf16_units(engine, source_ptr, source_len_raw)?;
    let required_units =
        u64::try_from(source_units.len()).context("LCMapStringW result length does not fit u64")?;

    let return_value = if dest_ptr == 0 || dest_len == 0 {
        required_units
    } else {
        let dest_len_usize =
            usize::try_from(dest_len).context("LCMapStringW output size does not fit usize")?;

        if dest_len_usize < source_units.len() {
            0
        } else {
            let output_byte_len = source_units
                .len()
                .checked_mul(2)
                .context("LCMapStringW output byte length overflow")?;

            let mut output_bytes = Vec::with_capacity(output_byte_len);

            for unit in &source_units {
                output_bytes.extend_from_slice(&unit.to_le_bytes());
            }

            engine
                .mem_write(dest_ptr, &output_bytes)
                .context("failed to write LCMapStringW output")?;

            required_units
        }
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from LCMapStringW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
