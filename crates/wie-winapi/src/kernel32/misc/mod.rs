use super::{
    Context, ERROR_INVALID_PARAMETER, FIXED_PERFORMANCE_FREQUENCY, FLS_OUT_OF_INDEXES, FlsSlot,
    GUEST_OS_BUILD, GUEST_OS_MAJOR, GUEST_OS_MINOR, GUEST_OS_PLATFORM_NT, HandlerContext,
    LANG_EN_US, OnceLock, Result, TIME_ZONE_ID_INVALID, TIME_ZONE_ID_UNKNOWN, WinApiHandlerResult,
    WinApiState, checked_field_address, low_u32, low_u32_to_i32, read_guest_ansi_lossy,
    read_guest_utf16_lossy, ret_bool_true, ret_u64, write_guest_u16, write_guest_u32,
    write_guest_u64, write_mock_string_a, write_mock_string_w,
};

pub use identity::*;
pub use time::*;

mod identity;
mod time;

pub(crate) fn packed_get_version() -> u64 {
    // Low byte major, next minor, high word build; bit 31 set ⇒ Windows NT family.
    let packed = GUEST_OS_MAJOR | (GUEST_OS_MINOR << 8) | (GUEST_OS_BUILD << 16) | 0x8000_0000;
    u64::from(packed)
}
/// Handles `KERNEL32.dll!GetVersion` (legacy packed DWORD).
pub fn handle_get_version(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_value = packed_get_version();
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetVersion")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetVersionExA`.
pub fn handle_get_version_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let version_info_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetVersionExA")?;

    if version_info_ptr == 0 {
        let return_address = engine
            .return_from_win64_api(0)
            .context("failed to return FALSE from GetVersionExA")?;

        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let major_version = GUEST_OS_MAJOR;
    let minor_version = GUEST_OS_MINOR;
    let build_number = GUEST_OS_BUILD;
    let platform_id = GUEST_OS_PLATFORM_NT;

    // OSVERSIONINFOA:
    // DWORD dwOSVersionInfoSize; offset 0
    // DWORD dwMajorVersion;      offset 4
    // DWORD dwMinorVersion;      offset 8
    // DWORD dwBuildNumber;       offset 12
    // DWORD dwPlatformId;        offset 16
    let major_version_address = checked_field_address(version_info_ptr, 4, "dwMajorVersion");
    let minor_version_address = checked_field_address(version_info_ptr, 8, "dwMinorVersion");
    let build_number_address = checked_field_address(version_info_ptr, 12, "dwBuildNumber");
    let platform_id_address = checked_field_address(version_info_ptr, 16, "dwPlatformId");

    write_guest_u32(engine, major_version_address, major_version)?;
    write_guest_u32(engine, minor_version_address, minor_version)?;
    write_guest_u32(engine, build_number_address, build_number)?;
    write_guest_u32(engine, platform_id_address, platform_id)?;

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return TRUE from GetVersionExA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!GetCommandLineA`.
pub fn handle_get_command_line_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let command_line_ptr = ctx.environment.command_line_a_ptr;
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(command_line_ptr)
        .context("failed to return from GetCommandLineA")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: command_line_ptr,
    })
}
/// Handles `KERNEL32.dll!GetCommandLineW`.
pub fn handle_get_command_line_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let command_line_ptr = ctx.environment.command_line_w_ptr;
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(command_line_ptr)
        .context("failed to return from GetCommandLineW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: command_line_ptr,
    })
}
/// Handles `KERNEL32.dll!GetTickCount`.
pub fn handle_get_tick_count(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_value = super::clock::tick_count_32();
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetTickCount")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!GetTickCount64`.
pub fn handle_get_tick_count_64(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_value = super::clock::tick_count_64();
    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetTickCount64")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!QueryPerformanceCounter`.
pub fn handle_query_performance_counter(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let counter_ptr = engine
        .read_rcx()
        .context("failed to read RCX for QueryPerformanceCounter")?;

    if counter_ptr != 0 {
        write_guest_u64(engine, counter_ptr, super::clock::performance_counter())?;
    }

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from QueryPerformanceCounter")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub(crate) fn publish_fls_slot(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &WinApiState,
    index: u32,
    value: u64,
) {
    let table = state.heap_state.guest_fls_table_va;
    if table == 0 || index >= 256 {
        return;
    }
    let va = table.saturating_add(u64::from(index).saturating_mul(8));
    drop(engine.mem_write(va, &value.to_le_bytes()));
}
/// Handles `KERNEL32.dll!FlsAlloc`.
pub fn handle_fls_alloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _callback = engine
        .read_rcx()
        .context("failed to read RCX for FlsAlloc")?;

    let index = state.heap_state.next_fls_index;

    let return_value = if index == u32::MAX {
        FLS_OUT_OF_INDEXES
    } else {
        state.heap_state.next_fls_index = index.checked_add(1).context("FLS index overflow")?;

        state.heap_state.fls_slots.push(FlsSlot { index, value: 0 });
        publish_fls_slot(engine, state, index, 0);

        u64::from(index)
    };

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from FlsAlloc")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!FlsFree`.
pub fn handle_fls_free(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let index_raw = engine
        .read_rcx()
        .context("failed to read RCX for FlsFree")?;

    let index = u32::try_from(index_raw).context("FlsFree index does not fit u32")?;

    state
        .heap_state
        .fls_slots
        .retain(|slot| slot.index != index);
    publish_fls_slot(engine, state, index, 0);

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from FlsFree")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!FlsSetValue`.
pub fn handle_fls_set_value(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let index_raw = engine
        .read_rcx()
        .context("failed to read RCX for FlsSetValue")?;

    let value = engine
        .read_rdx()
        .context("failed to read RDX for FlsSetValue")?;

    let index = u32::try_from(index_raw).context("FlsSetValue index does not fit u32")?;

    if let Some(slot) = state
        .heap_state
        .fls_slots
        .iter_mut()
        .find(|slot| slot.index == index)
    {
        slot.value = value;
    } else {
        state.heap_state.fls_slots.push(FlsSlot { index, value });
    }
    publish_fls_slot(engine, state, index, value);

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from FlsSetValue")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!FlsGetValue`.
pub fn handle_fls_get_value(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let index_raw = engine
        .read_rcx()
        .context("failed to read RCX for FlsGetValue")?;

    let index = u32::try_from(index_raw).context("FlsGetValue index does not fit u32")?;

    let return_value = state
        .heap_state
        .fls_slots
        .iter()
        .find(|slot| slot.index == index)
        .map_or(0, |slot| slot.value);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from FlsGetValue")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!SetHandleCount`.
pub fn handle_set_handle_count(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let handle_count = engine
        .read_rcx()
        .context("failed to read RCX for SetHandleCount")?;

    let return_address = engine
        .return_from_win64_api(handle_count)
        .context("failed to return from SetHandleCount")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: handle_count,
    })
}
/// Handles `KERNEL32.dll!GetEnvironmentStringsW`.
pub fn handle_get_environment_strings_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let environment_strings_w_ptr = ctx.environment.environment_strings_w_ptr;
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(environment_strings_w_ptr)
        .context("failed to return from GetEnvironmentStringsW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: environment_strings_w_ptr,
    })
}
/// Handles `KERNEL32.dll!FreeEnvironmentStringsW`.
pub fn handle_free_environment_strings_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _environment_block = engine
        .read_rcx()
        .context("failed to read RCX for FreeEnvironmentStringsW")?;

    let return_address = engine
        .return_from_win64_api(1)
        .context("failed to return from FreeEnvironmentStringsW")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!GetLastError`.
pub fn handle_get_last_error(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let return_value = u64::from(state.process.last_error);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from GetLastError")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
/// Handles `KERNEL32.dll!SetLastError`.
pub fn handle_set_last_error(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let error_raw = engine
        .read_rcx()
        .context("failed to read RCX for SetLastError")?;

    let error =
        u32::try_from(error_raw & 0xffff_ffff).context("SetLastError value does not fit u32")?;

    state.process.last_error = error;

    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from SetLastError")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `KERNEL32.dll!SetUnhandledExceptionFilter`.
pub fn handle_set_unhandled_exception_filter(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _filter_ptr = engine
        .read_rcx()
        .context("failed to read RCX for SetUnhandledExceptionFilter")?;

    let return_address = engine
        .return_from_win64_api(0)
        .context("failed to return from SetUnhandledExceptionFilter")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `KERNEL32.dll!GetSystemDefaultLangID`.
pub fn handle_get_system_default_lang_id(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(LANG_EN_US)
        .context("failed to return from GetSystemDefaultLangID")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: LANG_EN_US,
    })
}
/// Handles `KERNEL32.dll!GetUserDefaultLangID`.
pub fn handle_get_user_default_lang_id(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine
        .return_from_win64_api(LANG_EN_US)
        .context("failed to return from GetUserDefaultLangID")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: LANG_EN_US,
    })
}
/// Handles dynamic `KERNEL32.dll!EncodePointer`.
pub fn handle_encode_pointer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pointer = engine
        .read_rcx()
        .context("failed to read RCX for EncodePointer")?;

    let return_address = engine
        .return_from_win64_api(pointer)
        .context("failed to return from EncodePointer")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: pointer,
    })
}
/// Handles dynamic `KERNEL32.dll!DecodePointer`.
pub fn handle_decode_pointer(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pointer = engine
        .read_rcx()
        .context("failed to read RCX for DecodePointer")?;

    let return_address = engine
        .return_from_win64_api(pointer)
        .context("failed to return from DecodePointer")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value: pointer,
    })
}
pub fn handle_set_file_apis_to_oem(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    ret_u64(engine, 0, "SetFileApisToOEM")
}
pub fn handle_query_performance_frequency(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ptr = engine.read_rcx().context("QueryPerformanceFrequency RCX")?;
    if ptr != 0 {
        write_guest_u64(engine, ptr, FIXED_PERFORMANCE_FREQUENCY)?;
    }
    ret_bool_true(engine, "QueryPerformanceFrequency")
}
pub fn handle_get_system_info(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ptr = engine.read_rcx().context("GetSystemInfo RCX")?;
    if ptr != 0 {
        // SYSTEM_INFO on x64 (48 bytes):
        // union { DWORD dwOemId; struct { WORD wProcessorArchitecture; WORD wReserved; } }
        // DWORD dwPageSize;
        // LPVOID lpMinimumApplicationAddress;
        // LPVOID lpMaximumApplicationAddress;
        // DWORD_PTR dwActiveProcessorMask;
        // DWORD dwNumberOfProcessors;
        // DWORD dwProcessorType;
        // DWORD dwAllocationGranularity;
        // WORD wProcessorLevel;
        // WORD wProcessorRevision;
        let mut buf = [0_u8; 48];
        // wProcessorArchitecture = 9 (PROCESSOR_ARCHITECTURE_AMD64)
        buf[0..2].copy_from_slice(&9_u16.to_le_bytes());
        // dwPageSize = 0x1000
        buf[4..8].copy_from_slice(&0x1000_u32.to_le_bytes());
        // min app address 0x10000
        buf[8..16].copy_from_slice(&0x1_0000_u64.to_le_bytes());
        // max app address
        buf[16..24].copy_from_slice(&0x0000_7fff_ffff_ffff_u64.to_le_bytes());
        // active processor mask = 1
        buf[24..32].copy_from_slice(&1_u64.to_le_bytes());
        // number of processors = 1
        buf[32..36].copy_from_slice(&1_u32.to_le_bytes());
        // processor type = 8664
        buf[36..40].copy_from_slice(&8664_u32.to_le_bytes());
        // allocation granularity = 0x10000
        buf[40..44].copy_from_slice(&0x1_0000_u32.to_le_bytes());
        // level / revision
        buf[44..46].copy_from_slice(&6_u16.to_le_bytes());
        buf[46..48].copy_from_slice(&0x3c03_u16.to_le_bytes());
        engine.mem_write(ptr, &buf).context("GetSystemInfo write")?;
    }
    ret_u64(engine, 0, "GetSystemInfo")
}
pub fn handle_is_processor_feature_present(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let feature = low_u32(engine.read_rcx()?, "IsProcessorFeaturePresent")?;
    // Advertise a few common x64 features as present; unknown → FALSE.
    // 0=floating point, 6=compare exchange double, 7=MMX, 8=XMMI (SSE),
    // 10=3DNow, 13=SSE2, 14=SSE3, 21=NX, 23=RDTSC, 25=compare exchange 128.
    let present = matches!(feature, 0 | 6 | 7 | 8 | 10 | 13 | 14 | 21 | 23 | 25);
    ret_u64(engine, u64::from(present), "IsProcessorFeaturePresent")
}
pub fn handle_get_large_page_minimum(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    ret_u64(engine, 0, "GetLargePageMinimum")
}
pub fn handle_format_message_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _flags = engine.read_rcx()?;
    let _source = engine.read_rdx()?;
    let _message_id = engine.read_r8()?;
    let _language_id = engine.read_r9()?;
    // Buffer args ignored; report 0 characters written.
    ret_u64(engine, 0, "FormatMessageW")
}
/// Handles `KERNEL32.dll!IsDebuggerPresent` — return FALSE.
pub fn handle_is_debugger_present(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let return_address = engine.return_from_win64_api(0)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `KERNEL32.dll!DebugBreak` — emit a trace warning (no real break).
pub fn handle_debug_break(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    tracing::warn!("DebugBreak called");
    let return_address = engine.return_from_win64_api(0)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
/// Handles `KERNEL32.dll!OutputDebugStringA` — log and return.
pub fn handle_output_debug_string_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let msg_ptr = engine.read_rcx()?;
    if msg_ptr != 0 {
        let msg = read_guest_ansi_lossy(engine, msg_ptr, 1024).unwrap_or_default();
        tracing::debug!("OutputDebugStringA: {msg}");
    }
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!OutputDebugStringW` — log and return.
pub fn handle_output_debug_string_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let msg_ptr = engine.read_rcx()?;
    if msg_ptr != 0 {
        let msg = read_guest_utf16_lossy(engine, msg_ptr, 1024).unwrap_or_default();
        tracing::debug!("OutputDebugStringW: {msg}");
    }
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!SetErrorMode` — store and return previous mode.
pub fn handle_set_error_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let mode = u32::try_from(engine.read_rcx()? & 0xffff_ffff).unwrap_or(0);
    let prev = state.process.error_mode;
    state.process.error_mode = mode;
    let return_address = engine.return_from_win64_api(u64::from(prev))?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(prev),
    })
}
/// Handles `KERNEL32.dll!SetThreadErrorMode` — store new mode, return previous.
pub fn handle_set_thread_error_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let mode = u32::try_from(engine.read_rcx()? & 0xffff_ffff).unwrap_or(0);
    let prev_mode_ptr = engine.read_rdx()?;
    let prev = state.process.error_mode;
    state.process.error_mode = mode;
    if prev_mode_ptr != 0 {
        write_guest_u32(engine, prev_mode_ptr, prev)?;
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
pub(crate) fn handle_raise_exception(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::seh::dispatch_exception(engine, state)
}
pub(crate) fn handle_rtl_capture_context(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    use anyhow::Context;
    let engine = &mut *ctx.engine;

    let ctx_ptr = engine.read_rcx()?;
    if ctx_ptr == 0 {
        let return_address = engine
            .return_from_win64_api(0)
            .context("RtlCaptureContext: return failed")?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }

    let tctx = engine.snapshot_thread_context();
    // Write CONTEXT64 at ctx_ptr (subset of the Win64 CONTEXT layout).
    let mut cbuf = [0u8; 0x200];
    // ContextFlags at +0x30
    if let Some(slot) = cbuf.get_mut(0x30..0x34) {
        slot.copy_from_slice(&0x0010_001Fu32.to_le_bytes());
    }
    // GPRs at +0x78..+0xF8 (Rax..R15), Rip at +0xF8
    for reg in 0_usize..16 {
        let off = 0x78_usize.saturating_add(reg.saturating_mul(8));
        if let (Some(slot), Some(val)) =
            (cbuf.get_mut(off..off.saturating_add(8)), tctx.gpr.get(reg))
        {
            slot.copy_from_slice(&val.to_le_bytes());
        }
    }
    if let Some(slot) = cbuf.get_mut(0xF8..0x100) {
        slot.copy_from_slice(&tctx.rip.to_le_bytes());
    }
    // Rflags at +0x44
    if let Some(slot) = cbuf.get_mut(0x44..0x4C) {
        slot.copy_from_slice(&tctx.rflags.to_le_bytes());
    }
    // XMM0..XMM15 at +0x100
    for i in 0_usize..16 {
        let off = 0x100_usize.saturating_add(i.saturating_mul(16));
        if let (Some(slot), Some(val)) =
            (cbuf.get_mut(off..off.saturating_add(16)), tctx.xmm.get(i))
        {
            slot.copy_from_slice(&val.to_le_bytes());
        }
    }
    engine
        .mem_write(ctx_ptr, &cbuf)
        .context("RtlCaptureContext: failed to write context")?;
    let return_address = engine
        .return_from_win64_api(0)
        .context("RtlCaptureContext: return failed")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
pub(crate) fn handle_rtl_unwind_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let target_frame = engine.read_rcx()?;
    let target_ip = engine.read_rdx()?;
    let return_value = engine.read_r9()?;
    let target_frame_rsp = if target_frame == 0 {
        None
    } else {
        Some(target_frame)
    };
    crate::seh::forced_unwind_to(engine, state, target_ip, target_frame_rsp, return_value)
}
pub(crate) fn handle_tls_get_value(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let index = engine.read_rcx()? & 0xffff_ffff;
    let idx = usize::try_from(index).unwrap_or(usize::MAX);
    // Microsoft: invalid index → 0 and last-error ERROR_INVALID_PARAMETER (87).
    let allocated = usize::try_from(state.kernel.threads.tls_index_count).unwrap_or(0);
    if idx >= allocated {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    state.kernel.threads.grow_active_tls_to_process_count();
    let value = state
        .kernel
        .threads
        .active
        .tls_values
        .get(idx)
        .copied()
        .unwrap_or(0);
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}
pub(crate) fn handle_tls_set_value(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let index = engine.read_rcx()? & 0xffff_ffff;
    let value = engine.read_rdx()?;
    let idx = usize::try_from(index).unwrap_or(usize::MAX);
    let allocated = usize::try_from(state.kernel.threads.tls_index_count).unwrap_or(0);
    if idx >= allocated {
        state.process.last_error = ERROR_INVALID_PARAMETER;
        let return_address = engine.return_from_win64_api(0)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 0,
        });
    }
    state.kernel.threads.grow_active_tls_to_process_count();
    if let Some(slot) = state.kernel.threads.active.tls_values.get_mut(idx) {
        *slot = value;
        state.process.last_error = 0;
        let return_address = engine.return_from_win64_api(1)?;
        return Ok(WinApiHandlerResult {
            return_address,
            return_value: 1,
        });
    }
    state.process.last_error = ERROR_INVALID_PARAMETER;
    let return_address = engine.return_from_win64_api(0)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}
pub(crate) fn handle_tls_alloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    // Process-wide index space; value storage is per active guest thread.
    let index = u64::from(state.kernel.threads.tls_index_count);
    state.kernel.threads.tls_index_count = state.kernel.threads.tls_index_count.saturating_add(1);
    state.kernel.threads.grow_active_tls_to_process_count();
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(index)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: index,
    })
}
pub(crate) fn handle_tls_free(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let index_raw = engine.read_rcx()?;
    let index = usize::try_from(index_raw).unwrap_or(usize::MAX);
    // Clear active thread value; index remains allocated (Windows does not reuse
    // TLS indices after TlsFree in a way micros depend on — zero is enough).
    if let Some(slot) = state.kernel.threads.active.tls_values.get_mut(index) {
        *slot = 0;
    }
    state.process.last_error = 0;
    let return_address = engine.return_from_win64_api(1)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 1,
    })
}
/// Handles `KERNEL32.dll!Sleep`.
pub fn handle_sleep(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    // Flush any buffered console output so the frame is visible before sleeping.
    ctx.state.flush_console();
    let engine = &mut *ctx.engine;
    let milliseconds = engine.read_rcx().context("failed to read RCX for Sleep")?;
    let low32 = milliseconds & u64::from(u32::MAX);

    let policy = crate::idle::IdlePolicy::from_env_for(crate::idle::IdleContext::Persistent);
    crate::idle::apply_sleep(policy, low32);

    let return_value = 0;

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from Sleep")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
pub(crate) fn allocate_fake_heap_block(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    size: u64,
) -> u64 {
    state.heap_state.heap.alloc_coherent(engine, size)
}
/// Handles `KERNEL32.dll!MulDiv`.
pub fn handle_mul_div(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let number_raw = engine.read_rcx().context("failed to read RCX for MulDiv")?;

    let numerator_raw = engine.read_rdx().context("failed to read RDX for MulDiv")?;

    let denominator_raw = engine.read_r8().context("failed to read R8 for MulDiv")?;

    let number = i64::from(low_u32_to_i32(number_raw, "MulDiv number")?);
    let numerator = i64::from(low_u32_to_i32(numerator_raw, "MulDiv numerator")?);
    let denominator = i64::from(low_u32_to_i32(denominator_raw, "MulDiv denominator")?);

    let result = if denominator == 0 {
        -1_i64
    } else {
        number
            .checked_mul(numerator)
            .and_then(|product| product.checked_div(denominator))
            .unwrap_or(-1)
    };

    let result_i32 = i32::try_from(result).unwrap_or(-1);
    let result_u32 = u32::from_ne_bytes(result_i32.to_ne_bytes());
    let return_value = u64::from(result_u32);

    let return_address = engine
        .return_from_win64_api(return_value)
        .context("failed to return from MulDiv")?;

    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}
