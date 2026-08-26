//! Unit tests for the ntdll Nt*/Rtl* surface (handler smoke tests + the
//! dispatch-census guard). The user-facing api-set completeness contracts live
//! in `tests/api_sets.rs`; this module holds what needs private access.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use super::*;
use crate::{
    FileHandle, FindFileHandle, GuestHeap, GuestStdinMode, HeapState, KernelState, ModuleHandle,
    ModuleState, ProcessState, RegistryKeyHandle, ResourceHandle, ThreadState, WinApiEnvironment,
    WinApiState,
};
use ahash::HashMap;
use ahash::HashMapExt;
use std::sync::{Arc, Mutex};

use crate::sync_obj::SyncState;
use crate::vfs::VolumeConfig;
use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

const STACK_VA: u64 = 0x100_0000;
const STACK_SIZE: usize = 0x1_0000;
const STACK_TOP: u64 = 0x100_FF00;
const HEAP_CTRL: u64 = 0x2000;

// ─── Dispatch-census guard ────────────────────────────────────────────
//
// The user-facing completeness contracts (CRT families, oracle reporting,
// graceful gaps) live in `tests/api_sets.rs`; this module keeps the
// sync guard that needs private access to `NTDL_EXPORTS` plus the handler
// smoke tests.

/// Every reported export must dispatch to a handler (no is_export entry may
/// dangle). All-zero registers keep every handler on a safe early path, and
/// the guest-heap bump cursor is seeded so the heap forwards are safe too.
#[test]
fn every_reported_export_dispatches() {
    let mut engine = test_engine();
    engine
        .mem_write(HEAP_CTRL, &HEAP_CTRL.to_le_bytes())
        .expect("seed heap bump cursor");
    let mut state = test_state();
    for name in NTDL_EXPORTS {
        write_regs(&mut engine, 0, 0, 0, 0);
        engine
            .mem_write(STACK_TOP + 0x28, &0_u64.to_le_bytes())
            .expect("zero stack arg 5");
        engine
            .mem_write(STACK_TOP + 0x30, &0_u64.to_le_bytes())
            .expect("zero stack arg 6");
        let mut ctx = HandlerContext::new(&mut engine, test_environment(), &mut state);
        let result = dispatch_ntdll(&mut ctx, name)
            .unwrap_or_else(|e| panic!("{name} dispatch errored: {e}"));
        assert!(result.is_some(), "{name} must be dispatched");
    }
}

// ─── Handler smoke tests ───────────────────────────────────────────────

/// Minimal engine: guest pages, the heap-control page, and a stack with a
/// valid return address.
fn test_engine() -> IcedCpu {
    let mut cpu = IcedCpu::open_x86_64();
    cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
        .expect("map test memory");
    cpu.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)
        .expect("map test stack");
    cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
        .expect("write return address");
    cpu.write_rsp(STACK_TOP).ok();
    cpu
}

fn test_state() -> WinApiState {
    let mut heap = GuestHeap::new(0x2000, 0x10000);
    heap.attach_guest_control(0x2000);
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

fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
    cpu.write_rcx(rcx).ok();
    cpu.write_rdx(rdx).ok();
    cpu.write_r8(r8).ok();
    cpu.write_r9(r9).ok();
    cpu.write_rsp(STACK_TOP).ok();
}

/// Drive the ntdll string dispatch; panics unless a handler ran.
fn dispatch(engine: &mut IcedCpu, state: &mut WinApiState, name: &str) -> WinApiHandlerResult {
    let mut ctx = HandlerContext::new(engine, test_environment(), state);
    dispatch_ntdll(&mut ctx, name)
        .expect("ntdll dispatch must not error")
        .expect("export must be implemented")
}

fn read_guest_u64(engine: &mut IcedCpu, va: u64) -> u64 {
    let mut bytes = [0_u8; 8];
    engine.mem_read(va, &mut bytes).expect("read guest u64");
    u64::from_le_bytes(bytes)
}

fn read_guest_u32(engine: &mut IcedCpu, va: u64) -> u32 {
    let mut bytes = [0_u8; 4];
    engine.mem_read(va, &mut bytes).expect("read guest u32");
    u32::from_le_bytes(bytes)
}

// ─── Dispatch-census guard ────────────────────────────────────────────

#[test]
fn nt_query_information_process_writes_pbi() {
    let mut engine = test_engine();
    let mut state = test_state();
    let out_va: u64 = 0x5000;
    let ret_len_va: u64 = 0x5100;
    engine
        .mem_write(STACK_TOP + 0x28, &ret_len_va.to_le_bytes())
        .expect("stack arg 5");
    write_regs(&mut engine, 0, 0, out_va, 48); // handle, class 0, buffer, len

    let result = dispatch(&mut engine, &mut state, "ntqueryinformationprocess");
    assert_eq!(result.return_value, STATUS_SUCCESS);
    let mut buf = [0_u8; 48];
    engine.mem_read(out_va, &mut buf).expect("read PBI");
    assert_eq!(
        u32::from_le_bytes(buf[0..4].try_into().unwrap()),
        259,
        "ExitStatus = STILL_ACTIVE"
    );
    assert_eq!(u64::from_le_bytes(buf[8..16].try_into().unwrap()), 0);
    assert_eq!(
        u64::from_le_bytes(buf[16..24].try_into().unwrap()),
        1,
        "AffinityMask"
    );
    assert_eq!(
        u64::from_le_bytes(buf[32..40].try_into().unwrap()),
        FAKE_PROCESS_ID
    );
    assert_eq!(read_guest_u64(&mut engine, ret_len_va), PBI_SIZE);

    // Unknown classes → STATUS_INVALID_INFO_CLASS.
    write_regs(&mut engine, 0, 1, out_va, 48);
    let result = dispatch(&mut engine, &mut state, "ntqueryinformationprocess");
    assert_eq!(result.return_value, STATUS_INVALID_INFO_CLASS);
}

#[test]
fn nt_query_system_information_writes_sbi() {
    let mut engine = test_engine();
    let mut state = test_state();
    let out_va = 0x5200;
    write_regs(&mut engine, 0, out_va, 56, 0x5300); // class 0, buffer, len, retlen

    let result = dispatch(&mut engine, &mut state, "ntquerysysteminformation");
    assert_eq!(result.return_value, STATUS_SUCCESS);
    assert_eq!(read_guest_u32(&mut engine, out_va + 8), PAGE_SIZE);
    assert_eq!(read_guest_u64(&mut engine, out_va + 44), 1, "affinity mask");
    assert_eq!(
        read_guest_u64(&mut engine, 0x5300),
        SBI_SIZE,
        "ReturnLength"
    );

    write_regs(&mut engine, 5, out_va, 56, 0);
    let result = dispatch(&mut engine, &mut state, "ntquerysysteminformation");
    assert_eq!(result.return_value, STATUS_INVALID_INFO_CLASS);
}

#[test]
fn nt_close_returns_ntstatus() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_regs(&mut engine, 0, 0, 0, 0);
    assert_eq!(
        dispatch(&mut engine, &mut state, "ntclose").return_value,
        STATUS_INVALID_HANDLE
    );
    // Unknown nonzero handles are accepted (fake kernel objects), like
    // CloseHandle's no-op accept path — but with an NTSTATUS success.
    write_regs(&mut engine, 0x7777, 0, 0, 0);
    assert_eq!(
        dispatch(&mut engine, &mut state, "ntclose").return_value,
        STATUS_SUCCESS
    );
    write_regs(&mut engine, 0x7777, 0, 0, 0);
    assert_eq!(
        dispatch(&mut engine, &mut state, "rtlclosehandle").return_value,
        STATUS_SUCCESS
    );
}

#[test]
fn nt_query_performance_counter_writes_counter_and_frequency() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_regs(&mut engine, 0x5400, 0x5410, 0, 0);
    let result = dispatch(&mut engine, &mut state, "ntqueryperformancecounter");
    assert_eq!(result.return_value, STATUS_SUCCESS);
    assert!(
        read_guest_u64(&mut engine, 0x5400) > 0,
        "counter above zero"
    );
    assert_eq!(read_guest_u64(&mut engine, 0x5410), QPC_FREQUENCY);
}

#[test]
fn nt_query_system_time_writes_filetime() {
    let mut engine = test_engine();
    let mut state = test_state();
    write_regs(&mut engine, 0x5500, 0, 0, 0);
    let result = dispatch(&mut engine, &mut state, "ntquerysystemtime");
    assert_eq!(result.return_value, STATUS_SUCCESS);
    // Post-1601 FILETIME is always nonzero (frozen clock pins it too).
    let filetime = u64::from(read_guest_u32(&mut engine, 0x5504)) << 32
        | u64::from(read_guest_u32(&mut engine, 0x5500));
    assert_ne!(filetime, 0);
}

#[test]
fn rtl_compare_memory_counts_equal_prefix() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x6000, b"abcdefghij").expect("write src1");
    engine
        .mem_write(0x6100, b"abcdefxyz!") // 'abcdef' matches, then diverge
        .expect("write src2");
    write_regs(&mut engine, 0x6000, 0x6100, 10, 0);
    assert_eq!(
        dispatch(&mut engine, &mut state, "rtlcomparememory").return_value,
        6
    );
    // Fully equal buffers return the whole length (not the 64 B chunk).
    engine
        .mem_write(0x6200, b"twelve bytes!")
        .expect("write src3");
    write_regs(&mut engine, 0x6200, 0x6200, 12, 0);
    assert_eq!(
        dispatch(&mut engine, &mut state, "rtlcomparememory").return_value,
        12
    );
}

#[test]
fn rtl_init_unicode_string_writes_struct() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine
        .mem_write(0x6200, &[b'H', 0, b'i', 0, 0, 0])
        .expect("write source");
    write_regs(&mut engine, 0x6300, 0x6200, 0, 0);
    let result = dispatch(&mut engine, &mut state, "rtlinitunicodestring");
    assert_eq!(result.return_value, STATUS_SUCCESS);
    let header = read_guest_u32(&mut engine, 0x6300);
    assert_eq!(header & 0xffff, 4, "Length = 4 bytes");
    assert_eq!(header >> 16, 6, "MaximumLength = 6");
    assert_eq!(
        read_guest_u64(&mut engine, 0x6308),
        0x6200,
        "Buffer = source VA"
    );
}

#[test]
fn rtl_move_and_zero_memory_roundtrip() {
    let mut engine = test_engine();
    let mut state = test_state();
    engine.mem_write(0x6400, b"payload!").expect("write source");
    write_regs(&mut engine, 0x6500, 0x6400, 8, 0);
    assert_eq!(
        dispatch(&mut engine, &mut state, "rtlmovememory").return_value,
        STATUS_SUCCESS
    );
    let mut bytes = [0_u8; 8];
    engine.mem_read(0x6500, &mut bytes).expect("read dest");
    assert_eq!(&bytes, b"payload!");

    write_regs(&mut engine, 0x6600, 8, 0, 0);
    assert_eq!(
        dispatch(&mut engine, &mut state, "rtlzeromemory").return_value,
        STATUS_SUCCESS
    );
    engine.mem_read(0x6600, &mut bytes).expect("read zeroed");
    assert_eq!(bytes, [0_u8; 8]);
}

#[test]
fn rtl_allocate_heap_maps_null_handle_to_process_heap() {
    let mut engine = test_engine();
    engine
        .mem_write(HEAP_CTRL, &HEAP_CTRL.to_le_bytes())
        .expect("seed heap bump cursor");
    let mut state = test_state();
    write_regs(&mut engine, 0, 0, 64, 0); // HeapHandle = NULL
    let result = dispatch(&mut engine, &mut state, "rtlallocateheap");
    assert_ne!(
        result.return_value, 0,
        "NULL heap handle maps to the process heap"
    );
    assert!(state.heap_state.heap.is_live(result.return_value));

    write_regs(&mut engine, 0, 0, result.return_value, 0);
    let result = dispatch(&mut engine, &mut state, "rtlfreeheap");
    assert_eq!(result.return_value, 1, "RtlFreeHeap returns TRUE");
    assert!(!state.heap_state.heap.is_live(result.return_value));
}

#[test]
fn rtl_critical_section_handlers_forward_to_kernel32() {
    let mut engine = test_engine();
    let mut state = test_state();
    let cs = 0x6700;

    write_regs(&mut engine, cs, 0, 0, 0);
    dispatch(&mut engine, &mut state, "rtlinitializecriticalsection");
    assert_eq!(read_guest_u32(&mut engine, cs + 8), u32::MAX, "unlocked");

    write_regs(&mut engine, cs, 0, 0, 0);
    dispatch(&mut engine, &mut state, "rtlentercriticalsection");
    let tid = u64::from(state.kernel.threads.current_tid());
    assert_eq!(
        read_guest_u64(&mut engine, cs + 16),
        tid,
        "OwningThread set"
    );

    write_regs(&mut engine, cs, 0, 0, 0);
    dispatch(&mut engine, &mut state, "rtlleavecriticalsection");
    assert_eq!(
        read_guest_u64(&mut engine, cs + 16),
        0,
        "OwningThread cleared"
    );

    write_regs(&mut engine, cs, 0, 0, 0);
    dispatch(&mut engine, &mut state, "rtldeletecriticalsection");
    assert_eq!(read_guest_u32(&mut engine, cs + 8), u32::MAX);
}

#[test]
fn unknown_ntdll_names_return_none() {
    let mut engine = test_engine();
    let mut state = test_state();
    let mut ctx = HandlerContext::new(&mut engine, test_environment(), &mut state);
    let result = dispatch_ntdll(&mut ctx, "ntcreateprocess")
        .expect("dispatch must not error for unknown names");
    assert!(result.is_none());
}
