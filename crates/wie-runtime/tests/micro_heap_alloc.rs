//! Integration: freestanding HeapAlloc micro-PE reaches ExitProcess(0).
//!
//! Binary is produced by `make -C micro-exes` (mingw). A missing binary fails
//! the test unless `WIE_ALLOW_MISSING_GUESTS=1` is set (see
//! `tests/common/mod.rs`).

mod common;

use common::micro_exe;

#[test]
fn micro_heap_alloc_exits_zero() {
    let Some(path) = micro_exe("heap_alloc.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };

    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
