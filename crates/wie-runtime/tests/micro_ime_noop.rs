//! IMM32 micro-suite: `ime_noop` proves every IME API returns the benign
//! "no IME" value (NULL context, closed status, successful release) so apps
//! fall back to classic input.

mod common;

use common::micro_exe;

#[test]
fn ime_noop_calls_return_benign_values() {
    let Some(pe) = micro_exe("ime_noop.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&pe, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "ime_noop selftest must exit 0: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
