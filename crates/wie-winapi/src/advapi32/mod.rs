use crate::guest_memory::{
    checked_address, read_bytes, read_u32, read_u64, write_u32 as write_guest_u32,
    write_u64 as write_guest_u64,
};
use crate::guest_string::{
    read_ansi_lossy as read_guest_ansi_lossy, read_utf16_lossy as read_guest_utf16_lossy,
};
use crate::{HandlerContext, RegistryKey, WinApiHandlerResult, WinApiState};
use anyhow::{Context, Result};

const ERROR_SUCCESS: u64 = 0;
const ERROR_FILE_NOT_FOUND: u64 = 2;
const ERROR_INVALID_HANDLE: u64 = 6;
const ERROR_INVALID_PARAMETER: u64 = 87;
const ERROR_MORE_DATA: u64 = 234;
const ERROR_NO_MORE_ITEMS: u64 = 259;
/// Win32 `ERROR_INSUFFICIENT_BUFFER` — the caller's buffer was too small.
const ERROR_INSUFFICIENT_BUFFER: u64 = 122;
const REG_CREATED_NEW_KEY: u32 = 1;
const REG_OPENED_EXISTING_KEY: u32 = 2;

/// Handles `ADVAPI32.dll!RegCreateKeyExA`.
pub fn handle_reg_create_key_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_key = engine
        .read_rcx()
        .context("failed to read RCX for RegCreateKeyExA")?;

    let subkey_va = engine
        .read_rdx()
        .context("failed to read RDX for RegCreateKeyExA")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegCreateKeyExA")?;

    let phk_result_address = checked_address(rsp, 0x40, "RegCreateKeyExA phkResult");
    let disposition_address = checked_address(rsp, 0x48, "RegCreateKeyExA lpdwDisposition");

    let phk_result = read_u64(engine, phk_result_address)?;
    let disposition_va = read_u64(engine, disposition_address)?;

    let subkey = read_optional_ansi_string(engine, subkey_va)?;

    // RegCreateKeyEx is the only entry point permitted to create a key.
    let (handle, disposition) = open_or_create_registry_key(state, parent_key, subkey, true)?
        .context("RegCreateKeyExA: allow_create is set, key must be created")?;

    if phk_result != 0 {
        write_guest_u64(engine, phk_result, handle)?;
    }

    if disposition_va != 0 {
        write_guest_u32(engine, disposition_va, disposition)?;
    }

    return_status(engine, ERROR_SUCCESS)
}

/// Handles `ADVAPI32.dll!RegOpenKeyExA`.
pub fn handle_reg_open_key_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_key = engine
        .read_rcx()
        .context("failed to read RCX for RegOpenKeyExA")?;

    let subkey_va = engine
        .read_rdx()
        .context("failed to read RDX for RegOpenKeyExA")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegOpenKeyExA")?;

    let phk_result_address = checked_address(rsp, 0x28, "RegOpenKeyExA phkResult");
    let phk_result = read_u64(engine, phk_result_address)?;

    let subkey = read_optional_ansi_string(engine, subkey_va)?;
    // RegOpenKeyEx opens only: a missing key is ERROR_FILE_NOT_FOUND and
    // must not be materialized (that is RegCreateKeyEx's job).
    let Some((handle, _disposition)) =
        open_or_create_registry_key(state, parent_key, subkey, false)?
    else {
        if phk_result != 0 {
            write_guest_u64(engine, phk_result, 0)?;
        }
        return return_status(engine, ERROR_FILE_NOT_FOUND);
    };

    if phk_result != 0 {
        write_guest_u64(engine, phk_result, handle)?;
    }

    return_status(engine, ERROR_SUCCESS)
}

/// Handles `ADVAPI32.dll!RegOpenKeyExW`.
pub fn handle_reg_open_key_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_key = engine
        .read_rcx()
        .context("failed to read RCX for RegOpenKeyExW")?;

    let subkey_va = engine
        .read_rdx()
        .context("failed to read RDX for RegOpenKeyExW")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegOpenKeyExW")?;

    let phk_result_address = checked_address(rsp, 0x28, "RegOpenKeyExW phkResult");
    let phk_result = read_u64(engine, phk_result_address)?;

    let subkey = read_optional_utf16_string(engine, subkey_va)?;
    // RegOpenKeyEx opens only: a missing key is ERROR_FILE_NOT_FOUND and
    // must not be materialized (that is RegCreateKeyEx's job).
    let Some((handle, _disposition)) =
        open_or_create_registry_key(state, parent_key, subkey, false)?
    else {
        if phk_result != 0 {
            write_guest_u64(engine, phk_result, 0)?;
        }
        return return_status(engine, ERROR_FILE_NOT_FOUND);
    };

    if phk_result != 0 {
        write_guest_u64(engine, phk_result, handle)?;
    }

    return_status(engine, ERROR_SUCCESS)
}

/// Handles `ADVAPI32.dll!RegOpenKeyA` (legacy; ≡ RegOpenKeyExA with `KEY_READ`).
///
/// Legacy `RegOpenKey` does not create a missing key: it returns
/// `ERROR_FILE_NOT_FOUND` and leaves `*phkResult` as 0, matching the
/// open-only semantics of the Ex pair.
pub fn handle_reg_open_key_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_key = engine
        .read_rcx()
        .context("failed to read RCX for RegOpenKeyA")?;

    let subkey_va = engine
        .read_rdx()
        .context("failed to read RDX for RegOpenKeyA")?;

    // Third parameter (phkResult) travels in R8: the legacy ABI takes only
    // hKey, lpSubKey, phkResult — no samDesired / options / reserved args.
    let phk_result = engine
        .read_r8()
        .context("failed to read R8 for RegOpenKeyA")?;

    let subkey = read_optional_ansi_string(engine, subkey_va)?;
    // Legacy RegOpenKey is open-only, same as RegOpenKeyEx: a missing key is
    // ERROR_FILE_NOT_FOUND and is not materialized.
    let Some((handle, _disposition)) =
        open_or_create_registry_key(state, parent_key, subkey, false)?
    else {
        if phk_result != 0 {
            write_guest_u64(engine, phk_result, 0)?;
        }
        return return_status(engine, ERROR_FILE_NOT_FOUND);
    };
    if phk_result != 0 {
        write_guest_u64(engine, phk_result, handle)?;
    }
    return_status(engine, ERROR_SUCCESS)
}

/// Handles `ADVAPI32.dll!RegOpenKeyW` (legacy; ≡ RegOpenKeyExW with `KEY_READ`).
///
/// Legacy `RegOpenKey` does not create a missing key: it returns
/// `ERROR_FILE_NOT_FOUND` and leaves `*phkResult` as 0, matching the
/// open-only semantics of the Ex pair.
pub fn handle_reg_open_key_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_key = engine
        .read_rcx()
        .context("failed to read RCX for RegOpenKeyW")?;

    let subkey_va = engine
        .read_rdx()
        .context("failed to read RDX for RegOpenKeyW")?;

    let phk_result = engine
        .read_r8()
        .context("failed to read R8 for RegOpenKeyW")?;

    let subkey = read_optional_utf16_string(engine, subkey_va)?;
    // Legacy RegOpenKey is open-only, same as RegOpenKeyEx: a missing key is
    // ERROR_FILE_NOT_FOUND and is not materialized.
    let Some((handle, _disposition)) =
        open_or_create_registry_key(state, parent_key, subkey, false)?
    else {
        if phk_result != 0 {
            write_guest_u64(engine, phk_result, 0)?;
        }
        return return_status(engine, ERROR_FILE_NOT_FOUND);
    };
    if phk_result != 0 {
        write_guest_u64(engine, phk_result, handle)?;
    }
    return_status(engine, ERROR_SUCCESS)
}

/// Handles `ADVAPI32.dll!RegCreateKeyExW`.
pub fn handle_reg_create_key_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_key = engine
        .read_rcx()
        .context("failed to read RCX for RegCreateKeyExW")?;

    let subkey_va = engine
        .read_rdx()
        .context("failed to read RDX for RegCreateKeyExW")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegCreateKeyExW")?;

    let phk_result_address = checked_address(rsp, 0x40, "RegCreateKeyExW phkResult");
    let disposition_address = checked_address(rsp, 0x48, "RegCreateKeyExW lpdwDisposition");

    let phk_result = read_u64(engine, phk_result_address)?;
    let disposition_va = read_u64(engine, disposition_address)?;

    let subkey = read_optional_utf16_string(engine, subkey_va)?;

    // RegCreateKeyEx is the only entry point permitted to create a key.
    let (handle, disposition) = open_or_create_registry_key(state, parent_key, subkey, true)?
        .context("RegCreateKeyExW: allow_create is set, key must be created")?;

    if phk_result != 0 {
        write_guest_u64(engine, phk_result, handle)?;
    }

    if disposition_va != 0 {
        write_guest_u32(engine, disposition_va, disposition)?;
    }

    return_status(engine, ERROR_SUCCESS)
}

/// Soft-dispatch path for ADVAPI32 exports not yet in the dense `WinApiId` table.
pub fn dispatch_advapi32_extra(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "regopenkeyexw" => Ok(Some(handle_reg_open_key_ex_w(ctx)?)),
        "regcreatekeyexw" => Ok(Some(handle_reg_create_key_ex_w(ctx)?)),
        "regenumkeyexw" | "regenumkeyexa" => Ok(Some(handle_reg_enum_key_ex(ctx)?)),
        "regenumvaluew" | "regenumvaluea" => Ok(Some(handle_reg_enum_value(ctx)?)),
        "regdeletevaluew" => Ok(Some(handle_reg_delete_value_w(ctx)?)),
        "regdeletekeyw" => Ok(Some(handle_reg_delete_key_w(ctx)?)),
        "regdeletekeya" => Ok(Some(handle_reg_delete_key_a(ctx)?)),
        "regflushkey" => Ok(Some(handle_reg_flush_key(ctx)?)),
        "regsavekeyw" | "regsavekeya" => Ok(Some(handle_reg_save_key(ctx)?)),
        "openprocesstoken" => Ok(Some(handle_open_process_token(ctx)?)),
        "adjusttokenprivileges" => Ok(Some(handle_adjust_token_privileges(ctx)?)),
        "lookupprivilegevaluew" | "lookupprivilegevaluea" => {
            Ok(Some(handle_lookup_privilege_value(ctx)?))
        }
        "systemfunction036" => Ok(Some(handle_system_function036(ctx)?)),
        "getfilesecurityw" | "getfilesecuritya" => Ok(Some(handle_get_file_security(ctx)?)),
        "setfilesecurityw" | "setfilesecuritya" => Ok(Some(handle_set_file_security(ctx)?)),
        "istextunicode" => Ok(Some(handle_istextunicode(ctx)?)),
        _ => Ok(None),
    }
}

/// Handles `ADVAPI32.dll!IsTextUnicode` — KISS subset: BOM signatures
/// (0xFFFE/0xFEFF), null-byte density, odd length, ASCII16 (even-position bytes
/// all zero) and the classic odd/even byte-sum divergence heuristic (4x
/// threshold) for STATISTICS. The full flag word is ANDed with the caller's
/// in-mask before writing `*lpiResult`; the return value reflects the unmasked
/// determination.
pub fn handle_istextunicode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let (buf, lpi) = (engine.read_rcx()?, engine.read_r8()?);
    let n = usize::try_from((engine.read_rdx()? & 0xffff_ffff).min(0x1_0000)).unwrap_or(0);
    let mut b = vec![0_u8; n];
    let mut f = 0_u32;
    if buf != 0 && n > 0 {
        read_bytes(engine, buf, &mut b)?;
        if b.starts_with(&[0xff, 0xfe]) {
            f |= 0x0008;
        }
        if b.starts_with(&[0xfe, 0xff]) {
            f |= 0x0080;
        }
        if n & 1 == 1 {
            f |= 0x0200;
        }
        if b.iter().filter(|x| **x == 0).count() >= 2 {
            f |= 0x1000;
        }
        let e = b.iter().step_by(2).map(|&x| u64::from(x)).sum::<u64>();
        let o = b
            .iter()
            .skip(1)
            .step_by(2)
            .map(|&x| u64::from(x))
            .sum::<u64>();
        if e >= o * 4 {
            f |= 0x0002;
        } else if o > e * 4 {
            f |= 0x0020;
        }
        if e == 0 {
            f |= 0x0001;
        }
    }
    if lpi != 0 {
        let m = read_u32(engine, lpi)?;
        write_guest_u32(engine, lpi, f & m)?;
    }
    return_bool(engine, f != 0)
}

const FAKE_PROCESS_TOKEN: u64 = 0x0000_0000_7000_0001;

/// `BOOL OpenProcessToken(HANDLE ProcessHandle, DWORD DesiredAccess, PHANDLE TokenHandle)`.
fn handle_open_process_token(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _process = engine.read_rcx().context("OpenProcessToken RCX")?;
    let _access = engine.read_rdx().context("OpenProcessToken RDX")?;
    let token_out = engine.read_r8().context("OpenProcessToken R8")?;
    if token_out != 0 {
        write_guest_u64(engine, token_out, FAKE_PROCESS_TOKEN)?;
    }
    state.process.last_error = 0;
    return_bool(engine, true)
}

/// `BOOL AdjustTokenPrivileges(...)` — succeed without changing privileges.
fn handle_adjust_token_privileges(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _token = engine.read_rcx()?;
    let _disable_all = engine.read_rdx()?;
    let _new_state = engine.read_r8()?;
    let _buf_len = engine.read_r9()?;
    return_bool(engine, true)
}

/// `BOOL LookupPrivilegeValueW(LPCWSTR, LPCWSTR, PLUID)`.
fn handle_lookup_privilege_value(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _system = engine.read_rcx()?;
    let _name = engine.read_rdx()?;
    let luid = engine.read_r8()?;
    if luid != 0 {
        // LUID is 8 bytes (LowPart + HighPart).
        write_guest_u64(engine, luid, 0x20)?; // arbitrary Se* privilege id
    }
    return_bool(engine, true)
}

/// `BOOLEAN SystemFunction036(PVOID RandomBuffer, ULONG RandomBufferLength)` (RtlGenRandom).
fn handle_system_function036(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let buf = engine.read_rcx()?;
    let len = engine.read_rdx()? & 0xffff_ffff;
    let len_usize = usize::try_from(len).unwrap_or(0);
    if buf != 0 && len_usize > 0 {
        // Deterministic pseudo-random fill (not crypto-grade; enough for 7z nonces).
        let mut bytes = vec![0_u8; len_usize];
        let mut state = 0x00c0_ffee_u64;
        for b in &mut bytes {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            *b = u8::try_from((state >> 33) & 0xff).unwrap_or(0);
        }
        engine
            .mem_write(buf, &bytes)
            .context("SystemFunction036 write")?;
    }
    // BOOLEAN TRUE
    return_bool(engine, true)
}

/// Minimal security descriptor size claim for `GetFileSecurity*`.
const FAKE_SD_NEED: u32 = 20;

/// `BOOL GetFileSecurityW(...)` — report not enough buffer / fail soft.
fn handle_get_file_security(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _path = engine.read_rcx()?;
    let _si = engine.read_rdx()?;
    let sd = engine.read_r8()?;
    let len = engine.read_r9()? & 0xffff_ffff;
    let rsp = engine.read_rsp()?;
    let needed_va = read_u64(
        engine,
        checked_address(rsp, 0x28, "GetFileSecurity length needed"),
    )?;
    if needed_va != 0 {
        write_guest_u32(engine, needed_va, FAKE_SD_NEED)?;
    }
    if sd != 0 && len >= u64::from(FAKE_SD_NEED) {
        // Zeroed SD stub.
        let need = usize::try_from(FAKE_SD_NEED).unwrap_or(20);
        let zeros = vec![0_u8; need];
        engine.mem_write(sd, &zeros)?;
        return return_bool(engine, true);
    }
    return_bool(engine, false)
}

/// `BOOL SetFileSecurityW(...)` — accept.
fn handle_set_file_security(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _path = engine.read_rcx()?;
    let _si = engine.read_rdx()?;
    let _sd = engine.read_r8()?;
    return_bool(engine, true)
}

fn return_bool(engine: &mut dyn wie_cpu::CpuEngine, ok: bool) -> Result<WinApiHandlerResult> {
    let v = u64::from(ok);
    let return_address = engine
        .return_from_win64_api(v)
        .context("failed to return BOOL from ADVAPI32")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: v,
    })
}

/// Handles `ADVAPI32.dll!RegQueryValueExA`.
pub fn handle_reg_query_value_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let key = engine
        .read_rcx()
        .context("failed to read RCX for RegQueryValueExA")?;
    let value_name_va = engine
        .read_rdx()
        .context("failed to read RDX for RegQueryValueExA")?;
    let _reserved = engine
        .read_r8()
        .context("failed to read R8 for RegQueryValueExA")?;
    let type_va = engine
        .read_r9()
        .context("failed to read R9 for RegQueryValueExA")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegQueryValueExA")?;
    let data_va = read_u64(
        engine,
        checked_address(rsp, 0x28, "RegQueryValueExA lpData"),
    )?;
    let cb_va = read_u64(
        engine,
        checked_address(rsp, 0x30, "RegQueryValueExA lpcbData"),
    )?;
    let value_name = read_optional_ansi_string(engine, value_name_va)?;
    let state = &mut *ctx.state;
    query_registry_value(engine, state, key, &value_name, type_va, data_va, cb_va)
}

/// Handles `ADVAPI32.dll!RegQueryValueExW`.
pub fn handle_reg_query_value_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let key = engine
        .read_rcx()
        .context("failed to read RCX for RegQueryValueExW")?;
    let value_name_va = engine
        .read_rdx()
        .context("failed to read RDX for RegQueryValueExW")?;
    let _reserved = engine
        .read_r8()
        .context("failed to read R8 for RegQueryValueExW")?;
    let type_va = engine
        .read_r9()
        .context("failed to read R9 for RegQueryValueExW")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegQueryValueExW")?;
    let data_va = read_u64(
        engine,
        checked_address(rsp, 0x28, "RegQueryValueExW lpData"),
    )?;
    let cb_va = read_u64(
        engine,
        checked_address(rsp, 0x30, "RegQueryValueExW lpcbData"),
    )?;
    let value_name = read_optional_utf16_string(engine, value_name_va)?;
    let state = &mut *ctx.state;
    query_registry_value(engine, state, key, &value_name, type_va, data_va, cb_va)
}

/// Shared `RegQueryValueEx` body — the A/W variants differ only in how the
/// value name is decoded.
///
/// Semantics follow real Windows: the stored type is returned regardless of
/// what the caller requested; `*lpcbData` is the buffer size in and the actual
/// byte count out; a buffer that is too small returns `ERROR_MORE_DATA` with
/// the required size and `*lpData` untouched; a `NULL` `lpData` is a size
/// probe that returns `ERROR_SUCCESS`.
fn query_registry_value(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    key: u64,
    value_name: &str,
    type_va: u64,
    data_va: u64,
    cb_va: u64,
) -> Result<WinApiHandlerResult> {
    let Some(path) = registry_key_full_path(state, key) else {
        return return_status(engine, ERROR_INVALID_HANDLE);
    };
    let root = state.file_io.bottle_root.clone();
    let store = state.registry();
    store.ensure_loaded(root.as_deref());
    let Some(value) = store.get_value(&path, value_name) else {
        // Real Windows zeroes *lpcbData when the value is missing.
        if cb_va != 0 {
            write_guest_u32(engine, cb_va, 0)?;
        }
        return return_status(engine, ERROR_FILE_NOT_FOUND);
    };
    if type_va != 0 {
        write_guest_u32(engine, type_va, value.value_type)?;
    }
    let required = u32::try_from(value.data.len()).unwrap_or(u32::MAX);
    if cb_va == 0 {
        return return_status(engine, ERROR_SUCCESS);
    }
    let mut cb_buf = [0_u8; 4];
    engine.mem_read(cb_va, &mut cb_buf)?;
    let capacity = u32::from_le_bytes(cb_buf);
    write_guest_u32(engine, cb_va, required)?;
    if data_va == 0 {
        // Size probe: the caller wants the required size, not the data.
        return return_status(engine, ERROR_SUCCESS);
    }
    if required > capacity {
        // Real Windows leaves *lpData untouched on ERROR_MORE_DATA; the caller
        // is expected to retry with a buffer of the reported size.
        return return_status(engine, ERROR_MORE_DATA);
    }
    if !value.data.is_empty() {
        engine.mem_write(data_va, &value.data)?;
    }
    return_status(engine, ERROR_SUCCESS)
}

/// Handles `ADVAPI32.dll!RegSetValueExA`.
pub fn handle_reg_set_value_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let key = engine
        .read_rcx()
        .context("failed to read RCX for RegSetValueExA")?;
    let value_name_va = engine
        .read_rdx()
        .context("failed to read RDX for RegSetValueExA")?;
    let _reserved = engine
        .read_r8()
        .context("failed to read R8 for RegSetValueExA")?;
    let value_type = engine
        .read_r9()
        .context("failed to read R9 for RegSetValueExA")?;
    let value_type = u32::try_from(value_type & 0xffff_ffff).unwrap_or(0);
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegSetValueExA")?;
    let data_va = read_u64(engine, checked_address(rsp, 0x28, "RegSetValueExA lpData"))?;
    let mut cb_buf = [0_u8; 4];
    engine.mem_read(
        checked_address(rsp, 0x30, "RegSetValueExA cbData"),
        &mut cb_buf,
    )?;
    let cb_data = u32::from_le_bytes(cb_buf);
    let value_name = read_optional_ansi_string(engine, value_name_va)?;
    let state = &mut *ctx.state;
    set_registry_value(engine, state, key, value_name, value_type, data_va, cb_data)
}

/// Handles `ADVAPI32.dll!RegSetValueExW`.
pub fn handle_reg_set_value_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let key = engine
        .read_rcx()
        .context("failed to read RCX for RegSetValueExW")?;
    let value_name_va = engine
        .read_rdx()
        .context("failed to read RDX for RegSetValueExW")?;
    let _reserved = engine
        .read_r8()
        .context("failed to read R8 for RegSetValueExW")?;
    let value_type = engine
        .read_r9()
        .context("failed to read R9 for RegSetValueExW")?;
    let value_type = u32::try_from(value_type & 0xffff_ffff).unwrap_or(0);
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegSetValueExW")?;
    let data_va = read_u64(engine, checked_address(rsp, 0x28, "RegSetValueExW lpData"))?;
    let mut cb_buf = [0_u8; 4];
    engine.mem_read(
        checked_address(rsp, 0x30, "RegSetValueExW cbData"),
        &mut cb_buf,
    )?;
    let cb_data = u32::from_le_bytes(cb_buf);
    let value_name = read_optional_utf16_string(engine, value_name_va)?;
    let state = &mut *ctx.state;
    set_registry_value(engine, state, key, value_name, value_type, data_va, cb_data)
}

/// Shared `RegSetValueEx` body — stores the raw bytes exactly as given
/// (`REG_SZ` keeps its terminating NUL, `REG_DWORD` its 4 bytes) and writes the
/// whole hive through to disk so the value survives a relaunch.
fn set_registry_value(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    key: u64,
    value_name: String,
    value_type: u32,
    data_va: u64,
    cb_data: u32,
) -> Result<WinApiHandlerResult> {
    let Some(path) = registry_key_full_path(state, key) else {
        return return_status(engine, ERROR_INVALID_HANDLE);
    };
    let data_len = usize::try_from(cb_data).unwrap_or(0);
    if data_len > 0 && data_va == 0 {
        return return_status(engine, ERROR_INVALID_PARAMETER);
    }
    let data = if data_len == 0 {
        Vec::new()
    } else {
        let mut buf = vec![0_u8; data_len];
        engine.mem_read(data_va, &mut buf)?;
        buf
    };
    let root = state.file_io.bottle_root.clone();
    {
        let store = state.registry();
        store.ensure_loaded(root.as_deref());
        store.set_value(
            &path,
            crate::registry::RegistryValue::new(value_name, value_type, data),
        );
    }
    if root.is_some() {
        state.registry().persist(root.as_deref());
    }
    return_status(engine, ERROR_SUCCESS)
}

/// Handles `ADVAPI32.dll!RegDeleteValueA`.
pub fn handle_reg_delete_value_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let key = engine
        .read_rcx()
        .context("failed to read RCX for RegDeleteValueA")?;
    let value_name_va = engine
        .read_rdx()
        .context("failed to read RDX for RegDeleteValueA")?;
    let value_name = read_optional_ansi_string(engine, value_name_va)?;
    let state = &mut *ctx.state;
    delete_registry_value(engine, state, key, &value_name)
}

/// Handles `ADVAPI32.dll!RegDeleteValueW` (reached via soft dispatch; not in
/// the dense `WinApiId` table because no current guest imports it directly).
pub fn handle_reg_delete_value_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let key = engine
        .read_rcx()
        .context("failed to read RCX for RegDeleteValueW")?;
    let value_name_va = engine
        .read_rdx()
        .context("failed to read RDX for RegDeleteValueW")?;
    let value_name = read_optional_utf16_string(engine, value_name_va)?;
    let state = &mut *ctx.state;
    delete_registry_value(engine, state, key, &value_name)
}

/// Shared `RegDeleteValue` body.
fn delete_registry_value(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    key: u64,
    value_name: &str,
) -> Result<WinApiHandlerResult> {
    let Some(path) = registry_key_full_path(state, key) else {
        return return_status(engine, ERROR_INVALID_HANDLE);
    };
    let root = state.file_io.bottle_root.clone();
    let deleted = {
        let store = state.registry();
        store.ensure_loaded(root.as_deref());
        store.delete_value(&path, value_name)
    };
    if deleted && root.is_some() {
        state.registry().persist(root.as_deref());
    }
    return_status(
        engine,
        if deleted {
            ERROR_SUCCESS
        } else {
            ERROR_FILE_NOT_FOUND
        },
    )
}

/// Handles `ADVAPI32.dll!RegDeleteKeyW`.
pub fn handle_reg_delete_key_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent = engine.read_rcx().context("RegDeleteKeyW RCX")?;
    let subkey_va = engine.read_rdx().context("RegDeleteKeyW RDX")?;
    let subkey = read_optional_utf16_string(engine, subkey_va)?;
    delete_registry_key(engine, state, parent, &subkey)
}

/// Handles `ADVAPI32.dll!RegDeleteKeyA`.
pub fn handle_reg_delete_key_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent = engine.read_rcx().context("RegDeleteKeyA RCX")?;
    let subkey_va = engine.read_rdx().context("RegDeleteKeyA RDX")?;
    let subkey = read_optional_ansi_string(engine, subkey_va)?;
    delete_registry_key(engine, state, parent, &subkey)
}

/// Shared `RegDeleteKey` body: drops every handle record resolving to the
/// deleted path (or a subpath) and all values stored at-or-below it.
fn delete_registry_key(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    parent: u64,
    subkey: &str,
) -> Result<WinApiHandlerResult> {
    let Some(parent_path) = registry_key_full_path(state, parent) else {
        return return_status(engine, ERROR_INVALID_HANDLE);
    };
    let full_path = if parent_path.is_empty() {
        subkey.to_owned()
    } else {
        format!("{parent_path}\\{subkey}")
    };
    let prefix = format!("{full_path}\\");
    let doomed: Vec<u64> = state
        .process
        .registry_keys
        .iter()
        .filter(|k| {
            let Some(path) = registry_key_full_path(state, k.handle) else {
                return false;
            };
            path == full_path || path.starts_with(&prefix)
        })
        .map(|k| k.handle)
        .collect();
    state
        .process
        .registry_keys
        .retain(|k| !doomed.contains(&k.handle));
    let root = state.file_io.bottle_root.clone();
    let had_values = {
        let store = state.registry();
        store.ensure_loaded(root.as_deref());
        store.delete_key(&full_path)
    };
    let removed = had_values || !doomed.is_empty();
    if removed && root.is_some() {
        state.registry().persist(root.as_deref());
    }
    return_status(
        engine,
        if removed {
            ERROR_SUCCESS
        } else {
            ERROR_FILE_NOT_FOUND
        },
    )
}

/// `LSTATUS RegFlushKey(HKEY)` — values are write-through to the bottle hive
/// at mutation time, so the flush persists again (belt-and-braces) and
/// succeeds.
fn handle_reg_flush_key(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _key = engine.read_rcx().context("RegFlushKey RCX")?;
    let root = state.file_io.bottle_root.clone();
    if root.is_some() {
        state.registry().persist(root.as_deref());
    }
    return_status(engine, ERROR_SUCCESS)
}

/// `LSTATUS RegSaveKeyW(HKEY, LPCWSTR, LPSECURITY_ATTRIBUTES)` — documented
/// no-op: the host hive already persists per-bottle, and no guest file is
/// written.
fn handle_reg_save_key(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _key = engine.read_rcx().context("RegSaveKey RCX")?;
    return_status(engine, ERROR_SUCCESS)
}

/// Resolve a key handle to its full hive path (`HKCU\Software\...`).
///
/// Walks the parent chain of the key record up to the root-handle constant the
/// guest passed to `RegOpenKey*`/`RegCreateKeyEx*`. Returns `None` for an
/// unknown handle (the callers report `ERROR_INVALID_HANDLE`).
fn registry_key_full_path(state: &WinApiState, key: u64) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = key;
    loop {
        if let Some(prefix) = crate::registry::root_prefix(current) {
            parts.push(prefix.to_owned());
            break;
        }
        let record = state
            .process
            .registry_keys
            .iter()
            .find(|key_record| key_record.handle == current)?;
        parts.push(record.subkey.clone());
        current = record.parent;
    }
    parts.reverse();
    Some(parts.join("\\"))
}

/// Handles `ADVAPI32.dll!RegCloseKey`.
pub fn handle_reg_close_key(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _key = engine
        .read_rcx()
        .context("failed to read RCX for RegCloseKey")?;

    return_status(engine, ERROR_SUCCESS)
}

/// Handles `ADVAPI32.dll!InitializeSecurityDescriptor`.
pub fn handle_initialize_security_descriptor(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let security_descriptor_va = engine
        .read_rcx()
        .context("failed to read RCX for InitializeSecurityDescriptor")?;

    if security_descriptor_va != 0 {
        // Minimal SECURITY_DESCRIPTOR-like marker. Enough for code that only
        // expects the call to succeed.
        write_guest_u32(engine, security_descriptor_va, 1)?;
    }

    ctx.finish(1)
}

/// Handles `ADVAPI32.dll!SetSecurityDescriptorDacl`.
pub fn handle_set_security_descriptor_dacl(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let security_descriptor_va = engine
        .read_rcx()
        .context("failed to read RCX for SetSecurityDescriptorDacl")?;
    let _dacl_present = engine
        .read_rdx()
        .context("failed to read RDX for SetSecurityDescriptorDacl")?;
    let _dacl_va = engine
        .read_r8()
        .context("failed to read R8 for SetSecurityDescriptorDacl")?;
    let _dacl_defaulted = engine
        .read_r9()
        .context("failed to read R9 for SetSecurityDescriptorDacl")?;

    let return_value = u64::from(security_descriptor_va != 0);

    ctx.finish(return_value)
}

/// `LSTATUS RegEnumKeyExW(HKEY, DWORD, LPWSTR, LPDWORD, ...)`.
fn handle_reg_enum_key_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hkey = engine.read_rcx()?;
    let index = engine.read_rdx()? & 0xffff_ffff;
    let name_buf = engine.read_r8()?;
    let name_len_va = engine.read_r9()?;
    let rsp = engine.read_rsp()?;
    let _reserved = read_u64(engine, checked_address(rsp, 0x28, "lpReserved")).unwrap_or(0);
    let _class = read_u64(engine, checked_address(rsp, 0x30, "lpClass")).unwrap_or(0);
    let _class_len = read_u64(engine, checked_address(rsp, 0x38, "lpcClass")).unwrap_or(0);
    let _ft = read_u64(engine, checked_address(rsp, 0x40, "lpftLastWriteTime")).unwrap_or(0);

    // Gather all subkeys whose parent == hkey.
    let subkeys: Vec<&String> = state
        .process
        .registry_keys
        .iter()
        .filter(|k| k.parent == hkey)
        .map(|k| &k.subkey)
        .collect();

    let idx = usize::try_from(index).unwrap_or(usize::MAX);
    if idx >= subkeys.len() {
        return return_status(engine, ERROR_NO_MORE_ITEMS);
    }
    let Some(name) = subkeys.get(idx) else {
        return return_status(engine, ERROR_NO_MORE_ITEMS);
    };
    if name_buf == 0 || name_len_va == 0 {
        return return_status(engine, ERROR_INVALID_PARAMETER);
    }
    let mut len_buf = [0_u8; 4];
    engine.mem_read(name_len_va, &mut len_buf)?;
    let buf_len = u32::from_le_bytes(len_buf);
    let units: Vec<u16> = name.encode_utf16().collect();
    let needed = u32::try_from(units.len()).unwrap_or(0);
    if needed >= buf_len {
        write_guest_u32(engine, name_len_va, needed.saturating_add(1))?;
        return return_status(engine, ERROR_INSUFFICIENT_BUFFER);
    }
    let mut bytes = Vec::with_capacity(units.len().saturating_mul(2).saturating_add(2));
    for u in &units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(name_buf, &bytes)?;
    write_guest_u32(engine, name_len_va, needed)?;
    return_status(engine, ERROR_SUCCESS)
}

/// `LSTATUS RegEnumValueW(HKEY, DWORD, LPWSTR, LPDWORD, ...)` — no values stored.
fn handle_reg_enum_value(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hkey = engine.read_rcx()?;
    let _index = engine.read_rdx()?;
    // No registry values are stored in the current model.
    return_status(engine, ERROR_NO_MORE_ITEMS)
}

/// Look up an existing registry key handle; `None` when the key is absent.
/// Shared by every registry-key entry point so they all agree on how a
/// (parent, subkey) pair resolves.
fn find_registry_key(state: &WinApiState, parent: u64, subkey: &str) -> Option<u64> {
    state
        .process
        .registry_keys
        .iter()
        .find(|key| key.parent == parent && key.subkey == subkey)
        .map(|key| key.handle)
}

/// Resolve a registry key, creating it when absent only if `allow_create` is set.
///
/// Only `RegCreateKeyEx*` may materialize a key: `RegOpenKeyEx*` and the
/// legacy `RegOpenKey*` must report the key as missing instead. Returns
/// `Ok(None)` when the key is absent and creation is not permitted; the
/// disposition follows the real REG_CREATED_NEW_KEY / REG_OPENED_EXISTING_KEY
/// constants otherwise.
fn open_or_create_registry_key(
    state: &mut WinApiState,
    parent: u64,
    subkey: String,
    allow_create: bool,
) -> Result<Option<(u64, u32)>> {
    if let Some(handle) = find_registry_key(state, parent, &subkey) {
        return Ok(Some((handle, REG_OPENED_EXISTING_KEY)));
    }

    if !allow_create {
        return Ok(None);
    }

    let handle = state.process.next_registry_key_handle.as_u64();
    state.process.next_registry_key_handle = crate::RegistryKeyHandle::from(
        state
            .process
            .next_registry_key_handle
            .as_u64()
            .checked_add(1)
            .context("registry key handle overflow")?,
    );

    state.process.registry_keys.push(RegistryKey {
        handle,
        parent,
        subkey,
    });

    Ok(Some((handle, REG_CREATED_NEW_KEY)))
}

fn return_status(engine: &mut dyn wie_cpu::CpuEngine, status: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(status)
        .context("failed to return registry status")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: status,
    })
}

fn read_optional_ansi_string(engine: &mut dyn wie_cpu::CpuEngine, address: u64) -> Result<String> {
    if address == 0 {
        Ok(String::new())
    } else {
        read_guest_ansi_lossy(engine, address, 1024)
    }
}

fn read_optional_utf16_string(engine: &mut dyn wie_cpu::CpuEngine, address: u64) -> Result<String> {
    if address == 0 {
        Ok(String::new())
    } else {
        read_guest_utf16_lossy(engine, address, 1024)
    }
}

#[cfg(test)]
mod tests;
