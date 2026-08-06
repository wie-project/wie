use super::{
    Context, ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER,
    ERROR_TOO_MANY_POSTS, EnterCsResult, HandlerContext, Result, WinApiHandlerResult, WinApiState,
    checked_address, i32_to_rax, i64_to_rax, low_u32, read_u64, ret_u64, trunc_i32,
    write_guest_u32, write_guest_u64,
};

pub(crate) fn write_critical_section_unlocked(
    engine: &mut dyn wie_cpu::CpuEngine,
    critical_section_ptr: u64,
    spin_count: u64,
) -> Result<()> {
    // RTL_CRITICAL_SECTION on Win64 (40 bytes) — built once on the host stack
    // and pushed in a single mem_write. Was six scalar writes (each locking
    // guest memory and page-walking).
    // Layout:
    //   +0x00 DebugInfo      pointer (0)
    //   +0x08 LockCount      LONG, unlocked = -1 (u32::MAX bit pattern)
    //   +0x0c RecursionCount LONG (0)
    //   +0x10 OwningThread   HANDLE (0)
    //   +0x18 LockSemaphore  HANDLE (0)
    //   +0x20 SpinCount      ULONG_PTR
    let mut buf = [0_u8; 40];
    // DebugInfo, RecursionCount, OwningThread, LockSemaphore already zero.
    buf[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    buf[32..40].copy_from_slice(&spin_count.to_le_bytes());
    engine
        .mem_write(critical_section_ptr, &buf)
        .context("failed to write RTL_CRITICAL_SECTION init state")
}
/// Handles `KERNEL32.dll!InitializeCriticalSection`.
pub fn handle_initialize_critical_section(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let critical_section_ptr = engine
        .read_rcx()
        .context("failed to read RCX for InitializeCriticalSection")?;

    if critical_section_ptr != 0 {
        write_critical_section_unlocked(engine, critical_section_ptr, 0)?;
    }

    // void return; RAX is unused but cleared for determinism.
    ctx.finish(0)
}
/// Handles `KERNEL32.dll!EnterCriticalSection` (reentrant; blocks when needed).
pub fn handle_enter_critical_section(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let cs = engine
        .read_rcx()
        .context("failed to read RCX for EnterCriticalSection")?;

    if cs != 0 {
        match try_enter_critical_section_guest(engine, cs, state.kernel.threads.current_tid())? {
            EnterCsResult::Acquired => {}
            EnterCsResult::NeedPark => {
                return Err(crate::WinApiControlSignal::HostPark {
                    reason: crate::HostParkReason::CriticalSection { cs },
                }
                .into());
            }
        }
    }

    ctx.finish(0)
}
/// Handles `KERNEL32.dll!LeaveCriticalSection`.
pub fn handle_leave_critical_section(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let cs = engine
        .read_rcx()
        .context("failed to read RCX for LeaveCriticalSection")?;

    if cs != 0 {
        let unlocked =
            leave_critical_section_guest(engine, cs, state.kernel.threads.current_tid())?;
        if unlocked {
            // Wake one host waiter (if any) parked on this CS.
            if let Some(q) = state.kernel.sync.cs_waiters.get(&cs) {
                q.notify_one();
            }
        }
    }

    ctx.finish(0)
}
/// Handles `KERNEL32.dll!DeleteCriticalSection`.
pub fn handle_delete_critical_section(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let cs = engine
        .read_rcx()
        .context("failed to read RCX for DeleteCriticalSection")?;

    if cs != 0 {
        write_critical_section_unlocked(engine, cs, 0)?;
    }

    ctx.finish(0)
}
pub(crate) fn try_enter_critical_section_guest(
    engine: &mut dyn wie_cpu::CpuEngine,
    cs: u64,
    owner_tid: u32,
) -> Result<EnterCsResult> {
    let lock_va = checked_address(cs, 8, "LockCount");
    let recursion_va = checked_address(cs, 12, "RecursionCount");
    let owner_va = checked_address(cs, 16, "OwningThread");

    let owning = read_u64(engine, owner_va).unwrap_or(0);
    let me = u64::from(owner_tid);

    // Unlocked or recursive re-enter by owner.
    if owning == 0 || owning == me {
        let recursion = if owning == 0 {
            1_u32
        } else {
            let prev = read_guest_u32_cs(engine, recursion_va).unwrap_or(0);
            prev.saturating_add(1)
        };
        let lock_count = if owning == 0 {
            0_u32
        } else {
            let prev = read_guest_u32_cs(engine, lock_va).unwrap_or(0);
            prev.saturating_add(1)
        };
        write_guest_u32(engine, lock_va, lock_count)?;
        write_guest_u32(engine, recursion_va, recursion)?;
        write_guest_u64(engine, owner_va, me)?;
        return Ok(EnterCsResult::Acquired);
    }

    // Contended: park host (session waits on CS queue, then retries Enter).
    Ok(EnterCsResult::NeedPark)
}
pub(crate) fn leave_critical_section_guest(
    engine: &mut dyn wie_cpu::CpuEngine,
    cs: u64,
    owner_tid: u32,
) -> Result<bool> {
    let lock_va = checked_address(cs, 8, "LockCount");
    let recursion_va = checked_address(cs, 12, "RecursionCount");
    let owner_va = checked_address(cs, 16, "OwningThread");

    let owning = read_u64(engine, owner_va).unwrap_or(0);
    let me = u64::from(owner_tid);
    if owning != me {
        // Windows: leaving a CS you do not own is undefined; ignore.
        return Ok(false);
    }

    let recursion = read_guest_u32_cs(engine, recursion_va).unwrap_or(1);
    if recursion <= 1 {
        write_guest_u32(engine, lock_va, u32::MAX)?; // -1 unlocked
        write_guest_u32(engine, recursion_va, 0)?;
        write_guest_u64(engine, owner_va, 0)?;
        Ok(true)
    } else {
        let lock = read_guest_u32_cs(engine, lock_va).unwrap_or(1);
        write_guest_u32(engine, lock_va, lock.saturating_sub(1))?;
        write_guest_u32(engine, recursion_va, recursion.saturating_sub(1))?;
        Ok(false)
    }
}
pub(crate) fn read_guest_u32_cs(engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Option<u32> {
    let mut b = [0_u8; 4];
    engine.mem_read(va, &mut b).ok()?;
    Some(u32::from_le_bytes(b))
}
pub fn handle_initialize_critical_section_and_spin_count(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let critical_section_ptr = engine
        .read_rcx()
        .context("failed to read RCX for InitializeCriticalSectionAndSpinCount")?;

    let spin_count = engine
        .read_rdx()
        .context("failed to read RDX for InitializeCriticalSectionAndSpinCount")?;

    if critical_section_ptr != 0 {
        write_critical_section_unlocked(engine, critical_section_ptr, spin_count)?;
    }

    ctx.finish(1)
}
pub fn handle_create_semaphore(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _attrs = engine.read_rcx()?;
    let initial_raw = engine.read_rdx()?;
    let maximum_raw = engine.read_r8()?;
    let _name = engine.read_r9()?; // named: ignore (anonymous only)
    let initial = i32::from_le_bytes(
        u32::try_from(initial_raw & 0xffff_ffff)
            .unwrap_or(0)
            .to_le_bytes(),
    );
    let maximum = i32::from_le_bytes(
        u32::try_from(maximum_raw & 0xffff_ffff)
            .unwrap_or(0)
            .to_le_bytes(),
    );
    if maximum <= 0 || initial < 0 || initial > maximum {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ret_u64(engine, 0, "CreateSemaphore");
    }
    let (handle, _) = state.kernel.sync.register_semaphore(initial, maximum);
    state.process.last_error = 0;
    ret_u64(engine, handle, "CreateSemaphore")
}
pub fn handle_release_semaphore(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let release_raw = engine.read_rdx()?;
    let prev_out = engine.read_r8()?;
    let release = i32::from_le_bytes(
        u32::try_from(release_raw & 0xffff_ffff)
            .unwrap_or(0)
            .to_le_bytes(),
    );
    let Some(crate::KernelObject::Semaphore(sem)) = state.kernel.sync.object(handle).cloned()
    else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        return ret_u64(engine, 0, "ReleaseSemaphore");
    };
    if let Some(prev) = sem.release(release) {
        if prev_out != 0 {
            let prev_u = u32::from_ne_bytes(prev.to_ne_bytes());
            write_guest_u32(engine, prev_out, prev_u)?;
        }
        state.process.last_error = 0;
        ret_u64(engine, 1, "ReleaseSemaphore")
    } else {
        state.process.last_error = ERROR_TOO_MANY_POSTS;
        ret_u64(engine, 0, "ReleaseSemaphore")
    }
}
pub fn handle_open_event(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _access = engine.read_rcx()?;
    let _inherit = engine.read_rdx()?;
    let _name = engine.read_r9().or_else(|_| engine.read_r8())?;
    // Named events not supported yet.
    state.process.last_error = ERROR_FILE_NOT_FOUND;
    ret_u64(engine, 0, "OpenEventW")
}
pub fn handle_wait_for_multiple_objects(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let count = low_u32(engine.read_rcx()?, "WaitForMultipleObjects count")?;
    let handles_ptr = engine.read_rdx()?;
    let wait_all = (engine.read_r8()? & 0xffff_ffff) != 0;
    let timeout_raw = engine.read_r9()?;
    let timeout_ms = u32::try_from(timeout_raw & u64::from(u32::MAX)).unwrap_or(0);

    let count_usize = usize::try_from(count).unwrap_or(usize::MAX);
    if count == 0 || handles_ptr == 0 || count_usize > crate::MAXIMUM_WAIT_OBJECTS {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ret_u64(
            engine,
            u64::from(crate::WAIT_FAILED),
            "WaitForMultipleObjects",
        );
    }

    let mut handles = Vec::with_capacity(count_usize);
    for i in 0..count {
        let ha = handles_ptr.wrapping_add(u64::from(i).wrapping_mul(8));
        handles.push(read_u64(engine, ha)?);
    }

    // Fast path: already satisfied (no host park).
    if let Some(targets) = state.kernel.sync.wait_targets(&handles) {
        let result = crate::wait_multiple(&targets, wait_all, 0);
        if result != crate::WAIT_TIMEOUT {
            state.process.last_error = 0;
            return ret_u64(engine, u64::from(result), "WaitForMultipleObjects");
        }
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        return ret_u64(
            engine,
            u64::from(crate::WAIT_FAILED),
            "WaitForMultipleObjects",
        );
    }

    if timeout_ms == 0 {
        state.process.last_error = 0;
        return ret_u64(
            engine,
            u64::from(crate::WAIT_TIMEOUT),
            "WaitForMultipleObjects",
        );
    }

    // Stash args per waiter TID; HostPark reason stays small/Copy.
    let waiter = state.kernel.threads.current_tid();
    state.kernel.sync.multi_wait.insert(
        waiter,
        crate::sync_obj::MultiWaitRequest {
            handles,
            wait_all,
            timeout_ms,
        },
    );
    Err(crate::WinApiControlSignal::HostPark {
        reason: crate::HostParkReason::WaitMultiple,
    }
    .into())
}
/// Handles `KERNEL32.dll!SignalObjectAndWait` — wait on the event then return.
pub fn handle_signal_object_and_wait(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let signal_handle = engine.read_rcx()?;
    let wait_handle = engine.read_rdx()?;
    let _timeout = engine.read_r8()?;
    // Signal first object (only handles events).
    if let Some(crate::KernelObject::Event(e)) = state.kernel.sync.object(signal_handle) {
        e.set();
    }
    // Wait on the second object (only handles events).
    match state.kernel.sync.object(wait_handle) {
        Some(crate::KernelObject::Event(e)) => {
            if e.wait(0) {
                state.process.last_error = 0;
                return ctx.finish(u64::from(crate::WAIT_OBJECT_0));
            }
        }
        Some(crate::KernelObject::Thread(t)) if t.is_finished() => {
            state.process.last_error = 0;
            return ctx.finish(u64::from(crate::WAIT_OBJECT_0));
        }
        None => {
            state.process.last_error = ERROR_INVALID_HANDLE;
            return ctx.finish(u64::from(crate::WAIT_FAILED));
        }
        _ => {}
    }
    // Not immediately signaled, park the host.
    Err(crate::WinApiControlSignal::HostPark {
        reason: crate::HostParkReason::WaitObject {
            handle: wait_handle,
            timeout_ms: u32::try_from(engine.read_r8()? & u64::from(u32::MAX)).unwrap_or(0),
        },
    }
    .into())
}
pub(crate) fn interlocked_i32(
    engine: &mut dyn wie_cpu::CpuEngine,
    addr: u64,
    op: impl FnOnce(&std::sync::atomic::AtomicI32) -> i32,
    slow: impl FnOnce(i32) -> i32,
) -> Result<i32> {
    if addr == 0 {
        anyhow::bail!("Interlocked* null destination");
    }
    // Fast path: 4-byte aligned host span → true host atomic (ARM LDXR/STXR).
    if addr.is_multiple_of(4)
        && let Some(host) = engine.host_span(addr, 4, true)
    {
        // SAFETY: host_span checked SPC+arena; guest VA alignment implies host
        // alignment for soft-translate (offset preserved). Pointer lives while
        // GuestMemory (engine) is borrowed exclusively here.
        #[expect(unsafe_code)]
        let atom = unsafe { &*(host.cast::<std::sync::atomic::AtomicI32>()) };
        return Ok(op(atom));
    }
    // Slow path: emulate via ordinary guest load/store.
    let mut bytes = [0_u8; 4];
    engine
        .mem_read(addr, &mut bytes)
        .context("Interlocked* slow-path read")?;
    let old = i32::from_le_bytes(bytes);
    let new = slow(old);
    engine
        .mem_write(addr, &new.to_le_bytes())
        .context("Interlocked* slow-path write")?;
    Ok(new)
}
pub(crate) fn interlocked_i32_prev(
    engine: &mut dyn wie_cpu::CpuEngine,
    addr: u64,
    op: impl FnOnce(&std::sync::atomic::AtomicI32) -> i32,
    slow: impl FnOnce(i32) -> (i32 /*prev*/, i32 /*new*/),
) -> Result<i32> {
    if addr == 0 {
        anyhow::bail!("Interlocked* null destination");
    }
    if addr.is_multiple_of(4)
        && let Some(host) = engine.host_span(addr, 4, true)
    {
        #[expect(unsafe_code)]
        let atom = unsafe { &*(host.cast::<std::sync::atomic::AtomicI32>()) };
        return Ok(op(atom));
    }
    let mut bytes = [0_u8; 4];
    engine
        .mem_read(addr, &mut bytes)
        .context("Interlocked* slow-path read")?;
    let old = i32::from_le_bytes(bytes);
    let (prev, new) = slow(old);
    engine
        .mem_write(addr, &new.to_le_bytes())
        .context("Interlocked* slow-path write")?;
    Ok(prev)
}
pub(crate) fn interlocked_i64(
    engine: &mut dyn wie_cpu::CpuEngine,
    addr: u64,
    op: impl FnOnce(&std::sync::atomic::AtomicI64) -> i64,
    slow: impl FnOnce(i64) -> i64,
) -> Result<i64> {
    if addr == 0 {
        anyhow::bail!("Interlocked*64 null destination");
    }
    if addr.is_multiple_of(8)
        && let Some(host) = engine.host_span(addr, 8, true)
    {
        #[expect(unsafe_code)]
        let atom = unsafe { &*(host.cast::<std::sync::atomic::AtomicI64>()) };
        return Ok(op(atom));
    }
    let mut bytes = [0_u8; 8];
    engine
        .mem_read(addr, &mut bytes)
        .context("Interlocked*64 slow-path read")?;
    let old = i64::from_le_bytes(bytes);
    let new = slow(old);
    engine
        .mem_write(addr, &new.to_le_bytes())
        .context("Interlocked*64 slow-path write")?;
    Ok(new)
}
pub(crate) fn interlocked_i64_prev(
    engine: &mut dyn wie_cpu::CpuEngine,
    addr: u64,
    op: impl FnOnce(&std::sync::atomic::AtomicI64) -> i64,
    slow: impl FnOnce(i64) -> (i64, i64),
) -> Result<i64> {
    if addr == 0 {
        anyhow::bail!("Interlocked*64 null destination");
    }
    if addr.is_multiple_of(8)
        && let Some(host) = engine.host_span(addr, 8, true)
    {
        #[expect(unsafe_code)]
        let atom = unsafe { &*(host.cast::<std::sync::atomic::AtomicI64>()) };
        return Ok(op(atom));
    }
    let mut bytes = [0_u8; 8];
    engine
        .mem_read(addr, &mut bytes)
        .context("Interlocked*64 slow-path read")?;
    let old = i64::from_le_bytes(bytes);
    let (prev, new) = slow(old);
    engine
        .mem_write(addr, &new.to_le_bytes())
        .context("Interlocked*64 slow-path write")?;
    Ok(prev)
}
pub(crate) fn handle_interlocked_increment(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx().context("InterlockedIncrement RCX")?;
    let new = interlocked_i32(
        engine,
        addr,
        |a| a.fetch_add(1, Ordering::SeqCst).wrapping_add(1),
        |old| old.wrapping_add(1),
    )?;
    ctx.finish(i32_to_rax(new))
}
pub(crate) fn handle_interlocked_decrement(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx().context("InterlockedDecrement RCX")?;
    let new = interlocked_i32(
        engine,
        addr,
        |a| a.fetch_sub(1, Ordering::SeqCst).wrapping_sub(1),
        |old| old.wrapping_sub(1),
    )?;
    ctx.finish(i32_to_rax(new))
}
pub(crate) fn handle_interlocked_exchange(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx().context("InterlockedExchange RCX")?;
    // RDX carries the new LONG (low 32 bits).
    let value = trunc_i32(engine.read_rdx()?);
    let prev = interlocked_i32_prev(
        engine,
        addr,
        |a| a.swap(value, Ordering::SeqCst),
        |old| (old, value),
    )?;
    ctx.finish(i32_to_rax(prev))
}
pub(crate) fn handle_interlocked_compare_exchange(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx()?;
    let exchange = trunc_i32(engine.read_rdx()?);
    let comparand = trunc_i32(engine.read_r8()?);
    let prev = interlocked_i32_prev(
        engine,
        addr,
        |a| match a.compare_exchange(comparand, exchange, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(v) | Err(v) => v,
        },
        |old| {
            if old == comparand {
                (old, exchange)
            } else {
                (old, old)
            }
        },
    )?;
    ctx.finish(i32_to_rax(prev))
}
pub(crate) fn handle_interlocked_exchange_add(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx()?;
    let addend = trunc_i32(engine.read_rdx()?);
    let prev = interlocked_i32_prev(
        engine,
        addr,
        |a| a.fetch_add(addend, Ordering::SeqCst),
        |old| (old, old.wrapping_add(addend)),
    )?;
    ctx.finish(i32_to_rax(prev))
}
pub(crate) fn handle_interlocked_increment64(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx()?;
    let new = interlocked_i64(
        engine,
        addr,
        |a| a.fetch_add(1, Ordering::SeqCst).wrapping_add(1),
        |old| old.wrapping_add(1),
    )?;
    ctx.finish(i64_to_rax(new))
}
pub(crate) fn handle_interlocked_decrement64(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx()?;
    let new = interlocked_i64(
        engine,
        addr,
        |a| a.fetch_sub(1, Ordering::SeqCst).wrapping_sub(1),
        |old| old.wrapping_sub(1),
    )?;
    ctx.finish(i64_to_rax(new))
}
pub(crate) fn handle_interlocked_exchange64(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx()?;
    let value = i64::from_le_bytes(engine.read_rdx()?.to_le_bytes());
    let prev = interlocked_i64_prev(
        engine,
        addr,
        |a| a.swap(value, Ordering::SeqCst),
        |old| (old, value),
    )?;
    ctx.finish(i64_to_rax(prev))
}
pub(crate) fn handle_interlocked_compare_exchange64(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx()?;
    let exchange = i64::from_le_bytes(engine.read_rdx()?.to_le_bytes());
    let comparand = i64::from_le_bytes(engine.read_r8()?.to_le_bytes());
    let prev = interlocked_i64_prev(
        engine,
        addr,
        |a| match a.compare_exchange(comparand, exchange, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(v) | Err(v) => v,
        },
        |old| {
            if old == comparand {
                (old, exchange)
            } else {
                (old, old)
            }
        },
    )?;
    ctx.finish(i64_to_rax(prev))
}
pub(crate) fn handle_interlocked_exchange_add64(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use std::sync::atomic::Ordering;
    let engine = &mut *ctx.engine;
    let addr = engine.read_rcx()?;
    let addend = i64::from_le_bytes(engine.read_rdx()?.to_le_bytes());
    let prev = interlocked_i64_prev(
        engine,
        addr,
        |a| a.fetch_add(addend, Ordering::SeqCst),
        |old| (old, old.wrapping_add(addend)),
    )?;
    ctx.finish(i64_to_rax(prev))
}
pub(crate) fn handle_wait_for_single_object(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let timeout_raw = engine.read_rdx()?;
    let timeout_ms = u32::try_from(timeout_raw & u64::from(u32::MAX)).unwrap_or(0);

    if std::env::var_os("WIE_MT_DEBUG").is_some() {
        let kind = match state.kernel.sync.object(handle) {
            Some(crate::KernelObject::Thread(t)) => {
                format!("Thread(tid={:#x},fin={})", t.tid, t.is_finished())
            }
            Some(crate::KernelObject::Event(e)) => format!("Event(manual={})", e.manual_reset),
            Some(crate::KernelObject::Semaphore(_)) => "Sem".into(),
            None => "INVALID".into(),
        };
        tracing::error!(
            handle = format_args!("{handle:#x}"),
            timeout_ms = format_args!("{timeout_ms:#x}"),
            kind = %kind,
            active_tid = format_args!("{:#x}", state.kernel.threads.current_tid()),
            pending = state.kernel.sync.pending_spawns.len(),
            "[mt] WaitForSingleObject"
        );
    }

    // Fast path: already signaled — no park.
    match state.kernel.sync.object(handle) {
        Some(crate::KernelObject::Thread(t)) => {
            if t.is_finished() {
                state.process.last_error = 0;
                return ctx.finish(u64::from(crate::WAIT_OBJECT_0));
            }
        }
        Some(crate::KernelObject::Event(e)) => {
            if e.wait(0) {
                state.process.last_error = 0;
                return ctx.finish(u64::from(crate::WAIT_OBJECT_0));
            }
        }
        Some(crate::KernelObject::Semaphore(s)) => {
            if s.try_acquire() {
                state.process.last_error = 0;
                return ctx.finish(u64::from(crate::WAIT_OBJECT_0));
            }
        }
        None => {
            state.process.last_error = ERROR_INVALID_HANDLE;
            return ctx.finish(u64::from(crate::WAIT_FAILED));
        }
    }

    // Need to park host (drop CPU) then wait.
    Err(crate::WinApiControlSignal::HostPark {
        reason: crate::HostParkReason::WaitObject { handle, timeout_ms },
    }
    .into())
}
pub fn resolve_wait_target(
    state: &WinApiState,
    handle: u64,
) -> Option<crate::sync_obj::WaitTarget> {
    state.kernel.sync.wait_target(handle)
}
pub fn resolve_cs_queue(
    state: &mut WinApiState,
    cs: u64,
) -> std::sync::Arc<crate::sync_obj::CsWaitQueue> {
    state.kernel.sync.cs_queue(cs)
}
pub(crate) fn handle_create_event(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _security = engine.read_rcx()?;
    let manual = engine.read_rdx()? != 0;
    let initial = engine.read_r8()? != 0;
    let _name = engine.read_r9()?; // named events: ignore (anonymous only)

    let (handle, _) = state.kernel.sync.register_event(manual, initial);
    state.process.last_error = 0;
    ctx.finish(handle)
}
pub(crate) fn handle_set_event(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let ok = match state.kernel.sync.object(handle) {
        Some(crate::KernelObject::Event(e)) => {
            e.set();
            true
        }
        _ => false,
    };
    if ok {
        state.process.last_error = 0;
        ctx.finish(1)
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        ctx.finish(0)
    }
}
pub(crate) fn handle_reset_event(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine.read_rcx()?;
    let ok = match state.kernel.sync.object(handle) {
        Some(crate::KernelObject::Event(e)) => {
            e.reset();
            true
        }
        _ => false,
    };
    if ok {
        state.process.last_error = 0;
        ctx.finish(1)
    } else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        ctx.finish(0)
    }
}
pub(crate) fn handle_flush_instruction_cache(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _process = engine
        .read_rcx()
        .context("failed to read RCX for FlushInstructionCache")?;
    let base = engine
        .read_rdx()
        .context("failed to read RDX for FlushInstructionCache")?;
    let size = engine
        .read_r8()
        .context("failed to read R8 for FlushInstructionCache")?;
    let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
    if size_usize == usize::MAX && size != 0 {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        return ctx.finish(0);
    }
    match engine.flush_instruction_cache(base, size_usize) {
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
