//! Library/export → [`GuestStubKind`] classification under Microsoft Learn.

use super::config::{
    CLOCK_TABLE_SLOT_FILETIME, CLOCK_TABLE_SLOT_QPC, CLOCK_TABLE_SLOT_QPC_FREQ,
    CLOCK_TABLE_SLOT_TICK, CLOCK_TABLE_SLOT_TICK64, CLOCK_TABLE_SLOT_TIME, COLOR_COUNT,
    FAKE_DESKTOP_WINDOW, FAKE_SYSCOLOR_BRUSH_BASE, GuestStubConfig, LANG_EN_US, METRICS_COUNT,
};
use super::kind::GuestStubKind;
use wie_cpu::guest_layout::{
    CRT_ACMDLN_PTR_SLOT, CRT_ARGC_SLOT, CRT_ARGV_PTR_SLOT, CRT_COMMODE_SLOT, CRT_ENVIRON_PTR_SLOT,
    CRT_FMODE_SLOT,
};

/// Guest FLS table slot count (index 0..N-1 accelerated).
pub const GUEST_FLS_SLOT_COUNT: u32 = 256;

/// Marker library for the UCRT group in [`CLASSIFY_TABLE`].
///
/// `is_ucrt_library` matches many names (`api-ms-win-crt-*.dll`,
/// `ucrtbase.dll`, `msvcrt.dll`), so the table cannot use a literal. An
/// unmatched UCRT name falls out of the scan with `None` — the original
/// chain's early return, never falling through to other groups.
const UCRT_LIBRARY: &str = "<ucrt>";

/// Marker library for the KERNEL32/ntdll group, whose names match either
/// library (case-insensitively).
const NT_LIBRARY: &str = "<kernel-or-ntdll>";

/// Guest-stub payload builder: several kinds embed per-session guest VAs
/// (clock/metrics tables, command-line buffers), so the static table stores a
/// non-capturing constructor instead of a concrete [`GuestStubKind`].
type StubKindBuilder = fn(&GuestStubConfig) -> GuestStubKind;

/// Static library/export → [`GuestStubKind`] table, scanned first-match-wins
/// in table order.
///
/// Mirrors the original `eq_ignore_ascii_case` chain exactly: every name keeps
/// case-insensitive comparison, the library groups are exclusive (markers
/// above stand in for the predicate-based `is_ucrt_library` and the
/// KERNEL32-OR-ntdll guard), and the table order preserves the chain's
/// precedence.
static CLASSIFY_TABLE: &[(&str, &str, StubKindBuilder)] = &[
    // ── UCRT pure helpers (still no host-stop) ──
    (UCRT_LIBRARY, "__acrt_iob_func", |_| {
        GuestStubKind::AcrtIobFunc
    }),
    // `_initterm` / `_initterm_e` must invoke guest constructor tables.
    (UCRT_LIBRARY, "_initterm", |_| GuestStubKind::Initterm),
    (UCRT_LIBRARY, "_initterm_e", |_| GuestStubKind::InittermE),
    // No-op / fixed-success CRT init (host handlers only returned 0).
    (UCRT_LIBRARY, "fflush", |_| GuestStubKind::ReturnZero),
    (UCRT_LIBRARY, "setvbuf", |_| GuestStubKind::ReturnZero),
    (UCRT_LIBRARY, "_crt_atexit", |_| GuestStubKind::ReturnZero),
    (UCRT_LIBRARY, "_set_invalid_parameter_handler", |_| {
        GuestStubKind::ReturnZero
    }),
    (UCRT_LIBRARY, "_set_app_type", |_| GuestStubKind::ReturnZero),
    (UCRT_LIBRARY, "_set_new_mode", |_| GuestStubKind::ReturnZero),
    (UCRT_LIBRARY, "_configure_narrow_argv", |_| {
        GuestStubKind::ReturnZero
    }),
    (UCRT_LIBRARY, "_initialize_narrow_environment", |_| {
        GuestStubKind::ReturnZero
    }),
    (UCRT_LIBRARY, "__setusermatherr", |_| {
        GuestStubKind::ReturnZero
    }),
    (UCRT_LIBRARY, "_configthreadlocale", |_| {
        GuestStubKind::ReturnZero
    }),
    (UCRT_LIBRARY, "_cexit", |_| GuestStubKind::ReturnZero),
    (UCRT_LIBRARY, "signal", |_| GuestStubKind::ReturnZero),
    // CRT pointer slots in the guest UCRT data page.
    (UCRT_LIBRARY, "__p__environ", |_| {
        GuestStubKind::ReturnImm64(CRT_ENVIRON_PTR_SLOT)
    }),
    (UCRT_LIBRARY, "__p___argv", |_| {
        GuestStubKind::ReturnImm64(CRT_ARGV_PTR_SLOT)
    }),
    (UCRT_LIBRARY, "__p___argc", |_| {
        GuestStubKind::ReturnImm64(CRT_ARGC_SLOT)
    }),
    (UCRT_LIBRARY, "__p__commode", |_| {
        GuestStubKind::ReturnImm64(CRT_COMMODE_SLOT)
    }),
    (UCRT_LIBRARY, "__p__fmode", |_| {
        GuestStubKind::ReturnImm64(CRT_FMODE_SLOT)
    }),
    (UCRT_LIBRARY, "__p__acmdln", |_| {
        GuestStubKind::ReturnImm64(CRT_ACMDLN_PTR_SLOT)
    }),
    // ── USER32 pure queries (fixed guest desktop environment) ──
    ("USER32.dll", "GetSystemMetrics", |cfg| {
        GuestStubKind::LoadU32FromTable {
            table_va: cfg.metrics_table_va,
            max_index: METRICS_COUNT as u32,
        }
    }),
    ("USER32.dll", "GetSysColor", |cfg| {
        GuestStubKind::LoadU32FromTable {
            table_va: cfg.colors_table_va,
            max_index: COLOR_COUNT as u32,
        }
    }),
    // Microsoft: returns a handle to the logical brush; we use a stable
    // fake HBRUSH space base+index (same as host user32 handler).
    ("USER32.dll", "GetSysColorBrush", |_| {
        GuestStubKind::SysColorBrush {
            base: FAKE_SYSCOLOR_BRUSH_BASE,
        }
    }),
    // Microsoft: handle to the desktop window — single fake desktop HWND.
    ("USER32.dll", "GetDesktopWindow", |_| {
        GuestStubKind::ReturnImm64(FAKE_DESKTOP_WINDOW)
    }),
    ("USER32.dll", "DialogBoxParamA", |cfg| {
        GuestStubKind::DialogBoxParam {
            create_dialog_param_va: cfg.create_dialog_param_a_va,
            get_message_va: cfg.get_message_a_va,
            is_dialog_message_va: cfg.is_dialog_message_a_va,
            dispatch_message_va: cfg.dispatch_message_a_va,
            dialog_result_va: cfg.dialog_result_va,
        }
    }),
    ("USER32.dll", "DialogBoxParamW", |cfg| {
        GuestStubKind::DialogBoxParam {
            create_dialog_param_va: cfg.create_dialog_param_w_va,
            get_message_va: cfg.get_message_a_va,
            is_dialog_message_va: cfg.is_dialog_message_a_va,
            dispatch_message_va: cfg.dispatch_message_a_va,
            dialog_result_va: cfg.dialog_result_va,
        }
    }),
    // ── WINMM ──
    // timeGetTime reads the host-written guest clock table (slot 2).
    ("WINMM.dll", "timeGetTime", |cfg| {
        GuestStubKind::LoadZx32FromVa(cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_TIME))
    }),
    // ── KERNEL32 / ntdll ──
    (NT_LIBRARY, "EncodePointer", |_| {
        GuestStubKind::IdentityRcxToRax
    }),
    (NT_LIBRARY, "DecodePointer", |_| {
        GuestStubKind::IdentityRcxToRax
    }),
    // Enter/Leave/DeleteCriticalSection stay on host (real owner/
    // recursion); InitializeCriticalSection* writes RTL_CRITICAL_SECTION.
    // GetLastError/SetLastError plant GS-relative stubs: the GS base resolves
    // per engine, so every thread reads/writes ITS own TEB last-error slot.
    (NT_LIBRARY, "GetLastError", |_| GuestStubKind::LoadLastError),
    (NT_LIBRARY, "SetLastError", |_| {
        GuestStubKind::StoreLastError
    }),
    (NT_LIBRARY, "FlsGetValue", |cfg| {
        GuestStubKind::FlsGetValue {
            table_va: cfg.fls_table_va,
            max_slots: GUEST_FLS_SLOT_COUNT,
        }
    }),
    (NT_LIBRARY, "FlsSetValue", |cfg| {
        GuestStubKind::FlsSetValue {
            table_va: cfg.fls_table_va,
            max_slots: GUEST_FLS_SLOT_COUNT,
        }
    }),
    (NT_LIBRARY, "SetHandleCount", |_| GuestStubKind::VoidRet),
    // OutputDebugString* deliberately NOT stubbed: the guest's debug output
    // must reach the host trace (the kernel32 handler logs it), so a live run
    // shows WHY a guest failed — not just a bare exit code. A VoidRet stub
    // would swallow the message silently.
    // (NT_LIBRARY, "OutputDebugStringA", |_| GuestStubKind::VoidRet),
    // (NT_LIBRARY, "OutputDebugStringW", |_| GuestStubKind::VoidRet),
    // The host refreshes a guest clock table on every stop; these in-guest
    // stubs read it with no host stop. A constant stub is deliberately NOT
    // planted — a guest frame loop computing `dt = now - last` would busy-wait
    // on `dt == 0` forever (the frozen-clock trap). The table advances
    // monotonically because the refresh derives from one monotonic session
    // epoch; `WIE_FIXED_CLOCK=1` freezes it (write-once at session init).
    (NT_LIBRARY, "GetTickCount", |cfg| {
        GuestStubKind::LoadZx32FromVa(cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_TICK))
    }),
    (NT_LIBRARY, "GetTickCount64", |cfg| {
        GuestStubKind::LoadZx64FromVa(cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_TICK64))
    }),
    (NT_LIBRARY, "GetSystemTimeAsFileTime", |cfg| {
        GuestStubKind::CopyU64FromVaToRcxPtr {
            slot_va: cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_FILETIME),
        }
    }),
    (NT_LIBRARY, "QueryPerformanceCounter", |cfg| {
        GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
            slot_va: cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_QPC),
        }
    }),
    (NT_LIBRARY, "QueryPerformanceFrequency", |cfg| {
        GuestStubKind::CopyU64FromVaToRcxPtrRetOne {
            slot_va: cfg.clock_table_va.saturating_add(CLOCK_TABLE_SLOT_QPC_FREQ),
        }
    }),
    (NT_LIBRARY, "GetCurrentProcessId", |_| {
        GuestStubKind::ReturnImm32(0x1234)
    }),
    // Primary TID is fixed (`PRIMARY_THREAD_ID` / 0x5678). Host path reads
    // ThreadState when the stub is not planted (spawned workers).
    (NT_LIBRARY, "GetCurrentThreadId", |_| {
        GuestStubKind::ReturnImm32(wie_winapi::PRIMARY_THREAD_ID)
    }),
    (NT_LIBRARY, "IsDebuggerPresent", |_| {
        GuestStubKind::ReturnZero
    }),
    // Sleep is never planted: the host idle policy must see every call.
    (NT_LIBRARY, "GetACP", |_| GuestStubKind::ReturnImm32(1252)),
    (NT_LIBRARY, "GetOEMCP", |_| GuestStubKind::ReturnImm32(437)),
    // Microsoft Learn: LANGID en-US = 0x0409 for both when guest is fixed en-US.
    (NT_LIBRARY, "GetSystemDefaultLangID", |_| {
        GuestStubKind::ReturnImm32(LANG_EN_US)
    }),
    (NT_LIBRARY, "GetUserDefaultLangID", |_| {
        GuestStubKind::ReturnImm32(LANG_EN_US)
    }),
    // Microsoft: (HANDLE)(LONG_PTR)-1 process pseudohandle.
    (NT_LIBRARY, "GetCurrentProcess", |_| {
        GuestStubKind::ReturnImm64(u64::MAX)
    }),
    (NT_LIBRARY, "GetProcessHeap", |_| {
        GuestStubKind::ReturnImm64(crate::memory::PROCESS_HEAP_HANDLE)
    }),
    // Microsoft: returns pointer to the command-line string for the process.
    (NT_LIBRARY, "GetCommandLineA", |cfg| {
        GuestStubKind::ReturnImm64(cfg.command_line_a_va)
    }),
    (NT_LIBRARY, "GetCommandLineW", |cfg| {
        GuestStubKind::ReturnImm64(cfg.command_line_w_va)
    }),
    (NT_LIBRARY, "GetCurrentDirectoryW", |cfg| {
        GuestStubKind::GetCurrentDirectoryW {
            cwd_blob_va: cfg.cwd_blob_va,
        }
    }),
];

/// Classify a library/export as an in-guest stub when safe under Microsoft Learn.
#[must_use]
pub(crate) fn classify_guest_stub(
    library: &str,
    name: &str,
    cfg: &GuestStubConfig,
) -> Option<GuestStubKind> {
    // Pre-computed group membership mirrors the original chain's guards:
    // the UCRT branch used `is_ucrt_library` and the last branch accepted
    // either KERNEL32.dll or ntdll.dll (case-insensitive).
    let is_ucrt = wie_winapi::ucrt::is_ucrt_library(library);
    let is_nt =
        library.eq_ignore_ascii_case("KERNEL32.dll") || library.eq_ignore_ascii_case("ntdll.dll");

    for &(lib, export, builder) in CLASSIFY_TABLE {
        let lib_matches = match lib {
            UCRT_LIBRARY => is_ucrt,
            NT_LIBRARY => is_nt,
            _ => library.eq_ignore_ascii_case(lib),
        };
        if lib_matches && name.eq_ignore_ascii_case(export) {
            return Some(builder(cfg));
        }
    }

    // Intentionally NOT stubbed (would damage apps if simplified):
    // - VirtualProtect: NULL lpflOldProtect must fail (Learn); real protect stays host-side
    // - VirtualQuery: must describe real VA regions (RegionTable)
    // - LocalAlloc/GlobalAlloc: LMEM_MOVEABLE / lock / size-0 discard semantics
    // - SetUnhandledExceptionFilter: must return previous filter for chaining

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table entry must classify to its own builder's kind, and no
    /// `(library, name)` pair may appear twice — otherwise first-match-wins
    /// would depend on table order and drift silently.
    #[test]
    fn classify_table_is_consistent() {
        let cfg = GuestStubConfig::CLASSIFY_ONLY;
        let mut seen = std::collections::HashSet::new();
        for &(lib, name, builder) in CLASSIFY_TABLE {
            let key = (lib, name);
            assert!(
                seen.insert(key),
                "duplicate CLASSIFY_TABLE entry for {lib}!{name}"
            );
            let expected = builder(&cfg);
            // Marker libraries stand in for predicate-based groups, so
            // classify with a representative real library name.
            let classify_lib = match lib {
                UCRT_LIBRARY => "ucrtbase.dll",
                NT_LIBRARY => "KERNEL32.dll",
                _ => lib,
            };
            assert_eq!(
                classify_guest_stub(classify_lib, name, &cfg),
                Some(expected),
                "{lib}!{name} must classify to its table kind"
            );
        }
    }

    /// Library groups stay exclusive: a name that only lives in one group
    /// must never classify under another (the original chain's early returns),
    /// the NT group covers both KERNEL32.dll and ntdll.dll, and name matching
    /// stays case-insensitive.
    #[test]
    fn classify_table_groups_are_exclusive() {
        let cfg = GuestStubConfig::CLASSIFY_ONLY;
        // UCRT-only name: classifies under a UCRT library…
        assert!(classify_guest_stub("ucrtbase.dll", "_cexit", &cfg).is_some());
        // …and NOT under KERNEL32.dll (the original chain returned early).
        assert!(classify_guest_stub("KERNEL32.dll", "_cexit", &cfg).is_none());
        // KERNEL32-only name: not under a UCRT library.
        assert!(classify_guest_stub("ucrtbase.dll", "GetTickCount", &cfg).is_none());
        // The NT group matches ntdll.dll too.
        assert_eq!(
            classify_guest_stub("ntdll.dll", "EncodePointer", &cfg),
            Some(GuestStubKind::IdentityRcxToRax)
        );
        // Case-insensitive library and name matching is preserved.
        assert_eq!(
            classify_guest_stub("kernel32.dll", "gettickcount", &cfg),
            classify_guest_stub("KERNEL32.dll", "GetTickCount", &cfg)
        );
    }
}
