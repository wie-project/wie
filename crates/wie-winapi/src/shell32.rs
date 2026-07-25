//! Minimal `shell32.dll` stubs (folder paths / browse UI) for CLI tools.

use crate::guest_memory::write_u32 as write_guest_u32;
use crate::guest_string::write_utf16_c_string;
use crate::{WinApiHandlerResult, WinApiState};
use anyhow::{Context, Result};

/// `S_OK` / success for SH* path APIs that return HRESULT.
const S_OK: u64 = 0;
/// `E_FAIL` for optional shell UI we do not implement.
const E_FAIL: u64 = 0x8000_4005;

fn ret(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("shell32 return")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// Soft dispatch for `shell32.dll`.
pub fn dispatch_shell32(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "shgetfolderpathw" => Ok(Some(handle_sh_get_folder_path_w(engine)?)),
        "shgetpathfromidlistw" => Ok(Some(handle_sh_get_path_from_id_list_w(engine)?)),
        "shbrowseforfolderw" => Ok(Some(handle_sh_browse_for_folder_w(engine)?)),
        "commandlinetoargvw" => Ok(Some(handle_command_line_to_argv_w(engine, state)?)),
        _ => Ok(None),
    }
}

/// `HRESULT SHGetFolderPathW(hwnd, csidl, hToken, dwFlags, pszPath)`
///
/// Fills a fixed bottle-friendly path under `C:\Users\WIE\…` style so tools
/// that only need a writable home directory keep going.
fn handle_sh_get_folder_path_w(engine: &mut dyn wie_cpu::CpuEngine) -> Result<WinApiHandlerResult> {
    let _hwnd = engine.read_rcx()?;
    let csidl = engine.read_rdx()? & 0xffff_ffff;
    let _token = engine.read_r8()?;
    let _flags = engine.read_r9()?;
    let mut path_ptr_bytes = [0_u8; 8];
    let rsp = engine.read_rsp()?;
    engine.mem_read(rsp.wrapping_add(0x28), &mut path_ptr_bytes)?;
    let path_ptr = u64::from_le_bytes(path_ptr_bytes);

    // Common CSIDL values → synthetic bottle paths (MAX_PATH buffer expected).
    // `csidl` already masked to low 32 bits (u64).
    let path = match csidl {
        0x00 => r"C:\Users\WIE\Desktop",          // CSIDL_DESKTOP
        0x05 => r"C:\Users\WIE\Documents",        // CSIDL_PERSONAL / My Documents
        0x1a => r"C:\Users\WIE\AppData\Roaming",  // CSIDL_APPDATA
        0x1c => r"C:\Users\WIE\AppData\Local",    // CSIDL_LOCAL_APPDATA
        0x23 => r"C:\ProgramData",                // CSIDL_COMMON_APPDATA
        0x24 => r"C:\Windows",                    // CSIDL_WINDOWS
        0x25 => r"C:\Windows\System32",           // CSIDL_SYSTEM
        0x26 => r"C:\Program Files",              // CSIDL_PROGRAM_FILES
        0x2a => r"C:\Program Files\Common Files", // CSIDL_PROGRAM_FILES_COMMON
        // CSIDL_PROFILE (0x28) and unknown → user home.
        _ => r"C:\Users\WIE",
    };

    if path_ptr != 0 {
        write_utf16_c_string(engine, path_ptr, 260, path)?;
    }
    ret(engine, S_OK)
}

/// `LPWSTR* CommandLineToArgvW(LPCWSTR lpCmdLine, int* pNumArgs)`.
///
/// Parses a command-line string into an argv-style array, allocating the result
/// and the argument strings from the process heap.
fn handle_command_line_to_argv_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let cmd_line_ptr = engine.read_rcx()?;
    let num_args_ptr = engine.read_rdx()?;
    if cmd_line_ptr == 0 || num_args_ptr == 0 {
        return ret(engine, 0); // NULL → failure
    }
    // Read the command line.
    let mut units = Vec::new();
    let mut i = 0_u64;
    loop {
        let mut b = [0_u8; 2];
        engine.mem_read(cmd_line_ptr.wrapping_add(i.wrapping_mul(2)), &mut b)?;
        let w = u16::from_le_bytes(b);
        if w == 0 {
            break;
        }
        units.push(w);
        i = i.saturating_add(1);
        if i > 8192 {
            break;
        }
    }
    // Parse into arguments (simple whitespace splitting).
    let cmd = String::from_utf16_lossy(&units);
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in cmd.chars() {
        if ch == '"' {
            in_quote = !in_quote;
        } else if ch.is_whitespace() && !in_quote {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    // Allocate argv array: one pointer per arg, plus NULL terminator.
    let argv_bytes = args.len().saturating_add(1).checked_mul(8).unwrap_or(8);
    let argv_va = state
        .heap_state
        .heap
        .alloc_coherent(engine, u64::try_from(argv_bytes).unwrap_or(64));
    if argv_va == 0 {
        return ret(engine, 0);
    }
    let mut offset = 0_u64;
    for arg in &args {
        let units: Vec<u16> = arg.encode_utf16().collect();
        let bstr = alloc_shell_bstr(engine, state, &units)?;
        if bstr == 0 {
            return ret(engine, 0);
        }
        engine.mem_write(argv_va.wrapping_add(offset), &bstr.to_le_bytes())?;
        offset = offset.saturating_add(8);
    }
    // NULL terminator.
    engine.mem_write(argv_va.wrapping_add(offset), &[0_u8; 8])?;
    // Write argc.
    let argc_u32 = u32::try_from(args.len()).unwrap_or(0);
    drop(write_guest_u32(engine, num_args_ptr, argc_u32));
    ret(engine, argv_va)
}

/// Allocate a shell-style BSTR from the process heap.
fn alloc_shell_bstr(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    units: &[u16],
) -> Result<u64> {
    let byte_len = u32::try_from(units.len().saturating_mul(2)).unwrap_or(0);
    let total = 4_u64
        .saturating_add(u64::from(byte_len))
        .saturating_add(2)
        .saturating_add(8);
    let raw = state.heap_state.heap.alloc_coherent(engine, total);
    if raw == 0 {
        return Ok(0);
    }
    let data = raw.wrapping_add(4);
    engine.mem_write(data.wrapping_sub(4), &byte_len.to_le_bytes())?;
    let mut bytes = Vec::with_capacity(units.len().saturating_mul(2).saturating_add(2));
    for u in units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(data, &bytes)?;
    Ok(data)
}

/// `BOOL SHGetPathFromIDListW(pidl, pszPath)` — no real PIDLs; fail cleanly.
fn handle_sh_get_path_from_id_list_w(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let _pidl = engine.read_rcx()?;
    let path_ptr = engine.read_rdx()?;
    if path_ptr != 0 {
        write_utf16_c_string(engine, path_ptr, 260, "")?;
    }
    ret(engine, 0) // FALSE
}

/// `PIDLIST_ABSOLUTE SHBrowseForFolderW(lpbi)` — no UI; return NULL.
fn handle_sh_browse_for_folder_w(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let _lpbi = engine.read_rcx()?;
    let _ = E_FAIL;
    ret(engine, 0)
}
