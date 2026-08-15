//! Runtime run and smoke commands.

use super::util::write_entry_trace_summary;
use anyhow::{Context, Result, bail};
use std::io;
use std::path::{Path, PathBuf};
use wie_winapi::VolumeConfig;

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

/// A run source after bottle staging: the host path to load plus the guest
/// current directory the process should start in.
#[derive(Debug)]
pub(crate) struct StagedRunSource {
    /// Host path of the run source (the in-bottle copy when staged).
    pub run_path: PathBuf,
    /// Guest current directory for the launched process. `None` when no
    /// staging happened (the loader default `C:\` applies).
    pub guest_current_directory: Option<String>,
}

/// What to stage into the bottle for an out-of-bottle run source.
///
/// The `wie run app.exe` default is [`StageMode::ExeOnly`]: only the
/// executable enters the bottle, so unrelated sibling files from the exe's
/// host folder are never copied. The console/persistent entries keep the
/// legacy whole-parent-folder default ([`StageMode::ParentFolder`]);
/// `--app-dir <HOST_DIR>` names a complete folder explicitly
/// ([`StageMode::AppDir`]).
#[derive(Debug, Clone, Copy)]
pub(crate) enum StageMode<'a> {
    /// Copy only the executable file into the bottle.
    ExeOnly,
    /// Copy the executable's parent directory (the pre-`--app-dir` default,
    /// kept unchanged for `--console` / `--persistent`).
    ParentFolder,
    /// Copy the complete folder named by `--app-dir`, preserving relative
    /// paths (data files, DLLs, plugins, subdirectories).
    AppDir(&'a Path),
}

impl<'a> StageMode<'a> {
    /// The staging mode for a micro / GUI / screenshot run entry: an explicit
    /// `--app-dir`, else the exe-only default.
    #[must_use]
    pub(crate) fn from_run_entry(app_dir: Option<&'a Path>) -> Self {
        match app_dir {
            Some(dir) => Self::AppDir(dir),
            None => Self::ExeOnly,
        }
    }
}

/// Stage a run source into the bottle before the session starts.
///
/// FS policy, selected by `stage`: an exe launched from outside a
/// *configured* bottle lands in `{root}/drive_c/Program Files/{name}/` where
/// `{name}` is the exe's file stem (install-style — the source stays
/// untouched). The guest identity derives through the volume mapping, so the
/// copy's guest path is `C:\Program Files\{name}\{name}.exe` and the process
/// current directory is the staged folder: relative resource paths resolve
/// like a normal Windows launch.
///
/// [`StageMode::ExeOnly`] (the default) copies just the executable — sibling
/// files from the exe's host folder stay out of the bottle. The complete
/// folder is copied only when named explicitly: [`StageMode::AppDir`] for an
/// explicit `--app-dir` (relative paths, DLLs, plugins and data files
/// preserved), [`StageMode::ParentFolder`] for the legacy whole-parent-folder
/// behavior of the console/persistent entries.
///
/// Pass-through cases: no explicit bottle root in `volumes`, or the exe
/// already lives under a mapped volume (`{root}/drive_c` or the optional D:
/// bridge). Without an explicit root the exe runs in place — its own file ops
/// still land in the global app-data bottle, so nothing needs copying. A
/// nested in-bottle exe passes through unchanged even though its `C:\…` label
/// then maps to the volume root rather than the real file — accepted per the
/// in-bottle policy.
///
/// Symlinks are rejected, never followed: a link inside the source app
/// directory could point outside it (copying foreign files into the bottle)
/// or loop back on an ancestor, and a link planted at a destination path
/// could redirect the copy outside the bottle.
pub(crate) fn stage_run_source(
    host_path: &Path,
    volumes: &VolumeConfig,
    stage: StageMode<'_>,
) -> Result<StagedRunSource> {
    let Some(root) = volumes.bottle_root.as_deref() else {
        return Ok(StagedRunSource {
            run_path: host_path.to_path_buf(),
            guest_current_directory: None,
        });
    };
    if is_under(host_path, &root.join("drive_c"))
        || volumes
            .drive_d_root
            .as_deref()
            .is_some_and(|d| is_under(host_path, d))
    {
        return Ok(StagedRunSource {
            run_path: host_path.to_path_buf(),
            guest_current_directory: None,
        });
    }
    let exe_meta = std::fs::symlink_metadata(host_path)
        .with_context(|| format!("stat run source: {}", host_path.display()))?;
    if exe_meta.file_type().is_symlink() || !exe_meta.is_file() {
        bail!(
            "run source is not a regular file (symlinks are rejected): {}",
            host_path.display()
        );
    }
    let Some(file_name) = host_path.file_name() else {
        bail!("run source has no file name: {}", host_path.display());
    };
    // Install-style layout: the stem names the app dir under Program Files,
    // the file keeps its original basename.
    let stem = host_path
        .file_stem()
        .filter(|stem| !stem.is_empty())
        .unwrap_or(file_name);
    let drive_c = root.join("drive_c");
    let dest_dir = drive_c.join("Program Files").join(stem);

    // Resolve the source tree to copy (if any) and the run path relative to
    // the staged dir root. The exe-only mode copies a single file; both
    // folder modes copy a complete tree.
    let (source_dir, rel_run_path) = match stage {
        StageMode::ExeOnly => (None, PathBuf::from(file_name)),
        StageMode::ParentFolder => {
            let Some(source_dir) = host_path.parent() else {
                bail!(
                    "run source has no parent directory: {}",
                    host_path.display()
                );
            };
            require_real_dir(source_dir, "run source app dir")?;
            (Some(source_dir), PathBuf::from(file_name))
        }
        StageMode::AppDir(app_dir) => {
            let Some(rel) = relative_to(host_path, app_dir) else {
                bail!(
                    "run source is not inside --app-dir: {} is not under {}",
                    host_path.display(),
                    app_dir.display()
                );
            };
            require_real_dir(app_dir, "--app-dir")?;
            (Some(app_dir), rel)
        }
    };

    match source_dir {
        Some(source_dir) => {
            copy_tree_reject_symlinks(source_dir, &dest_dir)?;
            tracing::debug!(
                "bottle: staged app folder in ({} -> {})",
                source_dir.display(),
                dest_dir.display()
            );
        }
        None => {
            // Single-file staging: the dest dir must exist first, then the
            // tree copier's file branch applies the same symlink guards.
            reject_symlink_dest(&dest_dir)?;
            std::fs::create_dir_all(&dest_dir)
                .with_context(|| format!("create staged dir: {}", dest_dir.display()))?;
            copy_tree_reject_symlinks(host_path, &dest_dir.join(&rel_run_path))?;
            tracing::debug!(
                "bottle: staged exe in ({} -> {})",
                host_path.display(),
                dest_dir.display()
            );
        }
    }
    let run_path = dest_dir.join(&rel_run_path);
    // The staged dir's guest label is the process current directory (the
    // exe's own guest path stays `C:\Program Files\{name}\{name}.exe`).
    let guest_current_directory = wie_winapi::host_path_to_guest(volumes, &dest_dir);
    Ok(StagedRunSource {
        run_path,
        guest_current_directory,
    })
}

/// Resolve the effective volume config for a run: an explicit CLI flag wins
/// over `WIE_ROOT` / `WIE_DRIVE_D`. `None` for both leaves the environment as
/// the only source (the console / persistent / headless / windowed entries'
/// behavior).
pub(crate) fn resolve_volume_config(
    bottle_root: Option<&Path>,
    drive_d_root: Option<&Path>,
) -> VolumeConfig {
    VolumeConfig::from_parts(
        bottle_root
            .map(std::path::Path::to_path_buf)
            .or_else(wie_winapi::bottle_root_from_env),
        drive_d_root
            .map(std::path::Path::to_path_buf)
            .or_else(wie_winapi::drive_d_from_env),
    )
}

/// Recursively copy the source tree into `dst`, preserving relative paths.
///
/// Rejects every symlink instead of following it: a link could point outside
/// the source app directory and pull foreign files into the bottle (or loop
/// back on an ancestor). Regular files and directories only.
fn copy_tree_reject_symlinks(src: &Path, dst: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)
        .with_context(|| format!("stat source entry: {}", src.display()))?;
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        bail!("refusing to stage symlink: {}", src.display());
    }
    if file_type.is_dir() {
        reject_symlink_dest(dst)?;
        std::fs::create_dir_all(dst)
            .with_context(|| format!("create staged dir: {}", dst.display()))?;
        for entry in
            std::fs::read_dir(src).with_context(|| format!("read source dir: {}", src.display()))?
        {
            let entry =
                entry.with_context(|| format!("read source dir entry in {}", src.display()))?;
            copy_tree_reject_symlinks(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else if file_type.is_file() {
        reject_symlink_dest(dst)?;
        std::fs::copy(src, dst)
            .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
        Ok(())
    } else {
        bail!("refusing to stage non-regular file: {}", src.display());
    }
}

/// Reject a destination that is an existing symlink — a planted link could
/// redirect the copy outside the bottle. Missing or real entries pass (other
/// errors surface on the create/copy that follows).
fn reject_symlink_dest(dst: &Path) -> Result<()> {
    match std::fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!(
                "refusing to stage through symlinked dest: {}",
                dst.display()
            )
        }
        Ok(_) | Err(_) => Ok(()),
    }
}

/// A staged source dir must be a real directory: a symlinked dir would copy
/// a different tree than the user pointed at. `what` names the dir in the
/// error message ("run source app dir" / `--app-dir`).
fn require_real_dir(dir: &Path, what: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("stat {what}: {}", dir.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        bail!(
            "{what} is not a real directory (symlinks are rejected): {}",
            dir.display()
        );
    }
    Ok(())
}

/// Lexical containment that never touches the FS: `path` is inside `dir`
/// after `.`/`..` normalization. `Some(rel)` carries the relative path of
/// `path` within `dir`; `None` when not contained.
fn relative_to(path: &Path, dir: &Path) -> Option<PathBuf> {
    let path = std::path::absolute(path).ok()?;
    let dir = std::path::absolute(dir).ok()?;
    path.strip_prefix(&dir).ok().map(Path::to_path_buf)
}

/// Lexical containment check (`.`/`..` normalized) that never touches the FS.
fn is_under(path: &Path, dir: &Path) -> bool {
    relative_to(path, dir).is_some()
}
/// Context for a micro run entry: the volume/staging switches and the guest
/// bootstrap inputs (argv, stdin). Grouped so [`run_micro`] keeps a short
/// signature; `path` stays a separate argument because callers (bottle run)
/// resolve it independently.
pub(crate) struct MicroRunOptions<'a> {
    /// Cap host API stops (the CLI applies the mode default before building).
    pub max_api: usize,
    /// Expected ExitProcess code (default 0).
    pub expect_code: u32,
    pub bottle_root: Option<&'a Path>,
    pub drive_d: Option<&'a Path>,
    pub stdin_path: Option<&'a Path>,
    pub guest_args: &'a [String],
    pub app_dir: Option<&'a Path>,
}

/// Runs a freestanding / micro PE until `ExitProcess` and checks the exit code.
pub(crate) fn run_micro(path: &Path, options: MicroRunOptions<'_>) -> Result<()> {
    let volumes = resolve_volume_config(options.bottle_root, options.drive_d);
    let root = volumes.bottle_root.clone();
    let drive_d_root = volumes.drive_d_root.clone();
    match root.as_ref() {
        Some(r) => tracing::debug!("bottle_root: {} (override)", r.display()),
        None => tracing::debug!(
            "bottle_root: {} (global default)",
            wie_winapi::global_bottle_root().display()
        ),
    }
    if let Some(ref d) = drive_d_root {
        tracing::debug!("drive_d: {}", d.display());
    }
    // FS policy: an exe outside the bottle runs from a drive_c copy. Only the
    // exe itself is staged by default; `--app-dir` names a complete folder so
    // the guest identity's `C:\Program Files\{name}\{name}.exe` label maps
    // back to a real bottle file and relative resource paths resolve from the
    // staged folder.
    let staged = stage_run_source(path, &volumes, StageMode::from_run_entry(options.app_dir))?;
    let stdin_bytes = match options.stdin_path {
        Some(p) if is_interactive_stdin(p) => {
            // Interactive stdin: let the emulator read line-by-line from the host
            // TTY via LiveHost mode (empty bytes = live reading).
            tracing::debug!("stdin: interactive (LiveHost mode)");
            Vec::new()
        }
        Some(p) => std::fs::read(p)
            .with_context(|| format!("failed to read guest stdin file: {}", p.display()))?,
        None => Vec::new(),
    };
    if !options.guest_args.is_empty() {
        println!("guest_args: {:?}", options.guest_args);
    }
    if options.stdin_path.is_some() && !stdin_bytes.is_empty() {
        println!("guest_stdin_bytes: {} (inject)", stdin_bytes.len());
    }
    let summary = wie_runtime::run_micro_exe_with_options(
        &staged.run_path,
        options.max_api,
        wie_runtime::MicroRunOptions {
            bottle_root: root,
            drive_d_root,
            guest_args: options.guest_args.to_vec(),
            stdin_bytes,
            current_directory: staged.guest_current_directory,
        },
    )?;

    tracing::debug!("run_micro: path={}", summary.path);
    tracing::debug!("cpu_backend: {}", summary.cpu_backend);
    tracing::debug!(
        "entry={:#018x} initial_rsp={:#018x}",
        summary.entry_point_va,
        summary.initial_rsp
    );
    tracing::debug!(
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
            tracing::debug!(
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
            tracing::debug!(
                "  [{:>4}] {}!{} handled={} ret={:?}",
                event.index,
                event.library.as_ref(),
                event.name.as_ref(),
                event.handled,
                event.return_value
            );
        }
        let omitted = events.len().saturating_sub(HEAD + TAIL);
        tracing::debug!("  … {omitted} events omitted (set WIE_API_TRACE=1 for full dump) …");
        for event in events.iter().skip(events.len().saturating_sub(TAIL)) {
            tracing::debug!(
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
        Some(code) if code == options.expect_code => {
            tracing::debug!("run_micro: ok exit={code}");
            Ok(())
        }
        Some(code) => {
            bail!("run_micro: exit={code} expected={}", options.expect_code);
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
pub(crate) fn run_until_yield(
    path: &Path,
    max_api: usize,
    bottle_root: Option<&Path>,
) -> Result<()> {
    // FS policy: an exe outside the bottle runs from a drive_c copy of its
    // whole application folder first (the session options carry the staged
    // folder's guest cwd). Unchanged for `--persistent`: the entry never
    // takes `--app-dir`, so it keeps the legacy parent-folder default.
    let volumes = resolve_volume_config(bottle_root, None);
    let staged = stage_run_source(path, &volumes, StageMode::ParentFolder)?;
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
    let summary = wie_runtime::run_persistent_until_yield_with_options(
        &staged.run_path,
        max_api,
        wie_runtime::SessionOptions {
            current_directory: staged.guest_current_directory,
            // The staged root must reach the session: without it the session
            // would fall back to `WIE_ROOT` / the global bottle and map
            // `C:\…` differently than the staging above.
            bottle_root: bottle_root.map(std::path::Path::to_path_buf),
            ..wie_runtime::SessionOptions::default()
        },
    )?;
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
            tracing::warn!(
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

/// How many API stops one `run_until_stop` call may consume before the loop
/// re-enters. A per-quantum budget, not a session cap — an interactive game
/// runs until the guest exits.
const QUANTUM_MAX_API_DEFAULT: usize = 1_000_000;

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
pub(crate) fn run_console_interactive(
    path: &Path,
    max_api: Option<usize>,
    bottle_root: Option<&Path>,
) -> Result<()> {
    // FS policy: an exe outside the bottle runs from a drive_c copy of its
    // whole application folder first; the session options carry the staged
    // folder's guest current directory. Unchanged for `--console`: the entry
    // never takes `--app-dir`, so it keeps the legacy parent-folder default.
    let volumes = resolve_volume_config(bottle_root, None);
    let staged = stage_run_source(path, &volumes, StageMode::ParentFolder)?;
    let _raw = TerminalRawGuard::enter();

    // The guest's frame loop is Sleep + input poll, so Sleep(n>0) must park
    // the host or the loop spins at 100% CPU. Same rationale (and SAFETY
    // comment) as `run_until_yield`: main thread, before any guest thread.
    #[expect(unsafe_code)]
    unsafe {
        std::env::set_var("WIE_IDLE", "park");
    }

    let mut session = wie_runtime::RuntimeSession::new_with_options(
        &staged.run_path,
        wie_winapi::MessageQueueIdlePolicy::YieldOnIdle,
        wie_runtime::DEFAULT_LAYOUT,
        // Defaults: no guest argv and empty stdin bytes → LiveHost mode, so
        // ReadFile(STD_INPUT_HANDLE) and ReadConsoleInputW read from the host
        // terminal. The staging above used the env roots (or the `--bottle`
        // root), so the session resolves the same root — threaded explicitly
        // when a bottle was named, env → global bottle otherwise.
        wie_runtime::SessionOptions {
            current_directory: staged.guest_current_directory,
            bottle_root: bottle_root.map(std::path::Path::to_path_buf),
            ..wie_runtime::SessionOptions::default()
        },
    )?;

    // One quantum's worth of API stops; the loop re-enters, so this bounds a
    // single `run_until_stop` call, not the session (an interactive game runs
    // until the guest exits).
    let quantum_budget = max_api.unwrap_or(QUANTUM_MAX_API_DEFAULT);
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

    tracing::debug!("run_console: exit={exit_code}");
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

    /// A real file the staging can copy.
    fn fake_exe(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"MZ\x90\x00").expect("write fake exe");
        path
    }

    /// A volume config with only a C: bottle (the common test shape).
    fn c_volumes(bottle: &Path) -> VolumeConfig {
        VolumeConfig::from_parts(Some(bottle.to_path_buf()), None)
    }

    /// The default `wie run app.exe` stages ONLY the executable into the
    /// bottle: sibling files from the exe's host folder stay out, and the
    /// staged exe keeps the install-style Program Files identity + guest cwd.
    #[test]
    fn stages_standalone_exe_only_into_bottle() {
        let source = TempDir::new("exe-only-src");
        let src_exe = fake_exe(source.path(), "app.exe");
        // Siblings that must NOT enter the bottle under the exe-only default.
        std::fs::write(source.path().join("data.bin"), b"data").expect("write data");
        std::fs::write(source.path().join("libfoo.dll"), b"dll").expect("write dll");
        std::fs::create_dir_all(source.path().join("plugins")).expect("create plugins");
        std::fs::write(source.path().join("plugins/p1.dll"), b"p1").expect("write plugin");
        let bottle = TempDir::new("exe-only-bottle");

        let staged = stage_run_source(&src_exe, &c_volumes(bottle.path()), StageMode::ExeOnly)
            .expect("copy should succeed");
        let expected = bottle
            .path()
            .join("drive_c")
            .join("Program Files")
            .join("app")
            .join("app.exe");
        assert_eq!(
            staged.run_path, expected,
            "resolved path is the Program Files copy"
        );
        assert!(expected.is_file(), "bottle copy must exist");
        assert_eq!(
            std::fs::read(&expected).expect("read copy"),
            b"MZ\x90\x00",
            "copy carries the source bytes"
        );
        assert!(src_exe.is_file(), "copy is non-destructive: source stays");
        for rel in ["data.bin", "libfoo.dll", "plugins/p1.dll"] {
            assert!(
                !expected.parent().unwrap().join(rel).exists(),
                "sibling {rel} must not be staged under the exe-only default"
            );
        }
        assert_eq!(
            staged.guest_current_directory.as_deref(),
            Some(r"C:\Program Files\app"),
            "the staged app's guest current directory is its app folder"
        );
    }

    #[test]
    fn copied_exe_resolves_to_program_files_guest_path() {
        let source = TempDir::new("copy-guest-path-src");
        let src_exe = fake_exe(source.path(), "app.exe");
        let bottle = TempDir::new("copy-guest-path-bottle");

        let staged = stage_run_source(&src_exe, &c_volumes(bottle.path()), StageMode::ExeOnly)
            .expect("copy should succeed");
        // The loader default labels every exe `C:\{name}`; the runtime remaps
        // the module path through the volume config, which is what this test
        // mirrors (see session/init.rs identity derivation).
        let volumes = c_volumes(bottle.path());
        assert_eq!(
            wie_winapi::host_path_to_guest(&volumes, &staged.run_path).as_deref(),
            Some(r"C:\Program Files\app\app.exe"),
            "the in-bottle copy resolves to its Program Files guest path"
        );
    }

    #[test]
    fn in_bottle_exe_passes_through_unchanged() {
        let bottle = TempDir::new("inside-bottle");
        let drive_c = bottle.path().join("drive_c");
        std::fs::create_dir_all(&drive_c).expect("create drive_c");
        let exe = fake_exe(&drive_c, "app.exe");

        let staged = stage_run_source(&exe, &c_volumes(bottle.path()), StageMode::ExeOnly)
            .expect("pass through");
        assert_eq!(staged.run_path, exe, "in-bottle exe is its own run source");
        assert_eq!(
            staged.guest_current_directory, None,
            "an in-bottle exe keeps the default cwd"
        );
    }

    #[test]
    fn drive_d_bridge_exe_passes_through_unchanged() {
        let bridge = TempDir::new("inside-drive-d");
        let exe = fake_exe(bridge.path(), "app.exe");

        let staged = stage_run_source(
            &exe,
            &VolumeConfig::from_parts(
                Some(bridge.path().to_path_buf()),
                Some(bridge.path().to_path_buf()),
            ),
            StageMode::ExeOnly,
        )
        .expect("pass through");
        assert_eq!(staged.run_path, exe, "D: bridge exe is its own run source");
        assert_eq!(
            staged.guest_current_directory, None,
            "a D: bridge exe keeps the default cwd"
        );
    }

    #[test]
    fn no_bottle_passes_through_unchanged() {
        let source = TempDir::new("no-bottle");
        let exe = fake_exe(source.path(), "app.exe");

        let staged = stage_run_source(&exe, &VolumeConfig::default(), StageMode::ExeOnly)
            .expect("pass through");
        assert_eq!(staged.run_path, exe, "no bottle means no copy");
        assert_eq!(staged.guest_current_directory, None);
    }

    #[test]
    fn missing_source_is_an_error() {
        let bottle = TempDir::new("missing-src");
        let ghost = bottle.path().join("ghost.exe");

        let err = stage_run_source(&ghost, &c_volumes(bottle.path()), StageMode::ExeOnly)
            .expect_err("must fail");
        assert!(
            err.to_string().contains("ghost.exe"),
            "error names the missing file: {err}"
        );
    }

    /// `--app-dir` stages a complete app folder: the exe, sibling data files,
    /// DLLs, plugins and nested subdirectories all land in
    /// `Program Files/{name}/` with their relative paths preserved.
    #[test]
    fn explicit_app_dir_stages_complete_folder_preserving_nested_resources() {
        let source = TempDir::new("folder-src");
        std::fs::write(source.path().join("app.exe"), b"MZ").expect("write exe");
        std::fs::write(source.path().join("data.bin"), b"data").expect("write data");
        std::fs::write(source.path().join("libfoo.dll"), b"dll").expect("write dll");
        std::fs::create_dir_all(source.path().join("plugins")).expect("create plugins");
        std::fs::write(source.path().join("plugins/p1.dll"), b"p1").expect("write plugin");
        std::fs::create_dir_all(source.path().join("assets/sounds/deep"))
            .expect("create nested dirs");
        std::fs::write(source.path().join("assets/sounds/deep/amb.wav"), b"wav")
            .expect("write nested file");
        let bottle = TempDir::new("folder-bottle");

        let staged = stage_run_source(
            &source.path().join("app.exe"),
            &c_volumes(bottle.path()),
            StageMode::AppDir(source.path()),
        )
        .expect("stage the app folder");

        let root = bottle
            .path()
            .join("drive_c")
            .join("Program Files")
            .join("app");
        assert_eq!(staged.run_path, root.join("app.exe"));
        assert_eq!(
            std::fs::read(root.join("data.bin")).expect("staged data"),
            b"data"
        );
        assert_eq!(
            std::fs::read(root.join("libfoo.dll")).expect("staged dll"),
            b"dll"
        );
        assert_eq!(
            std::fs::read(root.join("plugins/p1.dll")).expect("staged plugin"),
            b"p1"
        );
        assert_eq!(
            std::fs::read(root.join("assets/sounds/deep/amb.wav")).expect("staged nested resource"),
            b"wav"
        );
        // The source tree is untouched.
        assert!(source.path().join("data.bin").is_file());
        assert!(source.path().join("plugins/p1.dll").is_file());
    }

    /// `--app-dir` with the exe nested inside the folder: the exe's relative
    /// path (and every sibling's) is preserved under the staged root, and the
    /// guest cwd stays the staged app folder.
    #[test]
    fn explicit_app_dir_stages_nested_exe_with_relative_paths() {
        let app_dir = TempDir::new("nested-app-dir");
        std::fs::create_dir_all(app_dir.path().join("bin")).expect("create bin");
        std::fs::write(app_dir.path().join("bin/app.exe"), b"MZ").expect("write exe");
        std::fs::write(app_dir.path().join("data.bin"), b"data").expect("write data");
        std::fs::create_dir_all(app_dir.path().join("plugins")).expect("create plugins");
        std::fs::write(app_dir.path().join("plugins/p1.dll"), b"p1").expect("write plugin");
        let bottle = TempDir::new("nested-app-bottle");

        let staged = stage_run_source(
            &app_dir.path().join("bin/app.exe"),
            &c_volumes(bottle.path()),
            StageMode::AppDir(app_dir.path()),
        )
        .expect("stage the app folder");

        let root = bottle
            .path()
            .join("drive_c")
            .join("Program Files")
            .join("app");
        assert_eq!(
            staged.run_path,
            root.join("bin/app.exe"),
            "the nested exe keeps its relative path under the staged root"
        );
        assert!(root.join("bin/app.exe").is_file(), "nested exe staged");
        assert_eq!(
            std::fs::read(root.join("data.bin")).expect("staged data"),
            b"data"
        );
        assert_eq!(
            std::fs::read(root.join("plugins/p1.dll")).expect("staged plugin"),
            b"p1"
        );
        assert_eq!(
            staged.guest_current_directory.as_deref(),
            Some(r"C:\Program Files\app"),
            "the guest cwd is the staged app folder, not the exe's subdir"
        );
    }

    /// An `--app-dir` that does not contain the run source fails up front with
    /// a clear error naming both paths.
    #[test]
    fn app_dir_not_containing_exe_is_rejected() {
        let app_dir = TempDir::new("app-dir-miss");
        let other = TempDir::new("app-dir-other");
        std::fs::write(app_dir.path().join("app.exe"), b"MZ").expect("write exe");
        let exe = fake_exe(other.path(), "stray.exe");
        let bottle = TempDir::new("app-dir-miss-bottle");

        let err = stage_run_source(
            &exe,
            &c_volumes(bottle.path()),
            StageMode::AppDir(app_dir.path()),
        )
        .expect_err("--app-dir must contain the run source");
        assert!(
            err.to_string().contains("not inside --app-dir"),
            "error is clearly a containment failure: {err}"
        );
        assert!(
            err.to_string().contains(&exe.display().to_string()),
            "error names the run source: {err}"
        );
        assert!(
            !bottle.path().join("drive_c").join("stray.exe").exists(),
            "nothing is staged when the app-dir check fails"
        );
    }

    /// An `--app-dir` that is not a real directory (here: a regular file)
    /// is rejected like any staged source dir.
    #[test]
    fn app_dir_must_be_a_real_directory() {
        let app_dir = TempDir::new("app-dir-file");
        let exe = fake_exe(app_dir.path(), "app.exe");
        let bottle = TempDir::new("app-dir-file-bottle");

        let err = stage_run_source(&exe, &c_volumes(bottle.path()), StageMode::AppDir(&exe))
            .expect_err("--app-dir pointing at a file must fail");
        assert!(
            err.to_string().contains("--app-dir"),
            "error names the app-dir: {err}"
        );
        assert!(
            err.to_string().contains("real directory"),
            "error is clearly a directory failure: {err}"
        );
    }

    /// A symlink inside the source app folder is rejected, never followed —
    /// a link pointing outside the app dir would copy foreign files into the
    /// bottle (or loop back on an ancestor).
    #[cfg(unix)]
    #[test]
    fn symlink_escape_in_source_is_rejected() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new("link-src");
        let app_dir = source.path().join("app");
        std::fs::create_dir_all(&app_dir).expect("create app dir");
        std::fs::write(app_dir.join("app.exe"), b"MZ").expect("write exe");
        // A link that escapes the app dir (and one looping to an ancestor).
        let outside = TempDir::new("link-outside");
        std::fs::write(outside.path().join("secret.txt"), b"s3cret").expect("write secret");
        symlink(outside.path(), app_dir.join("escape")).expect("plant escape link");
        symlink(&app_dir, app_dir.join("loop")).expect("plant loop link");
        let bottle = TempDir::new("link-bottle");

        let err = stage_run_source(
            &app_dir.join("app.exe"),
            &c_volumes(bottle.path()),
            StageMode::AppDir(&app_dir),
        )
        .expect_err("symlinks must be rejected");
        assert!(
            err.to_string().contains("symlink"),
            "error names the symlink: {err}"
        );
        assert!(
            !bottle.path().join("drive_c").join("secret.txt").exists(),
            "no foreign file may reach the bottle through a symlink"
        );
    }

    /// A symlinked exe (or symlinked app dir) is rejected up front — the
    /// copy would silently materialize a different tree than the user
    /// pointed at.
    #[cfg(unix)]
    #[test]
    fn symlinked_run_source_is_rejected() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new("exe-link-src");
        let real = source.path().join("real");
        std::fs::create_dir_all(&real).expect("create real dir");
        let real_exe = fake_exe(&real, "app.exe");
        let link = source.path().join("app.exe");
        symlink(&real_exe, &link).expect("symlink the exe");
        let bottle = TempDir::new("exe-link-bottle");

        let err = stage_run_source(&link, &c_volumes(bottle.path()), StageMode::ExeOnly)
            .expect_err("must reject");
        assert!(
            err.to_string().contains("symlink"),
            "error names the symlink: {err}"
        );
    }

    /// A symlinked `--app-dir` is rejected up front — the copy would silently
    /// materialize a different tree than the user pointed at. The exe is
    /// reached through the symlink, so the containment check passes lexically
    /// and the symlink rejection is what fires.
    #[cfg(unix)]
    #[test]
    fn symlinked_app_dir_is_rejected() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new("app-dir-link-src");
        let real = source.path().join("real");
        std::fs::create_dir_all(&real).expect("create real dir");
        fake_exe(&real, "app.exe");
        let link = source.path().join("app");
        symlink(&real, &link).expect("symlink the app dir");
        let bottle = TempDir::new("app-dir-link-bottle");

        let err = stage_run_source(
            &link.join("app.exe"),
            &c_volumes(bottle.path()),
            StageMode::AppDir(&link),
        )
        .expect_err("symlinked --app-dir must be rejected");
        assert!(
            err.to_string().contains("symlink"),
            "error names the symlink: {err}"
        );
    }

    /// A symlink planted at a destination path inside the bottle is rejected
    /// too — the copy would otherwise follow it and write outside the bottle.
    #[cfg(unix)]
    #[test]
    fn symlink_planted_in_dest_is_rejected() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new("dest-link-src");
        let app_dir = source.path().join("app");
        std::fs::create_dir_all(&app_dir).expect("create app dir");
        std::fs::write(app_dir.join("app.exe"), b"MZ").expect("write exe");
        let bottle = TempDir::new("dest-link-bottle");
        let dest = bottle
            .path()
            .join("drive_c")
            .join("Program Files")
            .join("app");
        std::fs::create_dir_all(&dest).expect("create dest");
        // Plant a symlinked subdir at the destination of the source's "data".
        std::fs::create_dir_all(app_dir.join("data")).expect("create source data dir");
        std::fs::write(app_dir.join("data/x.bin"), b"x").expect("write source data");
        symlink(bottle.path(), dest.join("data")).expect("plant dest link");

        let err = stage_run_source(
            &app_dir.join("app.exe"),
            &c_volumes(bottle.path()),
            StageMode::AppDir(&app_dir),
        )
        .expect_err("planted dest symlink must be rejected");
        assert!(
            err.to_string().contains("symlink"),
            "error names the planted symlink: {err}"
        );
    }

    /// End-to-end: a micro exe outside the bottle runs from a drive_c copy,
    /// the guest identity labels that copy (`C:\Program Files\…` → real
    /// bottle file), and the guest current directory is the staged app folder
    /// (`C:\Program Files\crt_hello`), not the drive root.
    #[test]
    fn run_micro_runs_outside_exe_from_bottle_copy() {
        let mut micro = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        micro.pop();
        micro.pop();
        micro.push("micro-exes/out");
        micro.push("crt_hello.exe");
        if !micro.is_file() {
            tracing::warn!("skip: micro-exes/out/crt_hello.exe not built (run make -C micro-exes)");
            return;
        }
        // Stage the exe outside the bottle, in an unrelated temp dir.
        let outside = TempDir::new("run-flow-outside");
        let src_exe = fake_exe(outside.path(), "crt_hello.exe");
        std::fs::copy(&micro, &src_exe).expect("stage exe outside the bottle");
        let bottle = TempDir::new("run-flow-bottle");

        run_micro(
            &src_exe,
            MicroRunOptions {
                max_api: 1024,
                expect_code: 0,
                bottle_root: Some(bottle.path()),
                drive_d: None,
                stdin_path: None,
                guest_args: &[],
                app_dir: None,
            },
        )
        .expect("run_micro exits 0 from the bottle copy");

        let copy = bottle
            .path()
            .join("drive_c")
            .join("Program Files")
            .join("crt_hello")
            .join("crt_hello.exe");
        assert!(copy.is_file(), "bottle copy must exist after the run");
        // The identity of the copy is the guest label, which the volume config
        // maps back to this same file. The runtime derives the module path
        // through that mapping (loader default is `C:\{name}`).
        let identity = wie_pe::process_identity_from_host_path_with_args(&copy, &[]);
        assert_eq!(identity.module_file_name, "crt_hello.exe");
        let volumes = c_volumes(bottle.path());
        assert_eq!(
            wie_winapi::host_path_to_guest(&volumes, &copy).as_deref(),
            Some(r"C:\Program Files\crt_hello\crt_hello.exe"),
            "the copy's guest module path is its Program Files location"
        );
        // The raw loader identity defaults the cwd to the drive root; the
        // session applies the staged folder's guest cwd (`stage_run_source`
        // reports it, and the session-level test pins the propagation).
        assert_eq!(identity.current_directory, r"C:\");
        let staged = stage_run_source(&src_exe, &c_volumes(bottle.path()), StageMode::ExeOnly)
            .expect("re-staging reports the cwd");
        assert_eq!(
            staged.guest_current_directory.as_deref(),
            Some(r"C:\Program Files\crt_hello"),
            "the staged app's guest current directory is its app folder"
        );
    }
}
