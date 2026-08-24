//! GUI launch preflight: explicit guest argv entries that name absolute
//! Windows paths must resolve to existing files in the mapped volumes.
//!
//! `--gui` launches the guest in a persistent window loop, so a typo in a file
//! argument (`C:\data\level.bin`) would otherwise surface only as a runtime
//! open failure inside the guest. This module verifies those explicit
//! dependencies BEFORE the guest thread starts and returns a clear launch
//! error instead.
//!
//! Deliberately narrow: it copies nothing (the exe and its app folder are
//! staged by `crate::commands::stage_run_source` before this runs), never
//! touches `FileDialogPolicy` (GetOpenFileName and friends stay interactive),
//! and ignores anything that is not an absolute `C:`/`D:` path argument —
//! flags, ordinary values and relative names are the guest's to interpret at
//! runtime (relative names resolve against the staged app folder's guest
//! current directory).

use anyhow::Result;
use wie_runtime::SessionOptions;
use wie_winapi::vfs::{VolumeConfig, guest_path_to_host, normalize_windows_path_separators};

/// Verify explicit guest argv file dependencies and build the session
/// bootstrap options for a GUI run.
///
/// Every absolute `C:\…` / `D:\…` guest argument must map (through the
/// configured bottle / drive bridge) to an existing host path; a missing one
/// fails the launch with the guest path named. The returned [`SessionOptions`]
/// carries the args through to `RuntimeSession::new_with_options`, preserving
/// argv verbatim (no reordering, no translation).
pub fn preflight_guest_args(
    guest_args: &[String],
    volumes: &VolumeConfig,
) -> Result<SessionOptions> {
    for arg in guest_args {
        preflight_guest_arg_file(arg, volumes)?;
    }
    Ok(SessionOptions {
        guest_args: guest_args.to_vec(),
        ..SessionOptions::default()
    })
}

/// Verify ONE guest argv entry when it names an absolute `C:`/`D:` path.
///
/// Non-path args, unmapped drives (a `D:` arg with no bridge, or a `..`
/// escape) and unmappable forms pass through: they are not file dependencies
/// the CLI can verify — the guest decides their meaning at runtime.
fn preflight_guest_arg_file(arg: &str, volumes: &VolumeConfig) -> Result<()> {
    let Some((drive, path)) = absolute_guest_arg_path(arg) else {
        return Ok(());
    };
    if !matches!(drive, 'C' | 'D') {
        return Ok(());
    }
    let Some(mapped) = guest_path_to_host(volumes, &path) else {
        return Ok(());
    };
    if !mapped.host.exists() {
        anyhow::bail!(
            "guest argument names a missing file: {path} (mapped host: {})",
            mapped.host.display()
        );
    }
    Ok(())
}

/// Parse `arg` as an absolute `X:\…` / `X:/…` Windows path (case-insensitive
/// drive letter, `/` normalized to `\`). `None` for anything else — flags,
/// ordinary values, relative names and the drive-relative bare `C:` form are
/// not absolute path dependencies.
#[must_use]
fn absolute_guest_arg_path(arg: &str) -> Option<(char, String)> {
    let trimmed = arg.trim().trim_matches('"');
    let norm = normalize_windows_path_separators(trimmed);
    let bytes = norm.as_bytes();
    let letter = bytes.first().copied()?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    if bytes.get(1) != Some(&b':') {
        return None;
    }
    if bytes.get(2) != Some(&b'\\') {
        return None;
    }
    Some((char::from(letter).to_ascii_uppercase(), norm))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::path::Path;

    /// Create a bottle with `drive_c/data/level.bin` and return its root.
    fn seeded_bottle(tag: &str) -> TempDir {
        let bottle = TempDir::new(tag);
        let data = bottle.path().join("drive_c").join("data");
        std::fs::create_dir_all(&data).expect("create drive_c/data");
        std::fs::write(data.join("level.bin"), b"level").expect("write level.bin");
        bottle
    }

    fn c_volumes(bottle: &Path) -> VolumeConfig {
        VolumeConfig::from_parts(Some(bottle.to_path_buf()), None)
    }

    /// An absolute `C:` argument naming an existing file passes the preflight,
    /// and the args propagate verbatim into the session bootstrap options
    /// (the GUI argument-propagation contract).
    #[test]
    fn present_c_arg_passes_and_propagates() {
        let bottle = seeded_bottle("present-c");
        let args = vec![r"C:\data\level.bin".to_owned()];

        let options = preflight_guest_args(&args, &c_volumes(bottle.path())).expect("file exists");
        assert_eq!(
            options.guest_args, args,
            "guest args reach the session options verbatim"
        );
    }

    /// A missing `C:` argument fails the launch with the guest path named.
    #[test]
    fn missing_c_arg_fails_before_launch() {
        let bottle = seeded_bottle("missing-c");
        let args = vec![r"C:\data\ghost.bin".to_owned()];

        let err = preflight_guest_args(&args, &c_volumes(bottle.path())).expect_err("must fail");
        assert!(
            err.to_string().contains(r"C:\data\ghost.bin"),
            "error names the guest path: {err}"
        );
        assert!(
            err.to_string().contains("missing file"),
            "error is clearly a missing-file error: {err}"
        );
    }

    /// A missing `D:` argument fails only when the D: bridge is mounted.
    #[test]
    fn missing_d_arg_fails_with_bridge() {
        let bottle = seeded_bottle("missing-d-bottle");
        let bridge = TempDir::new("missing-d-bridge");
        let args = vec![r"D:\assets\tex.dds".to_owned()];
        let volumes = VolumeConfig::from_parts(
            Some(bottle.path().to_path_buf()),
            Some(bridge.path().to_path_buf()),
        );

        let err = preflight_guest_args(&args, &volumes).expect_err("must fail");
        assert!(
            err.to_string().contains(r"D:\assets\tex.dds"),
            "error names the guest path: {err}"
        );
    }

    /// A `D:` argument with no bridge mounted is not a verifiable dependency —
    /// it passes through (the guest decides at runtime), matching today's
    /// behavior.
    #[test]
    fn d_arg_without_bridge_passes() {
        let bottle = seeded_bottle("d-no-bridge");
        let args = vec![r"D:\assets\tex.dds".to_owned()];

        let options =
            preflight_guest_args(&args, &c_volumes(bottle.path())).expect("unmapped D: passes");
        assert_eq!(options.guest_args, args);
    }

    /// Flags, ordinary values, relative names, bare drive letters and
    /// unmapped drives are not file dependencies — all pass through.
    #[test]
    fn non_path_args_pass() {
        let bottle = seeded_bottle("non-path");
        let args: Vec<String> = vec![
            "-n".to_owned(),
            "--config=fast".to_owned(),
            "3".to_owned(),
            "hello".to_owned(),
            "data/level.bin".to_owned(),
            "C:".to_owned(),
            r"Z:\thing.exe".to_owned(),
        ];

        let options = preflight_guest_args(&args, &c_volumes(bottle.path())).expect("no paths");
        assert_eq!(
            options.guest_args, args,
            "non-path args propagate untouched"
        );
    }

    /// The drive letter is case-insensitive and `/` separators normalize, the
    /// same way the VFS layer maps guest paths at runtime.
    #[test]
    fn lowercase_drive_and_forward_slashes_resolve() {
        let bottle = seeded_bottle("case-slashes");
        let args = vec!["c:/data/level.bin".to_owned()];

        let options =
            preflight_guest_args(&args, &c_volumes(bottle.path())).expect("maps to the file");
        assert_eq!(options.guest_args, args);
    }

    /// A missing file with a space in its name (the reason CLI quoting exists)
    /// is still caught.
    #[test]
    fn missing_quoted_path_with_space_fails() {
        let bottle = seeded_bottle("space-path");
        let args = vec![r"C:\data\my level.bin".to_owned()];

        let err = preflight_guest_args(&args, &c_volumes(bottle.path())).expect_err("must fail");
        assert!(err.to_string().contains(r"C:\data\my level.bin"));
    }

    /// A `C:` argument that maps through the bottle but escapes it (raw `..`)
    /// is unmappable, so it passes through rather than being misreported as a
    /// missing file.
    #[test]
    fn escape_path_passes_through() {
        let bottle = seeded_bottle("escape");
        let args = vec![r"C:\data\..\..\etc\passwd".to_owned()];

        let options = preflight_guest_args(&args, &c_volumes(bottle.path())).expect("unmappable");
        assert_eq!(options.guest_args, args);
    }
}
