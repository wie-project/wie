//! Unit tests for the UCRT dispatch surface, driven through the string
//! dispatch path (`crate::dispatch_winapi`) with a real iced CPU engine.
//!
//! The engine/state harness mirrors `state/tests.rs`; these tests cover the
//! startup exports RNotepad needs (`_initialize_wide_environment` + friends)
//! plus the wide-string helpers it imports from the string API set.

#![allow(clippy::expect_used)]

use super::*;
use crate::{HeapState, KernelState, ModuleState, ProcessState, WinApiState};
use ahash::HashMap;
use ahash::HashMapExt;

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
    let heap = std::sync::Arc::new(std::sync::Mutex::new(crate::guest_heap::GuestHeap::new(
        0x2000, 0x10000,
    )));
    heap.lock()
        .unwrap_or_else(|e| e.into_inner())
        .attach_guest_control(0x2000);
    WinApiState {
        display: crate::DisplayMetrics::default(),
        heap_state: HeapState {
            heap,
            next_fls_index: 0,
            fls_slots: Vec::new(),
            guest_fls_table_va: 0,
        },
        file_io: crate::FileIoState {
            executable_file_size: 0,
            executable_file_bytes: std::sync::Arc::new(Vec::new()),
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
        message_queue: std::sync::Arc::new(std::sync::Mutex::new(
            crate::present::MessageQueue::default(),
        )),
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
fn p_wenviron_materializes_wide_env_from_host() {
    let mut engine = test_engine();
    let mut state = test_state();
    // Initialise the guest heap control block bump cursor (0x2000 was
    // attached as ctrl in `test_state`), otherwise `alloc_coherent` sees
    // bump=0 < base.
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("write heap bump cursor");

    let r = dispatch(
        "api-ms-win-crt-environment-l1-1-0.dll",
        "__p__wenviron",
        &mut engine,
        &mut state,
    );
    assert_eq!(r.return_value, WENVIRON_PTR_SLOT);
    // *slot → wchar_t** table materialized from the host env.
    let mut slot = [0_u8; 8];
    engine
        .mem_read(WENVIRON_PTR_SLOT, &mut slot)
        .expect("read slot");
    let table = u64::from_le_bytes(slot);
    assert_ne!(table, 0, "wide env table must be materialized");

    // Walk the NULL-terminated table; every entry must parse as KEY=VALUE.
    let mut entries: Vec<String> = Vec::new();
    for i in 0_u64..4096 {
        let mut entry_va = [0_u8; 8];
        engine
            .mem_read(table.wrapping_add(i.wrapping_mul(8)), &mut entry_va)
            .expect("read entry pointer");
        let ptr = u64::from_le_bytes(entry_va);
        if ptr == 0 {
            break; // NULL terminator ends the table.
        }
        let entry = read_wide(&mut engine, ptr);
        let (key, _value) = entry
            .split_once('=')
            .expect("env entry must parse as KEY=VALUE");
        assert!(!key.is_empty(), "env key must be non-empty");
        entries.push(entry);
    }
    assert!(
        !entries.is_empty(),
        "host env must yield at least one variable"
    );
    // The materialized keys must mirror the host environment exactly.
    let host_keys: std::collections::HashSet<String> =
        std::env::vars().map(|(key, _)| key).collect();
    let guest_keys: std::collections::HashSet<&str> = entries
        .iter()
        .map(|entry| entry.split_once('=').map_or("", |(key, _)| key))
        .collect();
    let expected: std::collections::HashSet<&str> = host_keys.iter().map(String::as_str).collect();
    assert_eq!(
        guest_keys, expected,
        "wide env keys must mirror the host env"
    );
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

#[test]
fn vsnwprintf_d_truncates_to_low_32_bits() {
    // RNotepad's StringCchPrintfW builds its va_list over its own stack
    // frame, where the guest stores 32-bit varargs (`mov [rsp+0x20], eax`)
    // and the high slot bytes stay stale. `%d` must read only the low
    // 32 bits — 0x1CD_0000_0001 is the REAL trace payload for a stored
    // column value of 1 (the high 0x1CD leaked from a prior frame and made
    // the status bar print "column 1979979923457").
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "%d");
    write_va_list(&mut engine, 0x5000, &[0x1CD_0000_0001]);
    write_regs(&mut engine, 0x4000, 64, 0x3000, 0x5000);
    let r = dispatch("msvcrt.dll", "_vsnwprintf", &mut engine, &mut state);
    assert_eq!(read_wide(&mut engine, 0x4000), "1");
    assert_eq!(r.return_value, 1);
}

#[test]
fn vsnwprintf_negative_d_sign_extends_low_32_bits() {
    // A 32-bit negative int with stale high bytes: %d must print -101, not
    // the full-slot reinterpretation.
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "%d");
    write_va_list(&mut engine, 0x5000, &[0x1_FFFF_FF9B]);
    write_regs(&mut engine, 0x4000, 64, 0x3000, 0x5000);
    let r = dispatch("msvcrt.dll", "_vsnwprintf", &mut engine, &mut state);
    assert_eq!(read_wide(&mut engine, 0x4000), "-101");
    assert_eq!(r.return_value, 4);
}

#[test]
fn vsnwprintf_lld_reads_the_full_64_bit_slot() {
    // The `ll` modifier demands the whole slot — the low-32 truncation must
    // not apply to it.
    let mut engine = test_engine();
    let mut state = test_state();
    write_wide(&mut engine, 0x3000, "%lld");
    write_va_list(&mut engine, 0x5000, &[0x1_0000_0002]);
    write_regs(&mut engine, 0x4000, 64, 0x3000, 0x5000);
    let r = dispatch("msvcrt.dll", "_vsnwprintf", &mut engine, &mut state);
    assert_eq!(read_wide(&mut engine, 0x4000), "4294967298");
    assert_eq!(r.return_value, 10);
}

/// `iswctype(wc, mask)` — the wide ctype dispatch RNotepad's whole-word Find
/// depends on (`_istalnum` → `iswalnum` → `iswctype(c, _ALPHA|_DIGIT)`). The
/// handler must classify per the MS CRT `corecrt_wctype.h` mask bits, or the
/// session stops with "unsupported UCRT export: iswctype" the moment the
/// user enables "Match whole word" and clicks Find Next.
#[test]
fn iswctype_classifies_against_the_crt_mask() {
    let alnum = 0x107; // _UPPER|_LOWER|_DIGIT|_ALPHA
    let alpha = 0x103; // _ALPHA = 0x100|_UPPER|_LOWER
    let upper = 0x001;
    let lower = 0x002;
    let digit = 0x004;
    let space = 0x008;
    let punct = 0x010;
    let control = 0x020;
    let blank = 0x040;
    let hex = 0x080;

    let expect = |engine: &mut IcedCpu, state: &mut WinApiState, wc: u32, mask: u32, want: u64| {
        write_regs(engine, u64::from(wc), u64::from(mask), 0, 0);
        let r = dispatch("msvcrt.dll", "iswctype", engine, state);
        assert_eq!(r.return_value, want, "iswctype(0x{wc:x}, 0x{mask:x})");
    };

    let mut engine = test_engine();
    let mut state = test_state();
    // Whole-word search classifies the neighbours of a match.
    expect(&mut engine, &mut state, u32::from(' '), alnum, 0);
    expect(&mut engine, &mut state, u32::from('_'), alnum, 0);
    expect(&mut engine, &mut state, u32::from('a'), alnum, 1);
    expect(&mut engine, &mut state, u32::from('Z'), alnum, 1);
    expect(&mut engine, &mut state, u32::from('0'), alnum, 1);
    // Caseless letter: ALPHA yes, UPPER/LOWER no.
    expect(&mut engine, &mut state, 0x4E2D, alpha, 1); // 中
    expect(&mut engine, &mut state, 0x4E2D, upper, 0);
    expect(&mut engine, &mut state, 0x4E2D, lower, 0);
    // Case-sensitive masks distinguish upper/lower.
    expect(&mut engine, &mut state, u32::from('A'), upper, 1);
    expect(&mut engine, &mut state, u32::from('A'), lower, 0);
    expect(&mut engine, &mut state, u32::from('a'), lower, 1);
    expect(&mut engine, &mut state, u32::from('a'), upper, 0);
    // Digit/hex: 'f' is a hex digit but not a decimal digit.
    expect(&mut engine, &mut state, u32::from('7'), digit, 1);
    expect(&mut engine, &mut state, u32::from('7'), hex, 1);
    expect(&mut engine, &mut state, u32::from('f'), digit, 0);
    expect(&mut engine, &mut state, u32::from('f'), hex, 1);
    // Whitespace/blank/control.
    expect(&mut engine, &mut state, u32::from(' '), space, 1);
    expect(&mut engine, &mut state, u32::from('\t'), blank, 1);
    expect(&mut engine, &mut state, u32::from('\n'), space, 1);
    expect(&mut engine, &mut state, u32::from('\n'), blank, 0);
    expect(&mut engine, &mut state, u32::from('\n'), control, 1);
    expect(&mut engine, &mut state, 0x7F, control, 1);
    // Punctuation.
    expect(&mut engine, &mut state, u32::from('.'), punct, 1);
    expect(&mut engine, &mut state, u32::from('.'), alnum, 0);
    // WEOF never matches.
    expect(&mut engine, &mut state, 0xFFFF, alnum, 0);
    expect(&mut engine, &mut state, 0xFFFF, control, 0);
    // Surrogate (not a valid char) matches nothing.
    expect(&mut engine, &mut state, 0xD800, alnum, 0);
}

// --- Secure-CRT `_s` variants (MSVCR100+) ---

#[test]
fn memcpy_s_copies_within_bounds() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x4000, b"hello\0").expect("write src");
    write_regs(&mut engine, 0x3000, 16, 0x4000, 6);
    let r = dispatch("msvcrt.dll", "memcpy_s", &mut engine, &mut state);
    assert_eq!(r.return_value, 0);
    let mut out = [0_u8; 8];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    assert_eq!(&out[..6], b"hello\0");
}

#[test]
fn memcpy_s_oversized_count_zeroes_dest_and_returns_erange() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x4000, b"hello\0").expect("write src");
    // destsz=4 < count=6 → ERANGE (34), dest zeroed per the secure contract.
    write_regs(&mut engine, 0x3000, 4, 0x4000, 6);
    let r = dispatch("msvcrt.dll", "memcpy_s", &mut engine, &mut state);
    assert_eq!(r.return_value, i32_status_to_u64(34));
    let mut out = [0xff_u8; 8];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    assert_eq!(&out[..4], &[0, 0, 0, 0]);
}

#[test]
fn memcpy_s_null_dest_returns_einval() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x4000, b"x\0").expect("write src");
    write_regs(&mut engine, 0, 8, 0x4000, 1);
    let r = dispatch("msvcrt.dll", "memcpy_s", &mut engine, &mut state);
    assert_eq!(r.return_value, i32_status_to_u64(22));
}

#[test]
fn memset_s_fills_within_bounds() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_regs(&mut engine, 0x3000, 8, u64::from(b'x'), 4);
    let r = dispatch("msvcrt.dll", "memset_s", &mut engine, &mut state);
    assert_eq!(r.return_value, 0);
    let mut out = [0_u8; 6];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    assert_eq!(&out[..4], b"xxxx");
}

#[test]
fn strcpy_s_copies_and_nul_terminates() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x4000, b"hi\0").expect("write src");
    write_regs(&mut engine, 0x3000, 16, 0x4000, 0);
    let r = dispatch("msvcrt.dll", "strcpy_s", &mut engine, &mut state);
    assert_eq!(r.return_value, 0);
    let mut out = [0_u8; 4];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    assert_eq!(&out[..3], b"hi\0");
}

#[test]
fn strcpy_s_truncation_empties_dest() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x4000, b"hello\0").expect("write src");
    // destsz=3: "hello" + NUL needs 6 bytes → ERANGE, dest[0]=0.
    write_regs(&mut engine, 0x3000, 3, 0x4000, 0);
    let r = dispatch("msvcrt.dll", "strcpy_s", &mut engine, &mut state);
    assert_eq!(r.return_value, i32_status_to_u64(34));
    let mut out = [0xff_u8; 4];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    assert_eq!(out[0], 0);
}

#[test]
fn strncpy_s_pads_short_source() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x4000, b"ab\0").expect("write src");
    // count=6, src shorter → "ab" + NUL padded to 6.
    write_regs(&mut engine, 0x3000, 8, 0x4000, 6);
    let r = dispatch("msvcrt.dll", "strncpy_s", &mut engine, &mut state);
    assert_eq!(r.return_value, 0);
    let mut out = [0xff_u8; 8];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    assert_eq!(&out[..6], b"ab\0\0\0\0");
}

#[test]
fn strcat_s_appends_within_bounds() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x3000, b"ab\0").expect("write dest");
    engine.mem_write(0x4000, b"cd\0").expect("write src");
    write_regs(&mut engine, 0x3000, 8, 0x4000, 0);
    let r = dispatch("msvcrt.dll", "strcat_s", &mut engine, &mut state);
    assert_eq!(r.return_value, 0);
    let mut out = [0_u8; 6];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    assert_eq!(&out[..5], b"abcd\0");
}

#[test]
fn strtok_s_uses_guest_context() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x3000, b"a,b\0").expect("write str");
    engine.mem_write(0x4000, b",\0").expect("write delim");
    engine
        .mem_write(0x6000, &0_u64.to_le_bytes())
        .expect("write ctx");
    // First token: "a", context moves past the comma.
    write_regs(&mut engine, 0x3000, 0x4000, 0x6000, 0);
    let r = dispatch("msvcrt.dll", "strtok_s", &mut engine, &mut state);
    assert_eq!(r.return_value, 0x3000);
    let mut ctx = [0_u8; 8];
    engine.mem_read(0x6000, &mut ctx).expect("read ctx");
    assert_eq!(u64::from_le_bytes(ctx), 0x3002);
    // Second token with NULL str: "b", then the context is NULLed.
    write_regs(&mut engine, 0, 0x4000, 0x6000, 0);
    let r = dispatch("msvcrt.dll", "strtok_s", &mut engine, &mut state);
    assert_eq!(r.return_value, 0x3002);
    engine.mem_read(0x6000, &mut ctx).expect("read ctx");
    assert_eq!(u64::from_le_bytes(ctx), 0);
}

#[test]
fn sprintf_s_formats_register_vararg() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x5000, b"%d\0").expect("write format");
    // sprintf_s(buf, size, fmt, ...): the first vararg is in R9.
    write_regs(&mut engine, 0x3000, 64, 0x5000, 42);
    let r = dispatch("msvcrt.dll", "sprintf_s", &mut engine, &mut state);
    assert_eq!(r.return_value, 0);
    let mut out = [0_u8; 8];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    assert_eq!(&out[..3], b"42\0");
}

#[test]
fn vsnprintf_s_truncation_returns_erange() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x5000, b"%s\0").expect("write format");
    engine.mem_write(0x7000, b"hello\0").expect("write string");
    write_va_list(&mut engine, 0x6000, &[0x7000]);
    // _vsnprintf_s(buf, size, count, fmt, va_list): va_list at [rsp+0x28].
    write_regs(&mut engine, 0x3000, 8, 4, 0x5000);
    engine
        .mem_write(STACK_TOP + 0x28, &0x6000_u64.to_le_bytes())
        .expect("write va_list slot");
    let r = dispatch("msvcrt.dll", "_vsnprintf_s", &mut engine, &mut state);
    assert_eq!(r.return_value, i32_status_to_u64(34));
    // Truncation writes cap-1 content units + NUL ("hel\0" for cap=4).
    let mut out = [0xff_u8; 8];
    engine.mem_read(0x3000, &mut out).expect("read dest");
    assert_eq!(&out[..4], b"hel\0");
}

#[test]
fn fopen_s_writes_null_and_returns_enoent() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x4000, b"x.txt\0").expect("write name");
    engine.mem_write(0x5000, b"r\0").expect("write mode");
    engine
        .mem_write(0x3000, &[0xff_u8; 8])
        .expect("write pFile");
    write_regs(&mut engine, 0x3000, 0x4000, 0x5000, 0);
    let r = dispatch("msvcrt.dll", "fopen_s", &mut engine, &mut state);
    assert_eq!(r.return_value, i32_status_to_u64(2)); // ENOENT
    let mut cell = [0xff_u8; 8];
    engine.mem_read(0x3000, &mut cell).expect("read pFile");
    assert_eq!(u64::from_le_bytes(cell), 0);
}

#[test]
fn qsort_s_validates_and_shortcircuits() {
    let mut engine = test_engine();
    let mut state = test_state();
    // count <= 1: no comparator calls needed → success.
    write_regs(&mut engine, 0x3000, 1, 8, 0x7000);
    let r = dispatch("msvcrt.dll", "qsort_s", &mut engine, &mut state);
    assert_eq!(r.return_value, 0);
    // NULL base → EINVAL.
    write_regs(&mut engine, 0, 5, 4, 0x7000);
    let r = dispatch("msvcrt.dll", "qsort_s", &mut engine, &mut state);
    assert_eq!(r.return_value, i32_status_to_u64(22));
    // count > 0 with zero element size → EINVAL.
    write_regs(&mut engine, 0x3000, 5, 0, 0x7000);
    let r = dispatch("msvcrt.dll", "qsort_s", &mut engine, &mut state);
    assert_eq!(r.return_value, i32_status_to_u64(22));
}
