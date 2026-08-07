//! Pick-mounts: out-of-bottle file-dialog picks mapped back to their real
//! host files.
//!
//! # The consent boundary
//!
//! The native file dialog IS the human's explicit grant to touch one host
//! file. When the user picks a path the guest volumes cannot see (outside the
//! C: bottle and the optional D: bridge), the accept registers a PICK-MOUNT:
//! a guest path (`Z:\pick{N}\{name}`) bound to the EXACT host file the user
//! picked. The dialog returns the guest path; the guest's later
//! CreateFileW/ReadFile/WriteFile on that stored path reads/writes the REAL
//! host file in place — open in place, save in place, created where the user
//! picked.
//!
//! The mounts are populated ONLY by dialog accepts: the guest has no API to
//! forge them (there is no new WinApiId), and an unregistered guest path
//! under `Z:` resolves to nothing (the isolation test pins this).
//!
//! # Store
//!
//! Process-global `Mutex<Vec<_>>` — the same shared-mutable-state seam as the
//! volume layer's on-demand bottle creation. `guest_path_to_host` is a free
//! function taking only `&VolumeConfig`, so the table it consults must live
//! outside the state; one process runs one session (the CLI), so a
//! process-global table is exactly the session's table.
//!
//! # Collision semantics
//!
//! Each accepted pick gets a UNIQUE guest path (`Z:\pick1\a.txt`,
//! `Z:\pick2\b.txt`), so two picks of different host files that share a
//! basename (Desktop/a.txt + Downloads/a.txt) can never silently resolve to
//! the wrong file — an older saved path stays bound to its own file. Picking
//! the SAME host file again returns its existing guest path (dedup), so
//! repeat Save-As of one file is idempotent. The guest path is opaque to the
//! user (it surfaces only inside `lpstrFile` / GetFileTitle, where the
//! basename is what shows).

use super::path::paths_equal_ci;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The guest drive letter for pick-mounts.
///
/// `Z:` is never the C: bottle or the D: bridge, so a mounted path can never
/// collide with a volume path. The drive has no root (`GetDriveType` reports
/// `DRIVE_NO_ROOT_DIR`); only the exact mounted files exist under it.
pub(crate) const PICK_DRIVE: char = 'Z';

/// One registered pick: a guest path bound to its consented host file.
struct PickMount {
    guest_path: String,
    host_path: PathBuf,
}

/// The session-global pick-mount table.
///
/// Fail-closed on a poisoned lock: if a panicking thread held it, every later
/// resolve returns `None` (the mount stops being reachable) rather than
/// unwinding into the host.
static PICK_MOUNTS: Mutex<Vec<PickMount>> = Mutex::new(Vec::new());

/// Monotonic pick id — the per-pick unique component of the guest path.
static NEXT_PICK_ID: AtomicU64 = AtomicU64::new(1);

/// Resolve a mounted guest path to its host file (`None` when unregistered).
#[must_use]
pub(crate) fn resolve_pick_mount(guest_path: &str) -> Option<PathBuf> {
    let mounts = PICK_MOUNTS.lock().ok()?;
    mounts
        .iter()
        .find(|mount| paths_equal_ci(guest_path, &mount.guest_path))
        .map(|mount| mount.host_path.clone())
}

/// Register a pick-mount for `host_path`, returning its guest path.
///
/// Returns `None` only for a genuinely unmountable pick: a host path with no
/// file name (a directory or volume root cannot key a file mount). Picking an
/// already-mounted host file returns the existing guest path (dedup).
#[must_use]
pub(crate) fn register_pick_mount(host_path: &Path) -> Option<String> {
    let basename = sanitize_basename(host_path.file_name()?.to_str()?);
    let mut mounts = PICK_MOUNTS.lock().ok()?;
    if let Some(existing) = mounts.iter().find(|mount| mount.host_path == host_path) {
        return Some(existing.guest_path.clone());
    }
    let id = NEXT_PICK_ID.fetch_add(1, Ordering::Relaxed);
    let guest_path = format!("{PICK_DRIVE}:\\pick{id}\\{basename}");
    mounts.push(PickMount {
        guest_path: guest_path.clone(),
        host_path: host_path.to_path_buf(),
    });
    Some(guest_path)
}

/// Clear the table (unit tests only — the table is process-global).
#[cfg(test)]
pub(crate) fn clear_pick_mounts() {
    if let Ok(mut mounts) = PICK_MOUNTS.lock() {
        mounts.clear();
    }
}

/// Serializes the mount-dependent unit tests.
///
/// The pick-mount table is process-global and cargo runs test functions on
/// parallel threads: a test that clears the table would wipe the mounts a
/// concurrent test just registered. Every test that clears/registers/resolves
/// holds this lock for its whole body.
#[cfg(test)]
pub(crate) static TEST_SERIAL: Mutex<()> = Mutex::new(());

/// A Windows-safe basename for the guest path's final component.
///
/// The guest path is an opaque key (the guest receives it verbatim from the
/// dialog buffer and reuses it), so the basename only needs to be printable
/// and short — it survives a notepad title bar (`GetFileTitle` returns it).
fn sanitize_basename(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    // Windows forbids a trailing dot/space in a path component.
    while sanitized.ends_with(['.', ' ']) {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        sanitized.push_str("file");
    }
    // All surviving characters are ASCII, so the byte truncate is char-safe.
    sanitized.truncate(64);
    sanitized
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn temp_host_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wie-pick-mount-{}-{}", std::process::id(), name));
        let _unused = std::fs::remove_file(&dir);
        dir
    }

    #[test]
    fn registered_pick_resolves_case_insensitively() {
        let _serial = TEST_SERIAL.lock().expect("pick-mount test lock poisoned");
        clear_pick_mounts();
        let host = temp_host_file("resolve.txt");
        let guest = register_pick_mount(&host).expect("mountable pick");
        assert_eq!(
            resolve_pick_mount(&guest).as_deref(),
            Some(host.as_path()),
            "the guest path resolves back to the consented host file"
        );
        // Windows paths are case-insensitive: any spelling resolves.
        let lower: String = guest
            .chars()
            .map(|character| character.to_ascii_lowercase())
            .collect();
        assert_eq!(
            resolve_pick_mount(&lower).as_deref(),
            Some(host.as_path()),
            "case variants resolve identically"
        );
    }

    #[test]
    fn same_host_file_picked_twice_returns_the_same_guest_path() {
        let _serial = TEST_SERIAL.lock().expect("pick-mount test lock poisoned");
        clear_pick_mounts();
        let host = temp_host_file("dedup.txt");
        let first = register_pick_mount(&host).expect("first pick mounts");
        let second = register_pick_mount(&host).expect("second pick mounts");
        assert_eq!(
            first, second,
            "re-picking the same host file must dedup to its existing guest path"
        );
    }

    #[test]
    fn different_host_files_never_collide_even_with_the_same_basename() {
        let _serial = TEST_SERIAL.lock().expect("pick-mount test lock poisoned");
        clear_pick_mounts();
        let desktop = temp_host_file("same.txt");
        let downloads = std::env::temp_dir().join(format!(
            "wie-pick-mount-other-{}-same.txt",
            std::process::id()
        ));
        let first = register_pick_mount(&desktop).expect("desktop pick mounts");
        let second = register_pick_mount(&downloads).expect("downloads pick mounts");
        assert_ne!(
            first, second,
            "two picks of different host files must get distinct guest paths \
             (the same basename must never resolve to the wrong file)"
        );
        assert_eq!(
            resolve_pick_mount(&first).as_deref(),
            Some(desktop.as_path()),
            "the first guest path stays bound to the first file"
        );
        assert_eq!(
            resolve_pick_mount(&second).as_deref(),
            Some(downloads.as_path()),
            "the second guest path stays bound to the second file"
        );
    }

    #[test]
    fn unregistered_guest_path_under_the_mount_drive_resolves_to_nothing() {
        let _serial = TEST_SERIAL.lock().expect("pick-mount test lock poisoned");
        clear_pick_mounts();
        assert_eq!(
            resolve_pick_mount(r"Z:\pick1\not-registered.txt"),
            None,
            "the guest cannot reach a file it never picked — no host access"
        );
        assert_eq!(
            resolve_pick_mount(r"Z:\whatever\deep\path.bin"),
            None,
            "unregistered paths under Z: resolve to nothing"
        );
    }

    #[test]
    fn pick_without_a_file_name_cannot_be_mounted() {
        let _serial = TEST_SERIAL.lock().expect("pick-mount test lock poisoned");
        clear_pick_mounts();
        assert_eq!(
            register_pick_mount(Path::new("/")),
            None,
            "a volume root has no file name — no mount key"
        );
    }

    #[test]
    fn sanitize_basename_keeps_it_printable_and_short() {
        let _serial = TEST_SERIAL.lock().expect("pick-mount test lock poisoned");
        clear_pick_mounts();
        let host = std::env::temp_dir().join(format!(
            "wie-pick-mount-{}-weird<>:name?.txt",
            std::process::id()
        ));
        let guest = register_pick_mount(&host).expect("weird basename mounts");
        // Only the basename (after the drive's `Z:\pick{N}\`) is checked —
        // the drive prefix legitimately contains a colon.
        let basename = guest
            .rsplit('\\')
            .next()
            .expect("guest path has a basename");
        assert!(
            !basename.contains(['<', '>', ':', '?', '"', '|']),
            "the guest path must be Windows-printable: {guest}"
        );
        assert!(
            guest.chars().count() <= 80,
            "the guest path stays short: {guest}"
        );
    }
}
