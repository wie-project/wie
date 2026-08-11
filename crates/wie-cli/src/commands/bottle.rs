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

/// Create a named bottle (fails if it already exists).
fn create(bottles_dir: &Path, name: &str) -> Result<()> {
    let root = bottle_root(bottles_dir, name);
    if root.exists() {
        bail!("bottle '{name}' already exists at {}", root.display());
    }
    fs::create_dir_all(drive_c_dir(&root))
        .with_context(|| format!("failed to create bottle '{name}'"))?;
    Ok(())
}

/// Names of all existing bottles, sorted.
fn list(bottles_dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(bottles_dir)
        .with_context(|| format!("failed to read bottles directory {}", bottles_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && drive_c_dir(&path).is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Fresh temp dir unique per test call (parallel tests must not collide).
    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wie-bottle-test-{n}"));
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
        assert_eq!(
            dir,
            wie_winapi::global_bottle_root()
                .parent()
                .unwrap()
                .join("bottles")
        );
    }
}
