//! Advapi32 tests: SetSecurityDescriptorDacl, plus the registry hive (RegEnumKeyEx, RegEnumValue, RegOpenKeyA/W/Ex, RegCreateKeyExA/W).
use super::*;

// --- Advapi32 ---

#[test]
fn test_set_security_descriptor_dacl_null_fails() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    assert_return_value!(
        advapi32::handle_set_security_descriptor_dacl(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_set_security_descriptor_dacl_valid_succeeds() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x1000, 1, 0x2000, 1, 0);
    assert_return_value!(
        advapi32::handle_set_security_descriptor_dacl(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

// ── ADVAPI32 ──────────────────────────────────────────────────────

#[test]
fn test_reg_enum_key_ex() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Create a registry key with parent 0x7000_0001 (HKEY_CURRENT_USER)
    let parent = 0x7000_0001;
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent,
        subkey: "Software\\test".into(),
    });
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x101,
        parent: 0x100,
        subkey: "Nested".into(),
    });
    let name_buf = 0x4000;
    let name_len_va = 0x5000;
    let name_len: u32 = 32;
    engine.mem_write(name_len_va, &name_len.to_le_bytes()).ok();
    // RegEnumKeyExW(hKey=0x100, dwIndex=0, lpName=name_buf, lpcchName=name_len_va, ...)
    write_regs(&mut engine, 0x100, 0, name_buf, name_len_va, STACK_TOP);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        advapi32::dispatch_advapi32_extra(&mut ctx, "RegEnumKeyExW")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 0); // ERROR_SUCCESS
    let mut len_out = [0_u8; 4];
    engine.mem_read(name_len_va, &mut len_out).ok();
    assert_eq!(u32::from_le_bytes(len_out), 6); // "Nested" length
}

#[test]
fn test_reg_enum_value_returns_no_more() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x100, 0, 0x4000, 0x5000, STACK_TOP);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        advapi32::dispatch_advapi32_extra(&mut ctx, "RegEnumValueW")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 259); // ERROR_NO_MORE_ITEMS
}

/// The real `HKEY_CURRENT_USER` constant the guest bakes into its call site.
const HKEY_CURRENT_USER: u64 = 0x8000_0001;

/// Run `RegOpenKeyA/W` through the full dispatch path (names.rs → dense id → arm).
fn reg_open_key(library: &str, name: &str, state: &mut WinApiState, engine: &mut IcedCpu) -> u64 {
    let id = crate::resolve_winapi_id(library, name).expect("must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(engine, test_environment(), state),
        id,
    )
    .expect("RegOpenKey must dispatch");
    r.return_value
}

/// Read an 8-byte guest value (the `phkResult` handle output).
fn read_guest_handle(engine: &mut IcedCpu, addr: u64) -> u64 {
    let mut buf = [0_u8; 8];
    engine.mem_read(addr, &mut buf).expect("read guest handle");
    u64::from_le_bytes(buf)
}

#[test]
fn test_reg_open_key_w_opens_existing_key() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Microsoft\\Notepad".into(),
    });
    let subkey_va = 0x5000;
    let phk_va = 0x3000;
    write_guest_utf16(&mut engine, subkey_va, "Software\\Microsoft\\Notepad");
    // Sentinel: a failed open must not leave stale data behind.
    engine
        .mem_write(phk_va, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    // RegOpenKeyW(hKey=HKCU, lpSubKey=subkey_va, phkResult=phk_va)
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, phk_va, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyW", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_va), 0x100);
}

#[test]
fn test_reg_open_key_w_missing_returns_file_not_found() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let subkey_va = 0x5000;
    let phk_va = 0x3000;
    write_guest_utf16(&mut engine, subkey_va, "Software\\Microsoft\\Notepad");
    engine
        .mem_write(phk_va, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, phk_va, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyW", &mut state, &mut engine);
    assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, phk_va), 0);
    // Open-only: the missing key must not be materialized by the legacy pair.
    assert!(state.process.registry_keys.is_empty());
}

#[test]
fn test_reg_open_key_a_matches_w() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Microsoft\\Notepad".into(),
    });
    let subkey_va = 0x5000;
    let phk_va = 0x3000;
    write_guest_ansi(&mut engine, subkey_va, "Software\\Microsoft\\Notepad");
    engine
        .mem_write(phk_va, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    // RegOpenKeyA(hKey=HKCU, lpSubKey=subkey_va, phkResult=phk_va)
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, phk_va, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyA", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_va), 0x100);
    // ANSI missing path mirrors the W variant.
    let missing_va = 0x5000;
    let missing_phk = 0x3100;
    write_guest_ansi(&mut engine, missing_va, "Software\\Missing");
    engine
        .mem_write(missing_phk, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(
        &mut engine,
        HKEY_CURRENT_USER,
        missing_va,
        missing_phk,
        0,
        0,
    );
    let status = reg_open_key("advapi32.dll", "RegOpenKeyA", &mut state, &mut engine);
    assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, missing_phk), 0);
}

/// `RegOpenKeyExA` is open-only: a missing key must report
/// `ERROR_FILE_NOT_FOUND` and write 0 to `*phkResult` WITHOUT creating the
/// key — only `RegCreateKeyEx*` may materialize a key.
#[test]
fn test_reg_open_key_ex_a_missing_returns_file_not_found_and_does_not_create() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let subkey_va = 0x5000;
    let phk_va = 0x3000;
    write_guest_ansi(&mut engine, subkey_va, "Software\\Missing\\Key");
    // RegOpenKeyExA passes phkResult in the 5th stack slot: [rsp+0x30].
    engine
        .mem_write(STACK_TOP + 0x30, &u64::to_le_bytes(phk_va))
        .expect("write phkResult arg");
    engine
        .mem_write(phk_va, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    // RegOpenKeyExA(hKey=HKCU, lpSubKey=subkey_va, ulOptions=0, samDesired=0, phkResult=[rsp+0x30])
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyExA", &mut state, &mut engine);
    assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, phk_va), 0);
    // The key must not be materialized: a second open on the same path fails
    // identically (and again zeroes the output handle).
    assert!(state.process.registry_keys.is_empty());
    engine
        .mem_write(phk_va, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    // Re-arm the registers: the first dispatch clobbers them on return.
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyExA", &mut state, &mut engine);
    assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, phk_va), 0);
    assert!(state.process.registry_keys.is_empty());
}

/// The W variant (soft-dispatch path) must match the A variant's open-only
/// semantics: ERROR_FILE_NOT_FOUND, *phkResult = 0, no creation.
#[test]
fn test_reg_open_key_ex_w_missing_returns_file_not_found() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let subkey_va = 0x5000;
    let phk_va = 0x3000;
    write_guest_utf16(&mut engine, subkey_va, "Software\\Missing\\Key");
    engine
        .mem_write(STACK_TOP + 0x30, &u64::to_le_bytes(phk_va))
        .expect("write phkResult arg");
    engine
        .mem_write(phk_va, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, 0, 0, 0);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        advapi32::dispatch_advapi32_extra(&mut ctx, "RegOpenKeyExW")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_guest_handle(&mut engine, phk_va), 0);
    assert!(state.process.registry_keys.is_empty());
}

/// `RegOpenKeyExA` on an EXISTING key still returns the stored handle.
#[test]
fn test_reg_open_key_ex_a_opens_existing_key() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Microsoft\\Notepad".into(),
    });
    let subkey_va = 0x5000;
    let phk_va = 0x3000;
    write_guest_ansi(&mut engine, subkey_va, "Software\\Microsoft\\Notepad");
    engine
        .mem_write(STACK_TOP + 0x30, &u64::to_le_bytes(phk_va))
        .expect("write phkResult arg");
    engine
        .mem_write(phk_va, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyExA", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_va), 0x100);
}

/// `RegCreateKeyExA` is the ONLY entry point allowed to create: the same
/// missing path that failed to open is materialized here, and a subsequent
/// open then succeeds with the created handle.
#[test]
fn test_reg_create_key_ex_a_creates_missing_key() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Pre-existing key so the created handle is nonzero and distinguishable.
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Existing".into(),
    });
    state.process.next_registry_key_handle = crate::RegistryKeyHandle::from(0x101);
    let subkey_va = 0x5000;
    let phk_va = 0x3000;
    let disposition_va = 0x3100;
    write_guest_ansi(&mut engine, subkey_va, "Software\\Missing\\Key");
    // RegCreateKeyExA passes phkResult at [rsp+0x40] and lpdwDisposition at [rsp+0x48].
    engine
        .mem_write(STACK_TOP + 0x40, &u64::to_le_bytes(phk_va))
        .expect("write phkResult arg");
    engine
        .mem_write(STACK_TOP + 0x48, &u64::to_le_bytes(disposition_va))
        .expect("write lpdwDisposition arg");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegCreateKeyExA", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_va), 0x101);
    let mut disp = [0_u8; 4];
    engine
        .mem_read(disposition_va, &mut disp)
        .expect("read disposition");
    assert_eq!(u32::from_le_bytes(disp), 1); // REG_CREATED_NEW_KEY
    // The previously-missing path now opens with the created handle.
    let open_phk = 0x3200;
    engine
        .mem_write(STACK_TOP + 0x30, &u64::to_le_bytes(open_phk))
        .expect("write phkResult arg");
    engine
        .mem_write(open_phk, &0xDEAD_BEEF_u64.to_le_bytes())
        .expect("write sentinel phkResult");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, 0, 0, 0);
    let status = reg_open_key("advapi32.dll", "RegOpenKeyExA", &mut state, &mut engine);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, open_phk), 0x101);
}

/// W-variant parity for the create path (soft dispatch, what notepad uses).
#[test]
fn test_reg_create_key_ex_w_creates_missing_key() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.process.registry_keys.push(crate::RegistryKey {
        handle: 0x100,
        parent: HKEY_CURRENT_USER,
        subkey: "Software\\Existing".into(),
    });
    state.process.next_registry_key_handle = crate::RegistryKeyHandle::from(0x101);
    let subkey_va = 0x5000;
    let phk_va = 0x3000;
    let disposition_va = 0x3100;
    write_guest_utf16(&mut engine, subkey_va, "Software\\Missing\\Key");
    engine
        .mem_write(STACK_TOP + 0x40, &u64::to_le_bytes(phk_va))
        .expect("write phkResult arg");
    engine
        .mem_write(STACK_TOP + 0x48, &u64::to_le_bytes(disposition_va))
        .expect("write lpdwDisposition arg");
    write_regs(&mut engine, HKEY_CURRENT_USER, subkey_va, 0, 0, 0);
    let r = {
        let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
        advapi32::dispatch_advapi32_extra(&mut ctx, "RegCreateKeyExW")
    }
    .expect("dispatch")
    .expect("handled");
    assert_eq!(r.return_value, 0); // ERROR_SUCCESS
    assert_eq!(read_guest_handle(&mut engine, phk_va), 0x101);
    let mut disp = [0_u8; 4];
    engine
        .mem_read(disposition_va, &mut disp)
        .expect("read disposition");
    assert_eq!(u32::from_le_bytes(disp), 1); // REG_CREATED_NEW_KEY
}
