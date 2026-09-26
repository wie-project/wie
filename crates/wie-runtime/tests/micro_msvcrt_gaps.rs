//! msvcrt.dll import-census gaps micro-test: fgetwc, getc, vfprintf, _wcmdln.
//!
//! Binary from micro-exes/msvcrt_gaps (CRT-linked). A missing binary fails the test (see `tests/common/mod.rs`).
//! `__CxxFrameHandler` is exercised by the cpp_exes suite, not here (C micro).

mod common;

use common::micro_exe;

#[test]
fn msvcrt_notepad_gap_apis_roundtrip() {
    let Some(path) = micro_exe("msvcrt_gaps.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    // "AB" feeds the sequenced reads: fgetwc consumes 'A', getc consumes 'B'.
    let summary = wie_runtime::run_micro_exe_with_options(
        &path,
        256,
        wie_runtime::MicroRunOptions {
            stdin_bytes: b"AB".to_vec(),
            ..wie_runtime::MicroRunOptions::default()
        },
    )
    .expect("run_micro_exe_with_options");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "msvcrt_gaps: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
