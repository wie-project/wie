use super::{
    CREATE_SUSPENDED, Context, DEFAULT_MT_MAX_THREADS, DEFAULT_WORKER_STACK, ERROR_INVALID_HANDLE,
    ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY, ERROR_NOT_SUPPORTED_MT,
    FAKE_CURRENT_PROCESS_ID, FIXED_SYSTEM_FILETIME, HandlerContext, MEM_COMMIT, MEM_RESERVE,
    PAGE_READWRITE, Result, THREAD_ENTRY_HOME_AND_RET, WORKER_STACK_REGION_BASE,
    WORKER_STACK_STRIDE, WinApiHandlerResult, WinApiState, checked_address, checked_field_address,
    read_create_file_stack_u32, read_guest_u64, write_guest_u16, write_guest_u32, write_guest_u64,
};

/// Handles `KERNEL32.dll!GetStartupInfoA`.
pub fn handle_get_startup_info_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let startup_info_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetStartupInfoA")?;

    if startup_info_ptr != 0 {
        let cb_address = checked_field_address(startup_info_ptr, 0, "cb");
        let flags_address = checked_field_address(startup_info_ptr, 60, "dwFlags");
        let show_window_address = checked_field_address(startup_info_ptr, 64, "wShowWindow");

        // STARTUPINFOA on Win64 is 104 bytes.
        write_guest_u32(engine, cb_address, 104)?;
        write_guest_u32(engine, flags_address, 0)?;
        write_guest_u16(engine, show_window_address, 1)?;
    }

    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from GetStartupInfoA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `KERNEL32.dll!GetStartupInfoW`.
pub fn handle_get_startup_info_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let startup_info_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetStartupInfoW")?;

    if startup_info_ptr != 0 {
        let cb_address = checked_field_address(startup_info_ptr, 0, "cb");
        let flags_address = checked_field_address(startup_info_ptr, 60, "dwFlags");
        let show_window_address = checked_field_address(startup_info_ptr, 64, "wShowWindow");

        // STARTUPINFOW on Win64 is 104 bytes.
        write_guest_u32(engine, cb_address, 104)?;
        write_guest_u32(engine, flags_address, 0)?;
        write_guest_u16(engine, show_window_address, 1)?;
    }

    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from GetStartupInfoW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `KERNEL32.dll!GetProcessHeap`.
pub fn handle_get_process_heap(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let process_heap_handle = ctx.environment.process_heap_handle;
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(process_heap_handle)
        .context("failed to return from GetProcessHeap")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: process_heap_handle,
    })
}
/// Handles `KERNEL32.dll!GetSystemTimeAsFileTime`.
pub fn handle_get_system_time_as_file_time(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let filetime_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetSystemTimeAsFileTime")?;

    if filetime_ptr != 0 {
        let low_address = checked_field_address(filetime_ptr, 0, "dwLowDateTime");
        let high_address = checked_field_address(filetime_ptr, 4, "dwHighDateTime");

        // B5: matches the wall-clock FILETIME the host publishes into the guest
        // clock table (slot 3), which the in-guest stub copies verbatim.
        let filetime = super::clock::system_time_filetime();
        let low =
            u32::try_from(filetime & 0xffff_ffff).context("FILETIME low part does not fit u32")?;
        let high = u32::try_from(filetime >> 32).context("FILETIME high part does not fit u32")?;

        write_guest_u32(engine, low_address, low)?;
        write_guest_u32(engine, high_address, high)?;
    }

    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from GetSystemTimeAsFileTime")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `KERNEL32.dll!GetCurrentProcessId`.
pub fn handle_get_current_process_id(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(FAKE_CURRENT_PROCESS_ID)
        .context("failed to return from GetCurrentProcessId")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: FAKE_CURRENT_PROCESS_ID,
    })
}
/// Handles `KERNEL32.dll!GetCurrentThreadId`.
pub fn handle_get_current_thread_id(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let tid = u64::from(state.kernel.threads.current_tid());
    let return_address = engine
        .return_from_win64_api(tid)
        .context("failed to return from GetCurrentThreadId")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: tid,
    })
}
/// Handles `KERNEL32.dll!GetCurrentProcess`.
pub fn handle_get_current_process(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Windows pseudohandle for the current process: (HANDLE)-1.
    let return_value = u64::MAX;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetCurrentProcess")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub(crate) fn ret_bool_true(
    engine: &mut dyn wie_cpu::CpuEngine,
    api: &str,
) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(1)
        .with_context(|| format!("failed to return from {api}"))?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub(crate) fn ret_u64(
    engine: &mut dyn wie_cpu::CpuEngine,
    value: u64,
    api: &str,
) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .with_context(|| format!("failed to return from {api}"))?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}
pub fn handle_get_process_times(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process = engine.read_rcx().context("GetProcessTimes RCX")?;
    let creation = engine.read_rdx().context("GetProcessTimes RDX")?;
    let exit_t = engine.read_r8().context("GetProcessTimes R8")?;
    let kernel = engine.read_r9().context("GetProcessTimes R9")?;
    let rsp = engine.read_rsp().context("GetProcessTimes RSP")?;
    let user = read_guest_u64(
        engine,
        checked_address(rsp, 0x28, "GetProcessTimes lpUserTime"),
    )?;
    // Fixed synthetic times (100-ns ticks).
    if creation != 0 {
        write_guest_u64(engine, creation, FIXED_SYSTEM_FILETIME)?;
    }
    if exit_t != 0 {
        write_guest_u64(engine, exit_t, 0)?;
    }
    if kernel != 0 {
        write_guest_u64(engine, kernel, 10_000_000)?;
    }
    if user != 0 {
        write_guest_u64(engine, user, 20_000_000)?;
    }
    ret_bool_true(engine, "GetProcessTimes")
}
pub fn handle_get_process_affinity_mask(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process = engine.read_rcx()?;
    let proc_mask = engine.read_rdx()?;
    let sys_mask = engine.read_r8()?;
    if proc_mask != 0 {
        write_guest_u64(engine, proc_mask, 1)?;
    }
    if sys_mask != 0 {
        write_guest_u64(engine, sys_mask, 1)?;
    }
    ret_bool_true(engine, "GetProcessAffinityMask")
}
pub fn handle_set_process_affinity_mask(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process = engine.read_rcx()?;
    let _mask = engine.read_rdx()?;
    ret_bool_true(engine, "SetProcessAffinityMask")
}
pub fn handle_set_thread_affinity_mask(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _thread = engine.read_rcx()?;
    let _mask = engine.read_rdx()?;
    ret_u64(engine, 1, "SetThreadAffinityMask")
}
pub fn handle_resume_thread(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    // Previous suspend count: 1 if we had it suspended, 0 if already running, -1 on error.
    if let Some(spawn) = state.kernel.sync.suspended_spawns.remove(&handle) {
        if std::env::var_os("WIE_MT_DEBUG").is_some() {
            let pending_after = state.kernel.sync.pending_spawns.len().saturating_add(1);
            eprintln!(
                "[mt] ResumeThread handle={handle:#x} tid={:#x} pending→{pending_after}",
                spawn.tid,
            );
        }
        state.kernel.sync.pending_spawns.push(spawn);
        state.process.last_error = 0;
        return ret_u64(engine, 1, "ResumeThread");
    }
    if state.kernel.sync.thread_by_handle(handle).is_some() {
        // Already running (or finished) — suspend count was 0.
        state.process.last_error = 0;
        return ret_u64(engine, 0, "ResumeThread");
    }
    state.process.last_error = ERROR_INVALID_HANDLE;
    // `(DWORD)-1`
    ret_u64(engine, u64::from(u32::MAX), "ResumeThread")
}
/// Handles `KERNEL32.dll!OpenThread` — look up a thread by TID and return a handle.
pub fn handle_open_thread(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _desired = engine.read_rcx()?;
    let _inherit = engine.read_rdx()?;
    let tid = engine.read_r8()?;
    let tid_u32 = u32::try_from(tid & 0xffff_ffff).unwrap_or(u32::MAX);
    // Look for an existing thread object with this TID.
    let found = state
        .kernel
        .sync
        .objects
        .values()
        .find_map(|obj| match obj {
            crate::KernelObject::Thread(t) if t.tid == tid_u32 => Some(t.handle),
            _ => None,
        });
    if let Some(handle) = found {
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(handle)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: handle,
        });
    }
    // Thread not found — create a fresh thread object.
    let (handle, _) = state
        .kernel
        .sync
        .register_thread(tid_u32, wie_cpu::ThreadContext::default());
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(handle)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
/// Handles `KERNEL32.dll!CreateJobObjectW` — return handle tracked in sync state.
pub fn handle_create_job_object_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _sec = engine.read_rcx()?;
    let _name = engine.read_rdx()?;
    let handle = state.kernel.sync.next_handle.as_u64();
    state.kernel.sync.next_handle =
        crate::KernelHandle::from(state.kernel.sync.next_handle.as_u64().wrapping_add(4));
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(handle)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
/// Handles `KERNEL32.dll!CreateJobObjectA` — return handle tracked in sync state.
pub fn handle_create_job_object_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _sec = engine.read_rcx()?;
    let _name = engine.read_rdx()?;
    let handle = state.kernel.sync.next_handle.as_u64();
    state.kernel.sync.next_handle =
        crate::KernelHandle::from(state.kernel.sync.next_handle.as_u64().wrapping_add(4));
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(handle)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
/// Handles `KERNEL32.dll!AssignProcessToJobObject` — return TRUE (tracked in state).
pub fn handle_assign_process_to_job_object(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _job = engine.read_rcx()?;
    let _proc = engine.read_rdx()?;
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!TerminateProcess` — signal process exit.
pub fn handle_terminate_process(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _handle = engine.read_rcx()?;
    let _code = engine.read_rdx()?;
    state.kernel.sync.process_dying = true;
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!TerminateThread` — signal thread exit.
pub fn handle_terminate_thread(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let code_raw = engine.read_rdx()?;
    let code = u32::try_from(code_raw & 0xffff_ffff).unwrap_or(0);
    // Find the thread and mark it finished.
    if let Some(crate::KernelObject::Thread(t)) = state
        .kernel
        .sync
        .objects
        .get(&crate::KernelHandle::from(handle))
    {
        t.finish(code);
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(1)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 1,
        });
    }
    state.process.last_error = ERROR_INVALID_HANDLE;
    let return_address = engine.return_from_win64_api(0)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `KERNEL32.dll!SuspendThread` — track suspend count.
pub fn handle_suspend_thread(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    // Find the thread TID from the handle.
    let tid = state
        .kernel
        .sync
        .objects
        .values()
        .find_map(|obj| match obj {
            crate::KernelObject::Thread(t) if t.handle == handle => Some(t.tid),
            _ => None,
        });
    let Some(tid) = tid else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(u64::MAX)?; // THREAD_PRIORITY_ERROR_RETURN
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: u64::MAX,
        });
    };
    let prev = state
        .process
        .suspended_threads
        .get(&tid)
        .copied()
        .unwrap_or(0);
    state
        .process
        .suspended_threads
        .insert(tid, prev.saturating_add(1));
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(u64::from(prev))?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(prev),
    })
}
pub(crate) fn mt_create_thread_enabled() -> bool {
    !matches!(
        std::env::var("WIE_MT"),
        Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")
    )
}
pub(crate) fn mt_max_worker_threads() -> u32 {
    std::env::var("WIE_MT_MAX_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MT_MAX_THREADS)
}
pub(crate) fn trunc_i32(reg: u64) -> i32 {
    i32::from_le_bytes(u32::try_from(reg & 0xffff_ffff).unwrap_or(0).to_le_bytes())
}
pub(crate) fn i32_to_rax(v: i32) -> u64 {
    // Preserve full sign-extended bit pattern in the 64-bit register.
    u64::from_le_bytes(i64::from(v).to_le_bytes())
}
pub(crate) fn i64_to_rax(v: i64) -> u64 {
    u64::from_le_bytes(v.to_le_bytes())
}
pub(crate) fn handle_create_thread(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _security = engine.read_rcx()?;
    let stack_size_raw = engine.read_rdx()?;
    let start = engine.read_r8()?;
    let param = engine.read_r9()?;
    let flags = read_create_file_stack_u32(engine, 0x28).unwrap_or(0);
    let tid_out = read_stack_u64(engine, 0x30).unwrap_or(0);

    let handle = create_guest_thread(engine, state, stack_size_raw, start, param, flags, tid_out)?;
    let return_address = engine.return_from_win64_api(handle)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle,
    })
}
pub fn create_guest_thread(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    stack_size_raw: u64,
    start: u64,
    param: u64,
    flags: u32,
    tid_out: u64,
) -> Result<u64> {
    if !mt_create_thread_enabled() {
        state.process.last_error = ERROR_NOT_SUPPORTED_MT;
        return Ok(0);
    }

    if start == 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return Ok(0);
    }

    // Cap workers: by_tid includes primary; count pending + suspended too.
    let live_workers = state
        .kernel
        .threads
        .by_tid
        .len()
        .saturating_sub(1)
        .saturating_add(state.kernel.sync.pending_spawns.len())
        .saturating_add(state.kernel.sync.suspended_spawns.len());
    let max = usize::try_from(mt_max_worker_threads()).unwrap_or(64);
    if live_workers >= max {
        state.process.last_error = ERROR_NOT_ENOUGH_MEMORY;
        return Ok(0);
    }

    let stack_size = if stack_size_raw == 0 {
        DEFAULT_WORKER_STACK
    } else {
        usize::try_from(stack_size_raw).unwrap_or(DEFAULT_WORKER_STACK)
    };
    // Align up to page.
    let stack_size = stack_size.saturating_add(0xfff) & !0xfff;
    let stack_size = stack_size.max(0x1000);

    let slot = state.kernel.sync.next_stack_slot;
    state.kernel.sync.next_stack_slot = slot.saturating_add(1);
    let stack_base = WORKER_STACK_REGION_BASE
        .saturating_add(u64::from(slot).saturating_mul(WORKER_STACK_STRIDE));

    let alloc = engine.virtual_alloc(
        stack_base,
        stack_size,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    let stack_base = if let Ok(va) = alloc {
        va
    } else {
        // Fallback: map with mem_map if VirtualAlloc path rejects fixed VA.
        if engine
            .mem_map(stack_base, stack_size, wie_cpu::RwxPerms::READ_WRITE)
            .is_err()
        {
            state.process.last_error = ERROR_NOT_ENOUGH_MEMORY;
            return Ok(0);
        }
        stack_base
    };

    let stack_top = stack_base.saturating_add(u64::try_from(stack_size).unwrap_or(0));
    let aligned_top = stack_top & !0xF_u64;
    // Windows x64 thread entry / CALL ABI:
    //   [RSP+0x00]       = return address
    //   [RSP+0x08..0x28) = caller home space (32 bytes) — callees may store rbx/rdi there
    //                      via `mov [rsp+0x40], reg` after `sub rsp, 0x28`
    //   RSP % 16 == 8
    // Retaddr 0 → worker loop treats RIP=0 as natural exit (exit code from RAX).
    // Without the home space, prologues that spill into it fault just past stack_top
    // (seen as worker invalid_memory at stack_top+0x10 on 7za LZMA2 thread procs).
    let entry_rsp =
        aligned_top.saturating_sub(u64::try_from(THREAD_ENTRY_HOME_AND_RET).unwrap_or(0x28));
    // Zero home space + retaddr slot so stale data cannot look like live pointers.
    let entry_frame = [0_u8; THREAD_ENTRY_HOME_AND_RET];
    drop(engine.mem_write(entry_rsp, &entry_frame));

    let tid = state.kernel.threads.alloc_worker();
    let mut ctx = wie_cpu::ThreadContext::new();
    // RCX = lpParameter, RSP = entry, RIP = start
    if let Some(slot) = ctx.gpr.get_mut(1) {
        *slot = param;
    }
    if let Some(slot) = ctx.gpr.get_mut(4) {
        *slot = entry_rsp;
    }
    ctx.rip = start;

    let (handle, _obj) = state.kernel.sync.register_thread(tid, ctx);
    let spawn = crate::PendingSpawn {
        tid,
        handle,
        start_address: start,
        parameter: param,
        stack_base,
        stack_size,
    };
    if (flags & CREATE_SUSPENDED) != 0 {
        state.kernel.sync.suspended_spawns.insert(handle, spawn);
    } else {
        state.kernel.sync.pending_spawns.push(spawn);
    }

    if tid_out != 0 {
        drop(engine.mem_write(tid_out, &tid.to_le_bytes()));
    }

    if std::env::var_os("WIE_MT_DEBUG").is_some() {
        eprintln!(
            "[mt] CreateThread tid={tid:#x} handle={handle:#x} start={start:#x} param={param:#x} flags={flags:#x} suspended={} pending={} active_tid={:#x}",
            (flags & CREATE_SUSPENDED) != 0,
            state.kernel.sync.pending_spawns.len(),
            state.kernel.threads.current_tid(),
        );
    }

    state.process.last_error = 0;
    Ok(handle)
}
pub(crate) fn read_stack_u64(engine: &mut dyn wie_cpu::CpuEngine, offset: u64) -> Result<u64> {
    let rsp = engine.read_rsp()?;
    let address = rsp
        .checked_add(offset)
        .context("stack arg address overflow")?;
    let mut bytes = [0_u8; 8];
    engine.mem_read(address, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
pub(crate) fn handle_exit_thread(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let code_raw = engine.read_rcx()?;
    let code = u32::try_from(code_raw & u64::from(u32::MAX)).unwrap_or(0);
    let tid = state.kernel.threads.current_tid();
    // Find thread object by tid.
    for obj in state.kernel.sync.objects.values() {
        if let crate::KernelObject::Thread(t) = obj
            && t.tid == tid
        {
            t.finish(code);
            break;
        }
    }
    // Primary ExitThread: treat as process exit of this code for simplicity.
    if tid == crate::PRIMARY_THREAD_ID {
        // Still return control signal so runtime can tear down.
        return Err(crate::WinApiControlSignal::ExitThread { code }.into());
    }
    Err(crate::WinApiControlSignal::ExitThread { code }.into())
}
pub(crate) fn handle_get_exit_code_thread(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let out_ptr = engine.read_rdx()?;
    let code = if let Some(t) = state.kernel.sync.thread_by_handle(handle) {
        t.exit_code.load(std::sync::atomic::Ordering::Acquire)
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    };
    if out_ptr != 0 {
        drop(engine.mem_write(out_ptr, &code.to_le_bytes()));
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub(crate) fn handle_get_thread_priority(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let h_thread = engine.read_rcx()?;

    // Pseudohandle CURRENT_THREAD (-2), or a real kernel handle.
    let valid = h_thread == u64::MAX - 1
        || state
            .kernel
            .sync
            .objects
            .contains_key(&crate::KernelHandle::from(h_thread));

    if !valid {
        state.process.last_error = ERROR_INVALID_HANDLE;
        // THREAD_PRIORITY_ERROR_RETURN = MAXLONG (0x7FFFFFFF)
        let return_address = engine.return_from_win64_api(0x7FFF_FFFF)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0x7FFF_FFFF,
        });
    }

    let return_address = engine.return_from_win64_api(0)?; // THREAD_PRIORITY_NORMAL
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
pub(crate) fn handle_get_current_thread(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // CURRENT_THREAD_PSEUDO_HANDLE = (HANDLE)-2
    let return_value = u64::MAX - 1;
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
