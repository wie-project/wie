use super::{
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY,
    HandlerContext, Result, WinApiHandlerResult, ret_bool_true, ret_u64, write_guest_u32,
};
use crate::guest_memory::read_u64;
use anyhow::Context;

/// `MEM_COMMIT` (VirtualAlloc type bit).
const MEM_COMMIT: u32 = 0x1000;
/// `MEM_RESERVE` (VirtualAlloc type bit).
const MEM_RESERVE: u32 = 0x2000;
/// `MEM_RELEASE` (VirtualFree type bit).
const MEM_RELEASE: u32 = 0x8000;
/// `PAGE_READWRITE` (page protection).
const PAGE_READWRITE: u32 = 0x04;
/// Byte size of `MEMORY_BASIC_INFORMATION` (winnt.h, Win64).
const MEMORY_BASIC_INFORMATION_SIZE: u64 = 48;

/// Handles `KERNEL32.dll!CreateFileMappingW`.
///
/// Creates a file-mapping object bound to an open file handle. The mapping
/// records the file's guest path and size; `MapViewOfFile` later copies the
/// file's bytes into guest memory (notepad reads opened files exclusively
/// through a mapped view — it imports no ReadFile).
pub fn handle_create_file_mapping_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let file_handle = engine
        .read_rcx()
        .context("failed to read RCX for CreateFileMappingW")?;
    // lpAttributes (RDX) and flProtect (R8) are ignored: WIE mapping views
    // are host-copied, so no host security descriptor or page protection is
    // needed.
    let _ = (engine.read_rdx()?, engine.read_r8()?);
    let size_high = engine
        .read_r9()
        .context("failed to read R9 for CreateFileMappingW")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for CreateFileMappingW")?;
    let size_low = read_u64(engine, rsp.wrapping_add(0x28))
        .context("failed to read dwMaximumSizeLow for CreateFileMappingW")?;

    let Some(open_file) = state.file_io.open_files.get(&file_handle) else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        return ret_u64(engine, 0, "CreateFileMappingW");
    };
    let guest_path = open_file.path.to_string();
    let file_size = open_file.size();
    // dwMaximumSize 0 = the file's current size (the documented contract);
    // a nonzero size overrides it.
    let size = if size_high == 0 && size_low == 0 {
        file_size
    } else {
        (size_high << 32) | (size_low & 0xffff_ffff)
    };

    let (handle, _mapping) = state.kernel.sync.register_file_mapping(guest_path, size);
    state.process.last_error = 0;
    tracing::info!(file_handle, size, handle, "CreateFileMappingW");
    ret_u64(engine, handle, "CreateFileMappingW")
}

/// Handles `KERNEL32.dll!MapViewOfFile`.
///
/// Copies the mapped file's bytes (at `dwFileOffsetHigh/Low`) into a fresh
/// guest `VirtualAlloc` region and returns its VA. `dwNumberOfBytesToMap == 0`
/// maps through the end of the file. The view's guest VA is what the guest
/// reads the file content from; `UnmapViewOfFile` frees it.
pub fn handle_map_view_of_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let mapping_handle = engine
        .read_rcx()
        .context("failed to read RCX for MapViewOfFile")?;
    let _desired_access = engine
        .read_rdx()
        .context("failed to read RDX for MapViewOfFile")?;
    let offset_high = engine
        .read_r8()
        .context("failed to read R8 for MapViewOfFile")?;
    let offset_low = engine
        .read_r9()
        .context("failed to read R9 for MapViewOfFile")?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for MapViewOfFile")?;
    let num_bytes = read_u64(engine, rsp.wrapping_add(0x28))
        .context("failed to read dwNumberOfBytesToMap for MapViewOfFile")?;

    let Some(crate::KernelObject::FileMapping(mapping)) =
        state.kernel.sync.object(mapping_handle).cloned()
    else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        return ret_u64(engine, 0, "MapViewOfFile");
    };

    // Resolve the mapped file's current bytes (the file may have been written
    // through the same open handle after the mapping was created).
    let bytes =
        crate::kernel32::resolve_guest_file_bytes(state, &mapping.guest_path).unwrap_or_default();
    let file_len = u64::try_from(bytes.len()).unwrap_or(0);
    let offset = (offset_high << 32) | (offset_low & 0xffff_ffff);
    if offset > file_len {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ret_u64(engine, 0, "MapViewOfFile");
    }
    let view_len = if num_bytes == 0 {
        file_len - offset
    } else {
        num_bytes.min(file_len - offset)
    };
    let view_len_usize = usize::try_from(view_len).unwrap_or(0);
    if view_len_usize == 0 {
        state.process.last_error = ERROR_NOT_ENOUGH_MEMORY;
        return ret_u64(engine, 0, "MapViewOfFile");
    }

    // Allocate a private guest region and copy the file's bytes into it.
    let va = engine
        .virtual_alloc(0, view_len_usize, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
        .map_err(|e| anyhow::anyhow!("MapViewOfFile guest allocation failed: {e}"))?;
    let offset_usize = usize::try_from(offset).unwrap_or(0);
    let end = offset_usize
        .checked_add(view_len_usize)
        .context("MapViewOfFile byte range overflow")?;
    let slice = bytes
        .get(offset_usize..end)
        .context("MapViewOfFile byte slice out of range")?;
    engine
        .mem_write(va, slice)
        .context("failed to copy mapped file bytes into guest memory")?;
    state.process.last_error = 0;
    tracing::info!(mapping_handle, va, view_len, "MapViewOfFile");
    ret_u64(engine, va, "MapViewOfFile")
}

/// Handles `KERNEL32.dll!UnmapViewOfFile`.
///
/// Frees the guest region `MapViewOfFile` returned. Best-effort: an unknown
/// base address is not an error (the guest-visible return is always TRUE).
pub fn handle_unmap_view_of_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let base = engine
        .read_rcx()
        .context("failed to read RCX for UnmapViewOfFile")?;
    let _unused = engine.virtual_free(base, 0, MEM_RELEASE);
    ret_bool_true(engine, "UnmapViewOfFile")
}
pub fn handle_open_file_mapping(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _ = (engine.read_rcx()?, engine.read_rdx()?, engine.read_r8()?);
    state.process.last_error = ERROR_FILE_NOT_FOUND;
    ret_u64(engine, 0, "OpenFileMapping")
}
pub(crate) fn handle_virtual_alloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let addr = engine.read_rcx()?;
    let size = engine.read_rdx()?;
    let alloc_type = u32::try_from(engine.read_r8()? & 0xffff_ffff).unwrap_or(0);
    let protect = u32::try_from(engine.read_r9()? & 0xffff_ffff).unwrap_or(0);
    let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
    if size_usize == usize::MAX {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    match engine.virtual_alloc(addr, size_usize, alloc_type, protect) {
        Ok(base) => {
            state.process.last_error = 0;
            tracing::debug!(addr, size, alloc_type, protect, base, "VirtualAlloc ok");
            ctx.finish(base)
        }
        Err(e) => {
            state.process.last_error =
                wie_cpu::win32_from_cpu_error(&e).unwrap_or(ERROR_INVALID_PARAMETER);
            tracing::debug!(
                addr,
                size,
                alloc_type,
                protect,
                error = %e,
                last_error = state.process.last_error,
                "VirtualAlloc failed"
            );
            ctx.finish(0)
        }
    }
}
pub(crate) fn handle_virtual_free(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let addr = engine.read_rcx()?;
    let size = engine.read_rdx()?;
    let free_type = u32::try_from(engine.read_r8()? & 0xffff_ffff).unwrap_or(0);
    let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
    if size_usize == usize::MAX {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    match engine.virtual_free(addr, size_usize, free_type) {
        Ok(()) => {
            state.process.last_error = 0;
            ctx.finish(1)
        }
        Err(e) => {
            state.process.last_error =
                wie_cpu::win32_from_cpu_error(&e).unwrap_or(ERROR_INVALID_PARAMETER);
            ctx.finish(0)
        }
    }
}
pub(crate) fn handle_virtual_protect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let addr = engine.read_rcx()?;
    let size = engine.read_rdx()?;
    let new_protect = u32::try_from(engine.read_r8()? & 0xffff_ffff).unwrap_or(0);
    let old_prot = engine.read_r9()?;
    // Microsoft Learn: if lpflOldProtect is NULL or invalid, the function fails.
    if old_prot == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
    if size_usize == 0 || size_usize == usize::MAX {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    match engine.virtual_protect(addr, size_usize, new_protect) {
        Ok(old) => {
            write_guest_u32(engine, old_prot, old)?;
            state.process.last_error = 0;
            ctx.finish(1)
        }
        Err(e) => {
            state.process.last_error =
                wie_cpu::win32_from_cpu_error(&e).unwrap_or(ERROR_INVALID_PARAMETER);
            ctx.finish(0)
        }
    }
}
pub(crate) fn handle_virtual_query(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let address = engine.read_rcx()?;
    let buffer = engine.read_rdx()?;
    let length = engine.read_r8()?;

    if buffer == 0 || length < MEMORY_BASIC_INFORMATION_SIZE {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }

    let mbi = engine.virtual_query(address);
    let bytes = mbi.to_bytes();
    engine.mem_write(buffer, &bytes)?;
    state.process.last_error = 0;
    ctx.finish(MEMORY_BASIC_INFORMATION_SIZE)
}
