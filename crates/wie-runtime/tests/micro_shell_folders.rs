//! SHELL32 micro-test: CSIDL folder mapping (SHGetSpecialFolderPathW),
//! SHGetFileInfoW on a real guest file, and SHAddToRecentDocs no-crash.
//!
//! Binary from `make -C micro-exes shell_folders`. A missing binary fails the test (see `tests/common/mod.rs`).
//! The micro runs under an override bottle (its SHGetFileInfoW target resolves
//! inside `{root}/drive_c/Users/WIE/Documents`).

mod common;

use common::micro_exe;

#[test]
fn shell_folders_csidl_and_fileinfo_roundtrip() {
    let Some(path) = micro_exe("shell_folders.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };

    let pid = std::process::id();
    let bottle = std::env::temp_dir().join(format!("wie-shell-folders-bottle-{pid}"));

    let summary = wie_runtime::run_micro_exe_with_options(
        &path,
        256,
        wie_runtime::MicroRunOptions {
            bottle_root: Some(bottle.clone()),
            drive_d_root: None,
            guest_args: vec![],
            stdin_bytes: vec![],
            ..wie_runtime::MicroRunOptions::default()
        },
    )
    .expect("run shell_folders");

    assert_eq!(
        summary.exit_code,
        Some(0),
        "exit={:?} term={:?}",
        summary.exit_code,
        summary.run.termination
    );

    // Cleanup the temp bottle.
    let _ = std::fs::remove_dir_all(&bottle);
}
