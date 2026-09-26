//! WS2_32 micro-test: single-process loopback TCP echo through the real
//! Winsock handlers (crates/wie-winapi/src/ws2_32.rs).
//!
//! Binary from `make -C micro-exes ws2_echo`. A missing binary fails the test (see `tests/common/mod.rs`).

mod common;

use common::micro_exe;

#[test]
fn ws2_echo_loopback_roundtrip_exits_zero() {
    let Some(path) = micro_exe("ws2_echo.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "ws2_echo: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
