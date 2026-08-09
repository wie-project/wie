//! msvcrt.dll import-census gaps micro-test: fgetwc, getc, vfprintf, _wcmdln.
//!
//! Binary from micro-exes/msvcrt_gaps (CRT-linked). Skips if missing (no mingw).
//! `__CxxFrameHandler` is exercised by the cpp_exes suite, not here (C micro).

use std::path::PathBuf;

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

#[test]
fn msvcrt_notepad_gap_apis_roundtrip() {
    let Some(path) = micro_exe("msvcrt_gaps.exe") else {
        eprintln!(
            "skip: micro-exes/out/msvcrt_gaps.exe not built (run make -C micro-exes msvcrt_gaps)"
        );
        return;
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
