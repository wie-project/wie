//! USER32 micro-test: `user32_char` exercises the Tier-2 soft-dispatch
//! char/format lane (crates/wie-winapi/src/user32/charfmt.rs) end-to-end —
//! CharUpperW (string + single-char forms), CharPrevExA, wsprintfW, and the
//! SetProcessDefaultLayout / WinHelpW no-ops.
//!
//! The micro creates no windows, so it needs no GUI_SUITE_LOCK serialization.
//!
//! Binary from `make -C micro-exes user32_char`. A missing binary fails the test (see `tests/common/mod.rs`).

mod common;

use common::micro_exe;

#[test]
fn user32_notepad_gap_apis_roundtrip() {
    let Some(path) = micro_exe("user32_char.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "user32_char: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
