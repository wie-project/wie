//! Integration coverage for the run-entry helpers that `src/main.rs` used to
//! hide: the shared [`prepare_run`] setup and the argv-level guards.
//!
//! Everything here stays inside a per-test temp dir (no real bottle, no
//! `~/Library/Application Support/WIE`): the bottle root is always passed
//! explicitly, so `resolve_volume_config`'s `WIE_ROOT` fallback never fires and
//! the only directories created are the two temp dirs the test itself makes.

use std::path::{Path, PathBuf};

use wie_cli::commands::{RunSetup, StageMode, prepare_run};
use wie_cli::{reject_micro_only_flags, resolve_run_root};

/// A temp directory removed on drop; PID-scoped so parallel tests stay
/// disjoint and a crashed earlier run cannot leave a colliding dir behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("wie-it-{tag}-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&path));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

/// A fake executable file the staging copies.
fn fake_exe(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"MZ\x90\x00").expect("write fake exe");
    path
}

/// The `Program Files/<stem>` folder the staging installs into.
fn staged_app_dir(bottle: &Path) -> PathBuf {
    bottle.join("drive_c").join("Program Files").join("app")
}

#[test]
fn prepare_run_stages_the_exe_and_threads_the_bottle_root() {
    let source = TempDir::new("exe-only-src");
    let exe = fake_exe(source.path(), "app.exe");
    // A sibling the exe-only default must NOT stage.
    std::fs::write(source.path().join("data.bin"), b"data").expect("write data");
    let bottle = TempDir::new("exe-only-bottle");
    let guest_args = vec!["-iwad".to_owned(), "freedoom1.wad".to_owned()];

    let prepared = prepare_run(
        &exe,
        RunSetup {
            bottle_root: Some(bottle.path()),
            drive_d_root: None,
            stage: StageMode::from_run_entry(None),
            guest_args: &guest_args,
        },
    )
    .expect("prepare the run");

    let staged_app = staged_app_dir(bottle.path());
    assert_eq!(
        prepared.staged.run_path,
        staged_app.join("app.exe"),
        "an out-of-bottle exe runs from its Program Files copy"
    );
    assert!(
        !staged_app.join("data.bin").exists(),
        "the exe-only default stages no sibling files"
    );
    // The session must agree with the staging: same root, same guest cwd.
    assert_eq!(
        prepared.session_options.bottle_root.as_deref(),
        Some(bottle.path()),
        "the staged bottle root reaches the session"
    );
    assert_eq!(
        prepared.session_options.current_directory.as_deref(),
        Some(r"C:\Program Files\app"),
        "a staged app starts in its own folder"
    );
    assert_eq!(
        prepared.session_options.guest_args, guest_args,
        "guest argv propagates verbatim"
    );
    assert_eq!(
        prepared.volumes.bottle_root.as_deref(),
        Some(bottle.path()),
        "the volumes match the forwarded root"
    );
}

#[test]
fn prepare_run_parent_folder_mode_copies_the_whole_app_folder() {
    let source = TempDir::new("parent-folder-src");
    let exe = fake_exe(source.path(), "app.exe");
    std::fs::write(source.path().join("data.bin"), b"data").expect("write data");
    let bottle = TempDir::new("parent-folder-bottle");

    // The console / persistent entries stage the whole parent folder.
    let prepared = prepare_run(
        &exe,
        RunSetup {
            bottle_root: Some(bottle.path()),
            drive_d_root: None,
            stage: StageMode::ParentFolder,
            guest_args: &[],
        },
    )
    .expect("prepare the run");

    let staged_app = staged_app_dir(bottle.path());
    assert_eq!(
        prepared.staged.run_path,
        staged_app.join("app.exe"),
        "the exe itself is staged in its Program Files folder"
    );
    assert_eq!(
        std::fs::read(staged_app.join("data.bin")).expect("staged data"),
        b"data",
        "the parent-folder mode stages sibling files too"
    );
}

#[test]
fn prepare_run_app_dir_mode_preserves_nested_relative_paths() {
    let app_dir = TempDir::new("app-dir");
    std::fs::create_dir_all(app_dir.path().join("bin")).expect("create bin");
    std::fs::write(app_dir.path().join("bin/app.exe"), b"MZ").expect("write exe");
    std::fs::write(app_dir.path().join("data.bin"), b"data").expect("write data");
    let bottle = TempDir::new("app-dir-bottle");

    let prepared = prepare_run(
        &app_dir.path().join("bin/app.exe"),
        RunSetup {
            bottle_root: Some(bottle.path()),
            drive_d_root: None,
            stage: StageMode::from_run_entry(Some(app_dir.path())),
            guest_args: &[],
        },
    )
    .expect("prepare the run");

    let staged_app = staged_app_dir(bottle.path());
    assert_eq!(
        prepared.staged.run_path,
        staged_app.join("bin/app.exe"),
        "the nested exe keeps its relative path"
    );
    assert!(
        staged_app.join("data.bin").is_file(),
        "sibling resources land beside it"
    );
}

#[test]
fn prepare_run_forwards_both_roots_so_staging_and_guest_agree() {
    let source = TempDir::new("forwarding-src");
    let exe = fake_exe(source.path(), "app.exe");
    let bottle = TempDir::new("forwarding-bottle");
    let bridge = TempDir::new("forwarding-bridge");

    let prepared = prepare_run(
        &exe,
        RunSetup {
            bottle_root: Some(bottle.path()),
            drive_d_root: Some(bridge.path()),
            stage: StageMode::ExeOnly,
            guest_args: &[],
        },
    )
    .expect("prepare the run");

    assert_eq!(
        prepared.session_options.drive_d_root.as_deref(),
        Some(bridge.path()),
        "a --drive-d bridge the staging read must also be mounted for the guest"
    );
    assert_eq!(
        prepared.volumes.drive_d_root.as_deref(),
        Some(bridge.path()),
        "and the staging saw the same bridge"
    );
    assert_eq!(
        prepared.session_options.bottle_root.as_deref(),
        Some(bottle.path()),
        "the bottle root reaches the session too"
    );
}

#[test]
fn prepare_run_rejects_a_missing_source_before_the_session_starts() {
    let bottle = TempDir::new("missing-source-bottle");

    let err = prepare_run(
        &bottle.path().join("ghost.exe"),
        RunSetup {
            bottle_root: Some(bottle.path()),
            drive_d_root: None,
            stage: StageMode::ExeOnly,
            guest_args: &[],
        },
    )
    .expect_err("a missing run source must fail");
    assert!(
        err.to_string().contains("ghost.exe"),
        "the error names the run source: {err}"
    );
}

#[test]
fn resolve_run_root_passes_through_and_rejects_the_conflict() {
    // An explicit `--root` passes through unchanged.
    let root = PathBuf::from("/tmp/wie-it-root");
    assert_eq!(
        resolve_run_root(None, Some(root.clone())).expect("root passthrough"),
        Some(root.clone())
    );
    // Neither flag: `None`, leaving the `WIE_ROOT` fallback to the resolver.
    assert_eq!(resolve_run_root(None, None).expect("no flags"), None);
    // Both flags: rejected even on direct construction (clap blocks it at
    // parse time; this guards tests and future callers).
    let err = resolve_run_root(Some("games".to_owned()), Some(root))
        .expect_err("--bottle and --root must be rejected together");
    assert!(
        err.to_string().contains("mutually exclusive"),
        "the error names the conflict: {err}"
    );
    // A bottle that does not exist is a clear error, not a silent `None`.
    let err = resolve_run_root(Some("no-such-bottle".to_owned()), None)
        .expect_err("a missing bottle must fail");
    assert!(
        err.to_string().contains("does not exist"),
        "the error names the missing bottle: {err}"
    );
}

#[test]
fn reject_micro_only_flags_names_every_rejected_flag() {
    let root = Some(PathBuf::from("/tmp/wie-it-root"));
    let flag = Some(PathBuf::from("/tmp/wie-it-flag"));
    let args = vec!["-n".to_owned()];

    for mode in ["--console", "--persistent"] {
        assert!(
            reject_micro_only_flags(mode, &root, &None, &None, &None, 0, &[]).is_err(),
            "{mode} rejects --root"
        );
        assert!(
            reject_micro_only_flags(mode, &None, &flag, &None, &None, 0, &[]).is_err(),
            "{mode} rejects --stdin"
        );
        assert!(
            reject_micro_only_flags(mode, &None, &None, &flag, &None, 0, &[]).is_err(),
            "{mode} rejects --drive-d"
        );
        assert!(
            reject_micro_only_flags(mode, &None, &None, &None, &flag, 0, &[]).is_err(),
            "{mode} rejects --app-dir"
        );
        assert!(
            reject_micro_only_flags(mode, &None, &None, &None, &None, 7, &[]).is_err(),
            "{mode} rejects --expect-code"
        );
        let err =
            reject_micro_only_flags(mode, &None, &None, &None, &None, 0, &args).expect_err("argv");
        assert!(
            err.to_string().contains(mode),
            "the error names the entry ({mode}): {err}"
        );
    }
    // The micro entry's flag set is accepted.
    assert!(
        reject_micro_only_flags("--console", &None, &None, &None, &None, 0, &[]).is_ok(),
        "the bare console flag set passes"
    );
}
