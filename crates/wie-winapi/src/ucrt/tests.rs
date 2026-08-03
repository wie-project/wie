//! Unit tests for the UCRT dispatch surface, driven through the string
//! dispatch path (`crate::dispatch_winapi`) with a real iced CPU engine.
//!
//! The engine/state harness mirrors `state/tests.rs`; these tests cover the
//! startup exports RNotepad needs (`_initialize_wide_environment` + friends)
//! plus the wide-string helpers it imports from the string API set.

#![allow(clippy::expect_used)]

use super::*;
use crate::{GuestHeap, HeapState, KernelState, ModuleState, ProcessState, WinApiState};
use ahash::HashMap;
use ahash::HashMapExt;
use std::sync::{Arc, Mutex};

use crate::sync_obj::SyncState;
use crate::vfs::VolumeConfig;
use crate::{
    FileHandle, FindFileHandle, GuestStdinMode, ModuleHandle, RegistryKeyHandle, ResourceHandle,
    ThreadState, WinApiEnvironment,
};
use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

const STACK_VA: u64 = 0x100_0000;
const STACK_SIZE: usize = 0x1_0000;
// STACK_VA + STACK_SIZE - 0x100 (leave room for a dummy return address).
const STACK_TOP: u64 = 0x100_FF00;

/// Minimal engine for handler unit tests: maps guest pages, the synthetic CRT
/// page, and a stack with a valid return address.
fn test_engine() -> IcedCpu {
    let mut cpu = IcedCpu::open_x86_64();
    cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
        .expect("map test memory");
    cpu.mem_map(CRT_GUEST_BASE, 0x1000, RwxPerms::ALL)
        .expect("map CRT page");
    cpu.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)
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
    cpu.write_rsp(STACK_TOP).ok();
}

/// Zero-heavy default state; none of the UCRT startup handlers touch it beyond
/// the guest heap used for wide-argv materialization.
fn test_state() -> WinApiState {
    let mut heap = GuestHeap::new(0x2000, 0x10000);
    heap.attach_guest_control(0x2000);
    WinApiState {
        heap_state: HeapState {
            heap,
            next_fls_index: 0,
            fls_slots: Vec::new(),
            guest_fls_table_va: 0,
        },
        file_io: crate::FileIoState {
            executable_file_size: 0,
            executable_file_bytes: Arc::new(Vec::new()),
            executable_file_cursor: 0,
            next_find_handle: FindFileHandle::from(0),
            find_handles: Vec::new(),
            host_file_mounts: Vec::new(),
            virtual_files: Vec::new(),
            open_files: HashMap::new(),
            next_file_handle: FileHandle::from(0),
            next_resource_handle: ResourceHandle::from(0),
            resources: Vec::new(),
            current_directory_wide: Vec::new(),
            bottle_root: None,
            volumes: VolumeConfig::default(),
            guest_file_data_next: 0,
            guest_io: None,
            stdin_bytes: Vec::new(),
            stdin_cursor: 0,
            stdin_mode: GuestStdinMode::InjectOnly,
            ucrt_files: HashMap::new(),
            ucrt_next_file_va: 0x0000_0000_6900_0000,
            cached_streams: HashMap::new(),
        },
        process: ProcessState {
            last_error: 0,
            next_registry_key_handle: RegistryKeyHandle::from(0),
            registry_keys: Vec::new(),
            main_module_file_name: String::new(),
            main_module_path: String::new(),
            main_module_host_dir: None,
            error_mode: 0,
            suspended_threads: HashMap::new(),
            environment: Vec::new(),
            main_module_dialogs: Vec::new(),
            main_module_menus: Vec::new(),
            main_module_strings: Vec::new(),
            main_module_accelerators: Vec::new(),
        },
        kernel: KernelState {
            threads: ThreadState::primary(),
            sync: SyncState::new(),
            seh_pending: HashMap::new(),
        },
        dll_states: crate::DllStateMap::new(),
        message_queue: Arc::new(Mutex::new(crate::present::MessageQueue::default())),
        module_state: ModuleState {
            loaded_modules: HashMap::new(),
            import_resolver: None,
            get_proc_address_cache: HashMap::new(),
            next_module_handle: ModuleHandle::from(crate::dll_loader::REAL_MODULE_HANDLE_BASE),
        },
    }
}

fn test_environment() -> WinApiEnvironment {
    WinApiEnvironment {
        image_base: 0,
        command_line_a_ptr: 0,
        command_line_w_ptr: 0,
        environment_strings_w_ptr: 0,
        module_file_name_a_ptr: 0,
        module_file_name_w_ptr: 0,
        process_heap_handle: 1,
    }
}

/// Call the full string-dispatch path for a UCRT library export.
fn dispatch(
    library: &str,
    name: &str,
    engine: &mut IcedCpu,
    state: &mut WinApiState,
) -> crate::WinApiHandlerResult {
    let mut ctx = HandlerContext::new(engine, test_environment(), state);
    crate::dispatch_winapi(&mut ctx, library, name).expect("UCRT export should dispatch")
}

/// Write an 8-byte-slot vararg list at `va` (Win64 va_list layout: the list
/// pointer addresses slot 0; every slot is one full register-width argument).
fn write_va_list(engine: &mut IcedCpu, va: u64, args: &[u64]) {
    let mut bytes = Vec::new();
    for arg in args {
        bytes.extend_from_slice(&arg.to_le_bytes());
    }
    engine.mem_write(va, &bytes).expect("write va list");
}

/// Write a NUL-terminated UTF-16 string at `va`.
fn write_wide(engine: &mut IcedCpu, va: u64, s: &str) {
    let mut bytes = Vec::new();
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(va, &bytes).expect("write wide string");
}

/// Read a NUL-terminated UTF-16 string from `va`.
fn read_wide(engine: &mut IcedCpu, va: u64) -> String {
    let mut out = Vec::new();
    for i in 0_u64..4096 {
        let mut b = [0_u8; 2];
        engine
            .mem_read(va.wrapping_add(i.wrapping_mul(2)), &mut b)
            .expect("read wide string");
        let unit = u16::from_le_bytes(b);
        if unit == 0 {
            break;
        }
        out.push(unit);
    }
    String::from_utf16_lossy(&out)
}

// --- CRT startup exports ---

#[test]
fn initialize_wide_environment_succeeds() {
    let mut engine = test_engine();
    let mut state = test_state();
    let r = dispatch(
        "api-ms-win-crt-runtime-l1-1-0.dll",
        "_initialize_wide_environment",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0);
}

#[test]
fn configure_wide_argv_succeeds() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_regs(&mut engine, 1, 0, 0, 0); // _ARGV_WIDE
    let r = dispatch(
        "api-ms-win-crt-runtime-l1-1-0.dll",
        "_configure_wide_argv",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0);
}

#[test]
fn fpreset_succeeds() {
    let mut engine = test_engine();
    let mut state = test_state();
    let r = dispatch(
        "api-ms-win-crt-runtime-l1-1-0.dll",
        "_fpreset",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0);
}

#[test]
fn p_wenviron_returns_slot_holding_null_table() {
    let mut engine = test_engine();
    let mut state = test_state();
    let r = dispatch(
        "api-ms-win-crt-environment-l1-1-0.dll",
        "__p__wenviron",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, WENVIRON_PTR_SLOT);
    let mut slot = [0_u8; 8];
    engine
        .mem_read(WENVIRON_PTR_SLOT, &mut slot)
        .expect("read slot");
    assert_eq!(u64::from_le_bytes(slot), 0, "wide environment is empty");
}

#[test]
fn p_wargv_materializes_wide_argv_from_narrow() {
    let mut engine = test_engine();
    let mut state = test_state();
    // Initialise the guest heap control block bump cursor (0x2000 was attached
    // as ctrl in `test_state`), otherwise `alloc_coherent` sees bump=0 < base.
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("write heap bump cursor");
    // Pre-fill the session-materialized narrow argv on the CRT page.
    engine
        .mem_write(ARGC_SLOT, &2_u32.to_le_bytes())
        .expect("write argc");
    engine
        .mem_write(NARROW_ARGV_TABLE, &(CRT_GUEST_BASE + 0x500).to_le_bytes())
        .expect("write argv0 ptr");
    engine
        .mem_write(
            NARROW_ARGV_TABLE + 8,
            &(CRT_GUEST_BASE + 0x510).to_le_bytes(),
        )
        .expect("write argv1 ptr");
    engine
        .mem_write(NARROW_ARGV_TABLE + 16, &0_u64.to_le_bytes())
        .expect("write null terminator");
    engine
        .mem_write(CRT_GUEST_BASE + 0x500, b"notepad.exe\0")
        .expect("write argv0");
    engine
        .mem_write(CRT_GUEST_BASE + 0x510, b"-x\0")
        .expect("write argv1");

    let r = dispatch(
        "api-ms-win-crt-runtime-l1-1-0.dll",
        "__p___wargv",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, WARGV_PTR_SLOT);
    // *slot → wchar_t** table.
    let mut slot = [0_u8; 8];
    engine
        .mem_read(WARGV_PTR_SLOT, &mut slot)
        .expect("read slot");
    let table = u64::from_le_bytes(slot);
    assert_ne!(table, 0, "wide argv table must be materialized");
    // Entries mirror the narrow argv, then a NULL terminator.
    let mut e0 = [0_u8; 8];
    engine.mem_read(table, &mut e0).expect("read entry0");
    assert_eq!(
        read_wide(&mut engine, u64::from_le_bytes(e0)),
        "notepad.exe"
    );
    let mut e1 = [0_u8; 8];
    engine.mem_read(table + 8, &mut e1).expect("read entry1");
    assert_eq!(read_wide(&mut engine, u64::from_le_bytes(e1)), "-x");
    let mut e2 = [0_u8; 8];
    engine.mem_read(table + 16, &mut e2).expect("read entry2");
    assert_eq!(
        u64::from_le_bytes(e2),
        0,
        "wide argv table is NULL-terminated"
    );
}

// --- Wide string helpers ---

#[test]
fn wcslen_counts_wide_units() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "ab\u{2713}");
    write_regs(&mut engine, 0x3000, 0, 0, 0);
    let r = dispatch(
        "api-ms-win-crt-string-l1-1-0.dll",
        "wcslen",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 3);
}

#[test]
fn wcscat_appends_source() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "ab");
    write_wide(&mut engine, 0x4000, "cd");
    write_regs(&mut engine, 0x3000, 0x4000, 0, 0);
    let r = dispatch(
        "api-ms-win-crt-string-l1-1-0.dll",
        "wcscat",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0x3000);
    assert_eq!(read_wide(&mut engine, 0x3000), "abcd");
}

#[test]
fn wcscpy_copies_including_terminator() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x4000, "xy");
    write_wide(&mut engine, 0x3000, "zzz");
    write_regs(&mut engine, 0x3000, 0x4000, 0, 0);
    let r = dispatch(
        "api-ms-win-crt-string-l1-1-0.dll",
        "wcscpy",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0x3000);
    assert_eq!(read_wide(&mut engine, 0x3000), "xy");
}

#[test]
fn wcsncmp_respects_count() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "abc");
    write_wide(&mut engine, 0x4000, "abd");
    write_regs(&mut engine, 0x3000, 0x4000, 2, 0);
    let r = dispatch(
        "api-ms-win-crt-string-l1-1-0.dll",
        "wcsncmp",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0);
    write_regs(&mut engine, 0x3000, 0x4000, 3, 0);
    let r = dispatch(
        "api-ms-win-crt-string-l1-1-0.dll",
        "wcsncmp",
        &mut engine,
        &mut state,
    );
    assert_ne!(r.return_value, 0);
}

#[test]
fn wcsncpy_pads_short_source() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x4000, "ab");
    // dest: four 'Z' wide chars + terminator.
    let mut buf = Vec::new();
    for _ in 0..4 {
        buf.extend_from_slice(&0x5A_u16.to_le_bytes());
    }
    buf.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(0x3000, &buf).expect("write dest");
    write_regs(&mut engine, 0x3000, 0x4000, 4, 0);
    let r = dispatch(
        "api-ms-win-crt-string-l1-1-0.dll",
        "wcsncpy",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0x3000);
    let mut out = [0_u8; 10];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    // "ab" then NUL padding for the remaining 2 of 4 units; the 5th unit is
    // the untouched NUL terminator.
    assert_eq!(out, [0x61, 0, 0x62, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn wcsnicmp_is_case_insensitive() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "ABC");
    write_wide(&mut engine, 0x4000, "abc");
    write_regs(&mut engine, 0x3000, 0x4000, 3, 0);
    let r = dispatch(
        "api-ms-win-crt-string-l1-1-0.dll",
        "_wcsnicmp",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0);
    // Difference beyond the compared count is ignored.
    write_wide(&mut engine, 0x4000, "ABd");
    write_regs(&mut engine, 0x3000, 0x4000, 2, 0);
    let r = dispatch(
        "api-ms-win-crt-string-l1-1-0.dll",
        "_wcsnicmp",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0);
}

#[test]
fn towupper_uppercases_wide_char() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_regs(&mut engine, u64::from(0x0061_u16), 0, 0, 0); // 'a'
    let r = dispatch(
        "api-ms-win-crt-string-l1-1-0.dll",
        "towupper",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, u64::from(0x0041_u16)); // 'A'
}

#[test]
fn wcsrchr_finds_last_occurrence() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "abab");
    write_regs(&mut engine, 0x3000, u64::from(b'a'), 0, 0);
    let r = dispatch(
        "api-ms-win-crt-private-l1-1-0.dll",
        "wcsrchr",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0x3000 + 4, "third wide unit");
    // Missing char → NULL.
    write_regs(&mut engine, 0x3000, u64::from(b'z'), 0, 0);
    let r = dispatch(
        "api-ms-win-crt-private-l1-1-0.dll",
        "wcsrchr",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0);
    // The NUL terminator itself is findable.
    write_regs(&mut engine, 0x3000, 0, 0, 0);
    let r = dispatch(
        "api-ms-win-crt-private-l1-1-0.dll",
        "wcsrchr",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, 0x3000 + 8);
}

// --- _vsnwprintf / _vsnprintf (bounded printf-family format engine) ---

#[test]
fn vsnwprintf_formats_wide_args() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "%d %s");
    write_wide(&mut engine, 0x6000, "hi");
    write_va_list(&mut engine, 0x5000, &[42, 0x6000]);
    write_regs(&mut engine, 0x4000, 64, 0x3000, 0x5000);
    let r = dispatch("msvcrt.dll", "_vsnwprintf", &mut engine, &mut state);
    assert_eq!(r.return_value, 5);
    assert_eq!(read_wide(&mut engine, 0x4000), "42 hi");
}

#[test]
fn vsnwprintf_truncates_with_minus_one() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "%d %s");
    write_wide(&mut engine, 0x6000, "hi");
    write_va_list(&mut engine, 0x5000, &[42, 0x6000]);
    // count = 3: "42 hi" needs 6 wide units including the NUL, so only the
    // first count-1 = 2 units ("42") are written, NUL-terminated, and -1 is
    // returned (legacy `_vsn*` truncation semantics).
    write_regs(&mut engine, 0x4000, 3, 0x3000, 0x5000);
    let r = dispatch("msvcrt.dll", "_vsnwprintf", &mut engine, &mut state);
    assert_eq!(r.return_value, u64::MAX, "-1 as int must sign-extend");
    assert_eq!(read_wide(&mut engine, 0x4000), "42");
}

#[test]
fn vsnwprintf_zero_count_returns_minus_one() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "%d");
    write_va_list(&mut engine, 0x5000, &[42]);
    write_regs(&mut engine, 0x4000, 0, 0x3000, 0x5000);
    let r = dispatch("msvcrt.dll", "_vsnwprintf", &mut engine, &mut state);
    assert_eq!(r.return_value, u64::MAX);
}

#[test]
fn vsnprintf_formats_narrow_args() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x3000, b"%d:%s\0").expect("write format");
    engine.mem_write(0x6000, b"abc\0").expect("write string");
    write_va_list(&mut engine, 0x5000, &[7, 0x6000]);
    write_regs(&mut engine, 0x4000, 64, 0x3000, 0x5000);
    let r = dispatch("msvcrt.dll", "_vsnprintf", &mut engine, &mut state);
    assert_eq!(r.return_value, 5);
    let mut out = [0_u8; 8];
    engine.mem_read(0x4000, &mut out).expect("read buffer");
    assert_eq!(&out[..6], b"7:abc\0");
}

#[test]
fn vsnwprintf_supports_hex_unsigned_pointer_escape() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "%x %u %p %%");
    write_va_list(&mut engine, 0x5000, &[0x1a2b, 4294967295, 0x1234]);
    write_regs(&mut engine, 0x4000, 64, 0x3000, 0x5000);
    let r = dispatch("msvcrt.dll", "_vsnwprintf", &mut engine, &mut state);
    assert_eq!(read_wide(&mut engine, 0x4000), "1a2b 4294967295 0x1234 %");
    assert_eq!(r.return_value, 24);
}

#[test]
fn vsnprintf_applies_width_and_precision() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine
        .mem_write(0x3000, b"%5d|%.3u|%-4s|%.2s\0")
        .expect("write format");
    engine.mem_write(0x6000, b"abcd\0").expect("write string");
    write_va_list(&mut engine, 0x5000, &[42, 7, 0x6000, 0x6000]);
    write_regs(&mut engine, 0x4000, 64, 0x3000, 0x5000);
    let r = dispatch("msvcrt.dll", "_vsnprintf", &mut engine, &mut state);
    assert_eq!(r.return_value, 17);
    let mut out = [0_u8; 24];
    engine.mem_read(0x4000, &mut out).expect("read buffer");
    assert_eq!(&out[..18], b"   42|007|abcd|ab\0");
}
