//! IMM32 micro-suite: `ime_noop` proves every IME API returns the benign
//! "no IME" value (NULL context, closed status, successful release) so apps
//! fall back to classic input.

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
fn ime_noop_calls_return_benign_values() {
    let Some(pe) = micro_exe("ime_noop.exe") else {
        eprintln!("skip: ime_noop.exe not built (run make -C micro-exes ime_noop)");
        return;
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
