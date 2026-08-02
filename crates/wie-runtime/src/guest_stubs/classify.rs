//! Library/export → [`GuestStubKind`] classification under Microsoft Learn.

use super::config::{
    CLOCK_TABLE_SLOT_FILETIME, CLOCK_TABLE_SLOT_QPC, CLOCK_TABLE_SLOT_QPC_FREQ,
    CLOCK_TABLE_SLOT_TICK, CLOCK_TABLE_SLOT_TICK64, CLOCK_TABLE_SLOT_TIME, COLOR_COUNT,
    FAKE_DESKTOP_WINDOW, FAKE_SYSCOLOR_BRUSH_BASE, GuestStubConfig, LANG_EN_US, METRICS_COUNT,
};
use super::kind::GuestStubKind;

/// x64 TEB.LastErrorValue offset (also used as our guest mirror VA when TEB base is 0).
pub const TEB_LAST_ERROR_VA: u64 = wie_cpu::GS_BASE + 0x68;

/// Guest FLS table slot count (index 0..N-1 accelerated).
pub const GUEST_FLS_SLOT_COUNT: u32 = 256;

/// Classify a library/export as an in-guest stub when safe under Microsoft Learn.
#[must_use]
pub(crate) fn classify_guest_stub(
    library: &str,
    name: &str,
    cfg: &GuestStubConfig,
) -> Option<GuestStubKind> {
    // UCRT pure helpers: cover indirect `call reg` paths that miss the JIT near-call
    // fast path (still no host-stop). Matches FILE* cookies / CRT slots in `wie_winapi::ucrt`.
    if wie_winapi::ucrt::is_ucrt_library(library) {
        const CRT: u64 = 0x0000_0000_6800_0000;
        let n = name;
        if n.eq_ignore_ascii_case("__acrt_iob_func") {
            return Some(GuestStubKind::AcrtIobFunc);
        }
        // `_initterm` / `_initterm_e` must invoke guest constructor tables.
        if n.eq_ignore_ascii_case("_initterm") {
            return Some(GuestStubKind::Initterm);
        }
        if n.eq_ignore_ascii_case("_initterm_e") {
            return Some(GuestStubKind::InittermE);
        }
        // No-op / fixed-success CRT init (host handlers only returned 0).
        if n.eq_ignore_ascii_case("fflush")
            || n.eq_ignore_ascii_case("setvbuf")
            || n.eq_ignore_ascii_case("_crt_atexit")
            || n.eq_ignore_ascii_case("_set_invalid_parameter_handler")
            || n.eq_ignore_ascii_case("_set_app_type")
            || n.eq_ignore_ascii_case("_set_new_mode")
            || n.eq_ignore_ascii_case("_configure_narrow_argv")
            || n.eq_ignore_ascii_case("_initialize_narrow_environment")
            || n.eq_ignore_ascii_case("__setusermatherr")
            || n.eq_ignore_ascii_case("_configthreadlocale")
            || n.eq_ignore_ascii_case("_cexit")
            || n.eq_ignore_ascii_case("signal")
        {
            return Some(GuestStubKind::ReturnZero);
        }
        if n.eq_ignore_ascii_case("__p__environ") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x300));
        }
        if n.eq_ignore_ascii_case("__p___argv") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x308));
        }
        if n.eq_ignore_ascii_case("__p___argc") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x310));
        }
        if n.eq_ignore_ascii_case("__p__commode") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x318));
        }
        if n.eq_ignore_ascii_case("__p__fmode") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x320));
        }
        if n.eq_ignore_ascii_case("__p__acmdln") {
            return Some(GuestStubKind::ReturnImm64(CRT + 0x328));
        }
        return None;
    }

    let n = name;

    // --- USER32 pure queries (fixed guest desktop environment) ---
    if library.eq_ignore_ascii_case("USER32.dll") {
        if n.eq_ignore_ascii_case("GetSystemMetrics") {
            return Some(GuestStubKind::LoadU32FromTable {
                table_va: cfg.metrics_table_va,
                max_index: METRICS_COUNT as u32,
            });
        }
        if n.eq_ignore_ascii_case("GetSysColor") {
            return Some(GuestStubKind::LoadU32FromTable {
                table_va: cfg.colors_table_va,
                max_index: COLOR_COUNT as u32,
            });
        }
        if n.eq_ignore_ascii_case("GetSysColorBrush") {
            // Microsoft: returns a handle to the logical brush; we use a stable
            // fake HBRUSH space base+index (same as host user32 handler).
            return Some(GuestStubKind::SysColorBrush {
                base: FAKE_SYSCOLOR_BRUSH_BASE,
            });
        }
        if n.eq_ignore_ascii_case("GetDesktopWindow") {
            // Microsoft: handle to the desktop window — single fake desktop HWND.
            return Some(GuestStubKind::ReturnImm64(FAKE_DESKTOP_WINDOW));
        }
        if n.eq_ignore_ascii_case("DialogBoxParamA") {
            return Some(GuestStubKind::DialogBoxParam {
                create_dialog_param_va: cfg.create_dialog_param_a_va,
                get_message_va: cfg.get_message_a_va,
                is_dialog_message_va: cfg.is_dialog_message_a_va,
                dispatch_message_va: cfg.dispatch_message_a_va,
                dialog_result_va: cfg.dialog_result_va,
            });
        }
        if n.eq_ignore_ascii_case("DialogBoxParamW") {
            return Some(GuestStubKind::DialogBoxParam {
                create_dialog_param_va: cfg.create_dialog_param_w_va,
                get_message_va: cfg.get_message_a_va,
                is_dialog_message_va: cfg.is_dialog_message_a_va,
                dispatch_message_va: cfg.dispatch_message_a_va,
                dialog_result_va: cfg.dialog_result_va,
            });
        }
        return None;
    }

    if library.eq_ignore_ascii_case("WINMM.dll") {
        // B5: timeGetTime reads the host-written guest clock table (slot 2).
        if n.eq_ignore_ascii_case("timeGetTime") {
            return Some(GuestStubKind::LoadZx32FromVa(
                cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_TIME),
            ));
        }
        return None;
    }

    if !library.eq_ignore_ascii_case("KERNEL32.dll") && !library.eq_ignore_ascii_case("ntdll.dll") {
        return None;
    }

    if n.eq_ignore_ascii_case("EncodePointer") || n.eq_ignore_ascii_case("DecodePointer") {
        return Some(GuestStubKind::IdentityRcxToRax);
    }
    // Enter/Leave/DeleteCriticalSection: host only (MT.1 real owner/recursion).
    // InitializeCriticalSection* stays on host (writes RTL_CRITICAL_SECTION).
    if n.eq_ignore_ascii_case("GetLastError") {
        return Some(GuestStubKind::LoadZx32FromVa(TEB_LAST_ERROR_VA));
    }
    if n.eq_ignore_ascii_case("SetLastError") {
        return Some(GuestStubKind::StoreEcxToVa(TEB_LAST_ERROR_VA));
    }
    if n.eq_ignore_ascii_case("FlsGetValue") {
        return Some(GuestStubKind::FlsGetValue {
            table_va: cfg.fls_table_va,
            max_slots: GUEST_FLS_SLOT_COUNT,
        });
    }
    if n.eq_ignore_ascii_case("FlsSetValue") {
        return Some(GuestStubKind::FlsSetValue {
            table_va: cfg.fls_table_va,
            max_slots: GUEST_FLS_SLOT_COUNT,
        });
    }

    if n.eq_ignore_ascii_case("SetHandleCount")
        || n.eq_ignore_ascii_case("OutputDebugStringA")
        || n.eq_ignore_ascii_case("OutputDebugStringW")
    {
        return Some(GuestStubKind::VoidRet);
    }
    // B5: the host refreshes a guest clock table on every stop; these in-guest
    // stubs read it with no host stop. A constant stub is deliberately NOT
    // planted — a guest frame loop computing `dt = now - last` would busy-wait
    // on `dt == 0` forever (the frozen-clock trap documented below). The table
    // advances monotonically because the refresh derives from one monotonic
    // session epoch; `WIE_FIXED_CLOCK=1` freezes it (write-once at session
    // init), preserving the deterministic-trace kill switch.
    if n.eq_ignore_ascii_case("GetTickCount") {
        return Some(GuestStubKind::LoadZx32FromVa(
            cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_TICK),
        ));
    }
    if n.eq_ignore_ascii_case("GetTickCount64") {
        return Some(GuestStubKind::LoadZx64FromVa(
            cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_TICK64),
        ));
    }
    if n.eq_ignore_ascii_case("GetSystemTimeAsFileTime") {
        return Some(GuestStubKind::CopyU64FromVaToRcxPtr {
            slot_va: cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_FILETIME),
        });
    }
    if n.eq_ignore_ascii_case("QueryPerformanceCounter") {
        return Some(GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
            slot_va: cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_QPC),
        });
    }
    if n.eq_ignore_ascii_case("QueryPerformanceFrequency") {
        return Some(GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
            slot_va: cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_QPC_FREQ),
        });
    }
    if n.eq_ignore_ascii_case("GetCurrentProcessId") {
        return Some(GuestStubKind::ReturnImm32(0x1234));
    }
    // Primary TID is fixed (`PRIMARY_THREAD_ID` / 0x5678). Host path reads
    // ThreadState when the stub is not planted (workers in MT.2).
    if n.eq_ignore_ascii_case("GetCurrentThreadId") {
        return Some(GuestStubKind::ReturnImm32(wie_winapi::PRIMARY_THREAD_ID));
    }
    if n.eq_ignore_ascii_case("IsDebuggerPresent") {
        return Some(GuestStubKind::ReturnZero);
    }
    // Sleep is never planted: host idle policy (Phase 6) must see every call.
    if n.eq_ignore_ascii_case("GetACP") {
        return Some(GuestStubKind::ReturnImm32(1252));
    }
    if n.eq_ignore_ascii_case("GetOEMCP") {
        return Some(GuestStubKind::ReturnImm32(437));
    }
    // Microsoft Learn: LANGID en-US = 0x0409 for both when guest is fixed en-US.
    if n.eq_ignore_ascii_case("GetSystemDefaultLangID")
        || n.eq_ignore_ascii_case("GetUserDefaultLangID")
    {
        return Some(GuestStubKind::ReturnImm32(LANG_EN_US));
    }
    if n.eq_ignore_ascii_case("GetCurrentProcess") {
        // Microsoft: (HANDLE)(LONG_PTR)-1 process pseudohandle.
        return Some(GuestStubKind::ReturnImm64(u64::MAX));
    }
    if n.eq_ignore_ascii_case("GetProcessHeap") {
        return Some(GuestStubKind::ReturnImm64(0x0000_0000_5000_0000));
    }
    // Microsoft: returns pointer to the command-line string for the process.
    if n.eq_ignore_ascii_case("GetCommandLineA") {
        return Some(GuestStubKind::ReturnImm64(cfg.command_line_a_va));
    }
    if n.eq_ignore_ascii_case("GetCommandLineW") {
        return Some(GuestStubKind::ReturnImm64(cfg.command_line_w_va));
    }
    if n.eq_ignore_ascii_case("GetCurrentDirectoryW") {
        return Some(GuestStubKind::GetCurrentDirectoryW {
            cwd_blob_va: cfg.cwd_blob_va,
        });
    }

    // Intentionally NOT stubbed (would damage apps if simplified):
    // - VirtualProtect: NULL lpflOldProtect must fail (Learn); real protect later Phase 3
    // - VirtualQuery: must describe real VA regions (RegionTable)
    // - LocalAlloc/GlobalAlloc: LMEM_MOVEABLE / lock / size-0 discard semantics
    // - SetUnhandledExceptionFilter: must return previous filter for chaining

    None
}
