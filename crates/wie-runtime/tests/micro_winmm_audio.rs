//! WINMM micro-test: `winmm_audio` proves the timeGetTime / timeSetEvent /
//! waveOut* handlers round-trip (crates/wie-winapi/src/winmm.rs).
//!
//! Binary from `make -C micro-exes winmm_audio`. Skips if missing (no mingw).

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
fn winmm_audio_stubs_roundtrip() {
    let Some(path) = micro_exe("winmm_audio.exe") else {
        eprintln!(
            "skip: micro-exes/out/winmm_audio.exe not built (run make -C micro-exes winmm_audio)"
        );
        return;
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
