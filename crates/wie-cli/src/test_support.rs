//! Shared test scaffolding (compiled only under `#[cfg(test)]`).

use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

/// Unique temp dir under the system temp dir; removed on drop.
///
/// PID-scoped + pre-emptive cleanup: a crashed earlier run leaves stale
/// `wie-test-*` dirs that would otherwise break later runs. Parallel tests
/// must pass distinct tags.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// Create `<tmp>/wie-test-<tag>-<pid>`, clearing any stale dir first.
    #[allow(clippy::expect_used)] // test scaffolding
    pub(crate) fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("wie-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    /// The directory's host path.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl DerefMut for TempDir {
    fn deref_mut(&mut self) -> &mut Path {
        &mut self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
