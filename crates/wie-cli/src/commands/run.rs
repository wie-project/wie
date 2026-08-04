//! Runtime run and smoke commands.

use super::util::write_entry_trace_summary;
use anyhow::{Context, Result, bail};
use std::io;
use std::path::Path;

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
        path,
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
    let summary = wie_runtime::run_persistent_until_yield(path, max_api)?;
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
    let _raw = TerminalRawGuard::enter();

    // The guest's frame loop is Sleep + input poll, so Sleep(n>0) must park
    // the host or the loop spins at 100% CPU. Same rationale (and SAFETY
    // comment) as `run_until_yield`: main thread, before any guest thread.
    #[expect(unsafe_code)]
    unsafe {
        std::env::set_var("WIE_IDLE", "park");
    }

    let mut session = wie_runtime::RuntimeSession::new_with_options(
        path,
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
