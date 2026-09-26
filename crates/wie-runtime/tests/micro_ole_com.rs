//! OLE32/OLEAUT32 micro-suite: COM GUID string round-trip + SafeArray round-trip.
//!
//! Binaries from `make -C micro-exes`. A missing binary fails the test (see `tests/common/mod.rs`).

mod common;

use common::micro_exe;

/// CoInitializeEx / CoCreateGuid / StringFromCLSID↔CLSIDFromString /
/// SafeArray create→access→get-element→bounds→destroy, all through the
/// ole32 + oleaut32 handlers (see micro-exes/ole_com/main.c for exit codes).
#[test]
fn ole_com_guid_string_safearray_roundtrip() {
    let Some(path) = micro_exe("ole_com.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "ole_com: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
