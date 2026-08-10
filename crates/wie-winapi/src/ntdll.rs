//! Handles `ntdll.dll` — the Nt*/Rtl* surface used by modern toolchains
//! (string dispatch). Stateless: handlers forward to the kernel32
//! implementations where the semantics match. The Nt* layer maps the NTSTATUS
//! ABI onto kernel32 handler semantics (handle teardown, heap, virtual memory,
//! sleep, clocks); the Rtl* layer is mostly thin forwards plus guest-memory
//! primitives.
//!
//! Documented gaps (fail gracefully as `Ok(None)` / `is_export == false`):
//! `NtCreateProcess` / `NtCreateUserProcess` (kernel process creation is a
//! non-goal), the guest PEB (`RtlGetCurrentPeb` returns NULL), and
//! `api-ms-win-core-*` sets (not CRT families, do not route here).

use anyhow::{Context, Result};

use crate::guest_memory::{
    read_u64, write_u16 as write_guest_u16, write_u32 as write_guest_u32,
    write_u64 as write_guest_u64,
};
use crate::kernel32::{
    INVALID_HANDLE_VALUE, find_open_file, handle_delete_critical_section,
    handle_enter_critical_section, handle_heap_alloc, handle_heap_free, handle_heap_realloc,
    handle_initialize_critical_section, handle_leave_critical_section, handle_rtl_capture_context,
    handle_rtl_unwind_ex, handle_sleep, is_open_file_handle, persist_open_file_to_host,
    read_stack_u64, read_wide_string_from_cpu, sync_open_bytes_to_virtual,
};
use crate::{HandlerContext, KernelHandle, WinApiHandlerResult};

const STATUS_SUCCESS: u64 = 0;
const STATUS_INVALID_HANDLE: u64 = 0xC000_0008;
const STATUS_INVALID_INFO_CLASS: u64 = 0xC000_0003;
const STATUS_INFO_LENGTH_MISMATCH: u64 = 0xC000_0004;
const STATUS_INVALID_PARAMETER: u64 = 0xC000_000D;
const STATUS_NO_MEMORY: u64 = 0xC000_0017;
/// Mirrors `kernel32`'s private `FAKE_CURRENT_PROCESS_ID`.
const FAKE_PROCESS_ID: u64 = 0x1234;
/// Mirrors `kernel32::FIXED_PERFORMANCE_FREQUENCY` — the 10 MHz QPC base.
const QPC_FREQUENCY: u64 = 10_000_000;
/// `NORMAL_PRIORITY_CLASS` base priority in `PROCESS_BASIC_INFORMATION`.
const BASE_PRIORITY_NORMAL: u32 = 8;
/// 10 000 × 100 ns per millisecond (`NtDelayExecution` conversion).
const HUNDRED_NS_PER_MS: u64 = 10_000;
const PAGE_SIZE: u32 = 0x1000;
const ALLOCATION_GRANULARITY: u32 = 0x1_0000;
const MIN_USER_MODE_ADDRESS: u64 = 0x1_0000;
const MAX_USER_MODE_ADDRESS: u64 = 0x0000_7fff_ffff_ffff;
/// Byte width of `PROCESS_BASIC_INFORMATION` (x64).
const PBI_SIZE: u64 = 48;
/// Byte width of `SYSTEM_BASIC_INFORMATION` (x64).
const SBI_SIZE: u64 = 56;

/// Every `ntdll.dll` export this module dispatches. Census oracle for
/// `is_export`; must stay in sync with the match arms in [`dispatch_ntdll`]
/// (enforced by `every_reported_export_dispatches`).
const NTDL_EXPORTS: &[&str] = &[
    "ntallocatevirtualmemory",
    "ntclose",
    "ntdelayexecution",
    "ntfreevirtualmemory",
    "ntprotectvirtualmemory",
    "ntqueryinformationprocess",
    "ntqueryperformancecounter",
    "ntquerysysteminformation",
    "ntquerysystemtime",
    "ntqueryvirtualmemory",
    "rtlallocateheap",
    "rtlcapturecontext",
    "rtlclosehandle",
    "rtlcomparememory",
    "rtlcopymemory",
    "rtldeletecriticalsection",
    "rtlentercriticalsection",
    "rtlfreeheap",
    "rtlgetcurrentpeb",
    "rtlgetcurrentthread",
    "rtlinitializecriticalsection",
    "rtlinitunicodestring",
    "rtlleavecriticalsection",
    "rtlmovememory",
    "rtlreallocateheap",
    "rtlunwindex",
    "rtlzeromemory",
];

/// Cold-path string dispatch for `ntdll.dll` exports.
pub fn dispatch_ntdll(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "ntclose" | "rtlclosehandle" => Ok(Some(handle_nt_close(ctx)?)),
        "ntqueryinformationprocess" => Ok(Some(handle_nt_query_information_process(ctx)?)),
        "ntquerysysteminformation" => Ok(Some(handle_nt_query_system_information(ctx)?)),
        "ntdelayexecution" => Ok(Some(handle_nt_delay_execution(ctx)?)),
        "ntqueryperformancecounter" => Ok(Some(handle_nt_query_performance_counter(ctx)?)),
        "ntallocatevirtualmemory" => Ok(Some(handle_nt_allocate_virtual_memory(ctx)?)),
        "ntfreevirtualmemory" => Ok(Some(handle_nt_free_virtual_memory(ctx)?)),
        "ntprotectvirtualmemory" => Ok(Some(handle_nt_protect_virtual_memory(ctx)?)),
        "ntqueryvirtualmemory" => Ok(Some(handle_nt_query_virtual_memory(ctx)?)),
        "ntquerysystemtime" => Ok(Some(handle_nt_query_system_time(ctx)?)),
        "rtlallocateheap" => Ok(Some(handle_rtl_heap_forward(ctx, HeapOp::Alloc)?)),
        "rtlfreeheap" => Ok(Some(handle_rtl_heap_forward(ctx, HeapOp::Free)?)),
        "rtlreallocateheap" => Ok(Some(handle_rtl_heap_forward(ctx, HeapOp::Realloc)?)),
        "rtlmovememory" | "rtlcopymemory" => Ok(Some(handle_rtl_move_memory(ctx)?)),
        "rtlzeromemory" => Ok(Some(handle_rtl_zero_memory(ctx)?)),
        "rtlcomparememory" => Ok(Some(handle_rtl_compare_memory(ctx)?)),
        "rtlinitunicodestring" => Ok(Some(handle_rtl_init_unicode_string(ctx)?)),
        "rtlinitializecriticalsection" => Ok(Some(handle_initialize_critical_section(ctx)?)),
        "rtlentercriticalsection" => Ok(Some(handle_enter_critical_section(ctx)?)),
        "rtlleavecriticalsection" => Ok(Some(handle_leave_critical_section(ctx)?)),
        "rtldeletecriticalsection" => Ok(Some(handle_delete_critical_section(ctx)?)),
        "rtlgetcurrentpeb" => Ok(Some(handle_rtl_get_current_peb(ctx)?)),
        "rtlgetcurrentthread" => Ok(Some(handle_rtl_get_current_thread(ctx)?)),
        // Windows exports these from ntdll (CRT teardown imports them there).
        "rtlcapturecontext" => Ok(Some(handle_rtl_capture_context(ctx)?)),
        "rtlunwindex" => Ok(Some(handle_rtl_unwind_ex(ctx)?)),
        _ => Ok(None),
    }
}

/// Census oracle: which `ntdll.dll` exports are implemented.
pub fn is_export(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    NTDL_EXPORTS.contains(&n.as_str())
}

/// Handles `NtClose` / `RtlCloseHandle` — `CloseHandle` teardown with an
/// NTSTATUS return (`STATUS_SUCCESS` / `STATUS_INVALID_HANDLE`).
fn handle_nt_close(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for NtClose")?;

    let status = if handle == 0 || handle == INVALID_HANDLE_VALUE {
        STATUS_INVALID_HANDLE
    } else if is_open_file_handle(state, handle) {
        // Same teardown as CloseHandle: flush to virtual store and/or bottle.
        if let Some(open_file) = find_open_file(state, handle) {
            let path = open_file.path.clone();
            sync_open_bytes_to_virtual(state, &path, handle);
        }
        persist_open_file_to_host(state, handle);
        let _ = crate::guest_io_host::unregister_open_file(engine, state, handle).ok();
        state.file_io.open_files.remove(&handle);
        state.file_io.cached_streams.remove(&handle);
        STATUS_SUCCESS
    } else if state
        .kernel
        .sync
        .objects
        .remove(&KernelHandle::from(handle))
        .is_some()
    {
        // Thread / event kernel handles (object may still be live via Arc).
        STATUS_SUCCESS
    } else {
        // Console / module / other fake kernel objects: accept and no-op.
        STATUS_SUCCESS
    };

    ctx.finish(status)
}

/// Handles `NtQueryInformationProcess` — minimal: `ProcessBasicInformation`
/// (class 0) only; other classes → `STATUS_INVALID_INFO_CLASS`.
fn handle_nt_query_information_process(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process_handle = engine.read_rcx()?;
    let info_class = engine.read_rdx()?;
    let info_va = engine.read_r8()?;
    let info_len = engine.read_r9()?;
    let return_len_va = read_stack_u64(engine, 0x28)?;

    if info_class != 0 {
        return ctx.finish(STATUS_INVALID_INFO_CLASS);
    }
    if info_va == 0 || info_len < PBI_SIZE {
        return ctx.finish(STATUS_INFO_LENGTH_MISMATCH);
    }
    // ExitStatus, PebBaseAddress, AffinityMask, BasePriority, UniqueProcessId,
    // InheritedFromUniqueProcessId (PEB + inherited-from stay NULL).
    let mut buf = [0_u8; 48];
    buf[0..4].copy_from_slice(&crate::STILL_ACTIVE.to_le_bytes());
    buf[16..24].copy_from_slice(&1_u64.to_le_bytes()); // AffinityMask = 1
    buf[24..28].copy_from_slice(&BASE_PRIORITY_NORMAL.to_le_bytes());
    buf[32..40].copy_from_slice(&FAKE_PROCESS_ID.to_le_bytes());
    engine
        .mem_write(info_va, &buf)
        .context("failed to write PROCESS_BASIC_INFORMATION")?;
    if return_len_va != 0 {
        write_guest_u64(engine, return_len_va, PBI_SIZE)?;
    }
    ctx.finish(STATUS_SUCCESS)
}

/// Handles `NtQuerySystemInformation` — minimal: `SystemBasicInformation`
/// (class 0) only; other classes → `STATUS_INVALID_INFO_CLASS`.
fn handle_nt_query_system_information(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let info_class = engine.read_rcx()?;
    let info_va = engine.read_rdx()?;
    let info_len = engine.read_r8()?;
    let return_len_va = engine.read_r9()?;

    if info_class != 0 {
        return ctx.finish(STATUS_INVALID_INFO_CLASS);
    }
    if info_va == 0 || info_len < SBI_SIZE {
        return ctx.finish(STATUS_INFO_LENGTH_MISMATCH);
    }
    // Single-process emulator: processor count 1, affinity mask 1 (matches
    // GetSystemInfo). 4 GiB physical memory → 0x10_0000 pages.
    let mut buf = [0_u8; 56];
    buf[8..12].copy_from_slice(&PAGE_SIZE.to_le_bytes());
    buf[12..16].copy_from_slice(&0x10_0000_u32.to_le_bytes());
    buf[24..28].copy_from_slice(&ALLOCATION_GRANULARITY.to_le_bytes());
    buf[28..36].copy_from_slice(&MIN_USER_MODE_ADDRESS.to_le_bytes());
    buf[36..44].copy_from_slice(&MAX_USER_MODE_ADDRESS.to_le_bytes());
    buf[44..52].copy_from_slice(&1_u64.to_le_bytes()); // ActiveProcessorsAffinityMask
    buf[52] = 1; // NumberOfProcessors (CCHAR)
    engine
        .mem_write(info_va, &buf)
        .context("failed to write SYSTEM_BASIC_INFORMATION")?;
    if return_len_va != 0 {
        write_guest_u64(engine, return_len_va, SBI_SIZE)?;
    }
    ctx.finish(STATUS_SUCCESS)
}

/// Handles `NtDelayExecution` — `Sleep` semantics. The delay is a
/// `LARGE_INTEGER` in 100 ns units; negative values are relative delays.
fn handle_nt_delay_execution(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _alertable = engine.read_rcx()?;
    let interval_va = engine.read_rdx()?;

    let delay_100ns = if interval_va == 0 {
        0
    } else {
        let raw =
            read_u64(engine, interval_va).context("failed to read NtDelayExecution interval")?;
        // Bit-preserving i64 view: negative = relative delay.
        i64::from_le_bytes(raw.to_le_bytes()).unsigned_abs()
    };
    if delay_100ns == 0 {
        return ctx.finish(STATUS_SUCCESS);
    }

    // Ceil to whole milliseconds so sub-ms delays still yield at least 1 ms.
    let milliseconds = delay_100ns
        .checked_add(HUNDRED_NS_PER_MS - 1)
        .and_then(|sum| u32::try_from(sum / HUNDRED_NS_PER_MS).ok())
        .unwrap_or(u32::MAX)
        .max(1);

    ctx.engine
        .write_rcx(u64::from(milliseconds))
        .context("failed to write RCX for Sleep forward")?;
    // `handle_sleep` performs the host park and returns STATUS_SUCCESS (0).
    handle_sleep(ctx)
}

/// Handles `NtQueryPerformanceCounter` — writes the QPC counter and optional
/// frequency, sharing the kernel32 monotonic clock epoch.
fn handle_nt_query_performance_counter(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let counter_va = engine.read_rcx()?;
    let frequency_va = engine.read_rdx()?;

    if counter_va != 0 {
        write_guest_u64(engine, counter_va, crate::kernel32::performance_counter())?;
    }
    if frequency_va != 0 {
        write_guest_u64(engine, frequency_va, QPC_FREQUENCY)?;
    }
    ctx.finish(STATUS_SUCCESS)
}

/// Handles `NtAllocateVirtualMemory` — `VirtualAlloc` semantics with in/out
/// base-address and region-size pointers written back on success.
fn handle_nt_allocate_virtual_memory(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process_handle = engine.read_rcx()?;
    let base_ptr = engine.read_rdx()?;
    let _zero_bits = engine.read_r8()?;
    let size_ptr = engine.read_r9()?;
    let alloc_type_raw = read_stack_u64(engine, 0x28)?;
    let protect_raw = read_stack_u64(engine, 0x30)?;

    if base_ptr == 0 || size_ptr == 0 {
        return ctx.finish(STATUS_INVALID_PARAMETER);
    }
    let alloc_type = u32::try_from(alloc_type_raw & u64::from(u32::MAX)).unwrap_or(0);
    let protect = u32::try_from(protect_raw & u64::from(u32::MAX)).unwrap_or(0);
    let base = read_u64(engine, base_ptr)?;
    let size = read_u64(engine, size_ptr)?;
    let size_usize = usize::try_from(size).unwrap_or(usize::MAX);

    match engine.virtual_alloc(base, size_usize, alloc_type, protect) {
        Ok(new_base) => {
            write_guest_u64(engine, base_ptr, new_base)?;
            write_guest_u64(engine, size_ptr, size)?;
            ctx.finish(STATUS_SUCCESS)
        }
        Err(_) => {
            write_guest_u64(engine, base_ptr, 0)?; // Windows zeroes it on failure
            ctx.finish(STATUS_NO_MEMORY)
        }
    }
}

/// Handles `NtFreeVirtualMemory` — `VirtualFree` semantics with the in/out
/// base/size pointers zeroed on success.
fn handle_nt_free_virtual_memory(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process_handle = engine.read_rcx()?;
    let base_ptr = engine.read_rdx()?;
    let size_ptr = engine.read_r9()?;
    let free_type_raw = read_stack_u64(engine, 0x28)?;

    if base_ptr == 0 || size_ptr == 0 {
        return ctx.finish(STATUS_INVALID_PARAMETER);
    }
    let free_type = u32::try_from(free_type_raw & u64::from(u32::MAX)).unwrap_or(0);
    let base = read_u64(engine, base_ptr)?;
    let size = read_u64(engine, size_ptr)?;
    let size_usize = usize::try_from(size).unwrap_or(usize::MAX);

    match engine.virtual_free(base, size_usize, free_type) {
        Ok(()) => {
            write_guest_u64(engine, base_ptr, 0)?;
            write_guest_u64(engine, size_ptr, 0)?;
            ctx.finish(STATUS_SUCCESS)
        }
        Err(_) => ctx.finish(STATUS_INVALID_PARAMETER),
    }
}

/// Handles `NtProtectVirtualMemory` — `VirtualProtect` semantics with the
/// in/out base/size pointers and the old-protection out pointer.
fn handle_nt_protect_virtual_memory(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process_handle = engine.read_rcx()?;
    let base_ptr = engine.read_rdx()?;
    let size_ptr = engine.read_r9()?;
    let new_protect_raw = read_stack_u64(engine, 0x28)?;
    let old_protect_ptr = read_stack_u64(engine, 0x30)?;

    if base_ptr == 0 || size_ptr == 0 {
        return ctx.finish(STATUS_INVALID_PARAMETER);
    }
    let new_protect = u32::try_from(new_protect_raw & u64::from(u32::MAX)).unwrap_or(0);
    let base = read_u64(engine, base_ptr)?;
    let size = read_u64(engine, size_ptr)?;
    let size_usize = usize::try_from(size).unwrap_or(usize::MAX);

    match engine.virtual_protect(base, size_usize, new_protect) {
        Ok(old) => {
            if old_protect_ptr != 0 {
                write_guest_u32(engine, old_protect_ptr, old)?;
            }
            write_guest_u64(engine, base_ptr, base)?;
            write_guest_u64(engine, size_ptr, size)?;
            ctx.finish(STATUS_SUCCESS)
        }
        Err(_) => ctx.finish(STATUS_INVALID_PARAMETER),
    }
}

/// Handles `NtQueryVirtualMemory` — minimal: `MemoryBasicInformation`
/// (class 0) only; other classes → `STATUS_INVALID_INFO_CLASS`.
fn handle_nt_query_virtual_memory(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process_handle = engine.read_rcx()?;
    let address = engine.read_rdx()?;
    let info_class = engine.read_r8()?;
    let info_va = engine.read_r9()?;
    let info_len = read_stack_u64(engine, 0x28)?;
    let return_len_va = read_stack_u64(engine, 0x30)?;

    if info_class != 0 {
        return ctx.finish(STATUS_INVALID_INFO_CLASS);
    }
    if info_va == 0 || info_len < 48 {
        return ctx.finish(STATUS_INFO_LENGTH_MISMATCH);
    }
    let mbi = engine.virtual_query(address);
    engine
        .mem_write(info_va, &mbi.to_bytes())
        .context("failed to write MEMORY_BASIC_INFORMATION")?;
    if return_len_va != 0 {
        write_guest_u64(engine, return_len_va, 48)?;
    }
    ctx.finish(STATUS_SUCCESS)
}

/// Handles `NtQuerySystemTime` — writes the wall clock as a `FILETIME`
/// (100 ns since 1601-01-01 UTC).
fn handle_nt_query_system_time(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let system_time_va = engine.read_rcx()?;

    if system_time_va != 0 {
        let filetime = crate::kernel32::system_time_filetime();
        let low = u32::try_from(filetime & u64::from(u32::MAX)).unwrap_or(0);
        let high = u32::try_from(filetime >> 32).unwrap_or(0);
        write_guest_u32(engine, system_time_va, low)?;
        write_guest_u32(engine, system_time_va.wrapping_add(4), high)?;
    }
    ctx.finish(STATUS_SUCCESS)
}

/// Rewrite `rcx` to the default process heap when the guest passes NULL.
fn remap_default_heap(ctx: &mut HandlerContext<'_>, heap_handle: u64) -> Result<()> {
    let heap = if heap_handle == 0 {
        ctx.environment.process_heap_handle
    } else {
        heap_handle
    };
    ctx.engine.write_rcx(heap)?;
    Ok(())
}

/// Handles `RtlAllocateHeap` / `RtlFreeHeap` / `RtlReAllocateHeap` — forwards
/// to the `HeapAlloc` / `HeapFree` / `HeapReAlloc` handlers, mapping a NULL
/// heap handle to the default process heap.
#[derive(Clone, Copy)]
enum HeapOp {
    Alloc,
    Free,
    Realloc,
}

fn handle_rtl_heap_forward(
    ctx: &mut HandlerContext<'_>,
    op: HeapOp,
) -> Result<WinApiHandlerResult> {
    let heap_handle = ctx.engine.read_rcx()?;
    let flags = ctx.engine.read_rdx()?;
    remap_default_heap(ctx, heap_handle)?;
    ctx.engine.write_rdx(flags)?;
    match op {
        HeapOp::Alloc => {
            let size = ctx.engine.read_r8()?;
            ctx.engine.write_r8(size)?;
            handle_heap_alloc(ctx)
        }
        HeapOp::Free => {
            let memory = ctx.engine.read_r8()?;
            ctx.engine.write_r8(memory)?;
            handle_heap_free(ctx)
        }
        HeapOp::Realloc => {
            let memory = ctx.engine.read_r8()?;
            let new_size = ctx.engine.read_r9()?;
            ctx.engine.write_r8(memory)?;
            ctx.engine.write_r9(new_size)?;
            handle_heap_realloc(ctx)
        }
    }
}

/// Handles `RtlMoveMemory` / `RtlCopyMemory` — guest memcpy (memmove semantics
/// inside wie-cpu, so overlap is safe for both).
fn handle_rtl_move_memory(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dst = engine.read_rcx()?;
    let src = engine.read_rdx()?;
    let len = usize::try_from(engine.read_r8()?).unwrap_or(0);

    if len > 0 && !engine.mem_copy(dst, src, len) {
        // Cross-arena or SPC-denied: bounce through a host buffer.
        let mut bytes = vec![0_u8; len];
        engine.mem_read(src, &mut bytes)?;
        engine.mem_write(dst, &bytes)?;
    }
    ctx.finish(STATUS_SUCCESS)
}

/// Handles `RtlZeroMemory` — guest memset of zero bytes.
fn handle_rtl_zero_memory(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dst = engine.read_rcx()?;
    let len = usize::try_from(engine.read_rdx()?).unwrap_or(0);

    if len > 0 && !engine.mem_fill(dst, 0, len) {
        let zeros = vec![0_u8; len];
        engine.mem_write(dst, &zeros)?;
    }
    ctx.finish(STATUS_SUCCESS)
}

/// Handles `RtlCompareMemory` — bytewise compare returning the count of leading
/// equal bytes (`SIZE_T`).
fn handle_rtl_compare_memory(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let src1 = engine.read_rcx()?;
    let src2 = engine.read_rdx()?;
    let len = engine.read_r8()?;

    // Chunked (64 B) so a page boundary never needs a cross-page bulk read.
    let mut equal = 0_u64;
    let mut cursor = 0_u64;
    while cursor < len {
        let take = usize::try_from((len - cursor).min(64)).unwrap_or(0);
        let mut a = [0_u8; 64];
        let mut b = [0_u8; 64];
        let a_slice = a.get_mut(..take).context("RtlCompareMemory a slice")?;
        let b_slice = b.get_mut(..take).context("RtlCompareMemory b slice")?;
        engine.mem_read(src1.wrapping_add(cursor), a_slice)?;
        engine.mem_read(src2.wrapping_add(cursor), b_slice)?;
        for (xa, xb) in a_slice.iter().zip(b_slice.iter()) {
            if xa != xb {
                return ctx.finish(equal);
            }
            equal = equal.saturating_add(1);
        }
        cursor = cursor.saturating_add(u64::try_from(take).unwrap_or(0));
    }
    ctx.finish(equal)
}

/// Handles `RtlInitUnicodeString` — materialize a `UNICODE_STRING` (`USHORT
/// Length`, `USHORT MaximumLength`, `PWSTR Buffer`) from the source string.
fn handle_rtl_init_unicode_string(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dst_va = engine.read_rcx()?;
    let src_va = engine.read_rdx()?;

    if dst_va == 0 {
        return ctx.finish(STATUS_SUCCESS);
    }
    let (length, maximum_length, buffer) = if src_va == 0 {
        (0_u16, 0_u16, 0_u64)
    } else {
        let text = read_wide_string_from_cpu(engine, src_va, 32_768)
            .context("failed to read RtlInitUnicodeString source")?;
        // Byte length excludes the NUL; MaximumLength includes it.
        let byte_len =
            u16::try_from(text.encode_utf16().count().saturating_mul(2)).unwrap_or(u16::MAX);
        (byte_len, byte_len.saturating_add(2), src_va)
    };

    write_guest_u16(engine, dst_va, length)?;
    write_guest_u16(engine, dst_va.wrapping_add(2), maximum_length)?;
    write_guest_u64(engine, dst_va.wrapping_add(8), buffer)?;
    ctx.finish(STATUS_SUCCESS)
}

/// Handles `RtlGetCurrentPeb` — no guest PEB is materialized, return NULL
/// (documented gap).
fn handle_rtl_get_current_peb(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(0)
}

/// Handles `RtlGetCurrentThread` — returns the guest TEB VA (`wie_cpu::GS_BASE`),
/// the closest analogue the emulator exposes. Real Windows returns the
/// current-thread pseudo-handle (`(HANDLE)-2`); CRT teardown treats the value
/// as an opaque token, so the TEB VA is safe.
fn handle_rtl_get_current_thread(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.finish(wie_cpu::GS_BASE)
}

#[cfg(test)]
mod tests;
