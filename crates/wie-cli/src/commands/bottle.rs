//! `wie bottle` — manage named bottles (guest `C:\` roots).
//!
//! A bottle is a plain directory `{bottles_dir}/{name}/drive_c/` in the same
//! layout the VFS maps: guest `C:\App\x` → `{root}/drive_c/App/x`. The
//! default single bottle (`WIE/bottle`) is untouched — named bottles are
//! siblings under `WIE/bottles/`.

use crate::BottleCommand;
use crate::commands::util::write_line;
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

/// Resolve a bottle's host root, validating the name and requiring the
/// bottle to exist. Shared by every subcommand that operates on an
/// existing bottle — an unvalidated name like `..` would otherwise escape
/// the bottles directory (e.g. `add ..` copying into the global bottle).
fn require_bottle(bottles_dir: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let root = bottle_root(bottles_dir, name);
    if !root.is_dir() {
        bail!("bottle '{name}' does not exist");
    }
    Ok(root)
}

/// Resolve a named bottle's host root for `run --bottle <name>`: validates
/// the name and requires the bottle to exist (the same contract as the
/// existing-bottle subcommands). The root then feeds the run entries exactly
/// like an explicit `--root`.
pub(crate) fn resolve_bottle_root(name: &str) -> Result<PathBuf> {
    require_bottle(&bottles_dir(), name)
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
    // Seed the default Windows folder skeleton so path-returning APIs
    // (GetWindowsDirectory, GetSystemDirectory, GetTempPath, SHGetFolderPath)
    // point at directories that exist. Idempotent: create_dir_all no-ops on
    // re-runs, matching the session-start seeding policy.
    wie_winapi::seed_default_skeleton(&root)
        .with_context(|| format!("failed to seed bottle '{name}' at {}", root.display()))?;
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
///
/// Symlinks are NOT followed (a link to an ancestor would loop forever);
/// only regular files count toward the total. Read errors and missing
/// entries are skipped silently, so a partial/unreadable tree reports a
/// partial total — 0 is not proof the bottle is empty.
fn dir_size(root: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                total += dir_size(&path);
            } else if meta.is_file() {
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
    fs::remove_dir_all(&root)
        .with_context(|| format!("failed to delete bottle '{name}' at {}", root.display()))?;
    Ok(())
}

/// Recursively copy `src` to `dst` (file or directory tree).
///
/// Symlinks are NOT followed: a link to an ancestor would loop forever, and
/// a link to outside content would copy files that are not part of the
/// source tree. Symlinks in the source are skipped (mirrors `dir_size`'s
/// un-followed contract); only regular files are copied.
fn copy_into(src: &Path, dst: &Path) -> Result<()> {
    let meta =
        fs::symlink_metadata(src).with_context(|| format!("failed to stat {}", src.display()))?;
    if meta.is_dir() {
        fs::create_dir_all(dst).with_context(|| format!("failed to create {}", dst.display()))?;
        for entry in
            fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))?
        {
            let entry = entry?;
            copy_into(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else if meta.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(src, dst)
            .with_context(|| format!("failed to copy {} → {}", src.display(), dst.display()))?;
        Ok(())
    } else {
        // Symlink (or other non-regular entry): skipped, never followed.
        Ok(())
    }
}

/// Resolve a guest path (e.g. `C:\Apps\Foo`) to its host path inside the
/// bottle's `drive_c`. Rejects paths that do not stay under `C:\`.
fn resolve_guest_path(bottle_root: &Path, guest_path: &str) -> Result<PathBuf> {
    wie_winapi::bottle::guest_path_to_host(bottle_root, guest_path)
        .filter(|host| host.starts_with(drive_c_dir(bottle_root)))
        .with_context(|| format!("guest path '{guest_path}' must live under C:\\"))
}

/// Host path of a guest exe inside the bottle, e.g. `C:\App\app.exe`.
fn resolve_exe(bottle_root: &Path, guest_exe: &str) -> Result<PathBuf> {
    resolve_guest_path(bottle_root, guest_exe)
}

/// Number of candidates to list in an ambiguity error.
const AMBIGUITY_LIST_LIMIT: usize = 8;

/// Resolve the run-source argument for a selected named bottle.
///
/// The argument is treated as, in order:
/// 1. an existing host path — passed through unchanged;
/// 2. an explicit guest path (`C:\…`, `D:\…` or a `\\…` UNC form) — mapped
///    into the bottle's `drive_c`, requiring the mapped file to exist (the
///    `bottle run` contract);
/// 3. a search: the bottle's `drive_c` is walked recursively for a unique
///    file whose basename equals the argument (no separators) or whose
///    `drive_c`-relative path equals it (`/` or `\` separators present).
///    Matching is case-insensitive (Windows path semantics); the search
///    never leaves the selected bottle.
///
/// No match is an error. Several matches are an ambiguity error listing the
/// candidate guest paths and suggesting a full `C:\…` path.
pub(crate) fn resolve_bottle_exe(bottle_root: &Path, arg: &Path) -> Result<PathBuf> {
    if arg.exists() {
        return Ok(arg.to_path_buf());
    }
    let arg_str = arg.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "run source is not valid UTF-8 (cannot name a guest path): {}",
            arg.display()
        )
    })?;
    if is_explicit_guest_path(arg_str) {
        let host = resolve_exe(bottle_root, arg_str)?;
        if !host.is_file() {
            bail!("guest exe not found in bottle: '{arg_str}'");
        }
        return Ok(host);
    }
    let drive_c = drive_c_dir(bottle_root);
    let (is_basename, target) = normalize_search_target(arg_str);
    let mut matches = Vec::new();
    collect_drive_c_matches(&drive_c, &drive_c, is_basename, &target, &mut matches);

    let mut iter = matches.into_iter();
    let Some(first) = iter.next() else {
        bail!(
            "no executable '{arg_str}' found in bottle '{}': searched {} (recursive); \
             pass a full guest path (e.g. C:\\{arg_str}) or a host path",
            bottle_name(bottle_root),
            drive_c.display()
        );
    };
    let Some(second) = iter.next() else {
        return Ok(first);
    };
    let mut candidates = vec![first, second];
    candidates.extend(iter);
    let mut listing: Vec<String> = candidates
        .iter()
        .take(AMBIGUITY_LIST_LIMIT)
        .map(|host| format!("    {}", guest_label(bottle_root, host)))
        .collect();
    if candidates.len() > AMBIGUITY_LIST_LIMIT {
        listing.push(format!(
            "    … and {} more",
            candidates.len() - AMBIGUITY_LIST_LIMIT
        ));
    }
    let example = candidates
        .first()
        .map(|host| guest_label(bottle_root, host))
        .unwrap_or_else(|| format!("C:\\{arg_str}"));
    bail!(
        "ambiguous executable '{arg_str}' in bottle '{}': {} matches:\n{}\n\
         pass a full guest path to disambiguate (e.g. {example})",
        bottle_name(bottle_root),
        candidates.len(),
        listing.join("\n")
    );
}

/// Whether the argument is an explicit Windows guest path: a drive-letter
/// form (`C:\…`, `C:/…`, bare `C:`) or a UNC `\\…` form. These resolve
/// through the bottle mapping, never through a drive_c search.
fn is_explicit_guest_path(arg: &str) -> bool {
    let trimmed = arg.trim().trim_matches('"');
    wie_winapi::vfs::path::drive_letter(trimmed).is_some() || trimmed.starts_with(r"\\")
}

/// Split a search argument into a normalized drive_c-relative target.
///
/// Returns `(is_basename, target)`: `is_basename` when the argument is a
/// single path component (the match is by file name, anywhere in `drive_c`);
/// otherwise `target` is the `/`-joined, lowercased relative path a file's
/// `drive_c`-relative location must equal exactly. Empty and `.` components
/// are dropped; `..` components never match a real relative path and
/// therefore resolve to no match.
fn normalize_search_target(arg: &str) -> (bool, String) {
    let components: Vec<&str> = arg
        .split(['/', '\\'])
        .filter(|component| !component.is_empty() && *component != ".")
        .collect();
    let is_basename = components.len() == 1;
    let target = components.join("/").to_lowercase();
    (is_basename, target)
}

/// Recursively collect regular files under `dir` matching the search target
/// (see [`resolve_bottle_exe`]).
///
/// Symlinks are never followed (mirrors `copy_into` / `dir_size`): a link
/// could point outside the bottle or loop on an ancestor, so only real files
/// and directories are walked.
fn collect_drive_c_matches(
    dir: &Path,
    drive_c: &Path,
    is_basename: bool,
    target: &str,
    out: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_drive_c_matches(&path, drive_c, is_basename, target, out);
        } else if meta.is_file() {
            let hit = if is_basename {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().to_lowercase() == target)
            } else {
                path.strip_prefix(drive_c).is_ok_and(|rel| {
                    rel.to_string_lossy().replace('\\', "/").to_lowercase() == target
                })
            };
            if hit {
                out.push(path);
            }
        }
    }
}

/// The guest path (`C:\…`) of an in-bottle host file, for candidate listings
/// and disambiguation suggestions. Falls back to the host path when the file
/// does not map (should not happen for search results, which come from the
/// bottle's own `drive_c`).
fn guest_label(bottle_root: &Path, host: &Path) -> String {
    let volumes = wie_winapi::VolumeConfig {
        bottle_root: Some(bottle_root.to_path_buf()),
        drive_d_root: None,
    };
    wie_winapi::host_path_to_guest(&volumes, host).unwrap_or_else(|| host.display().to_string())
}

/// The bottle's name (its directory basename) for error messages.
fn bottle_name(bottle_root: &Path) -> String {
    bottle_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".to_owned())
}

/// Entry point for `wie bottle …` (called from `main`).
pub(crate) fn bottle(command: BottleCommand) -> Result<()> {
    match command {
        BottleCommand::Create { name } => create(&bottles_dir(), &name)?,
        BottleCommand::List => {
            let mut out = std::io::stdout().lock();
            for name in list(&bottles_dir())? {
                if !write_line(&mut out, &name)? {
                    return Ok(());
                }
            }
        }
        BottleCommand::Info { name } => {
            let root = require_bottle(&bottles_dir(), &name)?;
            let mut out = std::io::stdout().lock();
            write_line(
                &mut out,
                &format!(
                    "name: {name}\nroot: {}\ndrive_c: {}\nsize: {} bytes",
                    root.display(),
                    drive_c_dir(&root).display(),
                    dir_size(&root)
                ),
            )?;
        }
        BottleCommand::Path { name } => {
            let root = require_bottle(&bottles_dir(), &name)?;
            let mut out = std::io::stdout().lock();
            write_line(&mut out, &root.display().to_string())?;
        }
        BottleCommand::Delete { name, yes } => {
            validate_name(&name)?;
            if !yes {
                eprintln!("delete bottle '{name}' and ALL its contents? [y/N]");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                let answer = line.trim();
                if !(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")) {
                    eprintln!("aborted");
                    return Ok(());
                }
            }
            delete(&bottles_dir(), &name)?;
        }
        BottleCommand::Add {
            name,
            host_path,
            target,
        } => {
            let root = require_bottle(&bottles_dir(), &name)?;
            // The explicit source argument must not be a symlink: interior
            // symlinks are skipped by copy_into, but a symlinked *source*
            // would silently copy nothing.
            if fs::symlink_metadata(&host_path)
                .with_context(|| format!("failed to stat {}", host_path.display()))?
                .is_symlink()
            {
                bail!("source is a symlink: {}", host_path.display());
            }
            let guest_target = target.unwrap_or_else(|| {
                let base = host_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "root".to_owned());
                format!(r"C:\{base}")
            });
            let dst = resolve_guest_path(&root, &guest_target)?;
            copy_into(&host_path, &dst)?;
            eprintln!("copied {} → {}", host_path.display(), dst.display());
        }
        BottleCommand::Run {
            name,
            exe,
            max_api,
            expect_code,
            drive_d,
            stdin,
            app_dir,
            persistent,
            console,
            gui,
            screenshot,
            input_script,
            guest_args,
        } => {
            let root = require_bottle(&bottles_dir(), &name)?;
            // The exe argument may be a host path, an explicit guest path,
            // or a basename / relative path searched inside the bottle's
            // drive_c (unique match required — see resolve_bottle_exe).
            let host_exe = resolve_bottle_exe(&root, Path::new(&exe))?;
            // Dispatch through the same entry used by `run --bottle <name>`:
            // `root` is the effective bottle root (no `--root` flag), so the
            // micro-only rejection sees no raw flag while the bottle-derived
            // root is allowed on console/persistent.
            crate::run_entry(
                &host_exe,
                Some(root),
                None,
                max_api,
                expect_code,
                drive_d,
                stdin,
                app_dir,
                persistent,
                console,
                gui,
                screenshot,
                input_script,
                guest_args,
            )?;
        }
    }
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
    fn create_seeds_default_windows_skeleton() {
        let dir = temp_dir();
        create(&dir, "seeded").unwrap();
        let drive_c = drive_c_dir(&bottle_root(&dir, "seeded"));
        for rel in [
            "Windows",
            "Windows/System32",
            "Windows/SysWOW64",
            "Program Files",
            "Program Files (x86)",
            "ProgramData",
            "Users",
            "Temp",
        ] {
            let path = drive_c.join(rel);
            assert!(
                path.is_dir(),
                "new bottle must contain {} (missing: {})",
                rel,
                path.display()
            );
        }
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
    fn require_bottle_rejects_invalid_names() {
        let dir = temp_dir();
        create(&dir, "ok").unwrap();
        assert!(require_bottle(&dir, "..").is_err());
        assert!(require_bottle(&dir, "../evil").is_err());
        assert!(require_bottle(&dir, "").is_err());
        // Valid name + existing bottle resolves; missing bottle errors.
        assert!(require_bottle(&dir, "ok").is_ok());
        assert!(require_bottle(&dir, "missing").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_bottle_root_resolves_existing_bottle() {
        // The helper resolves via the real (global) bottles dir, so the
        // bottle must exist there; a PID-unique name keeps parallel runs
        // disjoint and a panicking assert leaves only a uniquely named bottle.
        let name = format!("resolve-run-test-{}", std::process::id());
        create(&bottles_dir(), &name).unwrap();
        let root = resolve_bottle_root(&name).unwrap();
        assert_eq!(root, bottle_root(&bottles_dir(), &name));
        assert!(drive_c_dir(&root).is_dir());
        delete(&bottles_dir(), &name).unwrap();
    }

    #[test]
    fn resolve_bottle_root_rejects_missing_and_invalid_names() {
        assert!(resolve_bottle_root("no-such-bottle").is_err());
        assert!(resolve_bottle_root("../evil").is_err());
        assert!(resolve_bottle_root("").is_err());
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
        // A 10-byte file inside drive_c, and a 20-byte file in a subdir.
        let drive_c = drive_c_dir(&bottle_root(&dir, "apps"));
        fs::write(drive_c.join("hello.txt"), b"0123456789").unwrap();
        fs::create_dir_all(drive_c.join("sub")).unwrap();
        fs::write(
            drive_c.join("sub").join("nested.bin"),
            b"01234567890123456789",
        )
        .unwrap();
        let root = bottle_root(&dir, "apps");
        assert_eq!(dir_size(&root), 30);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dir_size_skips_symlinks() {
        let dir = temp_dir();
        create(&dir, "links").unwrap();
        let root = bottle_root(&dir, "links");
        let real = root.join("real.txt");
        fs::write(&real, b"12345").unwrap();
        // A symlink to the file and a loop back to the bottle root itself:
        // neither may be followed (the loop would previously stack-overflow).
        std::os::unix::fs::symlink(&real, root.join("file-link")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("root-loop")).unwrap();
        assert_eq!(dir_size(&root), 5);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_removes_and_missing_fails() {
        let dir = temp_dir();
        create(&dir, "gone").unwrap();
        delete(&dir, "gone").unwrap();
        assert!(!bottle_root(&dir, "gone").exists());
        assert!(delete(&dir, "gone").is_err());
        // The bottles dir itself must survive a bottle delete.
        assert!(
            dir.is_dir(),
            "bottles dir itself must survive a bottle delete"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_guest_path_maps_under_drive_c() {
        let dir = temp_dir();
        create(&dir, "b").unwrap();
        let root = bottle_root(&dir, "b");
        let host = resolve_guest_path(&root, r"C:\Apps\Foo\a.txt").unwrap();
        assert_eq!(host, drive_c_dir(&root).join("Apps/Foo/a.txt"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_guest_path_rejects_escape() {
        let dir = temp_dir();
        create(&dir, "b").unwrap();
        let root = bottle_root(&dir, "b");
        assert!(resolve_guest_path(&root, r"C:\..\escape").is_err());
        assert!(resolve_guest_path(&root, r"D:\other").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_into_copies_file_and_dir() {
        let dir = temp_dir();
        let src_file = dir.join("a.txt");
        fs::write(&src_file, b"hi").unwrap();
        let src_dir = dir.join("tree");
        fs::create_dir_all(src_dir.join("sub")).unwrap();
        fs::write(src_dir.join("sub/b.txt"), b"yo").unwrap();

        let dst = dir.join("out");
        copy_into(&src_file, &dst.join("a.txt")).unwrap();
        copy_into(&src_dir, &dst.join("tree")).unwrap();
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hi");
        assert_eq!(fs::read(dst.join("tree/sub/b.txt")).unwrap(), b"yo");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_into_skips_symlinks() {
        let dir = temp_dir();
        let src_dir = dir.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("real.txt"), b"x").unwrap();
        // Symlink to an ancestor — following it would loop forever.
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dir, src_dir.join("loop")).unwrap();

        let dst = dir.join("dst");
        copy_into(&src_dir, &dst).unwrap();
        assert_eq!(fs::read(dst.join("real.txt")).unwrap(), b"x");
        #[cfg(unix)]
        assert!(
            !dst.join("loop").exists(),
            "symlinks must be skipped, not followed"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_exe_maps_guest_exe_to_host() {
        let dir = temp_dir();
        create(&dir, "b").unwrap();
        let root = bottle_root(&dir, "b");
        assert_eq!(
            resolve_exe(&root, r"C:\App\app.exe").unwrap(),
            drive_c_dir(&root).join("App/app.exe")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_defaults_target_to_c_basename() {
        let dir = temp_dir();
        let file = dir.join("tool.exe");
        fs::write(&file, b"xyz").unwrap();

        // The dispatcher resolves bottles via the real bottles dir, so the
        // bottle must exist there. Use a PID-unique name and clean up after;
        // on a panicking assert the uniquely named bottle may remain.
        let name = format!("test-{}", std::process::id());
        create(&bottles_dir(), &name).unwrap();

        // Exercise the real Add dispatcher arm with no --target: the default
        // `C:\<basename>` must land in drive_c and copy the file.
        bottle(crate::BottleCommand::Add {
            name: name.clone(),
            host_path: file,
            target: None,
        })
        .unwrap();
        let copied = drive_c_dir(&bottle_root(&bottles_dir(), &name)).join("tool.exe");
        assert_eq!(fs::read(&copied).unwrap(), b"xyz");

        delete(&bottles_dir(), &name).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A temp bottles dir with a `b` bottle whose `drive_c` holds a small
    /// exe tree; returns `(dir, root)`.
    fn exe_tree_bottle() -> (PathBuf, PathBuf) {
        let dir = temp_dir();
        create(&dir, "b").unwrap();
        let root = bottle_root(&dir, "b");
        let drive_c = drive_c_dir(&root);
        fs::create_dir_all(drive_c.join("DoomRetro")).unwrap();
        fs::write(drive_c.join("DoomRetro/doomretro.exe"), b"MZ").unwrap();
        fs::create_dir_all(drive_c.join("Tools")).unwrap();
        fs::write(drive_c.join("Tools/other.exe"), b"MZ").unwrap();
        (dir, root)
    }

    /// A bare basename with exactly one drive_c match resolves to that file,
    /// case-insensitively (Windows path semantics).
    #[test]
    fn resolve_bottle_exe_finds_unique_basename_match() {
        let (dir, root) = exe_tree_bottle();
        let expected = drive_c_dir(&root).join("DoomRetro/doomretro.exe");
        assert_eq!(
            resolve_bottle_exe(&root, Path::new("doomretro.exe")).unwrap(),
            expected
        );
        // Case-insensitive spelling resolves to the same file.
        assert_eq!(
            resolve_bottle_exe(&root, Path::new("DOOMRETRO.EXE")).unwrap(),
            expected
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A relative path (either separator) resolves to the exact
    /// drive_c-relative location.
    #[test]
    fn resolve_bottle_exe_matches_relative_path() {
        let (dir, root) = exe_tree_bottle();
        let expected = drive_c_dir(&root).join("DoomRetro/doomretro.exe");
        assert_eq!(
            resolve_bottle_exe(&root, Path::new("DoomRetro/doomretro.exe")).unwrap(),
            expected
        );
        assert_eq!(
            resolve_bottle_exe(&root, Path::new(r"DoomRetro\doomretro.exe")).unwrap(),
            expected
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// No drive_c match is a clear error naming the searched exe and bottle.
    #[test]
    fn resolve_bottle_exe_missing_match_errors() {
        let (dir, root) = exe_tree_bottle();
        let err =
            resolve_bottle_exe(&root, Path::new("ghost.exe")).expect_err("missing exe must fail");
        assert!(
            err.to_string().contains("ghost.exe"),
            "error names the missing exe: {err}"
        );
        assert!(
            err.to_string().contains("no executable"),
            "error is clearly a no-match error: {err}"
        );
        assert!(
            err.to_string().contains('b'),
            "error names the bottle: {err}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Several drive_c matches for the same basename are an ambiguity error
    /// listing the candidate guest paths and suggesting a full path.
    #[test]
    fn resolve_bottle_exe_ambiguous_match_errors() {
        let (dir, root) = exe_tree_bottle();
        let drive_c = drive_c_dir(&root);
        fs::create_dir_all(drive_c.join("Other")).unwrap();
        fs::write(drive_c.join("Other/doomretro.exe"), b"MZ").unwrap();

        let err = resolve_bottle_exe(&root, Path::new("doomretro.exe"))
            .expect_err("ambiguous exe must fail");
        let message = err.to_string();
        assert!(
            message.contains("ambiguous executable"),
            "error is clearly an ambiguity error: {message}"
        );
        assert!(
            message.contains(r"C:\DoomRetro\doomretro.exe"),
            "error lists the first candidate guest path: {message}"
        );
        assert!(
            message.contains(r"C:\Other\doomretro.exe"),
            "error lists the second candidate guest path: {message}"
        );
        assert!(
            message.contains("pass a full guest path"),
            "error suggests a full guest path: {message}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// An explicit guest path passes through the mapping unchanged (the
    /// `bottle run` contract): an existing exe resolves, a missing one fails.
    #[test]
    fn resolve_bottle_exe_explicit_guest_path_passthrough() {
        let (dir, root) = exe_tree_bottle();
        assert_eq!(
            resolve_bottle_exe(&root, Path::new(r"C:\DoomRetro\doomretro.exe")).unwrap(),
            drive_c_dir(&root).join("DoomRetro/doomretro.exe")
        );
        let err = resolve_bottle_exe(&root, Path::new(r"C:\DoomRetro\ghost.exe"))
            .expect_err("missing guest exe must fail");
        assert!(
            err.to_string().contains("guest exe not found"),
            "error is the explicit-path missing-file error: {err}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// An existing host path passes through unchanged — the bottle is never
    /// searched for it.
    #[test]
    fn resolve_bottle_exe_existing_host_path_passthrough() {
        let (dir, root) = exe_tree_bottle();
        let host_exe = dir.join("host.exe");
        fs::write(&host_exe, b"MZ").unwrap();
        assert_eq!(
            resolve_bottle_exe(&root, &host_exe).unwrap(),
            host_exe,
            "an existing host path is its own run source"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
