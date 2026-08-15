//! Regression: a host-side `SetFilePointerEx` seek must survive into the next
//! `ReadFile` for buffered (guest-I/O-registered) files.
//!
//! DOOM Retro loads its WADs through a CRT `fread`/`fseek` pair. `SetFilePointerEx`
//! used to update only the host-side `OpenGuestFile::cursor`; the next `ReadFile`
//! then pulled the STALE cursor back from the guest I/O table
//! (`guest_io_host::sync_host_cursor_from_guest`), so the read landed at the
//! pre-seek offset. The WAD-directory decode produced a bogus ~1.8 GiB lump size
//! (`Z_Malloc: Failure trying to allocate 1847620416 bytes`) and the game exited.
//!
//! This test drives the public handlers directly on a small host-backed file that
//! stays below the streaming threshold (hence mirrored into the guest I/O table)
//! and asserts that after a seek the bytes read are exactly the bytes at the
//! offset the `FILE_CURRENT` query returned.

use std::sync::{Arc, Mutex};

use ahash::{HashMap, HashMapExt};
use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};
use wie_winapi::{
    DEFAULT_ENVIRONMENT, DllStateMap, FileHandle, FileIoState, FindFileHandle, GuestHeap,
    GuestIoRuntimeConfig, GuestStdinMode, HandlerContext, HeapState, HostFileMount, KernelState,
    ModuleHandle, ModuleState, ProcessState, RegistryKeyHandle, ResourceHandle, SyncState,
    ThreadState, VolumeConfig, WinApiEnvironment, WinApiHandlerResult, WinApiState, dll_loader,
    present,
};

const STACK_VA: u64 = 0x100_0000;
const STACK_SIZE: usize = 0x1_0000;
// STACK_VA + STACK_SIZE - 0x100 (room for a dummy return address).
const STACK_TOP: u64 = 0x100_FF00;

// Scratch region: buffers, byte-count out-param, position out-param, filename.
const BUF_VA: u64 = 0x6000;
const N_VA: u64 = 0x6800;
const POS_VA: u64 = 0x7000;
const FILE_NAME_VA: u64 = 0x7800;

// Guest I/O table + mirrored file arena (matches `register_open_file`).
const TABLE_BASE: u64 = 0x2000_0000;
const TABLE_SIZE: usize = 128 * 40;
const ARENA_BASE: u64 = 0x2000_2000;
const ARENA_SIZE: usize = 0x100_000;
/// Page-aligned span covering the table and the mirrored file arena.
const GUEST_IO_MAP_SIZE: usize = 0x102_000;

// Slot layout (must match `guest_io_host`): handle +0, data_va +8, size +16,
// cursor +24, flags +32.
const GUEST_IO_SLOT_SIZE: u64 = 40;
const GUEST_IO_SLOT_CURSOR_OFFSET: u64 = 24;

// `SetFilePointerEx` move methods / `CreateFileW` disposition (winbase.h).
const FILE_BEGIN: u64 = 0;
const FILE_CURRENT: u64 = 1;
const FILE_END: u64 = 2;
const OPEN_EXISTING: u32 = 3;
const GENERIC_READ: u64 = 0x8000_0000;
const INVALID_HANDLE_VALUE: u64 = u64::MAX;

fn default_env() -> WinApiEnvironment {
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

fn default_winapi_state() -> WinApiState {
    let mut heap = GuestHeap::new(0x2000, 0x10000);
    heap.attach_guest_control(0x2000);
    WinApiState {
        heap_state: HeapState {
            heap,
            next_fls_index: 0,
            fls_slots: Vec::new(),
            guest_fls_table_va: 0,
        },
        file_io: FileIoState {
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
        message_queue: Arc::new(Mutex::new(present::MessageQueue::default())),
        module_state: ModuleState {
            loaded_modules: HashMap::new(),
            import_resolver: None,
            get_proc_address_cache: HashMap::new(),
            next_module_handle: ModuleHandle::from(dll_loader::REAL_MODULE_HANDLE_BASE),
        },
    }
}

/// Run one KERNEL32 handler with a fresh context and return its return value.
fn run(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    body: impl FnOnce(&mut HandlerContext<'_>) -> anyhow::Result<WinApiHandlerResult>,
) -> anyhow::Result<u64> {
    let mut ctx = HandlerContext::new(engine, default_env(), state);
    Ok(body(&mut ctx)?.return_value)
}

fn set_regs_read(
    engine: &mut IcedCpu,
    handle: u64,
    buf_va: u64,
    count: u64,
    n_va: u64,
) -> anyhow::Result<()> {
    engine.write_rcx(handle)?;
    engine.write_rdx(buf_va)?;
    engine.write_r8(count)?;
    engine.write_r9(n_va)?;
    engine.write_rsp(STACK_TOP)?;
    Ok(())
}

fn set_regs_seek(
    engine: &mut IcedCpu,
    handle: u64,
    distance: u64,
    new_pos_va: u64,
    method: u64,
) -> anyhow::Result<()> {
    engine.write_rcx(handle)?;
    engine.write_rdx(distance)?;
    engine.write_r8(new_pos_va)?;
    engine.write_r9(method)?;
    engine.write_rsp(STACK_TOP)?;
    Ok(())
}

fn write_wide(engine: &mut IcedCpu, va: u64, s: &str) -> anyhow::Result<()> {
    let mut bytes: Vec<u8> = Vec::new();
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(va, &bytes)?;
    Ok(())
}

fn guest_read_u64(engine: &mut IcedCpu, va: u64) -> anyhow::Result<u64> {
    let mut bytes = [0_u8; 8];
    engine.mem_read(va, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn guest_read_u32(engine: &mut IcedCpu, va: u64) -> anyhow::Result<u32> {
    let mut bytes = [0_u8; 4];
    engine.mem_read(va, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_guest_bytes(engine: &mut IcedCpu, va: u64, len: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0_u8; len];
    engine.mem_read(va, &mut buf)?;
    Ok(buf)
}

fn assert_buf_is_content(
    engine: &mut IcedCpu,
    buf_va: u64,
    content: &[u8],
    offset: usize,
    len: usize,
    label: &str,
) -> anyhow::Result<()> {
    let expected = content
        .get(offset..offset.saturating_add(len))
        .ok_or_else(|| anyhow::anyhow!("{label}: pattern slice out of range"))?;
    let got = read_guest_bytes(engine, buf_va, len)?;
    assert_eq!(
        got, expected,
        "{label}: read bytes must match the queried offset {offset}"
    );
    Ok(())
}

fn slot_cursor_va(table_va: u64, slot_index: u32) -> anyhow::Result<u64> {
    let slot_offset = u64::from(slot_index)
        .checked_mul(GUEST_IO_SLOT_SIZE)
        .ok_or_else(|| anyhow::anyhow!("guest I/O slot offset overflow"))?;
    Ok(table_va
        .checked_add(slot_offset)
        .ok_or_else(|| anyhow::anyhow!("guest I/O slot address overflow"))?
        .saturating_add(GUEST_IO_SLOT_CURSOR_OFFSET))
}

#[test]
fn seek_read_cursor_tracking_regression() -> anyhow::Result<()> {
    // content[i] = i mod 256, so the byte at offset `o` is `o mod 256`.
    let content: Vec<u8> = (0..1024_u16)
        .map(|i| u8::try_from(i % 256).unwrap_or(0))
        .collect();

    let tmp = std::env::temp_dir().join(format!("wie-cursor-sync-{}.bin", std::process::id()));
    std::fs::write(&tmp, &content)?;

    let mut engine = IcedCpu::open_x86_64();
    engine.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)?;
    engine.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)?;
    engine.mem_write(STACK_TOP, &0_u64.to_le_bytes())?;
    // Guest I/O handle table + file mirror arena (`register_open_file` writes both).
    engine.mem_map(TABLE_BASE, GUEST_IO_MAP_SIZE, RwxPerms::ALL)?;
    engine.mem_write(TABLE_BASE, &vec![0_u8; TABLE_SIZE])?;

    let mut state = default_winapi_state();
    state.file_io.guest_io = Some(GuestIoRuntimeConfig {
        table_va: TABLE_BASE,
        file_data_base: ARENA_BASE,
        file_data_size: ARENA_SIZE,
    });
    state.file_io.guest_file_data_next = ARENA_BASE;
    state.file_io.host_file_mounts.push(HostFileMount {
        guest_path: r"C:\probe.bin".to_owned(),
        host_path: tmp.clone(),
    });

    // CreateFileW(C:\probe.bin, GENERIC_READ, ..., OPEN_EXISTING) — the small
    // host-backed file stays buffered and is mirrored into the guest I/O table.
    write_wide(&mut engine, FILE_NAME_VA, r"C:\probe.bin")?;
    engine.write_rcx(FILE_NAME_VA)?;
    engine.write_rdx(GENERIC_READ)?;
    engine.write_r8(0)?;
    engine.write_r9(0)?;
    engine.write_rsp(STACK_TOP)?;
    engine.mem_write(STACK_TOP.saturating_add(0x28), &OPEN_EXISTING.to_le_bytes())?;
    let handle = run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_create_file_w(ctx)
    })?;
    assert_ne!(
        handle, INVALID_HANDLE_VALUE,
        "CreateFileW must open the mounted file"
    );

    let open_file = state
        .file_io
        .open_files
        .get(&handle)
        .ok_or_else(|| anyhow::anyhow!("CreateFileW did not open a file"))?;
    assert!(
        !open_file.streaming,
        "small host-backed file must be buffered"
    );
    let slot_index = open_file.guest_slot_index.ok_or_else(|| {
        anyhow::anyhow!("buffered file must be registered in the guest I/O table")
    })?;

    // Sequential read seeds both the host cursor and the guest table cursor at 8.
    set_regs_read(&mut engine, handle, BUF_VA, 8, N_VA)?;
    let ret = run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_read_file(ctx)
    })?;
    assert_eq!(ret, 1, "first ReadFile TRUE");
    assert_buf_is_content(&mut engine, BUF_VA, &content, 0, 8, "initial read")?;
    assert_eq!(guest_read_u32(&mut engine, N_VA)?, 8, "bytes-read count");

    // FILE_CURRENT query reflects the sequential position.
    set_regs_seek(&mut engine, handle, 0, POS_VA, FILE_CURRENT)?;
    let ret = run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_set_file_pointer_ex(ctx)
    })?;
    assert_eq!(ret, 1, "FILE_CURRENT query TRUE");
    assert_eq!(
        guest_read_u64(&mut engine, POS_VA)?,
        8,
        "query after sequential read"
    );

    // Host-side seek to 128. Only the OpenGuestFile cursor moves; the guest
    // table cursor must be pushed to match, or the next ReadFile pulls the
    // stale pre-seek value (8) and reads at the wrong offset.
    set_regs_seek(&mut engine, handle, 128, 0, FILE_BEGIN)?;
    let ret = run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_set_file_pointer_ex(ctx)
    })?;
    assert_eq!(ret, 1, "seek to 128 TRUE");

    set_regs_seek(&mut engine, handle, 0, POS_VA, FILE_CURRENT)?;
    let ret = run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_set_file_pointer_ex(ctx)
    })?;
    assert_eq!(ret, 1, "query after seek TRUE");
    assert_eq!(
        guest_read_u64(&mut engine, POS_VA)?,
        128,
        "query reflects the seek target"
    );

    // The regression: the read must land at the queried offset 128, not at the
    // stale pre-seek position 8.
    set_regs_read(&mut engine, handle, BUF_VA, 8, N_VA)?;
    let ret = run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_read_file(ctx)
    })?;
    assert_eq!(ret, 1, "read after seek TRUE");
    assert_buf_is_content(&mut engine, BUF_VA, &content, 128, 8, "read after seek")?;

    // White-box: the guest I/O table cursor must have followed the seek+read.
    let table_cursor = guest_read_u64(&mut engine, slot_cursor_va(TABLE_BASE, slot_index)?)?;
    assert_eq!(
        table_cursor, 136,
        "guest I/O table cursor must track the host cursor"
    );

    // Advancing offsets: seek → query → read must agree every round (forward,
    // backward, and again forward).
    for offset in [256_u64, 16, 32] {
        set_regs_seek(&mut engine, handle, offset, 0, FILE_BEGIN)?;
        run(&mut engine, &mut state, |ctx| {
            wie_winapi::kernel32::handle_set_file_pointer_ex(ctx)
        })?;
        set_regs_seek(&mut engine, handle, 0, POS_VA, FILE_CURRENT)?;
        run(&mut engine, &mut state, |ctx| {
            wie_winapi::kernel32::handle_set_file_pointer_ex(ctx)
        })?;
        let pos = guest_read_u64(&mut engine, POS_VA)?;
        assert_eq!(pos, offset, "query matches the seek target");
        set_regs_read(&mut engine, handle, BUF_VA, 8, N_VA)?;
        run(&mut engine, &mut state, |ctx| {
            wie_winapi::kernel32::handle_read_file(ctx)
        })?;
        assert_buf_is_content(
            &mut engine,
            BUF_VA,
            &content,
            usize::try_from(pos).unwrap_or(0),
            8,
            "read at queried offset",
        )?;
    }

    // Relative FILE_CURRENT seek (cursor is at 40 after the loop).
    let forward = u64::from_le_bytes(8_i64.to_le_bytes());
    set_regs_seek(&mut engine, handle, forward, 0, FILE_CURRENT)?;
    run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_set_file_pointer_ex(ctx)
    })?;
    set_regs_seek(&mut engine, handle, 0, POS_VA, FILE_CURRENT)?;
    run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_set_file_pointer_ex(ctx)
    })?;
    let pos = guest_read_u64(&mut engine, POS_VA)?;
    assert_eq!(pos, 48, "relative FILE_CURRENT seek lands at 48");
    set_regs_read(&mut engine, handle, BUF_VA, 8, N_VA)?;
    run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_read_file(ctx)
    })?;
    assert_buf_is_content(
        &mut engine,
        BUF_VA,
        &content,
        usize::try_from(pos).unwrap_or(0),
        8,
        "read after relative seek",
    )?;

    // Relative FILE_END seek: -8 → the file's final 8 bytes (1024-byte file).
    let back = u64::from_le_bytes((-8_i64).to_le_bytes());
    set_regs_seek(&mut engine, handle, back, 0, FILE_END)?;
    run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_set_file_pointer_ex(ctx)
    })?;
    set_regs_seek(&mut engine, handle, 0, POS_VA, FILE_CURRENT)?;
    run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_set_file_pointer_ex(ctx)
    })?;
    let pos = guest_read_u64(&mut engine, POS_VA)?;
    assert_eq!(pos, 1016, "FILE_END relative seek lands at size - 8");
    set_regs_read(&mut engine, handle, BUF_VA, 8, N_VA)?;
    run(&mut engine, &mut state, |ctx| {
        wie_winapi::kernel32::handle_read_file(ctx)
    })?;
    assert_buf_is_content(
        &mut engine,
        BUF_VA,
        &content,
        usize::try_from(pos).unwrap_or(0),
        8,
        "read at end of file",
    )?;

    let _ = std::fs::remove_file(&tmp);
    Ok(())
}
