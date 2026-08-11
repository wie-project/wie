//! `wie bottle` — manage named bottles (guest `C:\` roots).
//!
//! A bottle is a plain directory `{bottles_dir}/{name}/drive_c/` in the same
//! layout the VFS maps: guest `C:\App\x` → `{root}/drive_c/App/x`. The
//! default single bottle (`WIE/bottle`) is untouched — named bottles are
//! siblings under `WIE/bottles/`.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

/// The directory holding all named bottles: `~/Library/Application Support/WIE/bottles`.
fn bottles_dir() -> PathBuf {
    wie_winapi::global_bottle_root()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bottles")
}

/// Host root of a named bottle: `{bottles_dir}/{name}`.
fn bottle_root(bottles_dir: &Path, name: &str) -> PathBuf {
    bottles_dir.join(name)
}

/// `{bottle_root}/drive_c` — the guest `C:\` volume.
fn drive_c_dir(bottle_root: &Path) -> PathBuf {
    bottle_root.join("drive_c")
}

/// A bottle name is one non-empty path component: no separators, no `.`/`..`.
fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.contains(std::path::MAIN_SEPARATOR);
    if !valid {
        bail!("invalid bottle name '{name}': must be a single directory name");
    }
    Ok(())
}

/// Create a named bottle (fails if it already exists).
fn create(bottles_dir: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let root = bottle_root(bottles_dir, name);
    if root.exists() {
        bail!("bottle '{name}' already exists at {}", root.display());
    }
    fs::create_dir_all(drive_c_dir(&root))
        .with_context(|| format!("failed to create bottle '{name}' at {}", root.display()))?;
    Ok(())
}

/// Names of all existing bottles, sorted.
fn list(bottles_dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    match fs::read_dir(bottles_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() && drive_c_dir(&path).is_dir() {
                    names.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read bottles directory {}", bottles_dir.display())
            });
        }
    }
    names.sort();
    Ok(names)
}

/// Sum of all file sizes under `root`, recursively (bytes).
fn dir_size(root: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if let Ok(meta) = fs::metadata(&path) {
                total += meta.len();
            }
        }
    }
    total
}

/// Remove a bottle (fails if it does not exist).
fn delete(bottles_dir: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let root = bottle_root(bottles_dir, name);
    if !root.is_dir() {
        bail!("bottle '{name}' does not exist");
    }
    fs::remove_dir_all(&root).with_context(|| format!("failed to delete bottle '{name}'"))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Fresh temp dir unique per test call (parallel tests must not collide).
    /// PID-scoped + pre-emptive cleanup: a crashed earlier run leaves stale
    /// `wie-bottle-test-*` dirs that would otherwise break later runs.
    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wie-bottle-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn create_makes_drive_c_and_list_returns_it() {
        let dir = temp_dir();
        create(&dir, "games").unwrap();
        assert!(drive_c_dir(&bottle_root(&dir, "games")).is_dir());
        assert_eq!(list(&dir).unwrap(), vec!["games".to_owned()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_refuses_existing() {
        let dir = temp_dir();
        create(&dir, "dup").unwrap();
        assert!(create(&dir, "dup").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_rejects_escaping_names() {
        let dir = temp_dir();
        for name in ["../evil", "/tmp/evil", "a/b", "", "."] {
            assert!(
                create(&dir, name).is_err(),
                "name {name:?} must be rejected"
            );
        }
        assert_eq!(list(&dir).unwrap(), Vec::<String>::new());
        // Nothing may be created outside the bottles dir.
        assert!(!dir.join("..").join("evil").exists());
        assert!(!std::path::Path::new("/tmp/evil").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_missing_dir_returns_empty() {
        let dir = temp_dir();
        let missing = dir.join("does-not-exist");
        assert_eq!(list(&missing).unwrap(), Vec::<String>::new());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_ignores_non_bottle_dirs() {
        let dir = temp_dir();
        create(&dir, "real").unwrap();
        fs::create_dir_all(dir.join("junk")).unwrap(); // no drive_c inside
        assert_eq!(list(&dir).unwrap(), vec!["real".to_owned()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bottles_dir_sits_next_to_global_bottle() {
        let dir = bottles_dir();
        assert!(
            dir.ends_with(std::path::Path::new("WIE").join("bottles")),
            "bottles dir must be WIE/bottles, got {}",
            dir.display()
        );
    }

    #[test]
    fn info_reports_path_and_size() {
        let dir = temp_dir();
        create(&dir, "apps").unwrap();
        // A 10-byte file inside drive_c.
        let f = drive_c_dir(&bottle_root(&dir, "apps")).join("hello.txt");
        fs::write(&f, b"0123456789").unwrap();
        let root = bottle_root(&dir, "apps");
        assert_eq!(dir_size(&root), 10);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_removes_and_missing_fails() {
        let dir = temp_dir();
        create(&dir, "gone").unwrap();
        delete(&dir, "gone").unwrap();
        assert!(!bottle_root(&dir, "gone").exists());
        assert!(delete(&dir, "gone").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
