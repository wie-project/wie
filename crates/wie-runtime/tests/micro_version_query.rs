//! VERSION.dll micro-suite: the `version_query` micro reads its own
//! `RT_VERSION` resource through the standard flow
//! (GetFileVersionInfoSizeW → GetFileVersionInfoW → VerQueryValueW) and
//! asserts the fixed info, the FileVersion string, and the Translation pair.

mod common;

use common::micro_exe;

#[test]
fn version_query_reads_its_own_version_resource() {
    let Some(pe) = micro_exe("version_query.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };

    // The version handlers are file ops: they run under the bottle policy.
    let bottle = std::env::temp_dir().join(format!("wie-version-bottle-{}", std::process::id()));
    let summary = wie_runtime::run_micro_exe_with_root(&pe, 256, Some(bottle.clone()))
        .expect("version_query run");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "version_query selftest must exit 0: {:?}",
        summary.run.termination
    );

    let _ = std::fs::remove_dir_all(&bottle);
}
