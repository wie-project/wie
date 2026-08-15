use super::{
    Context, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, GlobalAtomRecord, HEAP_SIZE_FAILURE,
    HEAP_ZERO_MEMORY, HandlerContext, Result, WinApiHandlerResult, allocate_fake_heap_block,
    read_ansi_string_from_cpu, ret_bool_true, ret_u64,
};

/// Handles `KERNEL32.dll!HeapAlloc`.
pub fn handle_heap_alloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let heap_handle = engine.read_rcx()?;
    let flags = engine.read_rdx()?;
    let size = engine.read_r8()?;

    let return_value = if heap_handle == 0 {
        0
    } else {
        // Zero-byte requests still need a live block (round-up in GuestHeap).
        let alloc_size = if size == 0 { 1 } else { size };
        let addr = state.heap_state.heap.alloc_coherent(engine, alloc_size);
        if addr != 0 && (flags & HEAP_ZERO_MEMORY) != 0 {
            let zero_len = state.heap_state.heap.size_of(addr).unwrap_or(alloc_size);
            if let Ok(len) = usize::try_from(zero_len)
                && len > 0
                && !engine.mem_fill(addr, 0, len)
            {
                let zeros = vec![0_u8; len];
                engine.mem_write(addr, &zeros)?;
            }
        }
        addr
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!HeapFree`.
pub fn handle_heap_free(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _heap_handle = engine.read_rcx()?;
    let _flags = engine.read_rdx()?;
    let memory = engine.read_r8()?;

    let ok = memory == 0 || state.heap_state.heap.free_coherent(engine, memory);
    let return_value = if ok {
        1
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        0
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!HeapReAlloc`.
pub fn handle_heap_realloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let heap_handle = engine.read_rcx()?;
    let flags = engine.read_rdx()?;
    let memory = engine.read_r8()?;
    let new_size = engine.read_r9()?;

    let return_value = if heap_handle == 0 || memory == 0 {
        0
    } else if new_size == 0 {
        let _ = state.heap_state.heap.free_coherent(engine, memory);
        0
    } else if let Some(same) = state.heap_state.heap.try_realloc_in_place(memory, new_size) {
        // In-place only succeeds when the block already fits; no new bytes to zero.
        same
    } else {
        let old_size = state
            .heap_state
            .heap
            .size_of(memory)
            .or_else(|| {
                let mut hb = [0_u8; 8];
                engine
                    .mem_read(memory.wrapping_sub(HEAP_BLOCK_HEADER_BYTES), &mut hb)
                    .ok()
                    .map(|()| u64::from_le_bytes(hb))
            })
            .unwrap_or(0);
        let new_addr = state.heap_state.heap.alloc_coherent(engine, new_size);
        if new_addr == 0 {
            // Failure must leave the original block live (Microsoft Learn).
            0
        } else {
            let copy_len = usize::try_from(old_size.min(new_size)).unwrap_or(0);
            if copy_len > 0 {
                // memmove semantics inside wie-cpu; no overlap analysis needed
                // here, and no host bounce buffer on the fast path.
                if !engine.mem_copy(new_addr, memory, copy_len) {
                    let mut bytes = vec![0_u8; copy_len];
                    engine.mem_read(memory, &mut bytes)?;
                    engine.mem_write(new_addr, &bytes)?;
                }
            }
            if (flags & HEAP_ZERO_MEMORY) != 0 && new_size > old_size {
                let zero_start = old_size;
                let zero_len = usize::try_from(new_size.saturating_sub(old_size)).unwrap_or(0);
                if zero_len > 0 {
                    let dst_addr = new_addr.wrapping_add(zero_start);
                    if !engine.mem_fill(dst_addr, 0, zero_len) {
                        let zeros = vec![0_u8; zero_len];
                        engine.mem_write(dst_addr, &zeros)?;
                    }
                }
            }
            let _ = state.heap_state.heap.free_coherent(engine, memory);
            new_addr
        }
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!HeapCreate`.
pub fn handle_heap_create(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let process_heap_handle = ctx.environment.process_heap_handle;
    let engine = &mut *ctx.engine;
    let _options = engine
        .read_rcx()
        .context("failed to read RCX for HeapCreate")?;

    let _initial_size = engine
        .read_rdx()
        .context("failed to read RDX for HeapCreate")?;

    let _maximum_size = engine
        .read_r8()
        .context("failed to read R8 for HeapCreate")?;

    ctx.finish(process_heap_handle)
}
/// Handles `KERNEL32.dll!HeapSetInformation`.
pub fn handle_heap_set_information(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _heap_handle = engine
        .read_rcx()
        .context("failed to read RCX for HeapSetInformation")?;

    let _heap_information_class = engine
        .read_rdx()
        .context("failed to read RDX for HeapSetInformation")?;

    let _heap_information = engine
        .read_r8()
        .context("failed to read R8 for HeapSetInformation")?;

    ctx.finish(1)
}
/// Handles `KERNEL32.dll!HeapSize`.
pub fn handle_heap_size(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _heap_handle = engine.read_rcx()?;
    let _flags = engine.read_rdx()?;
    let memory = engine.read_r8()?;

    let return_value = state
        .heap_state
        .heap
        .size_of(memory)
        .or_else(|| {
            if memory == 0 {
                return None;
            }
            let mut hb = [0_u8; 8];
            engine
                .mem_read(memory.wrapping_sub(HEAP_BLOCK_HEADER_BYTES), &mut hb)
                .ok()
                .map(|()| u64::from_le_bytes(hb))
                .filter(|&s| s != 0)
        })
        .unwrap_or(HEAP_SIZE_FAILURE);

    ctx.finish(return_value)
}
/// Bytes of guest-heap block header stored immediately before the payload
/// (used to recover a block's size from its pointer).
const HEAP_BLOCK_HEADER_BYTES: u64 = 8;

/// Mock `dwMemoryLoad` reported by the memory-status structs (25% busy).
const MEM_LOAD_PERCENT: u32 = 25;
/// Mock `dwTotalPhys`: 8 GiB of physical memory.
const MEM_TOTAL_PHYS: u64 = 8 * 1024 * 1024 * 1024;
/// Mock `dwAvailPhys`: 6 GiB of physical memory available.
const MEM_AVAIL_PHYS: u64 = 6 * 1024 * 1024 * 1024;
/// Mock `dwTotalPageFile`: 16 GiB committed-page limit.
const MEM_TOTAL_PAGEFILE: u64 = 16 * 1024 * 1024 * 1024;
/// Mock `dwAvailPageFile`: 12 GiB available for commit.
const MEM_AVAIL_PAGEFILE: u64 = 12 * 1024 * 1024 * 1024;
/// Mock `dwTotalVirtual`: 128 GiB of user-mode address space.
const MEM_TOTAL_VIRTUAL: u64 = 128 * 1024 * 1024 * 1024;
/// Mock `dwAvailVirtual`: 120 GiB of address space available.
const MEM_AVAIL_VIRTUAL: u64 = 120 * 1024 * 1024 * 1024;
/// Byte size of `MEMORYSTATUS` (Win64).
const MEMORYSTATUS_SIZE: usize = 56;
/// Byte size of `MEMORYSTATUSEX` (Win64).
const MEMORYSTATUSEX_SIZE: usize = 64;

/// Handles `KERNEL32.dll!GlobalMemoryStatus`.
pub fn handle_global_memory_status(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let memory_status_va = engine
        .read_rcx()
        .context("failed to read RCX for GlobalMemoryStatus")?;

    if memory_status_va != 0 {
        // MEMORYSTATUS on Win64 (56 bytes) — build once and push in a single
        // mem_write to avoid 8 separate scalar writes (each locking guest mem
        // and page-walking).
        // Layout:
        //   +0  dwLength         u32
        //   +4  dwMemoryLoad     u32
        //   +8  dwTotalPhys      u64
        //   +16 dwAvailPhys      u64
        //   +24 dwTotalPageFile  u64
        //   +32 dwAvailPageFile  u64
        //   +40 dwTotalVirtual   u64
        //   +48 dwAvailVirtual   u64
        let mut buf = [0_u8; MEMORYSTATUS_SIZE];
        buf[0..4].copy_from_slice(&u32::try_from(MEMORYSTATUS_SIZE).unwrap_or(0).to_le_bytes());
        buf[4..8].copy_from_slice(&MEM_LOAD_PERCENT.to_le_bytes());
        buf[8..16].copy_from_slice(&MEM_TOTAL_PHYS.to_le_bytes());
        buf[16..24].copy_from_slice(&MEM_AVAIL_PHYS.to_le_bytes());
        buf[24..32].copy_from_slice(&MEM_TOTAL_PAGEFILE.to_le_bytes());
        buf[32..40].copy_from_slice(&MEM_AVAIL_PAGEFILE.to_le_bytes());
        buf[40..48].copy_from_slice(&MEM_TOTAL_VIRTUAL.to_le_bytes());
        buf[48..56].copy_from_slice(&MEM_AVAIL_VIRTUAL.to_le_bytes());
        engine
            .mem_write(memory_status_va, &buf)
            .context("failed to write MEMORYSTATUS")?;
    }

    ctx.finish(0)
}
pub fn handle_global_memory_status_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ptr = engine.read_rcx().context("GlobalMemoryStatusEx RCX")?;
    if ptr == 0 {
        return ret_u64(engine, 0, "GlobalMemoryStatusEx");
    }
    // Read dwLength from guest (caller must set it); we fill the rest.
    let length = {
        let mut b = [0_u8; 4];
        engine.mem_read(ptr, &mut b)?;
        u32::from_le_bytes(b)
    };
    if length < 64 {
        return ret_u64(engine, 0, "GlobalMemoryStatusEx");
    }
    // MEMORYSTATUSEX (64 bytes) — build on host stack, push in one mem_write:
    //   +0  dwLength         u32
    //   +4  dwMemoryLoad     u32
    //   +8  ullTotalPhys     u64
    //   +16 ullAvailPhys     u64
    //   +24 ullTotalPageFile u64
    //   +32 ullAvailPageFile u64
    //   +40 ullTotalVirtual  u64
    //   +48 ullAvailVirtual  u64
    //   +56 ullAvailExtVirt  u64
    let mut buf = [0_u8; MEMORYSTATUSEX_SIZE];
    buf[0..4].copy_from_slice(
        &u32::try_from(MEMORYSTATUSEX_SIZE)
            .unwrap_or(0)
            .to_le_bytes(),
    );
    buf[4..8].copy_from_slice(&MEM_LOAD_PERCENT.to_le_bytes());
    buf[8..16].copy_from_slice(&MEM_TOTAL_PHYS.to_le_bytes());
    buf[16..24].copy_from_slice(&MEM_AVAIL_PHYS.to_le_bytes());
    buf[24..32].copy_from_slice(&MEM_TOTAL_PAGEFILE.to_le_bytes());
    buf[32..40].copy_from_slice(&MEM_AVAIL_PAGEFILE.to_le_bytes());
    buf[40..48].copy_from_slice(&MEM_TOTAL_VIRTUAL.to_le_bytes());
    buf[48..56].copy_from_slice(&MEM_AVAIL_VIRTUAL.to_le_bytes());
    // ullAvailExtendedVirtual at [56..64] already zero.
    engine
        .mem_write(ptr, &buf)
        .context("failed to write MEMORYSTATUSEX")?;
    ret_bool_true(engine, "GlobalMemoryStatusEx")
}
/// Handles `KERNEL32.dll!LocalAlloc`.
pub fn handle_local_alloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _flags = engine
        .read_rcx()
        .context("failed to read RCX for LocalAlloc")?;

    let size = engine
        .read_rdx()
        .context("failed to read RDX for LocalAlloc")?;

    let return_value = allocate_fake_heap_block(engine, state, size);

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!LocalFree`.
pub fn handle_local_free(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for LocalFree")?;

    if memory == 0 {
        return ctx.finish(0);
    }

    let was_live = state.heap_state.heap.free_coherent(engine, memory);

    // LocalFree returns NULL on success and the original handle on failure.
    let return_value = if was_live { 0 } else { memory };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!LocalLock`.
///
/// WIE's guest heap is a flat bump allocator: the `HLOCAL` value IS the
/// payload pointer, so locking is a liveness check that returns the same
/// address (real Windows returns a pointer into the memory object's data —
/// identical semantics for a fixed heap). Invalid handles return NULL (0).
pub fn handle_local_lock(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for LocalLock")?;

    let return_value = if memory != 0 && state.heap_state.heap.is_live(memory) {
        memory
    } else {
        0
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!LocalUnlock`.
///
/// A live handle always unlocks successfully (TRUE). Real Windows with
/// `LMEM_MOVEABLE` blocks returns FALSE when the lock count reaches zero;
/// WIE's heap is fixed (handles are direct pointers, no lock counts), so that
/// distinction cannot arise — documented deviation.
pub fn handle_local_unlock(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for LocalUnlock")?;

    let live = memory != 0 && state.heap_state.heap.is_live(memory);
    if !live {
        state.process.last_error = ERROR_INVALID_HANDLE;
    }

    let return_value = u64::from(live);
    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GlobalAlloc`.
pub fn handle_global_alloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _flags = engine
        .read_rcx()
        .context("failed to read RCX for GlobalAlloc")?;

    let size = engine
        .read_rdx()
        .context("failed to read RDX for GlobalAlloc")?;

    let return_value = allocate_fake_heap_block(engine, state, size);

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GlobalFree`.
pub fn handle_global_free(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for GlobalFree")?;

    if memory == 0 {
        return ctx.finish(0);
    }

    let was_live = state.heap_state.heap.free_coherent(engine, memory);

    // GlobalFree returns NULL on success and the original handle on failure.
    let return_value = if was_live { 0 } else { memory };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GlobalLock`.
pub fn handle_global_lock(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for GlobalLock")?;

    let return_value = if state.heap_state.heap.is_live(memory) {
        memory
    } else {
        0
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GlobalUnlock`.
pub fn handle_global_unlock(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for GlobalUnlock")?;

    let was_live = state.heap_state.heap.is_live(memory);

    state.process.last_error = if was_live { 0 } else { ERROR_INVALID_HANDLE };

    let return_value = 0;

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GlobalSize`.
pub fn handle_global_size(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for GlobalSize")?;

    let return_value = state.heap_state.heap.size_of(memory).unwrap_or(0);

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GlobalAddAtomA`.
pub fn handle_global_add_atom_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let name_va = engine
        .read_rcx()
        .context("failed to read RCX for GlobalAddAtomA")?;

    let return_value = if name_va == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        0
    } else {
        let name = read_ansi_string_from_cpu(engine, name_va, 255)
            .context("failed to read GlobalAddAtomA name")?;

        if name.is_empty() {
            state.process.last_error = ERROR_INVALID_PARAMETER;
            0
        } else if let Some(existing) = state
            .window_state()
            .global_atoms
            .iter()
            .find(|record| record.name.eq_ignore_ascii_case(&name))
            .cloned()
        {
            state.process.last_error = 0;
            u64::from(existing.atom)
        } else {
            let atom = state.window_state().next_global_atom;

            state.window_state().next_global_atom = state
                .window_state()
                .next_global_atom
                .checked_add(1)
                .context("global atom identifier overflow")?;

            state
                .window_state()
                .global_atoms
                .push(GlobalAtomRecord { atom, name });
            state.process.last_error = 0;

            u64::from(atom)
        }
    };

    ctx.finish(return_value)
}
/// Handles `KERNEL32.dll!GlobalDeleteAtom`.
pub fn handle_global_delete_atom(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let atom_raw = engine
        .read_rcx()
        .context("failed to read RCX for GlobalDeleteAtom")?;

    let atom_low = atom_raw & u64::from(u16::MAX);
    let atom = u16::try_from(atom_low).context("GlobalDeleteAtom identifier does not fit u16")?;

    let was_live = state
        .window_state()
        .global_atoms
        .iter()
        .any(|record| record.atom == atom);

    if was_live {
        state
            .window_state()
            .global_atoms
            .retain(|record| record.atom != atom);

        state.process.last_error = 0;
    } else {
        state.process.last_error = ERROR_INVALID_PARAMETER;
    }

    // GlobalDeleteAtom returns zero on success, otherwise the original atom.
    let return_value = if was_live { 0 } else { u64::from(atom) };

    ctx.finish(return_value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::sync_obj::SyncState;
    use crate::vfs::VolumeConfig;
    use crate::{
        FileHandle, FindFileHandle, GuestHeap, GuestStdinMode, HeapState, KernelState,
        ModuleHandle, ModuleState, ProcessState, RegistryKeyHandle, ResourceHandle, ThreadState,
        WinApiEnvironment, WinApiState,
    };
    use ahash::HashMap;
    use ahash::HashMapExt;
    use std::sync::{Arc, Mutex};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    const STACK_TOP: u64 = 0x100_FF00;

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

    /// Allocate a block through the LocalAlloc handler and return its handle.
    fn alloc_local(engine: &mut IcedCpu, state: &mut WinApiState, size: u64) -> u64 {
        // Initialise the guest heap control bump cursor (0x2000 was attached as
        // ctrl in `test_state`), otherwise `alloc_coherent` sees bump=0 < base.
        engine
            .mem_write(0x2000, &0x2000_u64.to_le_bytes())
            .expect("write heap bump cursor");
        write_regs(engine, 0, size, 0, 0);
        let result =
            handle_local_alloc(&mut HandlerContext::new(engine, test_environment(), state))
                .expect("LocalAlloc should succeed");
        result.return_value
    }

    #[test]
    fn local_lock_returns_pointer_for_live_block() {
        let mut engine = test_engine();
        let mut state = test_state();
        let handle = alloc_local(&mut engine, &mut state, 64);
        assert_ne!(handle, 0, "LocalAlloc must yield a handle");

        write_regs(&mut engine, handle, 0, 0, 0);
        let result = handle_local_lock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("LocalLock should succeed");
        // Flat heap: the locked pointer IS the handle.
        assert_eq!(result.return_value, handle);

        write_regs(&mut engine, handle, 0, 0, 0);
        let result = handle_local_unlock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("LocalUnlock should succeed");
        assert_eq!(result.return_value, 1, "unlock of a live handle is TRUE");
    }

    #[test]
    fn local_lock_zero_returns_null() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_regs(&mut engine, 0, 0, 0, 0);
        let result = handle_local_lock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("LocalLock should succeed");
        assert_eq!(result.return_value, 0, "NULL handle locks to NULL");
    }

    #[test]
    fn local_lock_freed_block_returns_null() {
        let mut engine = test_engine();
        let mut state = test_state();
        let handle = alloc_local(&mut engine, &mut state, 64);
        assert_ne!(handle, 0);

        write_regs(&mut engine, handle, 0, 0, 0);
        handle_local_free(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("LocalFree should succeed");

        write_regs(&mut engine, handle, 0, 0, 0);
        let result = handle_local_lock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("LocalLock should succeed");
        assert_eq!(result.return_value, 0, "freed block locks to NULL");
    }

    #[test]
    fn local_unlock_invalid_handle_returns_false() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_regs(&mut engine, 0xdead_beef, 0, 0, 0);
        let result = handle_local_unlock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("LocalUnlock should succeed");
        assert_eq!(result.return_value, 0, "invalid handle unlocks to FALSE");
        assert_eq!(state.process.last_error, ERROR_INVALID_HANDLE);
    }

    /// The `EM_GETHANDLE` → `LocalLock` → `LocalUnlock` sequence notepad's save
    /// path runs: the lock must hand back the handle for a live edit buffer.
    #[test]
    fn lock_unlock_round_trip_keeps_block_live() {
        let mut engine = test_engine();
        let mut state = test_state();
        let handle = alloc_local(&mut engine, &mut state, 128);
        assert_ne!(handle, 0);

        write_regs(&mut engine, handle, 0, 0, 0);
        let locked = handle_local_lock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("LocalLock should succeed")
        .return_value;
        assert_eq!(locked, handle);

        // The block stays live (size preserved) after lock+unlock.
        write_regs(&mut engine, handle, 0, 0, 0);
        let unlocked = handle_local_unlock(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("LocalUnlock should succeed")
        .return_value;
        assert_eq!(unlocked, 1);
        assert!(state.heap_state.heap.is_live(handle));
    }
}
