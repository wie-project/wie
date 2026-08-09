//! OLE32/OLEAUT32 micro-suite: COM GUID string round-trip + SafeArray round-trip.
//!
//! Binaries from `make -C micro-exes`. Skips if missing (no mingw).

use std::path::PathBuf;

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

/// CoInitializeEx / CoCreateGuid / StringFromCLSID↔CLSIDFromString /
/// SafeArray create→access→get-element→bounds→destroy, all through the
/// ole32 + oleaut32 handlers (see micro-exes/ole_com/main.c for exit codes).
#[test]
fn ole_com_guid_string_safearray_roundtrip() {
    let Some(path) = micro_exe("ole_com.exe") else {
        eprintln!("skip: micro-exes/out/ole_com.exe not built (run make -C micro-exes)");
        return;
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "ole_com: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}
