//! Windows path normalize / resolve (clean room, Microsoft Learn path forms).
//!
//! This is the canonical guest-path module: every Windows↔host path conversion
//! in the VFS routes through here, so the parse/normalize/compare/convert
//! invariants live in one place and the hand-rolled duplicates elsewhere
//! (`guest_dir_of` in kernel32/module.rs, the firmlink strip and
//! drive-relative split in vfs/volume.rs) delegate to it.
//!
//! API map:
//! - parse: [`strip_extended_prefix`], [`drive_letter`], [`relative_after_drive`],
//!   [`guest_basename`], [`guest_parent`], [`guest_dir_of`], [`split_find_pattern`]
//! - normalize: [`normalize_windows_path_separators`],
//!   [`normalize_windows_path_components`], [`collapse_windows_components`],
//!   [`resolve_full_windows_path`]
//! - compare: [`paths_equal_ci`], [`wildcard_match`]
//! - convert (host side): [`normalize_host_path`], [`canonicalize_host_target`],
//!   [`guest_path_from_relative`]

use std::path::{Path, PathBuf};

/// Strip `\\?\` / `//?/` extended prefix when present.
#[must_use]
pub fn strip_extended_prefix(path: &str) -> &str {
    let p = path.trim();
    if let Some(rest) = p.strip_prefix(r"\\?\") {
        return rest;
    }
    if let Some(rest) = p.strip_prefix("//?/") {
        return rest;
    }
    // `\\?\UNC\server\share` left as-is for now (not bottle-mapped).
    if p.strip_prefix(r"\\?\UNC\").is_some() {
        return p;
    }
    p
}

#[must_use]
pub fn normalize_windows_path_separators(path: &str) -> String {
    normalize_windows_path_separators_cow(path).into_owned()
}

/// Cow-returning normaliser: borrows when the input contains no `/`, allocates
/// only when a real replacement is needed. Hot on VFS translation.
#[must_use]
pub fn normalize_windows_path_separators_cow(path: &str) -> std::borrow::Cow<'_, str> {
    if path.bytes().all(|b| b != b'/') {
        return std::borrow::Cow::Borrowed(path);
    }
    let mut out = String::with_capacity(path.len());
    for character in path.chars() {
        out.push(if character == '/' { '\\' } else { character });
    }
    std::borrow::Cow::Owned(out)
}

#[must_use]
pub fn is_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    let has_drive_prefix = bytes.get(1).is_some_and(|value| *value == b':')
        && bytes
            .get(2)
            .is_some_and(|value| *value == b'\\' || *value == b'/');
    let has_unc_prefix = bytes.first() == Some(&b'\\') && bytes.get(1) == Some(&b'\\');
    has_drive_prefix || has_unc_prefix
}

#[must_use]
pub fn join_windows_path(base: &str, relative: &str) -> String {
    let mut joined = base.trim_end_matches(['\\', '/']).to_owned();
    if !joined.is_empty() {
        joined.push('\\');
    }
    joined.push_str(relative);
    joined
}

/// Collapse `.` / `..` components; never walk above drive/UNC prefix.
#[must_use]
pub fn normalize_windows_path_components(path: &str) -> String {
    let normalized = normalize_windows_path_separators(path);
    let mut prefix = String::new();
    let mut remainder = normalized.as_str();
    let bytes = normalized.as_bytes();

    if bytes.get(1).is_some_and(|value| *value == b':') {
        if let Some(drive) = normalized.get(..2) {
            // Canonical drive letter: uppercase ASCII.
            for ch in drive.chars() {
                prefix.push(ch.to_ascii_uppercase());
            }
        }
        remainder = normalized.get(2..).unwrap_or_default();
        if remainder.starts_with('\\') {
            prefix.push('\\');
            remainder = remainder.trim_start_matches('\\');
        }
    } else if normalized.starts_with("\\\\") {
        prefix.push_str("\\\\");
        remainder = normalized.strip_prefix("\\\\").unwrap_or_default();
    }

    let components = collapse_windows_components(remainder, false).unwrap_or_default();

    let mut result = prefix;
    for component in components {
        if !result.is_empty() && !result.ends_with('\\') {
            result.push('\\');
        }
        result.push_str(&component);
    }
    result
}

/// Collapse `.` / `..` components of a separator-normalized remainder.
///
/// Shared core of the two `..` policies in the crate. Lenient mode (`strict`
/// false) clamps a `..` above the drive root, matching
/// [`normalize_windows_path_components`]. Strict mode (`strict` true) rejects
/// the path instead — the volume confinement must be fail-closed because the
/// drive root is the isolation boundary and must not be escapable.
pub(crate) fn collapse_windows_components(remainder: &str, strict: bool) -> Option<Vec<String>> {
    let mut components = Vec::<String>::new();
    for component in remainder.split('\\') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() && strict {
                    return None;
                }
            }
            _ => components.push(component.to_owned()),
        }
    }
    Some(components)
}

/// Resolve a Windows path against the process current directory.
///
/// - Absolute: `C:\…`, `\\server\share\…`, `\\?\C:\…`
/// - Drive-relative / relative: `file`, `.\file`, `subdir\file`, `..\file`
/// - Rooted on current drive: `\file` → `{drive}:\file`
/// - Drive-relative without slash: `D:foo` → `{D: cwd or D:\}` + `foo` (v1: `D:\foo`)
#[must_use]
pub fn resolve_full_windows_path(current_directory: &str, input_path: &str) -> String {
    let stripped = strip_extended_prefix(input_path.trim().trim_matches('"'));
    let normalized_input = normalize_windows_path_separators(stripped);
    let cwd = normalize_windows_path_separators(current_directory);

    let combined = if is_windows_absolute_path(&normalized_input) {
        normalized_input
    } else if looks_like_drive_relative(&normalized_input) {
        // `D:foo` — relative to root of that drive (v1 simplification).
        let drive = normalized_input.get(..2).unwrap_or("C:");
        let rest = normalized_input.get(2..).unwrap_or("");
        if rest.starts_with('\\') {
            format!("{drive}{rest}")
        } else if rest.is_empty() {
            format!("{drive}\\")
        } else {
            format!("{drive}\\{rest}")
        }
    } else if normalized_input.starts_with('\\') {
        let drive = cwd
            .get(..2)
            .filter(|d| d.as_bytes().get(1) == Some(&b':'))
            .unwrap_or("C:");
        format!("{drive}{normalized_input}")
    } else {
        join_windows_path(&cwd, &normalized_input)
    };

    normalize_windows_path_components(&combined)
}

fn looks_like_drive_relative(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 2
        && b.get(1) == Some(&b':')
        && b.first().is_some_and(u8::is_ascii_alphabetic)
        && b.get(2).is_none_or(|c| *c != b'\\')
}

/// Case-insensitive full path equality (ASCII fold).
///
/// Fast path: when both inputs are already normalised (no `.`/`..` segments,
/// no `//` runs, no forward slashes), skip the two normalise + to_lowercase
/// allocations and compare byte-wise with `eq_ignore_ascii_case`. The full
/// path stays correct because `normalize_windows_path_components` on an
/// already-normalised input returns the input verbatim.
#[must_use]
pub fn paths_equal_ci(a: &str, b: &str) -> bool {
    if is_already_normalised(a) && is_already_normalised(b) {
        return a.eq_ignore_ascii_case(b);
    }
    let norm_a = normalize_windows_path_components(a);
    let norm_b = normalize_windows_path_components(b);
    norm_a.eq_ignore_ascii_case(&norm_b)
}

#[inline]
fn is_already_normalised(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.contains(&b'/') {
        return false;
    }
    // Look for `\\` mid-path (leading `\\?\` extended prefix is handled elsewhere).
    // Also reject any `\.\`, `\..\`, or a trailing `\.`, `\..`.
    let mut i = 0;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'\\') {
            match bytes.get(i.saturating_add(1)) {
                Some(&b'\\') if i > 0 => return false, // mid-path `\\`
                Some(&b'.') => {
                    let after_dot = bytes.get(i.saturating_add(2));
                    match after_dot {
                        None | Some(&b'\\') => return false, // `\.` or `\.\`
                        Some(&b'.') => {
                            let after_dotdot = bytes.get(i.saturating_add(3));
                            if matches!(after_dotdot, None | Some(&b'\\')) {
                                return false; // `\..` or `\..\`
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        i = i.saturating_add(1);
    }
    true
}

/// Basename after last `\` or `/`.
#[must_use]
pub fn guest_basename(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// Parent directory of a Windows path (drive root stays drive root).
#[must_use]
pub fn guest_parent(path: &str) -> String {
    let norm = normalize_windows_path_components(path);
    if let Some(idx) = norm.rfind('\\') {
        let parent = norm.get(..=idx).unwrap_or(norm.as_str());
        if parent.len() <= 3 && parent.as_bytes().get(1) == Some(&b':') {
            // `C:\`
            return parent.trim_end_matches('\\').to_owned() + "\\";
        }
        return parent.trim_end_matches('\\').to_owned();
    }
    norm
}

/// Directory component of a guest Windows path, host-OS agnostic.
///
/// `Path::parent` is wrong here: on a Unix host backslashes are ordinary
/// characters, so `C:\App\main.exe` would read as a single component. Split on
/// both separators by hand; a bare drive letter (`C:`) maps to the drive root
/// `C:\`. Returns `None` for paths with no directory component (`main.exe`,
/// root-relative `\foo.exe`, empty). Unlike [`guest_parent`] the input is not
/// re-collapsed — module paths arrive already canonical.
#[must_use]
pub fn guest_dir_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        return None;
    }
    let idx = trimmed.rfind(['\\', '/'])?;
    if idx == 0 {
        return None; // root-relative like `\foo.exe`: no usable directory
    }
    let dir = trimmed.get(..idx)?.replace('/', "\\");
    if dir.ends_with(':') {
        Some(format!("{dir}\\"))
    } else {
        Some(dir)
    }
}

/// Split a find pattern into directory + file mask.
///
/// `C:\App\*.txt` → (`C:\App`, `*.txt`); `C:\App\` → (`C:\App`, `*`);
/// `file.txt` (no sep after resolve) handled by caller after full resolve.
#[must_use]
pub fn split_find_pattern(full_pattern: &str) -> (String, String) {
    let norm = normalize_windows_path_components(full_pattern);
    if let Some(idx) = norm.rfind('\\') {
        let dir = if idx <= 2 {
            // `C:\*`
            norm.get(..=idx).unwrap_or(norm.as_str()).to_owned()
        } else {
            norm.get(..idx).unwrap_or("").to_owned()
        };
        let mask_start = idx.saturating_add(1);
        let mask = norm.get(mask_start..).unwrap_or("").to_owned();
        let mask = if mask.is_empty() {
            "*".to_owned()
        } else {
            mask
        };
        (dir, mask)
    } else {
        (".".to_owned(), norm)
    }
}

/// Simple case-insensitive `*` / `?` wildcard match (Win32-ish, not full DOS 8.3).
#[must_use]
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    // Byte-wise ASCII fold; skips two `String` + two `Vec<char>` allocations
    // that the `.to_ascii_lowercase()` / `.chars().collect()` chain paid per call.
    match_glob_bytes(pattern.as_bytes(), name.as_bytes())
}

fn match_glob_bytes(pat: &[u8], text: &[u8]) -> bool {
    let mut pi = 0_usize;
    let mut ti = 0_usize;
    let mut star_pattern_pos: Option<usize> = None;
    let mut star_text_pos = 0_usize;
    while ti < text.len() {
        let pat_b = pat.get(pi).copied();
        let text_b = text.get(ti).copied();
        let matches = match (pat_b, text_b) {
            (Some(b'?'), Some(_)) => true,
            (Some(p), Some(t)) => p.eq_ignore_ascii_case(&t),
            _ => false,
        };
        if matches {
            pi = pi.saturating_add(1);
            ti = ti.saturating_add(1);
        } else if pat_b == Some(b'*') {
            star_pattern_pos = Some(pi);
            star_text_pos = ti;
            pi = pi.saturating_add(1);
        } else if let Some(star_pattern_pos) = star_pattern_pos {
            pi = star_pattern_pos.saturating_add(1);
            star_text_pos = star_text_pos.saturating_add(1);
            ti = star_text_pos;
        } else {
            return false;
        }
    }
    while pat.get(pi) == Some(&b'*') {
        pi = pi.saturating_add(1);
    }
    pi == pat.len()
}

/// Drive letter from `C:\…` form, uppercase, if any.
#[must_use]
pub fn drive_letter(path: &str) -> Option<char> {
    let norm = normalize_windows_path_separators(path);
    let b = norm.as_bytes();
    if b.len() >= 2 && b.get(1) == Some(&b':') && b.first().is_some_and(u8::is_ascii_alphabetic) {
        let letter = char::from(*b.first()?);
        Some(letter.to_ascii_uppercase())
    } else {
        None
    }
}

/// Path remainder after `{drive}:\` (or `""` for a bare `C:`), case-folded
/// against `drive`. `normalized` must already be separator-normalized.
pub(crate) fn relative_after_drive(normalized: &str, drive: char) -> Option<&str> {
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

/// Normalize a host path for volume mapping: canonicalize (resolves symlinks
/// such as /tmp → /private/tmp), then strip the macOS firmlink prefix
/// (`/System/Volumes/Data`) that AppKit path delivery can carry — realpath
/// leaves firmlinks untouched, so the two forms of the same file
/// (`/System/Volumes/Data/Users/…` vs `/Users/…`) only compare equal after
/// the prefix is removed. Nonexistent paths resolve on their deepest existing
/// ancestor (see [`canonicalize_host_target`]) instead of falling back to the
/// raw form, so a create-case host path and its root compare consistently.
#[must_use]
pub fn normalize_host_path(path: &Path) -> PathBuf {
    const FIRMLINK_PREFIX: &str = "/System/Volumes/Data";
    let path = canonicalize_host_target(path);
    path.strip_prefix(FIRMLINK_PREFIX)
        .map(|rest| PathBuf::from("/").join(rest))
        .unwrap_or(path)
}

/// Canonicalize a host path whose final components may not exist yet.
///
/// Resolves symlinks on the deepest existing ancestor and re-appends the
/// missing tail verbatim. This is what lets the volume mapping re-verify a
/// create target (`C:\new\file.txt` — neither `new` nor `file.txt` need to
/// exist): the symlink resolution that could point outside the bottle happens
/// on the ancestor that actually exists. A path with no existing ancestor
/// (nothing under the root exists yet) keeps its raw form.
#[must_use]
pub fn canonicalize_host_target(path: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = path;
    loop {
        match std::fs::canonicalize(probe) {
            Ok(canonical) => {
                let mut out = canonical;
                for component in tail.iter().rev() {
                    out.push(component);
                }
                return out;
            }
            Err(_) => {
                let Some(parent) = probe.parent() else {
                    // Filesystem root: nothing above it to resolve.
                    return path.to_path_buf();
                };
                // A relative probe walked off the root (empty parent):
                // nothing more to canonicalize, keep the raw form.
                if parent.as_os_str().is_empty() {
                    return path.to_path_buf();
                }
                if let Some(name) = probe.file_name() {
                    tail.push(name.to_os_string());
                }
                probe = parent;
            }
        }
    }
}

/// Build `{drive}:\<rel>` with backslash separators, collapsing the empty
/// relative path to the drive root (`C:\`).
pub(crate) fn guest_path_from_relative(drive: char, relative: &Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_dotdot() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r".\config.ini"),
            r"C:\App\config.ini"
        );
        assert_eq!(
            resolve_full_windows_path(r"C:\App\data", r"..\config.ini"),
            r"C:\App\config.ini"
        );
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r"\Windows\win.ini"),
            r"C:\Windows\win.ini"
        );
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r"D:\other\file.txt"),
            r"D:\other\file.txt"
        );
    }

    #[test]
    fn extended_prefix() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r"\\?\C:\Temp\x"),
            r"C:\Temp\x"
        );
    }

    #[test]
    fn drive_relative_v1() {
        assert_eq!(resolve_full_windows_path(r"C:\App", r"D:foo"), r"D:\foo");
    }

    #[test]
    fn wildcard_basic() {
        assert!(wildcard_match("*.txt", "a.txt"));
        assert!(wildcard_match("*.*", "a.txt"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("*.txt", "a.bin"));
        assert!(wildcard_match("file?.dat", "file1.dat"));
    }

    #[test]
    fn split_pattern() {
        let (d, m) = split_find_pattern(r"C:\App\*.7z");
        assert_eq!(d, r"C:\App");
        assert_eq!(m, "*.7z");
    }

    #[test]
    fn guest_dir_of_splits_windows_paths_host_agnostically() {
        assert_eq!(guest_dir_of(r"C:\App\main.exe"), Some(r"C:\App".to_owned()));
        assert_eq!(
            guest_dir_of(r"C:\Program Files\MyApp\main.exe"),
            Some(r"C:\Program Files\MyApp".to_owned())
        );
        // A bare drive letter maps to the drive root.
        assert_eq!(guest_dir_of(r"C:\main.exe"), Some(r"C:\".to_owned()));
        assert_eq!(guest_dir_of("main.exe"), None);
        assert_eq!(guest_dir_of(""), None);
        // Forward slashes are normalized to backslashes.
        assert_eq!(guest_dir_of(r"C:/App/main.exe"), Some(r"C:\App".to_owned()));
    }

    #[test]
    fn relative_after_drive_splits_any_case() {
        assert_eq!(
            relative_after_drive(r"C:\App\x.txt", 'C'),
            Some(r"App\x.txt")
        );
        assert_eq!(
            relative_after_drive(r"c:\App\x.txt", 'C'),
            Some(r"App\x.txt")
        );
        assert_eq!(relative_after_drive(r"D:\", 'D'), Some(""));
        assert_eq!(relative_after_drive("C:", 'C'), Some(""));
        assert_eq!(relative_after_drive(r"C:\x", 'D'), None);
    }

    #[test]
    fn collapse_components_strict_vs_lenient() {
        // Lenient clamps `..` at the root (normalize_windows_path_components).
        assert_eq!(
            collapse_windows_components(r"App\..\..\etc", false),
            Some(vec!["etc".to_owned()])
        );
        // Strict rejects the same path (volume confinement).
        assert_eq!(collapse_windows_components(r"App\..\..\etc", true), None);
        assert_eq!(
            collapse_windows_components(r"App\..\file.txt", true),
            Some(vec!["file.txt".to_owned()])
        );
    }

    // --- Property tests -----------------------------------------------------
    //
    // Deterministic LCG (no external rand dep); fixed seed so failures
    // reproduce. Invariants hold for every sampled input, not just one case.

    fn seed_rng(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state >> 33
        }
    }

    fn sample_component(rng: &mut impl FnMut() -> u64) -> String {
        const NAMES: &[&str] = &[
            "App", "data", "Temp", "file.txt", "My App", "a_b", "7za.exe",
        ];
        let word =
            NAMES[usize::try_from(rng() % u64::try_from(NAMES.len()).unwrap_or(1)).unwrap_or(0)];
        let mut out = String::new();
        for ch in word.chars() {
            // Randomly flip ASCII case to exercise the case-fold paths.
            if rng().is_multiple_of(2) {
                out.push(ch.to_ascii_uppercase());
            } else {
                out.push(ch.to_ascii_lowercase());
            }
        }
        out
    }

    fn sample_guest_path(rng: &mut impl FnMut() -> u64) -> String {
        let drive = if rng().is_multiple_of(2) { 'C' } else { 'D' };
        let depth = usize::try_from(rng() % 4).unwrap_or(0);
        let mut path = format!("{drive}:\\");
        for _ in 0..depth {
            path.push_str(&sample_component(rng));
            path.push('\\');
        }
        path.push_str(&sample_component(rng));
        path
    }

    #[test]
    fn property_normalize_is_idempotent_and_case_folds() {
        let mut rng = seed_rng(0x5EED_2026);
        for _ in 0..500 {
            let path = sample_guest_path(&mut rng);
            // Idempotence: normalizing a normalized path changes nothing.
            let once = normalize_windows_path_components(&path);
            let twice = normalize_windows_path_components(&once);
            assert_eq!(once, twice, "normalize must be idempotent for {path}");
            // The drive letter is folded to uppercase, and the result is
            // comparable case-insensitively with the raw input.
            assert!(paths_equal_ci(&once, &path), "compare folded vs raw {path}");
            // Drive letter is always uppercase ASCII.
            let first = once.as_bytes().first().copied();
            assert!(
                matches!(first, Some(b'C') | Some(b'D')),
                "drive letter must be an uppercase C or D, got {first:?} for {path}"
            );
        }
    }

    #[test]
    fn property_case_fold_equality_is_symmetric_and_reflexive() {
        let mut rng = seed_rng(0xC0FFEE);
        for _ in 0..500 {
            let path = sample_guest_path(&mut rng);
            let sep_form = normalize_windows_path_separators(&path.replace('\\', "/"));
            // `/`-separated form compares equal to the `\`-separated form.
            assert!(paths_equal_ci(&sep_form, &path), "separator fold {path}");
            // Reflexivity and symmetry.
            assert!(paths_equal_ci(&path, &path));
            assert!(paths_equal_ci(&path, &sep_form) == paths_equal_ci(&sep_form, &path));
        }
    }

    #[test]
    fn property_separator_and_dotdot_normalization() {
        let mut rng = seed_rng(0x5EED_C0DE);
        for _ in 0..500 {
            let path = sample_guest_path(&mut rng);
            // A `.` segment and a `..` pair cancel out under lenient normalize.
            let with_dots = format!(r"{}\x\.\..", path.trim_end_matches('\\'));
            let collapsed = normalize_windows_path_components(&with_dots);
            assert_eq!(
                collapsed,
                normalize_windows_path_components(&path),
                "dot segments must collapse for {path}"
            );
            // Mixed separators normalize to the same components.
            let slashy = with_dots.replace('\\', "/");
            assert_eq!(
                normalize_windows_path_components(&slashy),
                collapsed,
                "slash form must normalize identically"
            );
        }
    }
}
