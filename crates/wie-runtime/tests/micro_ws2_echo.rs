//! WS2_32 micro-test: single-process loopback TCP echo through the real
//! Winsock handlers (crates/wie-winapi/src/ws2_32.rs).
//!
//! Binary from `make -C micro-exes ws2_echo`. Skips if missing (no mingw).

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
fn ws2_echo_loopback_roundtrip_exits_zero() {
    let Some(path) = micro_exe("ws2_echo.exe") else {
        eprintln!("skip: micro-exes/out/ws2_echo.exe not built (run make -C micro-exes)");
        return;
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
