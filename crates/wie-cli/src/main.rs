//! `wie` — WIE PE64 userspace emulator CLI.

mod bmp;
mod commands;
mod gui;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Default host-API-stop cap for micro runs (freestanding PEs until
/// `ExitProcess`). Matches the runtime's own `run_micro_exe` default.
const MICRO_MAX_API_DEFAULT: usize = 256;
/// Default host-API-stop cap for the persistent run loop (yields on idle
/// instead of gating on `ExitProcess`).
const PERSISTENT_MAX_API_DEFAULT: usize = 3400;
/// Default cap for `trace` (controlled entry-point API trace).
const TRACE_MAX_API_DEFAULT: usize = 20;

#[derive(Debug, Parser)]
#[command(name = "wie")]
#[command(about = "WIE — PE64 userspace emulator")]
#[command(long_about = "\
Generic PE64 userspace emulator CLI.\n\
\n\
Fundamental commands: inspect | run | trace.\n\
CPU backend: WIE_CPU=jit (default) | iced.\n\
Guest memory: mmap arenas only (soft translate).\
")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// PE static inspection (metadata, sections, imports, image, WinAPI map).
    Inspect {
        path: PathBuf,

        /// List PE sections.
        #[arg(long)]
        sections: bool,

        /// List import address table entries.
        #[arg(long)]
        imports: bool,

        /// Filter imports by substring (implies --imports).
        #[arg(long)]
        find: Option<String>,

        /// Print loaded-image summary (Windows-loader-like).
        #[arg(long)]
        image: bool,

        /// Print WinAPI import coverage map.
        #[arg(long)]
        winapi_map: bool,

        /// Write WinAPI map to this path instead of stdout (implies --winapi-map).
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Run a PE until ExitProcess (micro gate) or until the persistent loop yields.
    #[command(alias = "run-micro")]
    Run {
        path: PathBuf,

        /// Cap host API stops (micro default `MICRO_MAX_API_DEFAULT`; persistent
        /// default `PERSISTENT_MAX_API_DEFAULT`).
        #[arg(long)]
        max_api: Option<usize>,

        /// Expected ExitProcess code (micro mode only; default 0).
        #[arg(long, default_value_t = 0)]
        expect_code: u32,

        /// Optional bottle override: guest `C:\…` maps to `{root}/drive_c/…`
        /// (also `WIE_ROOT`). Default: per-user app-data bottle
        /// (`~/Library/Application Support/WIE/bottle`), created on demand.
        #[arg(long)]
        root: Option<PathBuf>,

        /// Host root for guest `D:\…` bridge (also `WIE_DRIVE_D`; use `auto` for host cwd).
        #[arg(long)]
        drive_d: Option<PathBuf>,

        /// Host file whose bytes are injected as guest console stdin.
        #[arg(long)]
        stdin: Option<PathBuf>,

        /// Persistent run loop (old `run`): yield on idle instead of ExitProcess gate.
        #[arg(long)]
        persistent: bool,

        /// Raw-mode interactive console run: every keystroke reaches the guest
        /// immediately (no Enter), terminal restored on exit. For terminal games.
        #[arg(long)]
        console: bool,

        /// Show GUI window (requires `gui` feature).
        #[arg(long)]
        gui: bool,

        /// Write screenshot to this file instead of showing a window.
        #[arg(long)]
        screenshot: Option<PathBuf>,

        /// Drive the GUI guest with a scripted input file (lines: sleep <ms>
        /// | key <vk> [shift|ctrl] | type <text> | menu <id>). Requires
        /// --gui. The `WIE_INPUT_SCRIPT` env var names a script too.
        #[arg(long)]
        input_script: Option<PathBuf>,

        /// Guest argv after the module name (`wie run pe -- -n 3 -m hi`).
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
}

fn main() -> Result<()> {
    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_owned());
    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init()
    {
        bail!("failed to initialize tracing subscriber: {error}");
    }

    let cli = Cli::parse();

    match cli.command {
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
            max_api,
            expect_code,
            root,
            drive_d,
            stdin,
            persistent,
            console,
            gui,
            screenshot,
            input_script,
            guest_args,
        } => {
            // GUI/screenshot mode takes precedence over persistent/micro.
            if gui || screenshot.is_some() {
                if gui {
                    if let Some(script) = input_script.as_ref()
                        && !script.is_file()
                    {
                        bail!("input script not found: {}", script.display());
                    }
                    let script = gui::input_script::script_path(input_script.as_deref());
                    return gui::app::run_gui_windowed(
                        &path,
                        script,
                        root.as_deref(),
                        drive_d.as_deref(),
                    );
                }
                if let Some(out_path) = screenshot {
                    return gui::headless::run_screenshot(&path, &out_path);
                }
            }
            if input_script.is_some() {
                bail!("--input-script requires --gui");
            }

            if console {
                if persistent {
                    bail!("--console and --persistent are mutually exclusive");
                }
                if root.is_some() || stdin.is_some() || drive_d.is_some() {
                    bail!("--root / --drive-d / --stdin are only supported in micro mode");
                }
                if expect_code != 0 {
                    bail!("--expect-code is only supported in micro mode");
                }
                if !guest_args.is_empty() {
                    bail!("guest argv is only supported in micro mode (omit --console)");
                }
                commands::run_console_interactive(&path, max_api)?;
            } else if persistent {
                let max = max_api.unwrap_or(PERSISTENT_MAX_API_DEFAULT);
                if !guest_args.is_empty() {
                    bail!("guest argv is only supported in micro mode (omit --persistent)");
                }
                if root.is_some() || stdin.is_some() || drive_d.is_some() {
                    bail!("--root / --drive-d / --stdin are only supported in micro mode");
                }
                if expect_code != 0 {
                    bail!("--expect-code is only supported in micro mode");
                }
                commands::run_until_yield(&path, max)?;
            } else {
                let max = max_api.unwrap_or(MICRO_MAX_API_DEFAULT);
                commands::run_micro(
                    &path,
                    max,
                    expect_code,
                    root.as_deref(),
                    drive_d.as_deref(),
                    stdin.as_deref(),
                    &guest_args,
                )?;
            }
        }
        Command::Trace { path, max_api } => {
            commands::entry_trace(&path, max_api)?;
        }
    }

    Ok(())
}
