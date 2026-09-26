//! DBGHELP symbol stub micro-exe (freestanding PE64).
//!
//! Binary from `make -C micro-exes dbghelp_stub`. A missing binary fails the test (see `tests/common/mod.rs`).

mod common;

use common::micro_exe;

/// SymInitializeW succeeds, SymFromAddrW fails gracefully, SymCleanup succeeds.
#[test]
fn dbghelp_init_cleanup_roundtrip() {
    let Some(path) = micro_exe("dbghelp_stub.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "dbghelp_stub: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
