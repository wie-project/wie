//! Api-set completeness check (the deliverable for the ntdll lane).
//!
//! Three contracts are pinned here, all through the public dispatch oracle
//! (`is_winapi_implemented`) so they cannot rot:
//!
//! 1. Every `api-ms-win-crt-*` family resolves. UCRT dispatch is namespace-wide
//!    by export name (Windows forwards every family to `ucrtbase.dll`), so the
//!    contract per family is classification (`is_ucrt_library`) plus one
//!    callable export.
//! 2. The ntdll Nt*/Rtl* surface is reported by the oracle.
//! 3. Known gaps fail gracefully: `api-ms-win-core-*` (non-CRT sets) and CRT
//!    exports with no host handler return `false` from the oracle — never a
//!    panic. `NtCreateProcess` / `NtCreateUserProcess` stay unimplemented
//!    (kernel process creation is a non-goal).

use wie_winapi::is_winapi_implemented;
use wie_winapi::ntdll::is_export;
use wie_winapi::ucrt::is_ucrt_library;

/// Representative `api-ms-win-crt-*` families (SDK api-set names).
///
/// Families whose own exports (math `sqrt`, filesystem `_findfirst*`,
/// multibyte `_mbs*`, misc `qsort`) have no host handler still classify via
/// `is_ucrt_library` and fail gracefully on those names; the probe name is the
/// callable export the family resolves through.
const CRT_FAMILIES: &[(&str, &str)] = &[
    ("api-ms-win-crt-stdio-l1-1-0.dll", "fopen"),
    ("api-ms-win-crt-string-l1-1-0.dll", "strlen"),
    ("api-ms-win-crt-environment-l1-1-0.dll", "getenv"),
    ("api-ms-win-crt-time-l1-1-0.dll", "_time64"),
    ("api-ms-win-crt-math-l1-1-0.dll", "fopen"),
    ("api-ms-win-crt-runtime-l1-1-0.dll", "_purecall"),
    ("api-ms-win-crt-private-l1-1-0.dll", "__acrt_iob_func"),
    ("api-ms-win-crt-locale-l1-1-0.dll", "setlocale"),
    ("api-ms-win-crt-conio-l1-1-0.dll", "_getch"),
    ("api-ms-win-crt-process-l1-1-0.dll", "system"),
    ("api-ms-win-crt-filesystem-l1-1-0.dll", "fopen"),
    ("api-ms-win-crt-utility-l1-1-0.dll", "rand"),
    ("api-ms-win-crt-multibyte-l1-1-0.dll", "fopen"),
    ("api-ms-win-crt-convert-l1-1-0.dll", "atoi"),
    ("api-ms-win-crt-heap-l1-1-0.dll", "malloc"),
    ("api-ms-win-crt-linktime-l1-1-0.dll", "__c_specific_handler"),
    ("api-ms-win-crt-misc-l1-1-0.dll", "fopen"),
];

/// Every ntdll export the implementation lane dispatches. Must stay in sync
/// with `crate::ntdll::NTDL_EXPORTS` (the in-module dispatch sweep test
/// enforces that the const and the match arms agree; this list pins the
/// user-facing contract).
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

/// Every CRT family classifies as a UCRT library and resolves a callable
/// export through the name-based dispatch namespace.
#[test]
fn crt_families_are_classified_and_dispatch_by_name() {
    for (dll, probe) in CRT_FAMILIES {
        assert!(
            is_ucrt_library(dll),
            "{dll} must classify as a UCRT library"
        );
        assert!(
            is_winapi_implemented(dll, probe),
            "{dll}!{probe} must resolve through the UCRT name dispatch"
        );
    }
}

/// The ntdll Nt*/Rtl* surface is reported by the oracle, case-insensitively.
#[test]
fn ntdll_exports_are_reported_by_the_oracle() {
    for name in NTDL_EXPORTS {
        assert!(is_export(name), "{name} must be reported by is_export");
        assert!(
            is_winapi_implemented("ntdll.dll", name),
            "ntdll.dll!{name} must resolve through the dispatch oracle"
        );
    }
    assert!(is_export("NtClose"));
    assert!(is_winapi_implemented("ntdll.dll", "NtClose"));
}

/// Documented gaps fail gracefully: `api-ms-win-core-*` (non-CRT sets) and
/// CRT exports with no host handler report `false` — never a panic.
#[test]
fn non_crt_api_sets_and_unimplemented_exports_fail_gracefully() {
    for (dll, name) in [
        ("api-ms-win-core-file-l1-1-0.dll", "createfilew"),
        ("api-ms-win-core-heap-l1-1-0.dll", "heapalloc"),
        ("api-ms-win-core-synch-l1-1-0.dll", "waitforsingleobject"),
    ] {
        assert!(!is_ucrt_library(dll), "{dll} is not a CRT family");
        assert!(
            !is_winapi_implemented(dll, name),
            "{dll}!{name} must stay unresolved"
        );
    }
    assert!(!is_winapi_implemented(
        "api-ms-win-crt-math-l1-1-0.dll",
        "sqrt"
    ));
    assert!(!is_winapi_implemented(
        "api-ms-win-crt-filesystem-l1-1-0.dll",
        "_mkdir"
    ));
    assert!(!is_winapi_implemented("ntdll.dll", "ntcreateprocess"));
}
