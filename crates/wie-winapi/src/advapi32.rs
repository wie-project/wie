use crate::guest_memory::{
    checked_address, read_u64, write_u32 as write_guest_u32, write_u64 as write_guest_u64,
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
const REG_CREATED_NEW_KEY: u32 = 1;
const REG_OPENED_EXISTING_KEY: u32 = 2;

/// Handles `ADVAPI32.dll!RegCreateKeyExA`.
pub fn handle_reg_create_key_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let parent_key = engine
        .read_rcx()
        .context("failed to read RCX for RegCreateKeyExA")?;

    let subkey_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegCreateKeyExA")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegCreateKeyExA")?;

    let phk_result_address = checked_address(rsp, 0x40, "RegCreateKeyExA phkResult");
    let disposition_address = checked_address(rsp, 0x48, "RegCreateKeyExA lpdwDisposition");

    let phk_result = read_u64(engine, phk_result_address)?;
    let disposition_ptr = read_u64(engine, disposition_address)?;

    let subkey = read_optional_ansi_string(engine, subkey_ptr)?;

    // RegCreateKeyEx is the only entry point permitted to create a key.
    let (handle, disposition) = open_or_create_registry_key(state, parent_key, subkey, true)?
        .context("RegCreateKeyExA: allow_create is set, key must be created")?;

    if phk_result != 0 {
        write_guest_u64(engine, phk_result, handle)?;
    }

    if disposition_ptr != 0 {
        write_guest_u32(engine, disposition_ptr, disposition)?;
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

    let subkey_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegOpenKeyExA")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegOpenKeyExA")?;

    let phk_result_address = checked_address(rsp, 0x30, "RegOpenKeyExA phkResult");
    let phk_result = read_u64(engine, phk_result_address)?;

    let subkey = read_optional_ansi_string(engine, subkey_ptr)?;
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

    let subkey_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegOpenKeyExW")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegOpenKeyExW")?;

    let phk_result_address = checked_address(rsp, 0x30, "RegOpenKeyExW phkResult");
    let phk_result = read_u64(engine, phk_result_address)?;

    let subkey = read_optional_utf16_string(engine, subkey_ptr)?;
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

    let subkey_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegOpenKeyA")?;

    // Third parameter (phkResult) travels in R8: the legacy ABI takes only
    // hKey, lpSubKey, phkResult — no samDesired / options / reserved args.
    let phk_result = engine
        .read_r8()
        .context("failed to read R8 for RegOpenKeyA")?;

    let subkey = read_optional_ansi_string(engine, subkey_ptr)?;
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

    let subkey_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegOpenKeyW")?;

    let phk_result = engine
        .read_r8()
        .context("failed to read R8 for RegOpenKeyW")?;

    let subkey = read_optional_utf16_string(engine, subkey_ptr)?;
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

    let subkey_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegCreateKeyExW")?;

    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegCreateKeyExW")?;

    let phk_result_address = checked_address(rsp, 0x40, "RegCreateKeyExW phkResult");
    let disposition_address = checked_address(rsp, 0x48, "RegCreateKeyExW lpdwDisposition");

    let phk_result = read_u64(engine, phk_result_address)?;
    let disposition_ptr = read_u64(engine, disposition_address)?;

    let subkey = read_optional_utf16_string(engine, subkey_ptr)?;

    // RegCreateKeyEx is the only entry point permitted to create a key.
    let (handle, disposition) = open_or_create_registry_key(state, parent_key, subkey, true)?
        .context("RegCreateKeyExW: allow_create is set, key must be created")?;

    if phk_result != 0 {
        write_guest_u64(engine, phk_result, handle)?;
    }

    if disposition_ptr != 0 {
        write_guest_u32(engine, disposition_ptr, disposition)?;
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
        "openprocesstoken" => Ok(Some(handle_open_process_token(ctx)?)),
        "adjusttokenprivileges" => Ok(Some(handle_adjust_token_privileges(ctx)?)),
        "lookupprivilegevaluew" | "lookupprivilegevaluea" => {
            Ok(Some(handle_lookup_privilege_value(ctx)?))
        }
        "systemfunction036" => Ok(Some(handle_system_function036(ctx)?)),
        "getfilesecurityw" | "getfilesecuritya" => Ok(Some(handle_get_file_security(ctx)?)),
        "setfilesecurityw" | "setfilesecuritya" => Ok(Some(handle_set_file_security(ctx)?)),
        _ => Ok(None),
    }
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
    let needed_ptr = read_u64(
        engine,
        checked_address(rsp, 0x28, "GetFileSecurity length needed"),
    )?;
    if needed_ptr != 0 {
        write_guest_u32(engine, needed_ptr, FAKE_SD_NEED)?;
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
    let value_name_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegQueryValueExA")?;
    let _reserved = engine
        .read_r8()
        .context("failed to read R8 for RegQueryValueExA")?;
    let type_ptr = engine
        .read_r9()
        .context("failed to read R9 for RegQueryValueExA")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegQueryValueExA")?;
    let data_ptr = read_u64(
        engine,
        checked_address(rsp, 0x28, "RegQueryValueExA lpData"),
    )?;
    let cb_ptr = read_u64(
        engine,
        checked_address(rsp, 0x30, "RegQueryValueExA lpcbData"),
    )?;
    let value_name = read_optional_ansi_string(engine, value_name_ptr)?;
    let state = &mut *ctx.state;
    query_registry_value(engine, state, key, &value_name, type_ptr, data_ptr, cb_ptr)
}

/// Handles `ADVAPI32.dll!RegQueryValueExW`.
pub fn handle_reg_query_value_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let key = engine
        .read_rcx()
        .context("failed to read RCX for RegQueryValueExW")?;
    let value_name_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegQueryValueExW")?;
    let _reserved = engine
        .read_r8()
        .context("failed to read R8 for RegQueryValueExW")?;
    let type_ptr = engine
        .read_r9()
        .context("failed to read R9 for RegQueryValueExW")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for RegQueryValueExW")?;
    let data_ptr = read_u64(
        engine,
        checked_address(rsp, 0x28, "RegQueryValueExW lpData"),
    )?;
    let cb_ptr = read_u64(
        engine,
        checked_address(rsp, 0x30, "RegQueryValueExW lpcbData"),
    )?;
    let value_name = read_optional_utf16_string(engine, value_name_ptr)?;
    let state = &mut *ctx.state;
    query_registry_value(engine, state, key, &value_name, type_ptr, data_ptr, cb_ptr)
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
    type_ptr: u64,
    data_ptr: u64,
    cb_ptr: u64,
) -> Result<WinApiHandlerResult> {
    let Some(path) = registry_key_full_path(state, key) else {
        return return_status(engine, ERROR_INVALID_HANDLE);
    };
    let root = state.file_io.bottle_root.clone();
    let store = state.registry();
    store.ensure_loaded(root.as_deref());
    let Some(value) = store.get_value(&path, value_name) else {
        // Real Windows zeroes *lpcbData when the value is missing.
        if cb_ptr != 0 {
            write_guest_u32(engine, cb_ptr, 0)?;
        }
        return return_status(engine, ERROR_FILE_NOT_FOUND);
    };
    if type_ptr != 0 {
        write_guest_u32(engine, type_ptr, value.value_type)?;
    }
    let required = u32::try_from(value.data.len()).unwrap_or(u32::MAX);
    if cb_ptr == 0 {
        return return_status(engine, ERROR_SUCCESS);
    }
    let mut cb_buf = [0_u8; 4];
    engine.mem_read(cb_ptr, &mut cb_buf)?;
    let capacity = u32::from_le_bytes(cb_buf);
    write_guest_u32(engine, cb_ptr, required)?;
    if data_ptr == 0 {
        // Size probe: the caller wants the required size, not the data.
        return return_status(engine, ERROR_SUCCESS);
    }
    if required > capacity {
        // Real Windows leaves *lpData untouched on ERROR_MORE_DATA; the caller
        // is expected to retry with a buffer of the reported size.
        return return_status(engine, ERROR_MORE_DATA);
    }
    if !value.data.is_empty() {
        engine.mem_write(data_ptr, &value.data)?;
    }
    return_status(engine, ERROR_SUCCESS)
}

/// Handles `ADVAPI32.dll!RegSetValueExA`.
pub fn handle_reg_set_value_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let key = engine
        .read_rcx()
        .context("failed to read RCX for RegSetValueExA")?;
    let value_name_ptr = engine
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
    let data_ptr = read_u64(engine, checked_address(rsp, 0x28, "RegSetValueExA lpData"))?;
    let mut cb_buf = [0_u8; 4];
    engine.mem_read(
        checked_address(rsp, 0x30, "RegSetValueExA cbData"),
        &mut cb_buf,
    )?;
    let cb_data = u32::from_le_bytes(cb_buf);
    let value_name = read_optional_ansi_string(engine, value_name_ptr)?;
    let state = &mut *ctx.state;
    set_registry_value(
        engine, state, key, value_name, value_type, data_ptr, cb_data,
    )
}

/// Handles `ADVAPI32.dll!RegSetValueExW`.
pub fn handle_reg_set_value_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let key = engine
        .read_rcx()
        .context("failed to read RCX for RegSetValueExW")?;
    let value_name_ptr = engine
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
    let data_ptr = read_u64(engine, checked_address(rsp, 0x28, "RegSetValueExW lpData"))?;
    let mut cb_buf = [0_u8; 4];
    engine.mem_read(
        checked_address(rsp, 0x30, "RegSetValueExW cbData"),
        &mut cb_buf,
    )?;
    let cb_data = u32::from_le_bytes(cb_buf);
    let value_name = read_optional_utf16_string(engine, value_name_ptr)?;
    let state = &mut *ctx.state;
    set_registry_value(
        engine, state, key, value_name, value_type, data_ptr, cb_data,
    )
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
    data_ptr: u64,
    cb_data: u32,
) -> Result<WinApiHandlerResult> {
    let Some(path) = registry_key_full_path(state, key) else {
        return return_status(engine, ERROR_INVALID_HANDLE);
    };
    let data_len = usize::try_from(cb_data).unwrap_or(0);
    if data_len > 0 && data_ptr == 0 {
        return return_status(engine, ERROR_INVALID_PARAMETER);
    }
    let data = if data_len == 0 {
        Vec::new()
    } else {
        let mut buf = vec![0_u8; data_len];
        engine.mem_read(data_ptr, &mut buf)?;
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
    let value_name_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegDeleteValueA")?;
    let value_name = read_optional_ansi_string(engine, value_name_ptr)?;
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
    let value_name_ptr = engine
        .read_rdx()
        .context("failed to read RDX for RegDeleteValueW")?;
    let value_name = read_optional_utf16_string(engine, value_name_ptr)?;
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
    let security_descriptor_ptr = engine
        .read_rcx()
        .context("failed to read RCX for InitializeSecurityDescriptor")?;

    if security_descriptor_ptr != 0 {
        // Minimal SECURITY_DESCRIPTOR-like marker. Enough for code that only
        // expects the call to succeed.
        write_guest_u32(engine, security_descriptor_ptr, 1)?;
    }

    ctx.finish(1)
}

/// Handles `ADVAPI32.dll!SetSecurityDescriptorDacl`.
pub fn handle_set_security_descriptor_dacl(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let security_descriptor_ptr = engine
        .read_rcx()
        .context("failed to read RCX for SetSecurityDescriptorDacl")?;
    let _dacl_present = engine
        .read_rdx()
        .context("failed to read RDX for SetSecurityDescriptorDacl")?;
    let _dacl_ptr = engine
        .read_r8()
        .context("failed to read R8 for SetSecurityDescriptorDacl")?;
    let _dacl_defaulted = engine
        .read_r9()
        .context("failed to read R9 for SetSecurityDescriptorDacl")?;

    let return_value = u64::from(security_descriptor_ptr != 0);

    ctx.finish(return_value)
}

/// `LSTATUS RegEnumKeyExW(HKEY, DWORD, LPWSTR, LPDWORD, ...)`.
fn handle_reg_enum_key_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hkey = engine.read_rcx()?;
    let index = engine.read_rdx()? & 0xffff_ffff;
    let name_buf = engine.read_r8()?;
    let name_len_ptr = engine.read_r9()?;
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
    if name_buf == 0 || name_len_ptr == 0 {
        return return_status(engine, 87); // ERROR_INVALID_PARAMETER
    }
    let mut len_buf = [0_u8; 4];
    engine.mem_read(name_len_ptr, &mut len_buf)?;
    let buf_len = u32::from_le_bytes(len_buf);
    let units: Vec<u16> = name.encode_utf16().collect();
    let needed = u32::try_from(units.len()).unwrap_or(0);
    if needed >= buf_len {
        write_guest_u32(engine, name_len_ptr, needed.saturating_add(1))?;
        return return_status(engine, 122); // ERROR_INSUFFICIENT_BUFFER
    }
    let mut bytes = Vec::with_capacity(units.len().saturating_mul(2).saturating_add(2));
    for u in &units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(name_buf, &bytes)?;
    write_guest_u32(engine, name_len_ptr, needed)?;
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
mod tests {
    use super::*;
    use crate::guest_heap::GuestHeap;
    use crate::state::{
        DllStateMap, FileIoState, HeapState, KernelState, ModuleState, ProcessState,
        WinApiEnvironment,
    };
    use crate::sync_obj::SyncState;
    use crate::vfs::VolumeConfig;
    use crate::{HandlerContext, RegistryKeyHandle, ThreadState, WinApiState};
    use std::sync::{Arc, Mutex};
    use wie_cpu::{CpuEngine, IcedCpu};

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    // STACK_VA + STACK_SIZE - 0x100 (leave room for a dummy return address).
    const STACK_TOP: u64 = 0x100_FF00;
    const HKEY_CURRENT_USER: u64 = 0x8000_0001;
    const NOTEPAD_KEY: u64 = 0x100;

    /// Minimal engine for handler unit tests: maps guest pages with a valid
    /// return address on the stack (mirrors `state/tests.rs::test_engine`).
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, wie_cpu::RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, STACK_SIZE, wie_cpu::RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        // `return_from_win64_api` pops the return address, so RSP drifts 8
        // bytes past STACK_TOP after the first call; reset it every call.
        cpu.write_rsp(STACK_TOP).ok();
    }

    /// Write the 5th/6th stack arguments at their Win64 shadow-space slots.
    fn write_stack_args(cpu: &mut IcedCpu, fifth: u64, sixth: u64) {
        cpu.mem_write(STACK_TOP + 0x28, &fifth.to_le_bytes())
            .expect("write 5th stack arg");
        cpu.mem_write(STACK_TOP + 0x30, &sixth.to_le_bytes())
            .expect("write 6th stack arg");
    }

    fn write_utf16(cpu: &mut IcedCpu, addr: u64, s: &str) {
        let mut bytes = Vec::new();
        for unit in s.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        cpu.mem_write(addr, &bytes).expect("write utf16 string");
    }

    fn write_ansi(cpu: &mut IcedCpu, addr: u64, s: &str) {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        cpu.mem_write(addr, &bytes).expect("write ansi string");
    }

    fn test_env() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0x0000_0000_1400_0000,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 1,
        }
    }

    /// Default state with a bump heap covering [0x2000, 0x10000).
    fn default_winapi_state() -> WinApiState {
        WinApiState {
            heap_state: HeapState {
                heap: GuestHeap::new(0x2000, 0x10000),
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: crate::FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: ahash::HashMap::default(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: crate::GuestStdinMode::InjectOnly,
                ucrt_files: ahash::HashMap::default(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: ahash::HashMap::default(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: ahash::HashMap::default(),
                environment: crate::DEFAULT_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: ahash::HashMap::default(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(crate::present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: ahash::HashMap::default(),
                import_resolver: None,
                get_proc_address_cache: ahash::HashMap::default(),
                next_module_handle: crate::ModuleHandle::from(
                    crate::dll_loader::REAL_MODULE_HANDLE_BASE,
                ),
            },
        }
    }

    /// The Notepad key record exists under HKCU, as RNotepad creates it.
    fn seed_notepad_key(state: &mut WinApiState) {
        state.process.registry_keys.push(crate::RegistryKey {
            handle: NOTEPAD_KEY,
            parent: HKEY_CURRENT_USER,
            subkey: "Software\\Microsoft\\Notepad".into(),
        });
    }

    fn run_set_value_w(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        name_ptr: u64,
        value_type: u32,
        data_ptr: u64,
        cb_data: u32,
    ) -> u64 {
        write_regs(engine, NOTEPAD_KEY, name_ptr, 0, u64::from(value_type));
        write_stack_args(engine, data_ptr, u64::from(cb_data));
        let r = handle_reg_set_value_ex_w(&mut HandlerContext::new(engine, test_env(), state))
            .expect("RegSetValueExW handler");
        r.return_value
    }

    fn run_query_value_w(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        name_ptr: u64,
        type_ptr: u64,
        data_ptr: u64,
        cb_ptr: u64,
    ) -> u64 {
        write_regs(engine, NOTEPAD_KEY, name_ptr, 0, type_ptr);
        write_stack_args(engine, data_ptr, cb_ptr);
        let r = handle_reg_query_value_ex_w(&mut HandlerContext::new(engine, test_env(), state))
            .expect("RegQueryValueExW handler");
        r.return_value
    }

    fn read_u32_at(engine: &mut IcedCpu, addr: u64) -> u32 {
        let mut buf = [0_u8; 4];
        engine.mem_read(addr, &mut buf).expect("read guest u32");
        u32::from_le_bytes(buf)
    }

    fn read_bytes_at(engine: &mut IcedCpu, addr: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0_u8; len];
        engine.mem_read(addr, &mut buf).expect("read guest bytes");
        buf
    }

    #[test]
    fn test_set_query_dword_round_trip_w() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        seed_notepad_key(&mut state);
        let name_ptr = 0x5000;
        write_utf16(&mut engine, name_ptr, "fWrap");
        let data_ptr = 0x4200;
        engine
            .mem_write(data_ptr, &1_u32.to_le_bytes())
            .expect("write dword value");
        assert_eq!(
            run_set_value_w(&mut engine, &mut state, name_ptr, 4, data_ptr, 4),
            0
        );

        // Query with a 16-byte buffer: expect type + data + exact size back.
        let type_ptr = 0x4300;
        let query_buf = 0x4400;
        let cb_ptr = 0x4500;
        engine
            .mem_write(cb_ptr, &16_u32.to_le_bytes())
            .expect("write buffer capacity");
        assert_eq!(
            run_query_value_w(
                &mut engine,
                &mut state,
                name_ptr,
                type_ptr,
                query_buf,
                cb_ptr
            ),
            0 // ERROR_SUCCESS
        );
        assert_eq!(read_u32_at(&mut engine, type_ptr), 4); // REG_DWORD
        assert_eq!(read_u32_at(&mut engine, query_buf), 1);
        assert_eq!(read_u32_at(&mut engine, cb_ptr), 4); // required size, not capacity
    }

    #[test]
    fn test_query_missing_value_returns_file_not_found() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        seed_notepad_key(&mut state);
        let name_ptr = 0x5000;
        write_utf16(&mut engine, name_ptr, "noSuchValue");
        let cb_ptr = 0x4500;
        engine
            .mem_write(cb_ptr, &8_u32.to_le_bytes())
            .expect("write cbData");
        let status = run_query_value_w(&mut engine, &mut state, name_ptr, 0, 0, cb_ptr);
        assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
        assert_eq!(read_u32_at(&mut engine, cb_ptr), 0); // real Windows zeroes *lpcbData
    }

    #[test]
    fn test_query_small_buffer_returns_more_data() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        seed_notepad_key(&mut state);
        let name_ptr = 0x5000;
        write_utf16(&mut engine, name_ptr, "szHeader");
        let data_ptr = 0x4200;
        let stored = b"&f\0";
        engine.mem_write(data_ptr, stored).expect("write sz value");
        assert_eq!(
            run_set_value_w(&mut engine, &mut state, name_ptr, 1, data_ptr, 3),
            0
        );
        // Buffer of 2 bytes < stored 3 bytes.
        let query_buf = 0x4400;
        let cb_ptr = 0x4500;
        engine
            .mem_write(cb_ptr, &2_u32.to_le_bytes())
            .expect("write small capacity");
        // Sentinel the guest buffer; real Windows must leave it untouched on
        // ERROR_MORE_DATA (it only reports the required size in *lpcbData).
        let sentinel = [0xAA, 0xBB, 0xCC, 0xDD];
        engine
            .mem_write(query_buf, &sentinel)
            .expect("write sentinel into query buffer");
        let status = run_query_value_w(&mut engine, &mut state, name_ptr, 0, query_buf, cb_ptr);
        assert_eq!(status, 234); // ERROR_MORE_DATA
        assert_eq!(read_u32_at(&mut engine, cb_ptr), 3); // required size
        assert_eq!(read_bytes_at(&mut engine, query_buf, 4), sentinel); // buffer untouched
    }

    #[test]
    fn test_query_size_probe_with_null_data_succeeds() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        seed_notepad_key(&mut state);
        let name_ptr = 0x5000;
        write_utf16(&mut engine, name_ptr, "szTrailer");
        let data_ptr = 0x4200;
        let stored = b"&t\0";
        engine.mem_write(data_ptr, stored).expect("write sz value");
        assert_eq!(
            run_set_value_w(&mut engine, &mut state, name_ptr, 1, data_ptr, 3),
            0
        );
        // lpData = NULL: sizing query returns success and the required size.
        let cb_ptr = 0x4500;
        engine
            .mem_write(cb_ptr, &0_u32.to_le_bytes())
            .expect("write cbData");
        let status = run_query_value_w(&mut engine, &mut state, name_ptr, 0, 0, cb_ptr);
        assert_eq!(status, 0); // ERROR_SUCCESS
        assert_eq!(read_u32_at(&mut engine, cb_ptr), 3);
    }

    #[test]
    fn test_set_query_ansi_variant_round_trip() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        seed_notepad_key(&mut state);
        let name_ptr = 0x5000;
        write_ansi(&mut engine, name_ptr, "searchString");
        let data_ptr = 0x4200;
        engine
            .mem_write(data_ptr, b"notepad\0")
            .expect("write ansi sz value");
        // RegSetValueExA(hKey, "searchString", 0, REG_SZ, data, 8)
        write_regs(&mut engine, NOTEPAD_KEY, name_ptr, 0, u64::from(1_u32));
        write_stack_args(&mut engine, data_ptr, 8);
        let r = handle_reg_set_value_ex_a(&mut HandlerContext::new(
            &mut engine,
            test_env(),
            &mut state,
        ))
        .expect("RegSetValueExA handler");
        assert_eq!(r.return_value, 0);

        // Query via W (value names are store-wide; encoding only affects the call).
        let wname_ptr = 0x5100;
        write_utf16(&mut engine, wname_ptr, "searchString");
        let type_ptr = 0x4300;
        let query_buf = 0x4400;
        let cb_ptr = 0x4500;
        engine
            .mem_write(cb_ptr, &64_u32.to_le_bytes())
            .expect("write capacity");
        let status = run_query_value_w(
            &mut engine,
            &mut state,
            wname_ptr,
            type_ptr,
            query_buf,
            cb_ptr,
        );
        assert_eq!(status, 0);
        assert_eq!(read_u32_at(&mut engine, type_ptr), 1); // REG_SZ
        assert_eq!(read_bytes_at(&mut engine, query_buf, 8), b"notepad\0");
        assert_eq!(read_u32_at(&mut engine, cb_ptr), 8); // includes the NUL
    }

    #[test]
    fn test_delete_value_then_missing() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        seed_notepad_key(&mut state);
        let name_ptr = 0x5000;
        write_utf16(&mut engine, name_ptr, "fWrap");
        let data_ptr = 0x4200;
        engine
            .mem_write(data_ptr, &1_u32.to_le_bytes())
            .expect("write dword value");
        assert_eq!(
            run_set_value_w(&mut engine, &mut state, name_ptr, 4, data_ptr, 4),
            0
        );

        // RegDeleteValueA takes an ANSI name; the set used a UTF-16 buffer.
        let ansi_name_ptr = 0x5200;
        write_ansi(&mut engine, ansi_name_ptr, "fWrap");
        write_regs(&mut engine, NOTEPAD_KEY, ansi_name_ptr, 0, 0);
        let r = handle_reg_delete_value_a(&mut HandlerContext::new(
            &mut engine,
            test_env(),
            &mut state,
        ))
        .expect("RegDeleteValueA handler");
        assert_eq!(r.return_value, 0); // ERROR_SUCCESS

        let r = handle_reg_delete_value_a(&mut HandlerContext::new(
            &mut engine,
            test_env(),
            &mut state,
        ))
        .expect("RegDeleteValueA handler");
        assert_eq!(r.return_value, 2); // ERROR_FILE_NOT_FOUND
    }

    #[test]
    fn test_unknown_key_returns_invalid_handle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        seed_notepad_key(&mut state);
        let name_ptr = 0x5000;
        write_utf16(&mut engine, name_ptr, "fWrap");
        let data_ptr = 0x4200;
        engine
            .mem_write(data_ptr, &1_u32.to_le_bytes())
            .expect("write dword value");
        // Handle 0x999 does not exist.
        write_regs(&mut engine, 0x999, name_ptr, 0, u64::from(4_u32));
        write_stack_args(&mut engine, data_ptr, 4);
        let r = handle_reg_set_value_ex_w(&mut HandlerContext::new(
            &mut engine,
            test_env(),
            &mut state,
        ))
        .expect("RegSetValueExW handler");
        assert_eq!(r.return_value, 6); // ERROR_INVALID_HANDLE
    }

    #[test]
    fn test_values_persist_across_sessions() {
        let root = std::env::temp_dir().join("wie_registry_test_advapi32");
        std::fs::remove_dir_all(&root).ok();
        let name_ptr = 0x5000;
        let data_ptr = 0x4200;

        // Session 1: set iWindowPosX = 300 and let go of the state.
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        state.file_io.bottle_root = Some(root.clone());
        seed_notepad_key(&mut state);
        write_utf16(&mut engine, name_ptr, "iWindowPosX");
        engine
            .mem_write(data_ptr, &300_u32.to_le_bytes())
            .expect("write dword value");
        assert_eq!(
            run_set_value_w(&mut engine, &mut state, name_ptr, 4, data_ptr, 4),
            0
        );
        drop(state);

        // Session 2 (fresh state, same bottle): the value comes back.
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        state.file_io.bottle_root = Some(root.clone());
        seed_notepad_key(&mut state);
        write_utf16(&mut engine, name_ptr, "iWindowPosX");
        let type_ptr = 0x4300;
        let query_buf = 0x4400;
        let cb_ptr = 0x4500;
        engine
            .mem_write(cb_ptr, &64_u32.to_le_bytes())
            .expect("write capacity");
        let status = run_query_value_w(
            &mut engine,
            &mut state,
            name_ptr,
            type_ptr,
            query_buf,
            cb_ptr,
        );
        assert_eq!(status, 0);
        assert_eq!(read_u32_at(&mut engine, type_ptr), 4);
        assert_eq!(read_u32_at(&mut engine, query_buf), 300);
        assert_eq!(read_u32_at(&mut engine, cb_ptr), 4);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_nested_key_handles_share_value_path() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // Two record shapes for the same logical key: opened in two steps
        // (Software, then Microsoft\Notepad) vs opened directly from HKCU.
        state.process.registry_keys.push(crate::RegistryKey {
            handle: 0x200,
            parent: HKEY_CURRENT_USER,
            subkey: "Software".into(),
        });
        state.process.registry_keys.push(crate::RegistryKey {
            handle: 0x201,
            parent: 0x200,
            subkey: "Microsoft\\Notepad".into(),
        });
        state.process.registry_keys.push(crate::RegistryKey {
            handle: 0x202,
            parent: HKEY_CURRENT_USER,
            subkey: "Software\\Microsoft\\Notepad".into(),
        });

        let name_ptr = 0x5000;
        let data_ptr = 0x4200;
        write_utf16(&mut engine, name_ptr, "fWrap");
        engine
            .mem_write(data_ptr, &1_u32.to_le_bytes())
            .expect("write dword value");
        // Set through the nested handle.
        write_regs(&mut engine, 0x201, name_ptr, 0, u64::from(4_u32));
        write_stack_args(&mut engine, data_ptr, 4);
        let r = handle_reg_set_value_ex_w(&mut HandlerContext::new(
            &mut engine,
            test_env(),
            &mut state,
        ))
        .expect("RegSetValueExW handler");
        assert_eq!(r.return_value, 0);

        // Query through the directly-opened handle: same path, same value.
        let type_ptr = 0x4300;
        let query_buf = 0x4400;
        let cb_ptr = 0x4500;
        engine
            .mem_write(cb_ptr, &64_u32.to_le_bytes())
            .expect("write capacity");
        write_regs(&mut engine, 0x202, name_ptr, 0, type_ptr);
        write_stack_args(&mut engine, query_buf, cb_ptr);
        let r = handle_reg_query_value_ex_w(&mut HandlerContext::new(
            &mut engine,
            test_env(),
            &mut state,
        ))
        .expect("RegQueryValueExW handler");
        assert_eq!(r.return_value, 0);
        assert_eq!(read_u32_at(&mut engine, query_buf), 1);
    }
}
