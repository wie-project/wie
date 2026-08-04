//! Runtime run and smoke commands.

use super::util::write_entry_trace_summary;
use anyhow::{Context, Result, bail};
use std::io;
use std::path::{Path, PathBuf};

/// Whether a stdin path represents an interactive terminal rather than a file.
/// When true, the emulator reads line-by-line from the host TTY instead of
/// injecting pre-loaded bytes (LiveHost mode).
fn is_interactive_stdin(path: &Path) -> bool {
    // `/dev/stdin`, `/dev/tty`, or `-` are the common interactive markers.
    // OsStr/Path compares avoid the lossy-UTF-8 round trip; byte-level
    // semantics are identical (all three names are pure ASCII).
    path == Path::new("/dev/stdin")
        || path == Path::new("/dev/tty")
        || path == Path::new("-")
        || path
            .file_name()
            .is_some_and(|n| n == std::ffi::OsStr::new("stdin"))
}

/// Copy `host_path` into the bottle when it lives outside any mapped volume,
/// returning the host path the run should load.
///
/// FS policy: a program that needs the filesystem always runs inside a bottle.
/// An exe launched from outside gets a copy of its own at `{root}/drive_c/{name}`
/// (install-style — the source stays untouched). The guest identity label is
/// `C:\{name}` (derived from the basename), so with the copy in place that
/// label maps through the volume config to a real bottle file: GetModuleFileName
/// and the shell32 "New Window" relaunch both resolve the in-bottle copy.
///
/// Pass-through cases: no bottle (`None`), or the exe already lives under a
/// mapped volume (`{root}/drive_c` or the optional D: bridge). A nested in-bottle
/// exe passes through unchanged even though `C:\{name}` then maps to the volume
/// root rather than the real file — accepted per the in-bottle policy. A
/// same-named file already in `drive_c` is overwritten: the bottle copy is this
/// run's own (no hash check — that would be over-engineering).
pub(crate) fn ensure_exe_in_bottle(
    host_path: &Path,
    bottle_root: Option<&Path>,
    drive_d_root: Option<&Path>,
) -> Result<PathBuf> {
    let Some(root) = bottle_root else {
        return Ok(host_path.to_path_buf());
    };
    if is_under(host_path, &root.join("drive_c"))
        || drive_d_root.is_some_and(|d| is_under(host_path, d))
    {
        return Ok(host_path.to_path_buf());
    }
    if !host_path.is_file() {
        bail!("run source is not a file: {}", host_path.display());
    }
    let Some(file_name) = host_path.file_name() else {
        bail!("run source has no file name: {}", host_path.display());
    };
    let drive_c = root.join("drive_c");
    std::fs::create_dir_all(&drive_c)
        .with_context(|| format!("create bottle drive_c: {}", drive_c.display()))?;
    let dest = drive_c.join(file_name);
    std::fs::copy(host_path, &dest).with_context(|| {
        format!(
            "copy exe into bottle ({} -> {})",
            host_path.display(),
            dest.display()
        )
    })?;
    eprintln!(
        "bottle: copied exe in ({} -> {})",
        host_path.display(),
        dest.display()
    );
    Ok(dest)
}

/// Lexical containment check (`.`/`..` normalized) that never touches the FS.
fn is_under(path: &Path, dir: &Path) -> bool {
    let Ok(path) = std::path::absolute(path) else {
        return false;
    };
    let Ok(dir) = std::path::absolute(dir) else {
        return false;
    };
    path.starts_with(dir)
}
/// Runs a freestanding / micro PE until `ExitProcess` and checks the exit code.
pub(crate) fn run_micro(
    path: &Path,
    max_api: usize,
    expect_code: u32,
    bottle_root: Option<&Path>,
    drive_d: Option<&Path>,
    stdin_path: Option<&Path>,
    guest_args: &[String],
) -> Result<()> {
    let root = bottle_root
        .map(std::path::Path::to_path_buf)
        .or_else(wie_winapi::bottle_root_from_env);
    if let Some(ref r) = root {
        println!("bottle_root: {}", r.display());
    }
    let drive_d_root = drive_d
        .map(std::path::Path::to_path_buf)
        .or_else(wie_winapi::drive_d_from_env);
    if let Some(ref d) = drive_d_root {
        println!("drive_d: {}", d.display());
    }
    // FS policy: an exe outside the bottle runs from a drive_c copy so the
    // guest identity's `C:\{name}` label maps back to a real bottle file.
    let run_path = ensure_exe_in_bottle(path, root.as_deref(), drive_d_root.as_deref())?;
    let stdin_bytes = match stdin_path {
        Some(p) if is_interactive_stdin(p) => {
            // Interactive stdin: let the emulator read line-by-line from the host
            // TTY via LiveHost mode (empty bytes = live reading).
            eprintln!("stdin: interactive (LiveHost mode)");
            Vec::new()
        }
        Some(p) => std::fs::read(p)
            .with_context(|| format!("failed to read guest stdin file: {}", p.display()))?,
        None => Vec::new(),
    };
    if !guest_args.is_empty() {
        println!("guest_args: {guest_args:?}");
    }
    if stdin_path.is_some() && !stdin_bytes.is_empty() {
        println!("guest_stdin_bytes: {} (inject)", stdin_bytes.len());
    }
    let summary = wie_runtime::run_micro_exe_with_options(
        &run_path,
        max_api,
        wie_runtime::MicroRunOptions {
            bottle_root: root,
            drive_d_root,
            guest_args: guest_args.to_vec(),
            stdin_bytes,
        },
    )?;

    eprintln!("run_micro: path={}", summary.path);
    eprintln!("cpu_backend: {}", summary.cpu_backend);
    eprintln!(
        "entry={:#018x} initial_rsp={:#018x}",
        summary.entry_point_va, summary.initial_rsp
    );
    eprintln!(
        "events={} termination={:?}",
        summary.run.events.len(),
        summary.run.termination
    );

    // Full event dumps are useful for micros (small max-api). Real tools like
    // 7za generate tens of thousands of stops — printing them all drowns guest
    // console output and makes it look like "logs only appear at the end".
    // `WIE_API_TRACE=1` forces a full dump; otherwise show head+tail only.
    const HEAD: usize = 32;
    const TAIL: usize = 32;
    let events = &summary.run.events;
    let force_full = std::env::var_os("WIE_API_TRACE").is_some();
    if force_full || events.len() <= HEAD + TAIL {
        for event in events {
            eprintln!(
                "  [{:>4}] {}!{} handled={} ret={:?}",
                event.index,
                event.library.as_ref(),
                event.name.as_ref(),
                event.handled,
                event.return_value
            );
        }
    } else {
        for event in events.iter().take(HEAD) {
            eprintln!(
                "  [{:>4}] {}!{} handled={} ret={:?}",
                event.index,
                event.library.as_ref(),
                event.name.as_ref(),
                event.handled,
                event.return_value
            );
        }
        let omitted = events.len().saturating_sub(HEAD + TAIL);
        eprintln!("  … {omitted} events omitted (set WIE_API_TRACE=1 for full dump) …");
        for event in events.iter().skip(events.len().saturating_sub(TAIL)) {
            eprintln!(
                "  [{:>4}] {}!{} handled={} ret={:?}",
                event.index,
                event.library.as_ref(),
                event.name.as_ref(),
                event.handled,
                event.return_value
            );
        }
    }

    if let Some(profile) = &summary.profile {
        eprintln!("{}", profile.report());
    }

    match summary.exit_code {
        Some(code) if code == expect_code => {
            eprintln!("run_micro: ok exit={code}");
            Ok(())
        }
        Some(code) => {
            bail!("run_micro: exit={code} expected={expect_code}");
        }
        None => {
            bail!(
                "run_micro: did not reach ExitProcess (termination={:?})",
                summary.run.termination
            );
        }
    }
}

/// Runs a PE until the persistent runtime yields (or exits).
pub(crate) fn run_until_yield(path: &Path, max_api: usize) -> Result<()> {
    // FS policy: an exe outside the bottle runs from a drive_c copy first.
    let run_path = ensure_exe_in_bottle(
        path,
        wie_winapi::bottle_root_from_env().as_deref(),
        wie_winapi::drive_d_from_env().as_deref(),
    )?;
    // Ensure Sleep(n>0) actually sleeps and the idle loop parks the host
    // thread when waiting for messages. Otherwise every Sleep is a no-op
    // and interactive programs render all frames instantly.
    // SAFETY: `set_var` is unsafe only because concurrent reads from other
    // threads could observe a torn environment. This runs on the main thread
    // before any guest/worker threads spawn and before the runtime reads
    // `WIE_IDLE`, so no other thread accesses the environment concurrently.
    #[expect(unsafe_code)]
    unsafe {
        std::env::set_var("WIE_IDLE", "park");
    }
    let summary = wie_runtime::run_persistent_until_yield(&run_path, max_api)?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    write_entry_trace_summary(&mut output, &summary)
}

/// Owns raw-mode entry so the terminal is restored on *every* exit path —
/// normal return, `?` error propagation, and panics.
///
/// The guard always restores on drop: [`wie_winapi::console::restore_terminal`]
/// is idempotent, so it also covers raw mode the guest entered itself through
/// `ReadConsoleInput` / `_getch` (`pump::ensure_input_ready`).
struct TerminalRawGuard;

impl TerminalRawGuard {
    fn enter() -> Self {
        if !wie_winapi::console::set_raw_mode(true) {
            eprintln!(
                "warning: --console needs a terminal (stdin is not a tty); keys will require Enter"
            );
        }
        Self
    }
}

impl Drop for TerminalRawGuard {
    fn drop(&mut self) {
        wie_winapi::console::restore_terminal();
    }
}

/// Runs a PE under `--console`: raw-mode interactive input for terminal games.
///
/// Flow: switch the host terminal into raw mode → run the guest until
/// `ExitProcess` (per-quantum budget is re-entered, never a session cap) →
/// restore the terminal on every path via [`TerminalRawGuard`].
///
/// The guest ticks itself: its own `Sleep` + `ReadConsoleInputW(timeout)`
/// loop is the frame clock (Windows-identical). With `VMIN`/`VTIME = 0` the
/// read returns instantly with whatever the pump buffered, so every keystroke
/// arrives immediately — no Enter, no host tick source.
pub(crate) fn run_console_interactive(path: &Path, max_api: Option<usize>) -> Result<()> {
    // FS policy: an exe outside the bottle runs from a drive_c copy first.
    let run_path = ensure_exe_in_bottle(
        path,
        wie_winapi::bottle_root_from_env().as_deref(),
        wie_winapi::drive_d_from_env().as_deref(),
    )?;
    let _raw = TerminalRawGuard::enter();

    // The guest's frame loop is Sleep + input poll, so Sleep(n>0) must park
    // the host or the loop spins at 100% CPU. Same rationale (and SAFETY
    // comment) as `run_until_yield`: main thread, before any guest thread.
    #[expect(unsafe_code)]
    unsafe {
        std::env::set_var("WIE_IDLE", "park");
    }

    let mut session = wie_runtime::RuntimeSession::new_with_options(
        &run_path,
        wie_winapi::MessageQueueIdlePolicy::YieldOnIdle,
        wie_runtime::DEFAULT_LAYOUT,
        wie_runtime::SessionOptions {
            guest_args: Vec::new(),
            // Empty stdin bytes → LiveHost mode, so ReadFile(STD_INPUT_HANDLE)
            // and ReadConsoleInputW read from the host terminal.
            stdin_bytes: Vec::new(),
        },
    )?;

    // One quantum's worth of API stops; the loop re-enters, so this bounds a
    // single `run_until_stop` call, not the session (an interactive game runs
    // until the guest exits).
    let quantum_budget = max_api.unwrap_or(1_000_000);
    let exit_code = loop {
        let summary = session.run_until_stop(quantum_budget)?;
        match summary.termination {
            wie_runtime::EntryTraceTermination::ExitProcess { code } => break code,
            wie_runtime::EntryTraceTermination::WaitingForMessage => {
                // Empty GetMessage with no message source. Park briefly and
                // let the guest retry; a console game's Sleep loop resumes.
                wie_winapi::idle::apply_message_park();
            }
            wie_runtime::EntryTraceTermination::GuestCallbackRequested { .. } => {
                // Handled internally by the runtime; keep running.
            }
            wie_runtime::EntryTraceTermination::ApiLimit => {
                // A per-quantum budget, not a session timeout — re-enter.
            }
            other => {
                bail!("run_console: guest stopped early: {other:?}");
            }
        }
    };

    eprintln!("run_console: exit={exit_code}");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Unique temp dir under the system temp dir; removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("wie-bottle-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A real file that `ensure_exe_in_bottle` can copy.
    fn fake_exe(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"MZ\x90\x00").expect("write fake exe");
        path
    }

    #[test]
    fn copies_outside_exe_into_bottle_drive_c() {
        let source = TempDir::new("copy-src");
        let src_exe = fake_exe(source.path(), "app.exe");
        let bottle = TempDir::new("copy-bottle");

        let resolved =
            ensure_exe_in_bottle(&src_exe, Some(bottle.path()), None).expect("copy should succeed");
        let expected = bottle.path().join("drive_c").join("app.exe");
        assert_eq!(resolved, expected, "resolved path is the drive_c copy");
        assert!(expected.is_file(), "bottle copy must exist");
        assert_eq!(
            std::fs::read(&expected).expect("read copy"),
            b"MZ\x90\x00",
            "copy carries the source bytes"
        );
        assert!(src_exe.is_file(), "copy is non-destructive: source stays");
    }

    #[test]
    fn in_bottle_exe_passes_through_unchanged() {
        let bottle = TempDir::new("inside-bottle");
        let drive_c = bottle.path().join("drive_c");
        std::fs::create_dir_all(&drive_c).expect("create drive_c");
        let exe = fake_exe(&drive_c, "app.exe");

        let resolved = ensure_exe_in_bottle(&exe, Some(bottle.path()), None).expect("pass through");
        assert_eq!(resolved, exe, "in-bottle exe is its own run source");
    }

    #[test]
    fn drive_d_bridge_exe_passes_through_unchanged() {
        let bridge = TempDir::new("inside-drive-d");
        let exe = fake_exe(bridge.path(), "app.exe");

        let resolved = ensure_exe_in_bottle(&exe, Some(bridge.path()), Some(bridge.path()))
            .expect("pass through");
        assert_eq!(resolved, exe, "D: bridge exe is its own run source");
    }

    #[test]
    fn no_bottle_passes_through_unchanged() {
        let source = TempDir::new("no-bottle");
        let exe = fake_exe(source.path(), "app.exe");

        let resolved = ensure_exe_in_bottle(&exe, None, None).expect("pass through");
        assert_eq!(resolved, exe, "no bottle means no copy");
    }

    #[test]
    fn missing_source_is_an_error() {
        let bottle = TempDir::new("missing-src");
        let ghost = bottle.path().join("ghost.exe");

        let err = ensure_exe_in_bottle(&ghost, Some(bottle.path()), None).expect_err("must fail");
        assert!(
            err.to_string().contains("not a file"),
            "error names the missing file: {err}"
        );
    }

    /// End-to-end: a micro exe outside the bottle runs from a drive_c copy,
    /// and the guest identity labels that copy (`C:\{name}` → real file).
    #[test]
    fn run_micro_runs_outside_exe_from_bottle_copy() {
        let mut micro = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        micro.pop();
        micro.pop();
        micro.push("micro-exes/out");
        micro.push("crt_hello.exe");
        if !micro.is_file() {
            eprintln!("skip: micro-exes/out/crt_hello.exe not built (run make -C micro-exes)");
            return;
        }
        // Stage the exe outside the bottle, in an unrelated temp dir.
        let outside = TempDir::new("run-flow-outside");
        let src_exe = fake_exe(outside.path(), "crt_hello.exe");
        std::fs::copy(&micro, &src_exe).expect("stage exe outside the bottle");
        let bottle = TempDir::new("run-flow-bottle");

        run_micro(&src_exe, 1024, 0, Some(bottle.path()), None, None, &[])
            .expect("run_micro exits 0 from the bottle copy");

        let copy = bottle.path().join("drive_c").join("crt_hello.exe");
        assert!(copy.is_file(), "bottle copy must exist after the run");
        // The identity of the copy is the guest label, which the volume config
        // maps back to this same file.
        let identity = wie_pe::process_identity_from_host_path_with_args(&copy, &[]);
        assert_eq!(identity.module_file_name, "crt_hello.exe");
        assert_eq!(identity.module_path, r"C:\crt_hello.exe");
        assert_eq!(identity.current_directory, r"C:\");
    }
}
