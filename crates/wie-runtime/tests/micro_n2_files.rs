//! N2 micro-suite: bottle write/read + VFS volume helpers (freestanding PE64).

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
fn n2_write_and_read_file_in_bottle() {
    let Some(write_pe) = micro_exe("write_file.exe") else {
        eprintln!("skip: write_file.exe not built");
        return;
    };
    let Some(read_pe) = micro_exe("read_file.exe") else {
        eprintln!("skip: read_file.exe not built");
        return;
    };

    let bottle = std::env::temp_dir().join(format!("wie-bottle-test-{}", std::process::id()));
    let app = bottle.join("drive_c/App");
    std::fs::create_dir_all(&app).expect("mkdir bottle");
    std::fs::write(app.join("n2_in.txt"), b"hello-n2").expect("seed n2_in");

    let write_summary = wie_runtime::run_micro_exe_with_root(&write_pe, 256, Some(bottle.clone()))
        .expect("write_file run");
    assert_eq!(
        write_summary.exit_code,
        Some(0),
        "{:?}",
        write_summary.run.termination
    );

    let out = app.join("n2_out.txt");
    let bytes = std::fs::read(&out).expect("n2_out on host");
    assert_eq!(bytes, b"WIE_N2");

    let read_summary = wie_runtime::run_micro_exe_with_root(&read_pe, 256, Some(bottle.clone()))
        .expect("read_file run");
    assert_eq!(
        read_summary.exit_code,
        Some(0),
        "{:?}",
        read_summary.run.termination
    );

    // Skeleton dirs from seed_default_skeleton.
    assert!(bottle.join("drive_c/Windows/System32").is_dir());
    assert!(bottle.join("drive_c/Users/WIE/AppData/Local/Temp").is_dir());

    let _ = std::fs::remove_dir_all(&bottle);
}

/// The default Windows folder skeleton, end to end: a fresh bottle (explicit
/// temp root) gets the full folder set materialized by the time the guest's
/// first file op runs. The session start seeds the effective root, so the
/// write_file.exe run above leaves every `BOTTLE_SKELETON_DIRS` directory on
/// the host.
#[test]
fn n2_fresh_bottle_gets_the_full_default_skeleton() {
    let Some(write_pe) = micro_exe("write_file.exe") else {
        eprintln!("skip: write_file.exe not built");
        return;
    };

    let bottle = std::env::temp_dir().join(format!("wie-skeleton-test-{}", std::process::id()));
    let summary = wie_runtime::run_micro_exe_with_root(&write_pe, 256, Some(bottle.clone()))
        .expect("write_file run");
    assert_eq!(summary.exit_code, Some(0), "{:?}", summary.run.termination);

    for rel in wie_winapi::vfs::BOTTLE_SKELETON_DIRS {
        let dir = bottle.join("drive_c").join(rel);
        assert!(
            dir.is_dir(),
            "fresh bottle must be seeded with {rel}: {}",
            dir.display()
        );
    }

    let _ = std::fs::remove_dir_all(&bottle);
}

/// The global-bottle policy, end to end through a real session: a file op
/// with NO `--root` and NO `WIE_ROOT` succeeds — guest `C:\…` maps to the
/// default app-data bottle, which the write creates on demand.
#[test]
fn n2_write_file_without_root_uses_the_global_bottle() {
    let Some(write_pe) = micro_exe("write_file.exe") else {
        eprintln!("skip: write_file.exe not built");
        return;
    };

    // `None` root + `run_micro_exe_with_root` ignores WIE_ROOT, so this
    // deterministically exercises the global app-data bottle fallback.
    let summary = wie_runtime::run_micro_exe_with_root(&write_pe, 256, None).expect("run");
    assert_eq!(summary.exit_code, Some(0), "{:?}", summary.run.termination);

    let host = wie_winapi::global_bottle_root()
        .join("drive_c")
        .join("App")
        .join("n2_out.txt");
    assert!(
        host.is_file(),
        "write_file.exe must have created the global bottle file: {}",
        host.display()
    );
    assert_eq!(
        std::fs::read(&host).expect("read global bottle output"),
        b"WIE_N2",
        "bytes round-trip through the global bottle"
    );

    // Remove only the artifact; the global bottle dirs are the product's own
    // app-data layout and legitimately persist.
    let _cleanup = std::fs::remove_file(&host);
}

#[test]
fn vfs_drive_d_maps_host_tree() {
    let bottle = std::env::temp_dir().join(format!("wie-bottle-d-{}", std::process::id()));
    let drive_d = std::env::temp_dir().join(format!("wie-drive-d-{}", std::process::id()));
    std::fs::create_dir_all(&bottle).unwrap();
    std::fs::create_dir_all(&drive_d).unwrap();
    std::fs::write(drive_d.join("sample.txt"), b"from-d").unwrap();

    let volumes = wie_winapi::VolumeConfig::from_parts(Some(bottle.clone()), Some(drive_d.clone()));
    let map = wie_winapi::vfs::guest_path_to_host(&volumes, r"D:\sample.txt").expect("D map");
    assert_eq!(std::fs::read(&map.host).unwrap(), b"from-d");
    assert_eq!(
        wie_winapi::vfs::logical_drives_mask(&volumes),
        (1 << 2) | (1 << 3)
    );
    assert_eq!(
        wie_winapi::vfs::get_drive_type(&volumes, r"D:\"),
        wie_winapi::vfs::DRIVE_FIXED
    );

    let _ = std::fs::remove_dir_all(&bottle);
    let _ = std::fs::remove_dir_all(&drive_d);
}
