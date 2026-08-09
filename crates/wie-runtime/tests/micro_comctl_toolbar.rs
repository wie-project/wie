//! COMCTL32 micro-test: `comctl_toolbar` drives InitCommonControlsEx +
//! CreateToolbarEx through the host comctl32 handlers
//! (crates/wie-winapi/src/comctl32.rs).
//!
//! Binary from `make -C micro-exes comctl_toolbar`. Skips if missing (no
//! mingw). The micro is synchronous — it creates a parent, creates the
//! toolbar under it, destroys both, and returns from `main` without ever
//! entering a message loop — so `run_micro_exe` reaches ExitProcess directly
//! and no GUI_SUITE_LOCK serialization is needed.

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
fn comctl32_toolbar_creates() {
    let Some(path) = micro_exe("comctl_toolbar.exe") else {
        eprintln!(
            "skip: micro-exes/out/comctl_toolbar.exe not built (run make -C micro-exes comctl_toolbar)"
        );
        return;
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "comctl_toolbar: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
