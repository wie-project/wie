//! Volume table: bottle C: + optional host-bridge D:.
//!
//! # Bottle enforcement ("filesystem ⇒ bottle")
//!
//! The user policy: a program that needs the filesystem must ALWAYS run in a
//! bottle (`--root` / `WIE_ROOT`). The volume-resolution layer is the single
//! funnel every filesystem operation passes through (CreateFileW, the file
//! dialogs' listings, GetFullPathName, …), so it is the one place that
//! latches a missing bottle:
//!
//! - The first resolution that needs the C: volume while `bottle_root` is
//!   `None` sets the [`BOTTLE_MISSING_ENFORCED`] latch and the entry point
//!   resolves to `None` (the same contract as an unmapped path, so callers
//!   that treat `None` as "not found" need no change).
//! - File-op handlers call [`enforce_bottle`] once per operation. The first
//!   call without a bottle returns [`BottleMissingError`], which propagates
//!   through the handler `Result` and stops the session via the existing
//!   runtime emulation-error path. Every later call fails fast with the
//!   identical error — the bottle is never re-derived per call; the latch is
//!   what makes "enforce only once" hold.
//!
//! The latch is process-global rather than a field on [`VolumeConfig`]:
//! `VolumeConfig` is built from struct literals across the crate (including
//! the file-dialog confinement tests), so a new field would break those
//! sites. One process runs one session (the CLI), so a process-global latch
//! is exactly once per run; [`enforce_bottle`] still checks `bottle_root`
//! first, so a later state that does have a bottle never inherits an earlier
//! missing-bottle latch.
//!
//! # The D:-only edge
//!
//! A D: bridge (`--drive-d`) is a second volume that never *needs* the C:
//! bottle: `D:\…` resolves through `drive_d_root` and does not set the
//! latch. But the handler-level policy is unconditional: without a C: bottle
//! *any* file operation stops on its first call, even a `D:\` one — the
//! bottle provides the C: skeleton (TEMP, System32, CWD) that file code
//! depends on, so "run with `--root`" is the answer even for D:-only data.
//! Without a D: bridge either, `D:\` paths stay unmapped (`None`), unchanged.

use super::path::{
    canonicalize_host_target, collapse_windows_components, drive_letter, guest_path_from_relative,
    normalize_host_path, normalize_windows_path_separators, relative_after_drive,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Synthetic Win10-ish skeleton under bottle `drive_c` (no PE/DLL payloads).
pub const BOTTLE_SKELETON_DIRS: &[&str] = &[
    "App",
    "Windows/System32",
    "Windows/SysWOW64",
    "Users/WIE/AppData/Local/Temp",
    "Temp",
    "ProgramData",
];

/// Guest TEMP path (env + GetTempPath).
pub const GUEST_TEMP_PATH: &str = r"C:\Users\WIE\AppData\Local\Temp";

/// Guest Windows directory.
pub const GUEST_WINDOWS_DIR: &str = r"C:\Windows";

/// Guest System32 directory.
pub const GUEST_SYSTEM_DIR: &str = r"C:\Windows\System32";

/// One-shot record that a resolution needed the C: bottle while none was
/// configured. Set by the first missing-bottle resolution or the first
/// [`enforce_bottle`] call; every later enforcement fails fast on it.
///
/// Process-global, not a [`VolumeConfig`] field: `VolumeConfig` is built
/// from struct literals across the crate (including the file-dialog
/// confinement tests), so a new field would break those sites. One process
/// runs one session, so the latch is exactly once per run.
static BOTTLE_MISSING_ENFORCED: AtomicBool = AtomicBool::new(false);

/// The specific error for the "filesystem ⇒ bottle" policy.
///
/// The message is fixed and actionable: it names both ways to configure a
/// bottle (`--root` on the CLI, `WIE_ROOT` in the environment). Propagated
/// through handler `Result`s so the session stops with this text visible via
/// the existing runtime emulation-error path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BottleMissingError;

impl std::fmt::Display for BottleMissingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "file operations require a bottle: set --root or WIE_ROOT to map guest C:\\"
        )
    }
}

impl std::error::Error for BottleMissingError {}

/// Whether the missing-bottle condition was ever latched (tests + diagnostics).
#[must_use]
pub fn bottle_missing_enforced() -> bool {
    BOTTLE_MISSING_ENFORCED.load(Ordering::Relaxed)
}

/// Enforce the "filesystem ⇒ bottle" policy at the handler boundary.
///
/// File-op handlers call this once per operation. With a bottle configured
/// it is a no-op; without one the first call latches and returns
/// [`BottleMissingError`], and every later call returns the identical error
/// (there is nothing left to derive — the error is fixed, so the latch *is*
/// the short-circuit).
pub fn enforce_bottle(volumes: &VolumeConfig) -> Result<(), BottleMissingError> {
    if volumes.bottle_root.is_some() {
        return Ok(());
    }
    BOTTLE_MISSING_ENFORCED.store(true, Ordering::Relaxed);
    Err(BottleMissingError)
}

/// Record that a resolution needed the C: bottle while none was configured.
///
/// Called by the resolution entry points on their C:-missing path so the
/// latch is set by the resolution funnel itself, not only by the handler
/// boundary ([`enforce_bottle`]).
fn note_bottle_missing(volumes: &VolumeConfig) {
    if volumes.bottle_root.is_none() {
        BOTTLE_MISSING_ENFORCED.store(true, Ordering::Relaxed);
    }
}

/// Volume / path mapping configuration on `WinApiState`.
#[derive(Debug, Clone, Default)]
pub struct VolumeConfig {
    /// Bottle root: `C:\…` → `{root}/drive_c/…`.
    pub bottle_root: Option<PathBuf>,
    /// Optional host root for `D:\…`.
    pub drive_d_root: Option<PathBuf>,
}

impl VolumeConfig {
    #[must_use]
    pub fn from_parts(bottle_root: Option<PathBuf>, drive_d_root: Option<PathBuf>) -> Self {
        Self {
            bottle_root,
            drive_d_root,
        }
    }

    /// Whether guest drive D: is mounted.
    #[must_use]
    pub fn has_drive_d(&self) -> bool {
        self.drive_d_root.is_some()
    }
}

/// Successful guest → host path map.
#[derive(Debug, Clone)]
pub struct HostMap {
    pub host: PathBuf,
    pub drive: char,
}

/// Map guest Windows path to host path under C bottle and/or D bridge.
///
/// Rejects raw `..` components (fail-closed, no bottle escape) and unmapped drives.
/// Case of components preserved.
///
/// # Symlink-escape prevention
///
/// A bottle may contain symlinks planted on the host side (a drop, a user
/// `drive_c`). A direct file op through one pointing outside the bottle would
/// otherwise follow it on the host — `CreateFileW(r"C:\link\file")` where
/// `link` → a host dir outside the root. So the resolved host target is
/// canonicalized (its deepest existing ancestor for create-cases, where the
/// final component may not exist yet) and re-verified to stay within the
/// allowed roots: the bottle `drive_c` and the optional D: bridge. An escaping
/// target resolves to `None` and the file operation fails — real Windows
/// semantics for a symlink/junction resolving outside the volume.
///
/// The canonicalize is a few syscalls per mapping. That is acceptable because
/// file ops are syscall-heavy, and the one hot path — directory listing —
/// maps the directory once per listing (not per entry; the per-entry filter
/// is `host_path_to_guest`, the reverse direction). Hence the re-verify lives
/// at the mapping level, protecting every caller (CreateFile, stat probes,
/// FindFirstFile, dialog listings, DLL search) through one funnel.
///
/// The returned host path stays the joined (non-canonical) form: callers and
/// tests build on the raw `{root}/drive_c/…` layout, and the host resolves
/// symlinks itself on open — the re-verify only gates *which* mappings exist.
#[must_use]
pub fn guest_path_to_host(volumes: &VolumeConfig, guest_path: &str) -> Option<HostMap> {
    let trimmed = guest_path.trim().trim_matches('"');
    // Work on separator-normalized form *without* collapsing `..` so escape
    // probes like `C:\App\..\..\etc\passwd` are rejected (legacy bottle rule).
    let sep_norm = normalize_windows_path_separators(trimmed);
    let drive = drive_letter(&sep_norm)?;
    let relative = relative_after_drive(&sep_norm, drive)?;

    if relative.split('\\').any(|c| c == "..") {
        return None;
    }

    let host_root = match drive {
        'C' => {
            let Some(bottle) = volumes.bottle_root.as_ref() else {
                note_bottle_missing(volumes);
                return None;
            };
            bottle.join("drive_c")
        }
        'D' => volumes.drive_d_root.clone()?,
        _ => return None,
    };

    let mut host = host_root;
    for component in relative.split('\\').filter(|c| !c.is_empty() && *c != ".") {
        if component.contains('/') || component.contains('\\') {
            return None;
        }
        host.push(component);
    }

    if !host_resolves_within_roots(volumes, &host) {
        return None;
    }
    Some(HostMap { host, drive })
}

/// Whether the canonical host target stays within the allowed volume roots.
///
/// The allowed roots are the bottle `drive_c` and the optional D: bridge — the
/// two volumes the guest filesystem can legitimately reach. Both the target
/// and each root go through the same pipeline ([`canonicalize_host_target`]
/// then [`normalize_host_path`]) so symlinked roots (e.g. `/tmp` →
/// `/private/tmp`, or a `/System/Volumes/Data` root) compare consistently even
/// when the bottle does not exist yet.
fn host_resolves_within_roots(volumes: &VolumeConfig, host: &Path) -> bool {
    let canonical = normalize_host_path(&canonicalize_host_target(host));
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(bottle) = volumes.bottle_root.as_ref() {
        roots.push(bottle.join("drive_c"));
    }
    if let Some(drive_d) = volumes.drive_d_root.as_ref() {
        roots.push(drive_d.clone());
    }
    roots.iter().any(|root| {
        let root = normalize_host_path(&canonicalize_host_target(root));
        canonical.starts_with(&root)
    })
}

/// Legacy helper: C: only under bottle root (kept for bottle.rs compatibility).
#[must_use]
pub fn guest_path_to_host_bottle(bottle_root: &Path, guest_path: &str) -> Option<PathBuf> {
    let volumes = VolumeConfig {
        bottle_root: Some(bottle_root.to_path_buf()),
        drive_d_root: None,
    };
    guest_path_to_host(&volumes, guest_path).map(|m| m.host)
}

/// Confine a guest path to a mapped volume, collapsing `.`/`..` components.
///
/// Unlike [`guest_path_to_host`] (which rejects any raw `..` component),
/// within-volume ascent is allowed: `C:\App\..` resolves to `C:\`. A `..`
/// at the volume root (`C:\..`) is rejected, so a path can never leave the
/// bottle or the optional D: bridge. Unmapped drives and host paths return
/// `None`. The result is the canonical guest path (`C:\App\..\file.txt` →
/// `C:\file.txt`) — the file dialog's listing + accept confinement uses it.
#[must_use]
pub fn confine_guest_path(volumes: &VolumeConfig, guest_path: &str) -> Option<String> {
    let trimmed = guest_path.trim().trim_matches('"');
    let sep_norm = normalize_windows_path_separators(trimmed);
    let drive = drive_letter(&sep_norm)?;
    let relative = relative_after_drive(&sep_norm, drive)?;

    // The drive must name a configured volume: C: bottle or D: bridge.
    match drive {
        'C' => {
            if volumes.bottle_root.is_none() {
                note_bottle_missing(volumes);
                return None;
            }
        }
        'D' => {
            volumes.drive_d_root.as_ref()?;
        }
        _ => return None,
    }

    // Strict collapse: a `..` that would ascend above the volume root is an
    // escape and rejects the path (fail-closed) instead of clamping.
    let components = collapse_windows_components(relative, true)?;

    let mut out = String::with_capacity(relative.len() + 3);
    out.push(drive);
    out.push(':');
    out.push('\\');
    for component in components {
        out.push_str(&component);
        out.push('\\');
    }
    // The drive root `C:\` (length 3) keeps its trailing separator.
    if out.len() > 3 && out.ends_with('\\') {
        out.pop();
    }
    Some(out)
}

/// Map a host path to the guest-visible Windows path (`C:\…` / `D:\…`).
///
/// Inverse of [`guest_path_to_host`]: a host path under the bottle's
/// `drive_c` becomes `C:\<rel>`; a host path under the optional D: bridge
/// root becomes `D:\<rel>`. Returns `None` when the host path is outside both
/// volumes — no drive mapping exists, so the guest filesystem cannot see it
/// (used by the winit file-drop path, which must present the dropped file as
/// a guest path).
#[must_use]
pub fn host_path_to_guest(volumes: &VolumeConfig, host_path: &Path) -> Option<String> {
    // macOS firmlinks: AppKit can deliver a drop path through
    // /System/Volumes/Data/Users/… while the bottle root is /Users/….
    // realpath does NOT resolve firmlinks, so canonicalize alone cannot
    // align the two forms — strip the prefix explicitly (see
    // normalize_host_path).
    let host_path = normalize_host_path(host_path);
    if let Some(bottle) = volumes.bottle_root.as_ref() {
        let drive_c = normalize_host_path(&bottle.join("drive_c"));
        if let Ok(relative) = host_path.strip_prefix(&drive_c) {
            return Some(guest_path_from_relative('C', relative));
        }
    } else {
        // Without a bottle no host path can be a guest C: path; record the
        // missing-bottle condition for the enforcement latch.
        note_bottle_missing(volumes);
    }
    if let Some(drive_d) = volumes.drive_d_root.as_ref() {
        let drive_d = normalize_host_path(drive_d);
        if let Ok(relative) = host_path.strip_prefix(&drive_d) {
            return Some(guest_path_from_relative('D', relative));
        }
    }
    None
}

/// Resolve bottle root from `WIE_ROOT`.
#[must_use]
pub fn bottle_root_from_env() -> Option<PathBuf> {
    std::env::var_os("WIE_ROOT").map(PathBuf::from)
}

/// Resolve D: host root from `WIE_DRIVE_D`.
///
/// - unset / empty → None
/// - `auto` → current host working directory
/// - otherwise path
#[must_use]
pub fn drive_d_from_env() -> Option<PathBuf> {
    let val = std::env::var_os("WIE_DRIVE_D")?;
    if val.is_empty() {
        return None;
    }
    if val == "auto" {
        return std::env::current_dir().ok();
    }
    Some(PathBuf::from(val))
}

/// Create synthetic skeleton directories under the bottle (no files).
pub fn ensure_bottle_skeleton(bottle_root: &Path) -> std::io::Result<()> {
    let drive_c = bottle_root.join("drive_c");
    for rel in BOTTLE_SKELETON_DIRS {
        let path = drive_c.join(rel);
        std::fs::create_dir_all(&path)?;
    }
    Ok(())
}

/// GetDriveType values (Microsoft Learn).
pub const DRIVE_NO_ROOT_DIR: u32 = 1;
pub const DRIVE_FIXED: u32 = 3;

/// `GetDriveType` for a root like `C:\` or path.
#[must_use]
pub fn get_drive_type(volumes: &VolumeConfig, root_path: &str) -> u32 {
    let norm = normalize_windows_path_separators(root_path.trim());
    let letter = drive_letter(&norm).or_else(|| {
        // `C:` without slash
        let b = norm.as_bytes();
        if b.len() >= 2 && b.get(1) == Some(&b':') && b.first().is_some_and(u8::is_ascii_alphabetic)
        {
            b.first().map(|c| char::from(*c).to_ascii_uppercase())
        } else {
            None
        }
    });
    match letter {
        Some('C') => {
            // Bottle or pure virtual C: always FIXED for guest probes.
            DRIVE_FIXED
        }
        Some('D') => {
            if volumes.has_drive_d() {
                DRIVE_FIXED
            } else {
                DRIVE_NO_ROOT_DIR
            }
        }
        Some(_) | None => DRIVE_NO_ROOT_DIR,
    }
}

/// Bitmask for `GetLogicalDrives` (bit 0 = A:).
#[must_use]
pub fn logical_drives_mask(volumes: &VolumeConfig) -> u32 {
    let mut mask = 1u32 << 2; // C:
    if volumes.has_drive_d() {
        mask |= 1u32 << 3; // D:
    }
    mask
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn maps_c_and_rejects_escape() {
        let v = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        let m = guest_path_to_host(&v, r"C:\App\out.txt").expect("map");
        assert_eq!(m.host, PathBuf::from("/tmp/bottle/drive_c/App/out.txt"));
        assert!(guest_path_to_host(&v, r"C:\App\..\..\etc\passwd").is_none());
        assert!(guest_path_to_host(&v, r"D:\x").is_none());
    }

    #[test]
    fn maps_d_when_configured() {
        let v = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: Some(PathBuf::from("/Users/me/data")),
        };
        let m = guest_path_to_host(&v, r"D:\archive\a.7z").expect("d");
        assert_eq!(m.host, PathBuf::from("/Users/me/data/archive/a.7z"));
        assert_eq!(logical_drives_mask(&v), (1 << 2) | (1 << 3));
        assert_eq!(get_drive_type(&v, r"D:\"), DRIVE_FIXED);
    }

    #[test]
    fn host_path_to_guest_maps_bottle_and_drive_d() {
        let v = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: Some(PathBuf::from("/Users/me/data")),
        };
        // Bottle path → C:\.
        assert_eq!(
            host_path_to_guest(&v, Path::new("/tmp/bottle/drive_c/App/out.txt")),
            Some(r"C:\App\out.txt".to_owned())
        );
        // Drive-D bridge → D:\.
        assert_eq!(
            host_path_to_guest(&v, Path::new("/Users/me/data/archive/a.7z")),
            Some(r"D:\archive\a.7z".to_owned())
        );
        // The bottle root itself maps to the drive root.
        assert_eq!(
            host_path_to_guest(&v, Path::new("/tmp/bottle/drive_c")),
            Some(r"C:\".to_owned())
        );
        // Outside both volumes → no drive mapping exists.
        assert_eq!(host_path_to_guest(&v, Path::new("/etc/passwd")), None);
        // Drive-C path with no bottle configured → None.
        let no_bottle = VolumeConfig {
            bottle_root: None,
            drive_d_root: None,
        };
        assert_eq!(
            host_path_to_guest(&no_bottle, Path::new("/tmp/bottle/drive_c/x.txt")),
            None
        );
    }

    #[test]
    fn host_path_to_guest_resolves_symlink_prefixes() {
        // A real file under a real temp root: std::fs::canonicalize resolves
        // symlinks (on macOS /var → /private/var, /tmp → /private/tmp), so
        // the drop path AppKit delivers can carry a different prefix than
        // the bottle root — both sides must canonicalize to match (the
        // reported bug: drops under the bottle were skipped as "no guest
        // volume maps the host path").
        let root = std::env::temp_dir().join(format!("wie-firmlink-test-{}", std::process::id()));
        let drive_c = root.join("drive_c");
        std::fs::create_dir_all(&drive_c).expect("create test drive_c");
        let file = drive_c.join("sample.txt");
        std::fs::write(&file, b"x").expect("write test file");

        let volumes = VolumeConfig {
            bottle_root: Some(root.clone()),
            drive_d_root: None,
        };
        let canonical = std::fs::canonicalize(&file).expect("canonicalize test file");
        // The canonicalized form is what winit/AppKit hands the drop handler.
        assert_eq!(
            host_path_to_guest(&volumes, &canonical),
            Some(r"C:\sample.txt".to_owned())
        );
        // The raw form still works too.
        assert_eq!(
            host_path_to_guest(&volumes, &file),
            Some(r"C:\sample.txt".to_owned())
        );

        let _unused = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_path_to_guest_strips_macos_firmlink_prefix() {
        // AppKit can deliver drop paths through the /System/Volumes/Data
        // firmlink prefix while the bottle root is the plain /Users/… form.
        // realpath does NOT resolve firmlinks, so the prefix must be
        // stripped explicitly for the two forms to compare equal.
        let v = VolumeConfig {
            bottle_root: Some(PathBuf::from("/Users/me/wie-bottle")),
            drive_d_root: None,
        };
        assert_eq!(
            host_path_to_guest(
                &v,
                Path::new("/System/Volumes/Data/Users/me/wie-bottle/drive_c/sample.txt")
            ),
            Some(r"C:\sample.txt".to_owned())
        );
        // The plain form still maps.
        assert_eq!(
            host_path_to_guest(&v, Path::new("/Users/me/wie-bottle/drive_c/sample.txt")),
            Some(r"C:\sample.txt".to_owned())
        );
    }

    #[test]
    fn confine_guest_path_collapses_dotdot_within_volume() {
        let v = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        // Within-volume `..` ascent is allowed and collapses to the root.
        assert_eq!(
            confine_guest_path(&v, r"C:\App\.."),
            Some(r"C:\".to_owned())
        );
        assert_eq!(
            confine_guest_path(&v, r"C:\App\..\readme.txt"),
            Some(r"C:\readme.txt".to_owned())
        );
        assert_eq!(
            confine_guest_path(&v, r"C:\Windows\System32\..\win.ini"),
            Some(r"C:\Windows\win.ini".to_owned())
        );
        // `..` at or above the volume root escapes the bottle → rejected.
        assert_eq!(confine_guest_path(&v, r"C:\.."), None);
        assert_eq!(confine_guest_path(&v, r"C:\App\..\..\etc\passwd"), None);
        // Drive roots and bare drive letters confine to the volume root.
        assert_eq!(confine_guest_path(&v, r"C:\"), Some(r"C:\".to_owned()));
        assert_eq!(confine_guest_path(&v, r"C:"), Some(r"C:\".to_owned()));
        // Unmapped drives and host paths are not guest-visible.
        assert_eq!(confine_guest_path(&v, r"D:\x"), None);
        assert_eq!(confine_guest_path(&v, r"E:\x"), None);
        assert_eq!(confine_guest_path(&v, "/Users/me/x.txt"), None);
        assert_eq!(confine_guest_path(&v, r"\\server\share\x"), None);
    }

    #[test]
    fn confine_guest_path_keeps_drive_d_bridge_as_second_root() {
        let v = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: Some(PathBuf::from("/Users/me/data")),
        };
        assert_eq!(
            confine_guest_path(&v, r"D:\archive\a.7z"),
            Some(r"D:\archive\a.7z".to_owned())
        );
        assert_eq!(
            confine_guest_path(&v, r"D:\archive\.."),
            Some(r"D:\".to_owned())
        );
        // A D: path cannot ascend above the bridge root either.
        assert_eq!(confine_guest_path(&v, r"D:\archive\..\.."), None);
        assert_eq!(confine_guest_path(&v, r"D:\.."), None);
        // With no bridge configured, D: is not guest-visible.
        let no_bridge = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        assert_eq!(confine_guest_path(&no_bridge, r"D:\archive\a.7z"), None);
    }

    #[test]
    fn confine_guest_path_requires_a_volume() {
        let no_bottle = VolumeConfig::default();
        assert_eq!(confine_guest_path(&no_bottle, r"C:\App"), None);
        assert_eq!(confine_guest_path(&no_bottle, r"C:\"), None);
    }

    #[test]
    fn enforce_bottle_returns_error_and_latches_on_first_op() {
        let no_bottle = VolumeConfig::default();
        // First FS op without a bottle: the specific error, latch set.
        assert_eq!(enforce_bottle(&no_bottle), Err(BottleMissingError));
        assert!(bottle_missing_enforced());
        // Second op: identical error via the latch (no re-derivation).
        assert_eq!(enforce_bottle(&no_bottle), Err(BottleMissingError));
        // A different bottle-less config (D: bridge only) fails identically.
        let d_only = VolumeConfig {
            bottle_root: None,
            drive_d_root: Some(PathBuf::from("/Users/me/data")),
        };
        assert_eq!(enforce_bottle(&d_only), Err(BottleMissingError));
    }

    #[test]
    fn enforce_bottle_is_noop_with_bottle() {
        // A configured bottle wins even if an earlier bottle-less config
        // latched (the global latch must never leak into a bottle run).
        let with_bottle = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: None,
        };
        assert_eq!(enforce_bottle(&with_bottle), Ok(()));
    }

    #[test]
    fn bottle_missing_error_message_has_root_guidance() {
        let msg = BottleMissingError.to_string();
        assert!(msg.contains("--root"), "message: {msg}");
        assert!(msg.contains("WIE_ROOT"), "message: {msg}");
        assert!(msg.contains("bottle"), "message: {msg}");
    }

    #[test]
    fn c_resolutions_without_bottle_latch() {
        let no_bottle = VolumeConfig::default();
        assert!(guest_path_to_host(&no_bottle, r"C:\App\out.txt").is_none());
        assert!(confine_guest_path(&no_bottle, r"C:\App").is_none());
        assert!(host_path_to_guest(&no_bottle, Path::new("/tmp/bottle/drive_c/x.txt")).is_none());
        assert!(bottle_missing_enforced());
    }

    #[test]
    fn d_bridge_without_bottle_resolves_but_file_ops_still_fire() {
        let d_only = VolumeConfig {
            bottle_root: None,
            drive_d_root: Some(PathBuf::from("/Users/me/data")),
        };
        // D: resolution never needs the C: bottle — it maps through the bridge.
        let m = guest_path_to_host(&d_only, r"D:\archive\a.7z").expect("d bridge maps");
        assert_eq!(m.host, PathBuf::from("/Users/me/data/archive/a.7z"));
        // But the handler-level policy is unconditional: any file operation
        // without a C: bottle stops, even a D: one.
        assert_eq!(enforce_bottle(&d_only), Err(BottleMissingError));
    }

    /// A real bottle on disk with a small `drive_c` layout for mapping tests.
    fn temp_bottle(tag: &str) -> (PathBuf, VolumeConfig) {
        let root = std::env::temp_dir().join(format!("wie-{tag}-{}", std::process::id()));
        // A crashed earlier run (same pid reused) may have left this dir; a
        // stale symlink/`drive_c` would make the fixture setup fail.
        let _unused = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("drive_c").join("App")).expect("create drive_c/App");
        let volumes = VolumeConfig {
            bottle_root: Some(root.clone()),
            drive_d_root: None,
        };
        (root, volumes)
    }

    #[cfg(unix)]
    #[test]
    fn guest_path_to_host_rejects_symlink_escape() {
        // A bottle symlink pointing at a host dir outside the root: the
        // mapping must fail (None) so a direct file op cannot follow it on
        // the host — real Windows semantics for a junction resolving outside
        // the volume.
        let (root, volumes) = temp_bottle("escape");
        let outside = root.join("outside-secret");
        std::fs::create_dir_all(&outside).expect("create outside dir");
        std::os::unix::fs::symlink(&outside, root.join("drive_c").join("App").join("leak"))
            .expect("create escape symlink");

        // An existing file through the symlink → the resolved target escapes.
        assert!(guest_path_to_host(&volumes, r"C:\App\leak\secret.txt").is_none());
        // The symlink itself resolves outside → the op on it fails too.
        assert!(guest_path_to_host(&volumes, r"C:\App\leak").is_none());
        // A normal in-bottle path still maps.
        let m = guest_path_to_host(&volumes, r"C:\App\ok.txt").expect("in-bottle maps");
        assert_eq!(m.host, root.join("drive_c").join("App").join("ok.txt"));

        let _unused = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn guest_path_to_host_maps_in_bottle_symlinks_to_in_bottle_targets() {
        // A symlink that stays inside the bottle is not an escape: the
        // resolved target keeps the mapping (the raw path is returned; the
        // host resolves the link on open, landing in-bottle).
        let (root, volumes) = temp_bottle("symlink-ok");
        std::os::unix::fs::symlink(
            root.join("drive_c").join("App"),
            root.join("drive_c").join("alias"),
        )
        .expect("create in-bottle symlink");
        let m = guest_path_to_host(&volumes, r"C:\alias\file.txt").expect("in-bottle link maps");
        assert_eq!(m.host, root.join("drive_c").join("alias").join("file.txt"));

        let _unused = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn guest_path_to_host_maps_create_case_nonexistent_target() {
        // Create-case: neither the target nor its parent exists yet. The
        // deepest existing ancestor canonicalizes in-bottle and the final
        // components append verbatim, so the mapping succeeds.
        let (root, volumes) = temp_bottle("create-case");
        let m =
            guest_path_to_host(&volumes, r"C:\App\new\deep\file.txt").expect("create-case maps");
        assert_eq!(
            m.host,
            root.join("drive_c")
                .join("App")
                .join("new")
                .join("deep")
                .join("file.txt")
        );

        // A brand-new bottle (nothing under it exists) maps its drive root.
        let fresh = std::env::temp_dir().join(format!("wie-fresh-{}", std::process::id()));
        let _unused = std::fs::remove_dir_all(&fresh);
        let fresh_volumes = VolumeConfig {
            bottle_root: Some(fresh.clone()),
            drive_d_root: None,
        };
        let m = guest_path_to_host(&fresh_volumes, r"C:\x.txt").expect("fresh bottle maps");
        assert_eq!(m.host, fresh.join("drive_c").join("x.txt"));

        let _unused = std::fs::remove_dir_all(&root);
        let _unused = std::fs::remove_dir_all(&fresh);
    }

    #[test]
    fn property_round_trip_guest_host_guest_preserves_path() {
        // guest → host → guest must recover the original guest path for every
        // sampled in-bottle path, whether or not the mapped file exists on the
        // host (create-cases go through the same pipeline).
        let (root, volumes) = temp_bottle("roundtrip");
        for guest in [
            r"C:\App\out.txt",
            r"C:\App\sub\deep\file.bin",
            r"C:\App\new\not-there.txt", // create-case: does not exist on host
            r"C:\root.txt",
            r"C:\",
        ] {
            let Some(map) = guest_path_to_host(&volumes, guest) else {
                panic!("in-bottle path must map: {guest}");
            };
            let back = host_path_to_guest(&volumes, &map.host).unwrap_or_else(|| {
                panic!("host must map back for {guest}: {}", map.host.display())
            });
            assert_eq!(back, guest, "round-trip must preserve the path");
        }
        let _unused = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn property_confinement_holds_across_case_and_separator_variants() {
        // The confinement is invariant under guest-side case and separator
        // variants: every spelling of an in-bottle path maps to the same
        // host location, and none of them escapes the root.
        let (root, volumes) = temp_bottle("confinement");
        for guest in [
            r"C:\App\out.txt",
            r"c:\app\out.txt",
            r"C:/App/out.txt",
            r"C:\App\.\out.txt",
            r"C:\App\.\.\out.txt",
        ] {
            let Some(map) = guest_path_to_host(&volumes, guest) else {
                panic!("variant must map: {guest}");
            };
            // Component case is preserved verbatim (documented), so compare
            // the normalized target — same underlying in-bottle file.
            let canonical = normalize_host_path(&map.host)
                .to_string_lossy()
                .to_ascii_lowercase();
            let expected = normalize_host_path(&root.join("drive_c").join("App").join("out.txt"))
                .to_string_lossy()
                .to_ascii_lowercase();
            assert_eq!(
                canonical, expected,
                "variant must land on the same host target: {guest}"
            );
        }
        // And no escape probe maps (raw `..` is rejected before mapping).
        for escape in [
            r"C:\..\..\etc\passwd",
            r"C:\App\..\..\..\etc\passwd",
            r"C:\App\sub\..\..\..\etc\passwd",
            r"D:\anything",
        ] {
            assert!(
                guest_path_to_host(&volumes, escape).is_none(),
                "escape must not map: {escape}"
            );
        }
        let _unused = std::fs::remove_dir_all(&root);
    }
}
