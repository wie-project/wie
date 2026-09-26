//! Micro test: ADVAPI32!IsTextUnicode distinguishes UTF-16 from ASCII.

mod common;

use common::micro_exe;

#[test]
fn advapi32_istextunicode_detects_wide_and_ansi() {
    let Some(pe) = micro_exe("istextunicode.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&pe, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "{pe:?}: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
