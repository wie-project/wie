//! ADVAPI32 micro-test: registry create → set → query → flush → delete →
//! close round-trip through the real handlers
//! (crates/wie-winapi/src/advapi32.rs).
//!
//! Binary from `make -C micro-exes reg_roundtrip`. Skips if missing (no mingw).

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
fn advapi32_registry_create_set_query_flush_delete_roundtrip() {
    let Some(path) = micro_exe("reg_roundtrip.exe") else {
        eprintln!("skip: micro-exes/out/reg_roundtrip.exe not built (run make -C micro-exes)");
        return;
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
