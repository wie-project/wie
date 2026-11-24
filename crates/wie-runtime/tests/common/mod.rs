//! Shared guest-PE fixture resolution for the `wie-runtime` integration
//! tests.
//!
//! # A missing guest PE is a failure, not a skip
//!
//! These tests exist to run *real* guest binaries. They used to resolve the
//! fixture with `path.is_file().then_some(path)` and `return` early when it was
//! absent, so the hole was invisible: a clean checkout (`micro-exes/out/`
//! empty, no mingw cross toolchain) reported a green suite while the
//! frame-hash guards for present/paint/D3D9/GL and every behaviour assertion
//! silently never ran. Those binaries are not tracked in git
//! (`micro-exes/.gitignore` ignores `out/`), so "not built" is the *default*
//! state of a fresh clone — the exact state in which the regression tests must
//! not be allowed to pass.
//!
//! The rules implemented here:
//!
//! * **`micro-exes/out/<name>` (a micro fixture) is required.** A missing one
//!   fails the test with the expected path and the build command. There is one
//!   documented escape hatch — `WIE_ALLOW_MISSING_GUESTS=1` — for a developer
//!   without the mingw-w64 cross toolchain; it restores the old skip, with a
//!   notice on stderr, so the skip is still visible.
//! * **`real_exes/<name>` (a real application) is also required.** Those
//!   binaries are gitignored and fetched on demand
//!   (`./scripts/fetch.sh notepad`, `./scripts/fetch.sh 7za`), but an absent
//!   one is still a **failure**, for the same reason: a test that quietly
//!   returns `()` is recorded as a *pass* by libtest and nextest, and nextest
//!   only prints a failing test's captured stderr — so a skip-notice is
//!   invisible in exactly the case it exists to report. `real_exe` used to do
//!   precisely that: with `real_exes/notepad.exe` absent, 39 GUI tests
//!   "passed" in 0.02–0.08 s having executed nothing. CI now fetches notepad
//!   (`.github/workflows/ci.yml`) so the loud failure has its prerequisite.
//!
//! `WIE_ALLOW_MISSING_GUESTS` intentionally does *not* apply to `real_exe`:
//! honouring it there would reintroduce exactly the silent-pass hole this
//! function exists to close.

#![allow(dead_code)] // each test binary links this module and uses part of it

use std::path::PathBuf;

/// Escape hatch: with this set, a missing `micro-exes/out` fixture reports a
/// notice and returns `None` (the test skips) instead of failing.
pub(crate) const ALLOW_MISSING_GUESTS: &str = "WIE_ALLOW_MISSING_GUESTS";

/// Repository root, derived from this crate's manifest directory
/// (`<repo>/crates/wie-runtime`).
fn repo_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // repo root
    path
}

/// Absolute path a micro fixture would live at, whether or not it is built.
pub(crate) fn micro_exe_path(name: &str) -> PathBuf {
    repo_root().join("micro-exes/out").join(name)
}

/// Absolute path a real guest binary would live at, whether or not it is
/// fetched.
pub(crate) fn real_exe_path(name: &str) -> PathBuf {
    repo_root().join("real_exes").join(name)
}

/// Resolve a mingw-built micro PE under `micro-exes/out`.
///
/// `Some(path)` when the binary exists. When it does not: fail the test with an
/// actionable message, unless [`ALLOW_MISSING_GUESTS`] is set, in which case
/// print a notice and return `None` so the caller skips.
pub(crate) fn micro_exe(name: &str) -> Option<PathBuf> {
    let path = micro_exe_path(name);
    if path.is_file() {
        return Some(path);
    }
    if std::env::var_os(ALLOW_MISSING_GUESTS).is_some() {
        eprintln!(
            "SKIPPED: missing guest fixture {} (allowed by {ALLOW_MISSING_GUESTS})",
            path.display()
        );
        return None;
    }
    panic!(
        "missing guest fixture: {}\n\
         \n\
         This test runs a real guest PE, so an absent fixture is a failure, not a\n\
         skip — otherwise the regression guard it carries would never run.\n\
         Build the fixtures with:\n\
         \n    make -C micro-exes\n\
         \n\
         (requires the mingw-w64 cross toolchain: x86_64-w64-mingw32-gcc).\n\
         To run the suite without that toolchain and skip these tests, set\n\
         {ALLOW_MISSING_GUESTS}=1.",
        path.display()
    );
}

/// Resolve a real (non-micro) guest binary under `real_exes/` — e.g. the
/// RNotepad build produced by `./scripts/fetch.sh notepad`.
///
/// Required, exactly like [`micro_exe`]: an absent binary fails the test with
/// the expected path and the fetch command. It is never a skip, and
/// [`ALLOW_MISSING_GUESTS`] does not apply — a silent pass for a test that ran
/// nothing is the failure mode this guards against, and a green suite that
/// hides it is worse than a red one.
///
/// The fetch target is the file stem (`notepad.exe` → `notepad`), which is how
/// `scripts/fetch.sh` names its apps.
pub(crate) fn real_exe(name: &str) -> PathBuf {
    let path = real_exe_path(name);
    if path.is_file() {
        return path;
    }
    let target = name.strip_suffix(".exe").unwrap_or(name);
    panic!(
        "missing real guest fixture: {}\n\
         \n\
         This test runs a real guest PE, so an absent fixture is a failure, not a\n\
         skip — a silent return here is recorded as a PASS by nextest while the\n\
         test executes nothing.\n\
         Fetch the fixture with:\n\
         \n    ./scripts/fetch.sh {target}\n\
         \n\
         (see `./scripts/fetch.sh --list` for the available apps; the binaries are\n\
         gitignored, so this is required once per clone. CI fetches notepad in\n\
         .github/workflows/ci.yml).",
        path.display()
    );
}
