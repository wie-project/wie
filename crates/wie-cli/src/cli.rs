//! Command-line surface: the clap argument tree, the mode precedence, and the
//! run-entry dispatch shared by `run` and `bottle run`.
//!
//! The definitions live in the library (not in `main.rs`) so the flag
//! surface, its help text and the dispatch rules can be exercised from
//! `tests/` without going through the process boundary.

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use crate::commands;
use crate::gui;

/// Default host-API-stop cap for micro runs (freestanding PEs until
/// `ExitProcess`). Matches the runtime's own `run_micro_exe` default.
/// Raised from 256 so real guests (SDL2 games, tools) get far enough through
/// startup to reach a meaningful point without an explicit `--max-api`.
pub(crate) const MICRO_MAX_API_DEFAULT: usize = 2_000;
/// Default host-API-stop cap for the persistent run loop (yields on idle
/// instead of gating on `ExitProcess`). Real guests (SDL2 games) make
/// millions of host stops — every guest WinAPI call stops the host — so the
/// cap is generous; `--max-api` still bounds it for CI micro runs.
pub(crate) const PERSISTENT_MAX_API_DEFAULT: usize = 5_000_000;
/// Default cap for `trace` (controlled entry-point API trace). Games need
/// more than a handful of stops to reach a meaningful point.
const TRACE_MAX_API_DEFAULT: usize = 1_000;

/// The parsed `wie` command line.
#[derive(Debug, Parser)]
#[command(name = "wie")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "WIE — PE64 userspace emulator")]
#[command(long_about = "\
Generic PE64 userspace emulator CLI.\n\
\n\
Commands: inspect | run | trace | bottle.\n\
run modes: micro (default, until ExitProcess) | --persistent | --console | --gui | --screenshot.\n\
CPU backend: WIE_CPU=jit (default) | iced.\n\
Guest memory: mmap arenas only (soft translate).\n\
")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Run flags shared by both run entries (`run` and `bottle run`).
///
/// Flattened into each variant so the flag surface, defaults and help text
/// cannot drift apart. `guest_args` stays per-variant: its doc text names the
/// entry (`module name` vs `exe`) and must keep rendering each command's help
/// exactly as before.
#[derive(Args, Debug, Default)]
pub struct RunArgs {
    /// Cap host API stops (defaults: 2,000 micro; 5,000,000 persistent;
    /// 1,000,000 per quantum console).
    #[arg(long)]
    pub max_api: Option<usize>,

    /// Expected ExitProcess code (micro mode only; default 0).
    #[arg(long, default_value_t = 0)]
    pub expect_code: u32,

    /// Host root for guest `D:\…` bridge (env `WIE_DRIVE_D`; `auto` =
    /// host cwd). Micro / `--gui` / `--screenshot` entries only — rejected
    /// with `--console` / `--persistent`.
    #[arg(long)]
    pub drive_d: Option<PathBuf>,

    /// Host file whose bytes are injected as guest console stdin (micro
    /// mode only; `/dev/stdin`, `/dev/tty` or `-` read live from the
    /// terminal).
    #[arg(long)]
    pub stdin: Option<PathBuf>,

    /// Stage this complete host folder into the bottle instead of just
    /// the executable: relative paths, DLLs, plugins and data files are
    /// preserved under `C:\Program Files\<name>\`. The run source must
    /// live inside this folder. Micro / `--gui` / `--screenshot` entries
    /// only — rejected with `--console` / `--persistent`.
    #[arg(long)]
    pub app_dir: Option<PathBuf>,

    /// Persistent run loop: run the guest session as a message-driven
    /// loop that yields on idle instead of gating on `ExitProcess`. For
    /// message-loop guests (games, GUI apps). Bounded by `--max-api`.
    #[arg(long)]
    pub persistent: bool,

    /// Raw-mode interactive console run for terminal games: every
    /// keystroke reaches the guest immediately (no Enter), terminal
    /// restored on exit. Runs until the guest exits.
    #[arg(long)]
    pub console: bool,

    /// Native windowed GUI run: guest windows render in a macOS window
    /// (winit + wgpu/Metal), the loop yields on idle, and guest menu bar
    /// and dialogs are bridged to native UI.
    #[arg(long)]
    pub gui: bool,

    /// Headless GUI run: render the guest without a window and write the
    /// first captured frame to this BMP file.
    #[arg(long)]
    pub screenshot: Option<PathBuf>,

    /// Drive a `--gui` guest with a scripted input file (lines: sleep
    /// <ms> | key <vk> [shift|ctrl] | type <text> | menu <id> | click
    /// <x> <y> | snapshot <file>). Requires --gui. The `WIE_INPUT_SCRIPT`
    /// env var names a script too.
    #[arg(long)]
    pub input_script: Option<PathBuf>,
}

/// The `wie` subcommand tree.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// PE static inspection (metadata, sections, imports, image, WinAPI map).
    Inspect {
        path: PathBuf,

        /// List PE sections.
        #[arg(long)]
        sections: bool,

        /// List import address table entries.
        #[arg(long)]
        imports: bool,

        /// Filter imports by substring (implies `--imports`).
        #[arg(long)]
        find: Option<String>,

        /// Print loaded-image summary (Windows-loader-like).
        #[arg(long)]
        image: bool,

        /// Print WinAPI import coverage map.
        #[arg(long)]
        winapi_map: bool,

        /// Write WinAPI map to this path instead of stdout (implies `--winapi-map`).
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Run a PE: micro (default) until ExitProcess, or `--persistent` /
    /// `--console` for message-driven or raw-terminal interactive runs.
    #[command(alias = "run-micro")]
    Run {
        path: PathBuf,

        /// Bottle override: guest `C:\…` maps to `{root}/drive_c/…` (env
        /// `WIE_ROOT`; default: per-user app-data bottle
        /// `~/Library/Application Support/WIE/bottle`, created on demand).
        /// Micro / `--gui` / `--screenshot` entries only — rejected with
        /// `--console` / `--persistent` (use `--bottle` there).
        #[arg(long)]
        root: Option<PathBuf>,

        /// Named bottle to run from: guest `C:\…` maps to
        /// `{WIE/bottles}/{NAME}/drive_c/…` (mutually exclusive with `--root`;
        /// the only root form `--console` / `--persistent` accept). With a
        /// named bottle the exe argument may also be a basename or relative
        /// path resolved inside the bottle's `drive_c` (unique match
        /// required).
        #[arg(long, conflicts_with = "root")]
        bottle: Option<String>,

        /// Run-mode flags shared with `bottle run`.
        #[command(flatten)]
        args: RunArgs,

        /// Guest argv after the module name: everything after `--` passes
        /// verbatim (`wie run app.exe -- -n 3 -m hi`). Micro and `--gui`
        /// entries accept it; `--console` / `--persistent` reject it.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        guest_args: Vec<String>,
    },

    /// Controlled entry-point API trace (first N host stops).
    #[command(alias = "entry-trace")]
    Trace {
        path: PathBuf,
        #[arg(long, default_value_t = TRACE_MAX_API_DEFAULT)]
        max_api: usize,
    },

    /// Named-bottle management (create/list/info/delete/add/run).
    Bottle {
        #[command(subcommand)]
        command: BottleCommand,
    },
}

/// Named-bottle management (`Bottle` subcommand surface).
#[derive(Debug, Subcommand)]
pub enum BottleCommand {
    /// Create a new named bottle (guest `C:\` root under `WIE/bottles/<name>`).
    Create {
        /// Bottle name (a directory name under the bottles dir).
        name: String,
    },

    /// List all named bottles.
    List,

    /// Show a bottle's host path, drive layout and size.
    Info { name: String },

    /// Print a bottle's host root path (for `run --root` scripting).
    Path { name: String },

    /// Delete a bottle and its contents.
    Delete {
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },

    /// Copy a host file or folder into a bottle's `drive_c`.
    Add {
        name: String,
        /// Host path to copy (file or directory, copied recursively).
        host_path: PathBuf,
        /// Guest destination under `C:\`, e.g. `C:\Apps\Foo` (default: `C:\<basename>`).
        #[arg(long)]
        target: Option<String>,
    },

    /// Run a guest exe inside a bottle (delegates to `run --bottle`).
    Run {
        name: String,
        /// Exe to run: a full guest path (`C:\App\app.exe`), an existing
        /// host path, or a basename / relative path resolved inside the
        /// bottle's `drive_c` (unique match required).
        exe: String,

        /// Run-mode flags shared with `run`.
        #[command(flatten)]
        args: RunArgs,

        /// Guest argv after the exe: everything after `--` passes verbatim.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        guest_args: Vec<String>,
    },
}

/// Reject the micro-mode-only flags (`--root` / `--drive-d` / `--stdin` /
/// `--app-dir` / `--expect-code` / guest argv) on the non-micro run entries.
/// `mode` names the entry for the argv error message
/// (`--console` / `--persistent`).
pub fn reject_micro_only_flags(
    mode: &str,
    root: &Option<PathBuf>,
    stdin: &Option<PathBuf>,
    drive_d: &Option<PathBuf>,
    app_dir: &Option<PathBuf>,
    expect_code: u32,
    guest_args: &[String],
) -> Result<()> {
    if root.is_some() || stdin.is_some() || drive_d.is_some() || app_dir.is_some() {
        bail!("--root / --drive-d / --stdin / --app-dir are only supported in micro mode");
    }
    if expect_code != 0 {
        bail!("--expect-code is only supported in micro mode");
    }
    if !guest_args.is_empty() {
        bail!("guest argv is only supported in micro mode (omit {mode})");
    }
    Ok(())
}

/// Shared run-entry dispatch, used by both `run` (via `--bottle` / `--root`)
/// and `bottle run <name>`.
///
/// `path` is the already-resolved run source — an in-bottle host exe when the
/// entry named a bottle, a plain host path otherwise. `root` is the effective
/// bottle root (the resolved bottle / `--root`, or `None` for the global
/// default). `root_flag` is the raw `--root` flag, kept separate so the
/// console/persistent entries can still reject it while permitting the
/// bottle-derived root.
///
/// Applies the mode precedence GUI → screenshot → console/persistent/micro
/// and the micro-only flag rejections, then dispatches to the runtime. A
/// single source of truth so `bottle run` behaves identically to
/// `run --bottle`.
pub fn run_entry(
    path: &std::path::Path,
    root: Option<PathBuf>,
    root_flag: Option<PathBuf>,
    args: RunArgs,
    guest_args: Vec<String>,
) -> Result<()> {
    let RunArgs {
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
    } = args;
    // Profiling Ctrl+C (`WIE_RUNTIME_PROFILE` armed): install the signal
    // hooks BEFORE anything runs. Lazy installation happens only when a
    // console session enters cbreak mode, which micro and `--gui` runs never
    // do — the default SIGINT disposition would kill the process there with
    // no chance to dump the profile report. Gate-off cost: one cached-bool
    // load; behavior is untouched when profiling is off.
    if wie_winapi::console::profile_sigint_armed() {
        wie_winapi::console::ensure_hooks_installed();
    }

    // GUI/screenshot mode takes precedence over persistent/micro.
    if gui || screenshot.is_some() {
        if gui {
            if let Some(script) = input_script.as_ref()
                && !script.is_file()
            {
                bail!("input script not found: {}", script.display());
            }
            // The winit/wgpu presenter is macOS-only (`gui::app` is gated in
            // `gui/mod.rs`, with winit/wgpu behind the same `cfg` in
            // Cargo.toml). Non-macOS builds keep the flag so the surface and
            // its help text do not move, but reject it with an explanation
            // instead of failing to link or reporting an unknown flag.
            #[cfg(target_os = "macos")]
            {
                let script = gui::input_script::script_path(input_script.as_deref());
                return gui::app::run_gui_windowed(
                    path,
                    script,
                    root.as_deref(),
                    drive_d.as_deref(),
                    app_dir.as_deref(),
                    &guest_args,
                );
            }
            #[cfg(not(target_os = "macos"))]
            bail!(
                "--gui needs the macOS window presenter (winit + wgpu/Metal) and is \
                 not built for this target; use --screenshot <file.bmp> for a \
                 headless render, or --console / --persistent for a message-loop run"
            );
        }
        if let Some(out_path) = screenshot {
            return gui::headless::run_screenshot(
                path,
                &out_path,
                root.as_deref(),
                drive_d.as_deref(),
                app_dir.as_deref(),
                &guest_args,
            );
        }
    }
    if input_script.is_some() {
        bail!("--input-script requires --gui");
    }

    if console {
        if persistent {
            bail!("--console and --persistent are mutually exclusive");
        }
        reject_micro_only_flags(
            "--console",
            &root_flag,
            &stdin,
            &drive_d,
            &app_dir,
            expect_code,
            &guest_args,
        )?;
        commands::run_console_interactive(path, max_api, root.as_deref())?;
    } else if persistent {
        reject_micro_only_flags(
            "--persistent",
            &root_flag,
            &stdin,
            &drive_d,
            &app_dir,
            expect_code,
            &guest_args,
        )?;
        let max = max_api.unwrap_or(PERSISTENT_MAX_API_DEFAULT);
        commands::run_until_yield(path, max, root.as_deref())?;
    } else {
        let max = max_api.unwrap_or(MICRO_MAX_API_DEFAULT);
        commands::run_micro(
            path,
            commands::MicroRunOptions {
                max_api: max,
                expect_code,
                bottle_root: root.as_deref(),
                drive_d: drive_d.as_deref(),
                stdin_path: stdin.as_deref(),
                guest_args: &guest_args,
                app_dir: app_dir.as_deref(),
            },
        )?;
    }
    Ok(())
}

/// Effective bottle root for a run entry.
///
/// `--bottle <name>` resolves through the named-bottle helpers to the bottle's
/// host root; an explicit `--root` passes through unchanged; `None` leaves the
/// `WIE_ROOT` env fallback in place (applied later by
/// [`commands::resolve_volume_config`]). Both flags together are a conflict —
/// clap enforces it at parse time, and the guard also covers direct
/// construction in tests.
pub fn resolve_run_root(bottle: Option<String>, root: Option<PathBuf>) -> Result<Option<PathBuf>> {
    match (bottle, root) {
        (Some(_), Some(_)) => bail!("--bottle and --root are mutually exclusive"),
        (Some(name), None) => Ok(Some(commands::resolve_bottle_root(&name)?)),
        (None, root) => Ok(root),
    }
}

/// Dispatch one parsed subcommand to its [`commands`] implementation.
///
/// `run` resolves its bottle root and in-bottle run source here (before mode
/// dispatch, so micro / gui / screenshot / console / persistent all run the
/// resolved exe) and then defers to [`run_entry`].
pub fn run_command(command: Command) -> Result<()> {
    match command {
        Command::Inspect {
            path,
            sections,
            imports,
            find,
            image,
            winapi_map,
            out,
        } => {
            let want_imports = imports || find.is_some();
            let want_winapi = winapi_map || out.is_some();
            let any_detail = sections || want_imports || image || want_winapi;
            if !any_detail {
                commands::inspect(&path)?;
            } else {
                // Always print core metadata first when any detail flag is set.
                commands::inspect(&path)?;
                if sections {
                    println!();
                    commands::sections(&path)?;
                }
                if want_imports {
                    println!();
                    commands::imports(&path, find.as_deref())?;
                }
                if image {
                    println!();
                    commands::image(&path)?;
                }
                if want_winapi {
                    println!();
                    commands::winapi_map(&path, out.as_deref())?;
                }
            }
        }
        Command::Run {
            path,
            root,
            bottle,
            args,
            guest_args,
        } => {
            // The raw `--root` flag stays separate from the effective root:
            // the micro-only rejection in console/persistent mode checks the
            // flag, while the bottle-derived root is allowed there.
            let root_flag = root;
            let bottle_named = bottle.is_some();
            let root = resolve_run_root(bottle, root_flag.clone())?;
            // Named-bottle run source: with `--bottle <name>` the path
            // argument may name an exe inside the bottle (basename or
            // drive_c-relative path) instead of a host file. Resolution
            // happens before mode dispatch so every entry (micro / gui /
            // screenshot / console / persistent) runs the resolved in-bottle
            // exe. An explicit `--root` never searches — only the selected
            // bottle is consulted (see commands::resolve_bottle_exe).
            let path = match (bottle_named, root.as_deref()) {
                (true, Some(root)) => commands::resolve_bottle_exe(root, &path)?,
                _ => path,
            };

            run_entry(&path, root, root_flag, args, guest_args)?;
        }
        Command::Trace { path, max_api } => {
            commands::entry_trace(&path, max_api)?;
        }
        Command::Bottle { command } => commands::bottle(command)?,
    }
    Ok(())
}
