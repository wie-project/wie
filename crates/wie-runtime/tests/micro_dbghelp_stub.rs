//! DBGHELP symbol stub micro-exe (freestanding PE64).
//!
//! Binary from `make -C micro-exes dbghelp_stub`. Skips if missing (no mingw).

use std::path::PathBuf;

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

/// SymInitializeW succeeds, SymFromAddrW fails gracefully, SymCleanup succeeds.
#[test]
fn dbghelp_init_cleanup_roundtrip() {
    let Some(path) = micro_exe("dbghelp_stub.exe") else {
        eprintln!("skip: micro-exes/out/dbghelp_stub.exe not built (run make -C micro-exes)");
        return;
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "dbghelp_stub: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
