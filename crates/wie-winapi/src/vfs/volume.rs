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

use super::path::{drive_letter, normalize_windows_path_separators};
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
    Some(HostMap { host, drive })
}

fn relative_after_drive(normalized: &str, drive: char) -> Option<&str> {
    let lower = normalized.to_ascii_lowercase();
    let prefix = format!("{}:\\", drive.to_ascii_lowercase());
    if lower.starts_with(&prefix) {
        return normalized.get(3..);
    }
    let bare = format!("{}:", drive.to_ascii_lowercase());
    if lower == bare {
        return Some("");
    }
    None
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

    let mut components: Vec<&str> = Vec::new();
    for component in relative.split('\\') {
        match component {
            "" | "." => {}
            ".." => {
                // `..` with nothing left to pop would ascend above the
                // volume root — escaping the bottle. `?` propagates that
                // rejection as `None`.
                components.pop()?;
            }
            _ => {
                if component.contains('\\') || component.contains('/') {
                    return None;
                }
                components.push(component);
            }
        }
    }

    let mut out = String::with_capacity(relative.len() + 3);
    out.push(drive);
    out.push(':');
    out.push('\\');
    for component in components {
        out.push_str(component);
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
            return Some(guest_from_relative('C', relative));
        }
    } else {
        // Without a bottle no host path can be a guest C: path; record the
        // missing-bottle condition for the enforcement latch.
        note_bottle_missing(volumes);
    }
    if let Some(drive_d) = volumes.drive_d_root.as_ref() {
        let drive_d = normalize_host_path(drive_d);
        if let Ok(relative) = host_path.strip_prefix(&drive_d) {
            return Some(guest_from_relative('D', relative));
        }
    }
    None
}

/// Normalize a host path for volume mapping: canonicalize (resolves
/// symlinks such as /tmp → /private/tmp), then strip the macOS firmlink
/// prefix (`/System/Volumes/Data`) that AppKit path delivery can carry —
/// realpath leaves firmlinks untouched, so the two forms of the same file
/// (`/System/Volumes/Data/Users/…` vs `/Users/…`) only compare equal after
/// the prefix is removed. Nonexistent paths fall back to the raw form.
fn normalize_host_path(path: &Path) -> PathBuf {
    const FIRMLINK_PREFIX: &str = "/System/Volumes/Data";
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.strip_prefix(FIRMLINK_PREFIX)
        .map(|rest| PathBuf::from("/").join(rest))
        .unwrap_or(path)
}

/// Build `{drive}:\<rel>` with backslash separators, collapsing the empty
/// relative path to the drive root (`C:\`).
fn guest_from_relative(drive: char, relative: &Path) -> String {
    let mut out = String::new();
    out.push(drive);
    out.push_str(":\\");
    for component in relative.components() {
        if let std::path::Component::Normal(part) = component {
            out.push_str(&part.to_string_lossy());
            out.push('\\');
        }
    }
    // Strip the trailing separator so `C:\dir` (not `C:\dir\`); the drive
    // root `C:\` (length 3) keeps it.
    if out.len() > 3 && out.ends_with('\\') {
        out.pop();
    }
    out
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
}
