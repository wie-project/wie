//! COMCTL32 micro-test: `comctl_toolbar` drives InitCommonControlsEx +
//! CreateToolbarEx through the host comctl32 handlers
//! (crates/wie-winapi/src/comctl32.rs).
//!
//! Binary from `make -C micro-exes comctl_toolbar`; a missing binary fails the
//! test (see `tests/common/mod.rs`). The micro is synchronous — it creates a parent, creates the
//! toolbar under it, destroys both, and returns from `main` without ever
//! entering a message loop — so `run_micro_exe` reaches ExitProcess directly
//! and no GUI_SUITE_LOCK serialization is needed.

mod common;

use common::micro_exe;

#[test]
fn comctl32_toolbar_creates() {
    let Some(path) = micro_exe("comctl_toolbar.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
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
