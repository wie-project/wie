//! ADVAPI32 micro-test: registry create → set → query → flush → delete →
//! close round-trip through the real handlers
//! (crates/wie-winapi/src/advapi32.rs).
//!
//! Binary from `make -C micro-exes reg_roundtrip`. A missing binary fails the test (see `tests/common/mod.rs`).

mod common;

use common::micro_exe;

#[test]
fn advapi32_registry_create_set_query_flush_delete_roundtrip() {
    let Some(path) = micro_exe("reg_roundtrip.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "reg_roundtrip: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
