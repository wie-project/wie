//! SHELL32 micro-test: CSIDL folder mapping (SHGetSpecialFolderPathW),
//! SHGetFileInfoW on a real guest file, and SHAddToRecentDocs no-crash.
//!
//! Binary from `make -C micro-exes shell_folders`. Skips if missing (no mingw).
//! The micro runs under an override bottle (its SHGetFileInfoW target resolves
//! inside `{root}/drive_c/Users/WIE/Documents`).

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
fn shell_folders_csidl_and_fileinfo_roundtrip() {
    let Some(path) = micro_exe("shell_folders.exe") else {
        eprintln!(
            "skip: micro-exes/out/shell_folders.exe not built (run make -C micro-exes shell_folders)"
        );
        return;
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
