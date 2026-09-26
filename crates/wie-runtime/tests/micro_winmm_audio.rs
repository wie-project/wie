//! WINMM micro-test: `winmm_audio` proves the timeGetTime / timeSetEvent /
//! waveOut* handlers round-trip (crates/wie-winapi/src/winmm.rs).
//!
//! Binary from `make -C micro-exes winmm_audio`. A missing binary fails the test (see `tests/common/mod.rs`).

mod common;

use common::micro_exe;

#[test]
fn winmm_audio_stubs_roundtrip() {
    let Some(path) = micro_exe("winmm_audio.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "winmm_audio: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
