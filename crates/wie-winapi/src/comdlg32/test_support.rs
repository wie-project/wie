//! Shared test scaffolding for the `comdlg32` submodule tests (guest engines,
//! session state, and small fixtures).

use crate::guest_heap::GuestHeap;
use crate::present::MessageQueue;
use crate::state::{FileIoState, HeapState, ProcessState, WinApiEnvironment};
use crate::sync_obj::SyncState;
use crate::thread::ThreadState;
use crate::user32::{CreateWindowRequest, WS_VISIBLE, WindowClassIdentifier, create_window_record};
use crate::vfs::VolumeConfig;
use crate::{DEFAULT_ENVIRONMENT, DllStateMap, KernelState, ModuleState, WinApiState};
use ahash::{HashMap, HashMapExt};
use std::sync::{Arc, Mutex};
use wie_cpu::{CpuEngine, IcedCpu};

pub(crate) const STACK_VA: u64 = 0x100_0000;
pub(crate) const STACK_SIZE: usize = 0x1_0000;
pub(crate) const STACK_TOP: u64 = 0x100_FF00;
/// Minimal engine for handler tests: guest pages + a return address.
pub(crate) fn test_engine() -> IcedCpu {
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
pub(crate) fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
    cpu.write_rcx(rcx).ok();
    cpu.write_rdx(rdx).ok();
    cpu.write_r8(r8).ok();
    cpu.write_r9(r9).ok();
    cpu.write_rsp(STACK_TOP).ok();
}
pub(crate) fn test_state() -> WinApiState {
    WinApiState {
        display: crate::DisplayMetrics::default(),
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
            open_files: HashMap::new(),
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
            ucrt_files: HashMap::new(),
            ucrt_next_file_va: 0x0000_0000_6900_0000,
            cached_streams: HashMap::new(),
        },
        process: ProcessState {
            last_error: 0,
            next_registry_key_handle: crate::RegistryKeyHandle::from(0),
            registry_keys: Vec::new(),
            main_module_file_name: String::new(),
            main_module_path: String::new(),
            main_module_host_dir: None,
            error_mode: 0,
            suspended_threads: HashMap::new(),
            environment: DEFAULT_ENVIRONMENT
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
            seh_pending: HashMap::new(),
        },
        dll_states: DllStateMap::new(),
        message_queue: Arc::new(Mutex::new(MessageQueue::default())),
        module_state: ModuleState {
            loaded_modules: HashMap::new(),
            import_resolver: None,
            get_proc_address_cache: HashMap::new(),
            next_module_handle: crate::ModuleHandle::from(
                crate::dll_loader::REAL_MODULE_HANDLE_BASE,
            ),
        },
    }
}
pub(crate) fn test_environment() -> WinApiEnvironment {
    WinApiEnvironment {
        image_base: 0,
        command_line_a_ptr: 0,
        command_line_w_ptr: 0,
        environment_strings_w_ptr: 0,
        module_file_name_a_ptr: 0,
        module_file_name_w_ptr: 0,
        process_heap_handle: 0,
    }
}
/// Write an `OPENFILENAME` (Win64) into guest memory at `ofn_va` with
/// `lpstrFile` → `file_buf` and optional `lpstrDefExt` → `def_ext_va`.
pub(crate) fn write_ofn(
    engine: &mut IcedCpu,
    ofn_va: u64,
    file_buf: u64,
    max_file: u32,
    def_ext_va: u64,
) {
    engine.mem_write(ofn_va, &0x58_u32.to_le_bytes()).ok(); // lStructSize
    engine.mem_write(ofn_va + 8, &0_u64.to_le_bytes()).ok(); // hwndOwner
    engine.mem_write(ofn_va + 48, &file_buf.to_le_bytes()).ok(); // lpstrFile
    engine.mem_write(ofn_va + 56, &max_file.to_le_bytes()).ok(); // nMaxFile
    engine.mem_write(ofn_va + 64, &0_u64.to_le_bytes()).ok(); // lpstrFileTitle
    engine.mem_write(ofn_va + 72, &0_u32.to_le_bytes()).ok(); // nMaxFileTitle
    engine.mem_write(ofn_va + 80, &0_u64.to_le_bytes()).ok(); // lpstrInitialDir (0 → guest cwd fallback)
    engine
        .mem_write(ofn_va + 104, &def_ext_va.to_le_bytes())
        .ok(); // lpstrDefExt
}
pub(crate) fn read_guest_utf16(engine: &mut IcedCpu, ptr: u64, max_units: usize) -> String {
    let mut units = Vec::new();
    for i in 0..max_units {
        let mut buf = [0_u8; 2];
        let ok = engine.mem_read(ptr + (i as u64) * 2, &mut buf).ok();
        let unit = u16::from_le_bytes(buf);
        if unit == 0 || ok.is_none() {
            break;
        }
        units.push(unit);
    }
    String::from_utf16_lossy(&units)
}
/// NUL-terminated UTF-16LE bytes for a guest string literal.
pub(crate) fn utf16_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}
pub(crate) fn read_guest_u32_at(engine: &mut IcedCpu, address: u64) -> u32 {
    let mut bytes = [0_u8; 4];
    engine.mem_read(address, &mut bytes).ok();
    u32::from_le_bytes(bytes)
}
/// Create a plain top-level window for the dialog to be owned by.
pub(crate) fn create_owner_window(state: &mut WinApiState) -> u64 {
    let (hwnd, _, _) = create_window_record(
        state,
        CreateWindowRequest {
            class_identifier: WindowClassIdentifier::Name("Owner".to_owned()),
            title: "Owner".to_owned(),
            style: WS_VISIBLE,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0,
            instance_handle: 0,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        },
        false,
    )
    .expect("owner window created");
    hwnd
}
