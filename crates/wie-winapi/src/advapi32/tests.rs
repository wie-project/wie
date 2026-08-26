use super::*;
use crate::registry::HKEY_CURRENT_USER;
use crate::state::{
    DllStateMap, FileIoState, HeapState, KernelState, ModuleState, ProcessState, WinApiEnvironment,
};
use crate::sync_obj::SyncState;
use crate::vfs::VolumeConfig;
use crate::{HandlerContext, RegistryKeyHandle, ThreadState, WinApiState};
use wie_cpu::{CpuEngine, IcedCpu};

const STACK_VA: u64 = 0x100_0000;
const STACK_SIZE: usize = 0x1_0000;
// STACK_VA + STACK_SIZE - 0x100 (leave room for a dummy return address).
const STACK_TOP: u64 = 0x100_FF00;
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
        display: crate::DisplayMetrics::default(),
        heap_state: HeapState {
            heap: std::sync::Arc::new(std::sync::Mutex::new(crate::guest_heap::GuestHeap::new(
                0x2000, 0x10000,
            ))),
            next_fls_index: 0,
            fls_slots: Vec::new(),
            guest_fls_table_va: 0,
        },
        file_io: FileIoState {
            executable_file_size: 0,
            executable_file_bytes: std::sync::Arc::new(Vec::new()),
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
        message_queue: std::sync::Arc::new(std::sync::Mutex::new(
            crate::present::MessageQueue::default(),
        )),
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
    name_va: u64,
    value_type: u32,
    data_va: u64,
    cb_data: u32,
) -> u64 {
    write_regs(engine, NOTEPAD_KEY, name_va, 0, u64::from(value_type));
    write_stack_args(engine, data_va, u64::from(cb_data));
    let r = handle_reg_set_value_ex_w(&mut HandlerContext::new(engine, test_env(), state))
        .expect("RegSetValueExW handler");
    r.return_value
}

fn run_query_value_w(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    name_va: u64,
    type_va: u64,
    data_va: u64,
    cb_va: u64,
) -> u64 {
    write_regs(engine, NOTEPAD_KEY, name_va, 0, type_va);
    write_stack_args(engine, data_va, cb_va);
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
    let name_va = 0x5000;
    write_utf16(&mut engine, name_va, "fWrap");
    let data_va = 0x4200;
    engine
        .mem_write(data_va, &1_u32.to_le_bytes())
        .expect("write dword value");
    assert_eq!(
        run_set_value_w(&mut engine, &mut state, name_va, 4, data_va, 4),
        0
    );

    // Query with a 16-byte buffer: expect type + data + exact size back.
    let type_va = 0x4300;
    let query_buf = 0x4400;
    let cb_va = 0x4500;
    engine
        .mem_write(cb_va, &16_u32.to_le_bytes())
        .expect("write buffer capacity");
    assert_eq!(
        run_query_value_w(&mut engine, &mut state, name_va, type_va, query_buf, cb_va),
        0 // ERROR_SUCCESS
    );
    assert_eq!(read_u32_at(&mut engine, type_va), 4); // REG_DWORD
    assert_eq!(read_u32_at(&mut engine, query_buf), 1);
    assert_eq!(read_u32_at(&mut engine, cb_va), 4); // required size, not capacity
}

#[test]
fn test_query_missing_value_returns_file_not_found() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    seed_notepad_key(&mut state);
    let name_va = 0x5000;
    write_utf16(&mut engine, name_va, "noSuchValue");
    let cb_va = 0x4500;
    engine
        .mem_write(cb_va, &8_u32.to_le_bytes())
        .expect("write cbData");
    let status = run_query_value_w(&mut engine, &mut state, name_va, 0, 0, cb_va);
    assert_eq!(status, 2); // ERROR_FILE_NOT_FOUND
    assert_eq!(read_u32_at(&mut engine, cb_va), 0); // real Windows zeroes *lpcbData
}

#[test]
fn test_query_small_buffer_returns_more_data() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    seed_notepad_key(&mut state);
    let name_va = 0x5000;
    write_utf16(&mut engine, name_va, "szHeader");
    let data_va = 0x4200;
    let stored = b"&f\0";
    engine.mem_write(data_va, stored).expect("write sz value");
    assert_eq!(
        run_set_value_w(&mut engine, &mut state, name_va, 1, data_va, 3),
        0
    );
    // Buffer of 2 bytes < stored 3 bytes.
    let query_buf = 0x4400;
    let cb_va = 0x4500;
    engine
        .mem_write(cb_va, &2_u32.to_le_bytes())
        .expect("write small capacity");
    // Sentinel the guest buffer; real Windows must leave it untouched on
    // ERROR_MORE_DATA (it only reports the required size in *lpcbData).
    let sentinel = [0xAA, 0xBB, 0xCC, 0xDD];
    engine
        .mem_write(query_buf, &sentinel)
        .expect("write sentinel into query buffer");
    let status = run_query_value_w(&mut engine, &mut state, name_va, 0, query_buf, cb_va);
    assert_eq!(status, 234); // ERROR_MORE_DATA
    assert_eq!(read_u32_at(&mut engine, cb_va), 3); // required size
    assert_eq!(read_bytes_at(&mut engine, query_buf, 4), sentinel); // buffer untouched
}

#[test]
fn test_query_size_probe_with_null_data_succeeds() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    seed_notepad_key(&mut state);
    let name_va = 0x5000;
    write_utf16(&mut engine, name_va, "szTrailer");
    let data_va = 0x4200;
    let stored = b"&t\0";
    engine.mem_write(data_va, stored).expect("write sz value");
    assert_eq!(
        run_set_value_w(&mut engine, &mut state, name_va, 1, data_va, 3),
        0
    );
    // lpData = NULL: sizing query returns success and the required size.
    let cb_va = 0x4500;
    engine
        .mem_write(cb_va, &0_u32.to_le_bytes())
        .expect("write cbData");
    let status = run_query_value_w(&mut engine, &mut state, name_va, 0, 0, cb_va);
    assert_eq!(status, 0); // ERROR_SUCCESS
    assert_eq!(read_u32_at(&mut engine, cb_va), 3);
}

#[test]
fn test_set_query_ansi_variant_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    seed_notepad_key(&mut state);
    let name_va = 0x5000;
    write_ansi(&mut engine, name_va, "searchString");
    let data_va = 0x4200;
    engine
        .mem_write(data_va, b"notepad\0")
        .expect("write ansi sz value");
    // RegSetValueExA(hKey, "searchString", 0, REG_SZ, data, 8)
    write_regs(&mut engine, NOTEPAD_KEY, name_va, 0, u64::from(1_u32));
    write_stack_args(&mut engine, data_va, 8);
    let r = handle_reg_set_value_ex_a(&mut HandlerContext::new(
        &mut engine,
        test_env(),
        &mut state,
    ))
    .expect("RegSetValueExA handler");
    assert_eq!(r.return_value, 0);

    // Query via W (value names are store-wide; encoding only affects the call).
    let wname_va = 0x5100;
    write_utf16(&mut engine, wname_va, "searchString");
    let type_va = 0x4300;
    let query_buf = 0x4400;
    let cb_va = 0x4500;
    engine
        .mem_write(cb_va, &64_u32.to_le_bytes())
        .expect("write capacity");
    let status = run_query_value_w(&mut engine, &mut state, wname_va, type_va, query_buf, cb_va);
    assert_eq!(status, 0);
    assert_eq!(read_u32_at(&mut engine, type_va), 1); // REG_SZ
    assert_eq!(read_bytes_at(&mut engine, query_buf, 8), b"notepad\0");
    assert_eq!(read_u32_at(&mut engine, cb_va), 8); // includes the NUL
}

#[test]
fn test_delete_value_then_missing() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    seed_notepad_key(&mut state);
    let name_va = 0x5000;
    write_utf16(&mut engine, name_va, "fWrap");
    let data_va = 0x4200;
    engine
        .mem_write(data_va, &1_u32.to_le_bytes())
        .expect("write dword value");
    assert_eq!(
        run_set_value_w(&mut engine, &mut state, name_va, 4, data_va, 4),
        0
    );

    // RegDeleteValueA takes an ANSI name; the set used a UTF-16 buffer.
    let ansi_name_va = 0x5200;
    write_ansi(&mut engine, ansi_name_va, "fWrap");
    write_regs(&mut engine, NOTEPAD_KEY, ansi_name_va, 0, 0);
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
    let name_va = 0x5000;
    write_utf16(&mut engine, name_va, "fWrap");
    let data_va = 0x4200;
    engine
        .mem_write(data_va, &1_u32.to_le_bytes())
        .expect("write dword value");
    // Handle 0x999 does not exist.
    write_regs(&mut engine, 0x999, name_va, 0, u64::from(4_u32));
    write_stack_args(&mut engine, data_va, 4);
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
    let name_va = 0x5000;
    let data_va = 0x4200;

    // Session 1: set iWindowPosX = 300 and let go of the state.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.file_io.bottle_root = Some(root.clone());
    seed_notepad_key(&mut state);
    write_utf16(&mut engine, name_va, "iWindowPosX");
    engine
        .mem_write(data_va, &300_u32.to_le_bytes())
        .expect("write dword value");
    assert_eq!(
        run_set_value_w(&mut engine, &mut state, name_va, 4, data_va, 4),
        0
    );
    drop(state);

    // Session 2 (fresh state, same bottle): the value comes back.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.file_io.bottle_root = Some(root.clone());
    seed_notepad_key(&mut state);
    write_utf16(&mut engine, name_va, "iWindowPosX");
    let type_va = 0x4300;
    let query_buf = 0x4400;
    let cb_va = 0x4500;
    engine
        .mem_write(cb_va, &64_u32.to_le_bytes())
        .expect("write capacity");
    let status = run_query_value_w(&mut engine, &mut state, name_va, type_va, query_buf, cb_va);
    assert_eq!(status, 0);
    assert_eq!(read_u32_at(&mut engine, type_va), 4);
    assert_eq!(read_u32_at(&mut engine, query_buf), 300);
    assert_eq!(read_u32_at(&mut engine, cb_va), 4);

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

    let name_va = 0x5000;
    let data_va = 0x4200;
    write_utf16(&mut engine, name_va, "fWrap");
    engine
        .mem_write(data_va, &1_u32.to_le_bytes())
        .expect("write dword value");
    // Set through the nested handle.
    write_regs(&mut engine, 0x201, name_va, 0, u64::from(4_u32));
    write_stack_args(&mut engine, data_va, 4);
    let r = handle_reg_set_value_ex_w(&mut HandlerContext::new(
        &mut engine,
        test_env(),
        &mut state,
    ))
    .expect("RegSetValueExW handler");
    assert_eq!(r.return_value, 0);

    // Query through the directly-opened handle: same path, same value.
    let type_va = 0x4300;
    let query_buf = 0x4400;
    let cb_va = 0x4500;
    engine
        .mem_write(cb_va, &64_u32.to_le_bytes())
        .expect("write capacity");
    write_regs(&mut engine, 0x202, name_va, 0, type_va);
    write_stack_args(&mut engine, query_buf, cb_va);
    let r = handle_reg_query_value_ex_w(&mut HandlerContext::new(
        &mut engine,
        test_env(),
        &mut state,
    ))
    .expect("RegQueryValueExW handler");
    assert_eq!(r.return_value, 0);
    assert_eq!(read_u32_at(&mut engine, query_buf), 1);
}
