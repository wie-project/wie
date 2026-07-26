use super::{
    Context, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, GlobalAtomRecord, HEAP_SIZE_FAILURE,
    HEAP_ZERO_MEMORY, Result, WinApiHandlerResult, WinApiState, allocate_fake_heap_block,
    read_ansi_string_from_cpu, ret_bool_true, ret_u64,
};

/// Handles `KERNEL32.dll!HeapAlloc`.
pub fn handle_heap_alloc(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
            {
                if let Some(host) = engine.host_span(addr, len, true) {
                    // SAFETY: host_span validated a single-arena writable span;
                    // engine borrow is exclusive so no concurrent guest write races.
                    #[allow(unsafe_code)]
                    unsafe {
                        std::ptr::write_bytes(host, 0, len);
                    }
                } else {
                    let zeros = vec![0_u8; len];
                    engine.mem_write(addr, &zeros)?;
                }
            }
        }
        addr
    };

    let return_address = engine.return_from_win64_api(return_value)?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!HeapFree`.
pub fn handle_heap_free(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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

    let return_address = engine.return_from_win64_api(return_value)?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!HeapReAlloc`.
pub fn handle_heap_realloc(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
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
                    .mem_read(memory.wrapping_sub(8), &mut hb)
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
                let src_host = engine.host_span(memory, copy_len, false);
                let dst_host = engine.host_span(new_addr, copy_len, true);
                let overlap = match (src_host, dst_host) {
                    (Some(s), Some(d)) => {
                        let se = s.wrapping_add(copy_len);
                        let de = d.wrapping_add(copy_len);
                        s < de && d < se
                    }
                    _ => true,
                };
                if let (Some(s), Some(d)) = (src_host, dst_host)
                    && !overlap
                {
                    // SAFETY: both spans validated; blocks come from GuestHeap
                    // arenas that never overlap between distinct allocations.
                    #[allow(unsafe_code)]
                    unsafe {
                        std::ptr::copy_nonoverlapping(s, d, copy_len);
                    }
                } else {
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
                    if let Some(host) = engine.host_span(dst_addr, zero_len, true) {
                        // SAFETY: host_span validated writable span; exclusive engine borrow.
                        #[allow(unsafe_code)]
                        unsafe {
                            std::ptr::write_bytes(host, 0, zero_len);
                        }
                    } else {
                        let zeros = vec![0_u8; zero_len];
                        engine.mem_write(dst_addr, &zeros)?;
                    }
                }
            }
            let _ = state.heap_state.heap.free_coherent(engine, memory);
            new_addr
        }
    };

    let return_address = engine.return_from_win64_api(return_value)?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!HeapCreate`.
pub fn handle_heap_create(
    engine: &mut dyn wie_cpu::CpuEngine,
    process_heap_handle: u64,
) -> Result<WinApiHandlerResult> {
    let _options = engine
        .read_rcx()
        .context("failed to read RCX for HeapCreate")?;

    let _initial_size = engine
        .read_rdx()
        .context("failed to read RDX for HeapCreate")?;

    let _maximum_size = engine
        .read_r8()
        .context("failed to read R8 for HeapCreate")?;

    let return_address = engine
        .return_from_win64_api(process_heap_handle)
        .context("failed to return from HeapCreate")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: process_heap_handle,
    })
}
/// Handles `KERNEL32.dll!HeapSetInformation`.
pub fn handle_heap_set_information(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let _heap_handle = engine
        .read_rcx()
        .context("failed to read RCX for HeapSetInformation")?;

    let _heap_information_class = engine
        .read_rdx()
        .context("failed to read RDX for HeapSetInformation")?;

    let _heap_information = engine
        .read_r8()
        .context("failed to read R8 for HeapSetInformation")?;

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from HeapSetInformation")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!HeapSize`.
pub fn handle_heap_size(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
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
                .mem_read(memory.wrapping_sub(8), &mut hb)
                .ok()
                .map(|()| u64::from_le_bytes(hb))
                .filter(|&s| s != 0)
        })
        .unwrap_or(HEAP_SIZE_FAILURE);

    let return_address = engine.return_from_win64_api(return_value)?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GlobalMemoryStatus`.
pub fn handle_global_memory_status(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
    let memory_status_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GlobalMemoryStatus")?;

    if memory_status_ptr != 0 {
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
        let mut buf = [0_u8; 56];
        buf[0..4].copy_from_slice(&56_u32.to_le_bytes());
        buf[4..8].copy_from_slice(&25_u32.to_le_bytes());
        buf[8..16].copy_from_slice(&(8_u64 * 1024 * 1024 * 1024).to_le_bytes());
        buf[16..24].copy_from_slice(&(6_u64 * 1024 * 1024 * 1024).to_le_bytes());
        buf[24..32].copy_from_slice(&(16_u64 * 1024 * 1024 * 1024).to_le_bytes());
        buf[32..40].copy_from_slice(&(12_u64 * 1024 * 1024 * 1024).to_le_bytes());
        buf[40..48].copy_from_slice(&(128_u64 * 1024 * 1024 * 1024).to_le_bytes());
        buf[48..56].copy_from_slice(&(120_u64 * 1024 * 1024 * 1024).to_le_bytes());
        engine
            .mem_write(memory_status_ptr, &buf)
            .context("failed to write MEMORYSTATUS")?;
    }

    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from GlobalMemoryStatus")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
pub fn handle_global_memory_status_ex(
    engine: &mut dyn wie_cpu::CpuEngine,
) -> Result<WinApiHandlerResult> {
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
    let mut buf = [0_u8; 64];
    buf[0..4].copy_from_slice(&64_u32.to_le_bytes());
    buf[4..8].copy_from_slice(&25_u32.to_le_bytes());
    buf[8..16].copy_from_slice(&(8_u64 * 1024 * 1024 * 1024).to_le_bytes());
    buf[16..24].copy_from_slice(&(6_u64 * 1024 * 1024 * 1024).to_le_bytes());
    buf[24..32].copy_from_slice(&(16_u64 * 1024 * 1024 * 1024).to_le_bytes());
    buf[32..40].copy_from_slice(&(12_u64 * 1024 * 1024 * 1024).to_le_bytes());
    buf[40..48].copy_from_slice(&(128_u64 * 1024 * 1024 * 1024).to_le_bytes());
    buf[48..56].copy_from_slice(&(120_u64 * 1024 * 1024 * 1024).to_le_bytes());
    // ullAvailExtendedVirtual at [56..64] already zero.
    engine
        .mem_write(ptr, &buf)
        .context("failed to write MEMORYSTATUSEX")?;
    ret_bool_true(engine, "GlobalMemoryStatusEx")
}
/// Handles `KERNEL32.dll!LocalAlloc`.
pub fn handle_local_alloc(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _flags = engine
        .read_rcx()
        .context("failed to read RCX for LocalAlloc")?;

    let size = engine
        .read_rdx()
        .context("failed to read RDX for LocalAlloc")?;

    let return_value = allocate_fake_heap_block(engine, state, size);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from LocalAlloc")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!LocalFree`.
pub fn handle_local_free(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for LocalFree")?;

    if memory == 0 {
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from LocalFree")?;

        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let existed = state.heap_state.heap.free_coherent(engine, memory);

    // LocalFree returns NULL on success and the original handle on failure.
    let return_value = if existed { 0 } else { memory };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from LocalFree")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GlobalAlloc`.
pub fn handle_global_alloc(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let _flags = engine
        .read_rcx()
        .context("failed to read RCX for GlobalAlloc")?;

    let size = engine
        .read_rdx()
        .context("failed to read RDX for GlobalAlloc")?;

    let return_value = allocate_fake_heap_block(engine, state, size);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GlobalAlloc")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GlobalFree`.
pub fn handle_global_free(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for GlobalFree")?;

    if memory == 0 {
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return from GlobalFree")?;

        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let existed = state.heap_state.heap.free_coherent(engine, memory);

    // GlobalFree returns NULL on success and the original handle on failure.
    let return_value = if existed { 0 } else { memory };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GlobalFree")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GlobalLock`.
pub fn handle_global_lock(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for GlobalLock")?;

    let return_value = if state.heap_state.heap.is_live(memory) {
        memory
    } else {
        0
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GlobalLock")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GlobalUnlock`.
pub fn handle_global_unlock(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for GlobalUnlock")?;

    let existed = state.heap_state.heap.is_live(memory);

    state.process.last_error = if existed { 0 } else { ERROR_INVALID_HANDLE };

    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GlobalUnlock")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GlobalSize`.
pub fn handle_global_size(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
) -> Result<WinApiHandlerResult> {
    let memory = engine
        .read_rcx()
        .context("failed to read RCX for GlobalSize")?;

    let return_value = state.heap_state.heap.size_of(memory).unwrap_or(0);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GlobalSize")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GlobalAddAtomA`.
pub fn handle_global_add_atom_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let name_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GlobalAddAtomA")?;

    let return_value = if name_ptr == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        0
    } else {
        let name = read_ansi_string_from_cpu(engine, name_ptr, 255)
            .context("failed to read GlobalAddAtomA name")?;

        if name.is_empty() {
            state.process.last_error = ERROR_INVALID_PARAMETER;
            0
        } else if let Some(existing) = state
            .window_state
            .global_atoms
            .iter()
            .find(|record| record.name.eq_ignore_ascii_case(&name))
        {
            state.process.last_error = 0;
            u64::from(existing.atom)
        } else {
            let atom = state.window_state.next_global_atom;

            state.window_state.next_global_atom = state
                .window_state
                .next_global_atom
                .checked_add(1)
                .context("global atom identifier overflow")?;

            state
                .window_state
                .global_atoms
                .push(GlobalAtomRecord { atom, name });
            state.process.last_error = 0;

            u64::from(atom)
        }
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GlobalAddAtomA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GlobalDeleteAtom`.
pub fn handle_global_delete_atom(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let atom_raw = engine
        .read_rcx()
        .context("failed to read RCX for GlobalDeleteAtom")?;

    let atom_low = atom_raw & u64::from(u16::MAX);
    let atom = u16::try_from(atom_low).context("GlobalDeleteAtom identifier does not fit u16")?;

    let existed = state
        .window_state
        .global_atoms
        .iter()
        .any(|record| record.atom == atom);

    if existed {
        state
            .window_state
            .global_atoms
            .retain(|record| record.atom != atom);

        state.process.last_error = 0;
    } else {
        state.process.last_error = ERROR_INVALID_PARAMETER;
    }

    // GlobalDeleteAtom returns zero on success, otherwise the original atom.
    let return_value = if existed { 0 } else { u64::from(atom) };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GlobalDeleteAtom")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
