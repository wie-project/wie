//! SETUPAPI device-list stub micro-exe (freestanding PE64).
//!
//! Binary from `make -C micro-exes setupapi_list`. A missing binary fails the test (see `tests/common/mod.rs`).

mod common;

use common::micro_exe;

/// SetupDiGetClassDevsW → fake HDEVINFO, empty enumeration, destroy succeeds.
#[test]
fn setupapi_enumeration_returns_empty_list() {
    let Some(path) = micro_exe("setupapi_list.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "setupapi_list: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
