//! Micro test: ADVAPI32!IsTextUnicode distinguishes UTF-16 from ASCII.

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
fn advapi32_istextunicode_detects_wide_and_ansi() {
    let Some(pe) = micro_exe("istextunicode.exe") else {
        eprintln!("skip: istextunicode.exe not built (run make -C micro-exes istextunicode)");
        return;
    };
    let summary = wie_runtime::run_micro_exe(&pe, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "{pe:?}: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
