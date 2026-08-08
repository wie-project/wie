//! SETUPAPI device-list stub micro-exe (freestanding PE64).
//!
//! Binary from `make -C micro-exes setupapi_list`. Skips if missing (no mingw).

use std::path::PathBuf;

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

/// SetupDiGetClassDevsW → fake HDEVINFO, empty enumeration, destroy succeeds.
#[test]
fn setupapi_enumeration_returns_empty_list() {
    let Some(path) = micro_exe("setupapi_list.exe") else {
        eprintln!("skip: micro-exes/out/setupapi_list.exe not built (run make -C micro-exes)");
        return;
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "setupapi_list: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
