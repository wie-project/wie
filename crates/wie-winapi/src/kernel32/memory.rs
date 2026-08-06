use super::{
    ERROR_INVALID_PARAMETER, HandlerContext, Result, WinApiHandlerResult, ret_bool_true, ret_u64,
    write_guest_u32,
};

pub fn handle_map_view_of_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _ = (
        engine.read_rcx()?,
        engine.read_rdx()?,
        engine.read_r8()?,
        engine.read_r9()?,
    );
    state.process.last_error = 8; // ERROR_NOT_ENOUGH_MEMORY
    ret_u64(engine, 0, "MapViewOfFile")
}
pub fn handle_unmap_view_of_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _base = engine.read_rcx()?;
    ret_bool_true(engine, "UnmapViewOfFile")
}
pub fn handle_open_file_mapping(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _ = (engine.read_rcx()?, engine.read_rdx()?, engine.read_r8()?);
    state.process.last_error = 2; // ERROR_FILE_NOT_FOUND
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

    if buffer == 0 || length < 48 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }

    let mbi = engine.virtual_query(address);
    let bytes = mbi.to_bytes();
    engine.mem_write(buffer, &bytes)?;
    state.process.last_error = 0;
    ctx.finish(48)
}
