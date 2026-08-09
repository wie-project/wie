//! USER32 micro-test: `user32_enum` exercises the Tier-2 soft-dispatch lane
//! (crates/wie-winapi/src/user32/enum_caret.rs) end-to-end — EnumWindows with
//! a real guest callback, FindWindowW, the caret Create/SetPos/GetPos/Destroy
//! round-trip, and DrawIcon.
//!
//! The micro creates a top-level window (never shown, no message loop), so it
//! needs no GUI_SUITE_LOCK serialization: the fake window system is host-side
//! bookkeeping with no winit/present interaction in a headless run.
//!
//! Binary from `make -C micro-exes user32_enum`. Skips if missing (no mingw).

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
fn user32_enum_find_caret_roundtrip() {
    let Some(path) = micro_exe("user32_enum.exe") else {
        eprintln!(
            "skip: micro-exes/out/user32_enum.exe not built (run make -C micro-exes user32_enum)"
        );
        return;
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "user32_enum: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
