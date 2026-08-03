//! Volume table: bottle C: + optional host-bridge D:.

use super::path::{drive_letter, normalize_windows_path_separators};
use std::path::{Path, PathBuf};

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
            let bottle = volumes.bottle_root.as_ref()?;
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
}
