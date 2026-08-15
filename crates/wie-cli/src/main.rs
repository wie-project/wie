//! `wie` — WIE PE64 userspace emulator CLI.

mod bmp;
mod commands;
mod gui;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Default host-API-stop cap for micro runs (freestanding PEs until
/// `ExitProcess`). Matches the runtime's own `run_micro_exe` default.
/// Raised from 256 so real guests (SDL2 games, tools) get far enough through
/// startup to reach a meaningful point without an explicit `--max-api`.
const MICRO_MAX_API_DEFAULT: usize = 2_000;
/// Default host-API-stop cap for the persistent run loop (yields on idle
/// instead of gating on `ExitProcess`). Real guests (SDL2 games) make
/// millions of host stops — every guest WinAPI call stops the host — so the
/// cap is generous; `--max-api` still bounds it for CI micro runs.
const PERSISTENT_MAX_API_DEFAULT: usize = 5_000_000;
/// Default cap for `trace` (controlled entry-point API trace). Games need
/// more than a handful of stops to reach a meaningful point.
const TRACE_MAX_API_DEFAULT: usize = 1_000;

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

    /// Run a PE: micro (default) until ExitProcess, or `--persistent` /
    /// `--console` for message-driven or raw-terminal interactive runs.
    #[command(alias = "run-micro")]
    Run {
        path: PathBuf,

        /// Cap host API stops (defaults: 2,000 micro; 5,000,000 persistent;
        /// 1,000,000 per quantum console).
        #[arg(long)]
        max_api: Option<usize>,

        /// Expected ExitProcess code (micro mode only; default 0).
        #[arg(long, default_value_t = 0)]
        expect_code: u32,

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

        /// Host root for guest `D:\…` bridge (env `WIE_DRIVE_D`; `auto` =
        /// host cwd). Micro / `--gui` / `--screenshot` entries only — rejected
        /// with `--console` / `--persistent`.
        #[arg(long)]
        drive_d: Option<PathBuf>,

        /// Host file whose bytes are injected as guest console stdin (micro
        /// mode only; `/dev/stdin`, `/dev/tty` or `-` read live from the
        /// terminal).
        #[arg(long)]
        stdin: Option<PathBuf>,

        /// Stage this complete host folder into the bottle instead of just
        /// the executable: relative paths, DLLs, plugins and data files are
        /// preserved under `C:\Program Files\<name>\`. The run source must
        /// live inside this folder. Micro / `--gui` / `--screenshot` entries
        /// only — rejected with `--console` / `--persistent`.
        #[arg(long)]
        app_dir: Option<PathBuf>,

        /// Persistent run loop: run the guest session as a message-driven
        /// loop that yields on idle instead of gating on `ExitProcess`. For
        /// message-loop guests (games, GUI apps). Bounded by `--max-api`.
        #[arg(long)]
        persistent: bool,

        /// Raw-mode interactive console run for terminal games: every
        /// keystroke reaches the guest immediately (no Enter), terminal
        /// restored on exit. Runs until the guest exits.
        #[arg(long)]
        console: bool,

        /// Native windowed GUI run: guest windows render in a macOS window
        /// (winit + wgpu/Metal), the loop yields on idle, and guest menu bar
        /// and dialogs are bridged to native UI.
        #[arg(long)]
        gui: bool,

        /// Headless GUI run: render the guest without a window and write the
        /// first captured frame to this BMP file.
        #[arg(long)]
        screenshot: Option<PathBuf>,

        /// Drive a `--gui` guest with a scripted input file (lines: sleep
        /// <ms> | key <vk> [shift|ctrl] | type <text> | menu <id> | click
        /// <x> <y> | snapshot <file>). Requires --gui. The `WIE_INPUT_SCRIPT`
        /// env var names a script too.
        #[arg(long)]
        input_script: Option<PathBuf>,

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
pub(crate) enum BottleCommand {
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

    /// Run a guest exe inside a bottle (delegates to `run --root`).
    Run {
        name: String,
        /// Exe to run: a full guest path (`C:\App\app.exe`), an existing
        /// host path, or a basename / relative path resolved inside the
        /// bottle's `drive_c` (unique match required).
        exe: String,
        /// Guest argv after the exe.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        guest_args: Vec<String>,
    },
}

/// Reject the micro-mode-only flags (`--root` / `--drive-d` / `--stdin` /
/// `--app-dir` / `--expect-code` / guest argv) on the non-micro run entries.
/// `mode` names the entry for the argv error message
/// (`--console` / `--persistent`).
fn reject_micro_only_flags(
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

/// Effective bottle root for a run entry.
///
/// `--bottle <name>` resolves through the named-bottle helpers to the bottle's
/// host root; an explicit `--root` passes through unchanged; `None` leaves the
/// `WIE_ROOT` env fallback in place (applied later by
/// [`commands::resolve_volume_config`]). Both flags together are a conflict —
/// clap enforces it at parse time, and the guard also covers direct
/// construction in tests.
fn resolve_run_root(bottle: Option<String>, root: Option<PathBuf>) -> Result<Option<PathBuf>> {
    match (bottle, root) {
        (Some(_), Some(_)) => bail!("--bottle and --root are mutually exclusive"),
        (Some(name), None) => Ok(Some(commands::resolve_bottle_root(&name)?)),
        (None, root) => Ok(root),
    }
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
            bottle,
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
                        app_dir.as_deref(),
                        &guest_args,
                    );
                }
                if let Some(out_path) = screenshot {
                    return gui::headless::run_screenshot(
                        &path,
                        &out_path,
                        root.as_deref(),
                        drive_d.as_deref(),
                        app_dir.as_deref(),
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
                commands::run_console_interactive(&path, max_api, root.as_deref())?;
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
                commands::run_until_yield(&path, max, root.as_deref())?;
            } else {
                let max = max_api.unwrap_or(MICRO_MAX_API_DEFAULT);
                commands::run_micro(
                    &path,
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
        }
        Command::Trace { path, max_api } => {
            commands::entry_trace(&path, max_api)?;
        }
        Command::Bottle { command } => commands::bottle(command)?,
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Parse `wie run app.exe <args>` and return the parsed `Command`.
    fn parse_run(args: &[&str]) -> Command {
        let mut argv = vec!["wie", "run", "app.exe"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).expect("parse run argv").command
    }

    #[test]
    fn bottle_flag_parses_with_name() {
        assert!(matches!(
            parse_run(&["--bottle", "games"]),
            Command::Run {
                bottle: Some(ref name),
                root: None,
                ..
            } if name.as_str() == "games"
        ));
    }

    /// `--app-dir` parses on the micro entry and reaches the run command
    /// (the flag value survives the clap round-trip).
    #[test]
    fn app_dir_flag_parses_with_path() {
        assert!(matches!(
            parse_run(&["--app-dir", "/tmp/MyApp"]),
            Command::Run {
                app_dir: Some(ref dir),
                ..
            } if dir == std::path::Path::new("/tmp/MyApp")
        ));
    }

    /// `--app-dir` combines with GUI and screenshot modes (the same entries
    /// that accept `--root` / `--drive-d`).
    #[test]
    fn app_dir_combines_with_gui_and_screenshot_modes() {
        assert!(matches!(
            parse_run(&["--gui", "--app-dir", "/tmp/MyApp"]),
            Command::Run {
                gui: true,
                app_dir: Some(ref dir),
                ..
            } if dir == std::path::Path::new("/tmp/MyApp")
        ));
        assert!(matches!(
            parse_run(&["--screenshot", "out.bmp", "--app-dir", "/tmp/MyApp"]),
            Command::Run {
                screenshot: Some(ref out),
                app_dir: Some(ref dir),
                ..
            } if out == std::path::Path::new("out.bmp")
                && dir == std::path::Path::new("/tmp/MyApp")
        ));
    }

    /// `--console` / `--persistent` reject `--app-dir` at runtime (they keep
    /// the legacy parent-folder staging), while the flags still parse.
    #[test]
    fn app_dir_is_rejected_for_console_and_persistent() {
        let app_dir = Some(PathBuf::from("/tmp/MyApp"));
        for mode in ["--console", "--persistent"] {
            let err = reject_micro_only_flags(mode, &None, &None, &None, &app_dir, 0, &[])
                .expect_err("--app-dir must be rejected");
            assert!(
                err.to_string().contains("--app-dir"),
                "error names the rejected flag: {err}"
            );
        }
        // It still parses — the rejection happens in `main`, not clap (the
        // same contract as `--root` on these entries).
        assert!(matches!(
            parse_run(&["--console", "--app-dir", "/tmp/MyApp"]),
            Command::Run {
                console: true,
                app_dir: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn bottle_and_root_conflict_at_parse() {
        let err = Cli::try_parse_from([
            "wie",
            "run",
            "app.exe",
            "--bottle",
            "games",
            "--root",
            "/tmp/bottle",
        ])
        .expect_err("--bottle and --root must conflict at parse time");
        assert!(
            err.to_string().contains("cannot be used with"),
            "clap must report the conflict: {err}"
        );
    }

    #[test]
    fn bottle_combines_with_run_modes() {
        // Console: `--bottle` is captured — the resolved root later threads
        // into run_console_interactive (the `--root` flag itself stays
        // rejected there).
        assert!(matches!(
            parse_run(&["--console", "--bottle", "games"]),
            Command::Run {
                console: true,
                bottle: Some(ref name),
                root: None,
                ..
            } if name.as_str() == "games"
        ));
        // Screenshot mode.
        assert!(matches!(
            parse_run(&["--screenshot", "out.bmp", "--bottle", "games"]),
            Command::Run {
                screenshot: Some(ref out),
                bottle: Some(ref name),
                ..
            } if out == std::path::Path::new("out.bmp") && name.as_str() == "games"
        ));
        // GUI mode.
        assert!(matches!(
            parse_run(&["--gui", "--bottle", "games"]),
            Command::Run {
                gui: true,
                bottle: Some(ref name),
                ..
            } if name.as_str() == "games"
        ));
    }

    #[test]
    fn resolve_run_root_conflict_and_passthrough() {
        // Both flags set: rejected even on direct construction (clap blocks
        // it at parse time; this guards tests and future callers).
        let err = resolve_run_root(Some("games".to_owned()), Some(PathBuf::from("/tmp/r")))
            .expect_err("--bottle and --root must be rejected together");
        assert!(
            err.to_string().contains("mutually exclusive"),
            "error names the conflict: {err}"
        );
        // An explicit `--root` passes through unchanged.
        let root = PathBuf::from("/tmp/r");
        assert_eq!(
            resolve_run_root(None, Some(root.clone())).expect("root passthrough"),
            Some(root)
        );
        // Neither flag: `None`, leaving the `WIE_ROOT` env fallback in place.
        assert_eq!(resolve_run_root(None, None).expect("no flags"), None);
    }

    #[test]
    fn resolve_run_root_resolves_named_bottle() {
        // Round-trip through the real named-bottle dispatcher: create,
        // resolve `--bottle <name>` to the bottle's host root, then delete.
        // A PID-unique name keeps parallel test runs disjoint.
        let name = format!("run-root-{}", std::process::id());
        commands::bottle(BottleCommand::Create { name: name.clone() }).expect("create bottle");
        let root = resolve_run_root(Some(name.clone()), None)
            .expect("resolve --bottle")
            .expect("resolved root is present");
        assert!(root.join("drive_c").is_dir(), "bottle root has drive_c");
        commands::bottle(BottleCommand::Delete { name, yes: true }).expect("delete bottle");
    }

    #[test]
    fn resolve_run_root_rejects_missing_bottle() {
        let err = resolve_run_root(Some("no-such-bottle".to_owned()), None)
            .expect_err("missing bottle must fail");
        assert!(
            err.to_string().contains("does not exist"),
            "error names the missing bottle: {err}"
        );
    }

    /// The `run` help must document current behavior: no stale `gui` feature
    /// claim, and flag semantics matching what `main` actually enforces.
    #[test]
    fn run_help_documents_current_flag_semantics() {
        use clap::CommandFactory;
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("run")
            .expect("run subcommand exists")
            .render_long_help()
            .to_string();
        // The GUI entry is compiled in unconditionally — no feature gate.
        assert!(
            !help.contains("gui` feature"),
            "stale gui feature claim: {help}"
        );
        assert!(
            help.contains("Native windowed GUI"),
            "describes the windowed GUI: {help}"
        );
        // Persistent and console are distinct entries, both documented.
        assert!(
            help.contains("message-driven"),
            "documents the persistent loop: {help}"
        );
        assert!(
            help.contains("no Enter"),
            "documents raw console input: {help}"
        );
        // Screenshot output and every input-script step.
        assert!(
            help.contains("BMP"),
            "documents the screenshot output: {help}"
        );
        for step in ["sleep", "key", "type", "menu", "click", "snapshot"] {
            assert!(
                help.contains(step),
                "input script documents the {step} step: {help}"
            );
        }
        // Volume flags: env fallbacks, bottle exclusivity, mode scoping.
        assert!(
            help.contains("WIE_ROOT"),
            "documents the WIE_ROOT env: {help}"
        );
        assert!(
            help.contains("WIE_DRIVE_D"),
            "documents the WIE_DRIVE_D env: {help}"
        );
        assert!(
            help.contains("mutually exclusive"),
            "documents the root/bottle conflict: {help}"
        );
        // Explicit application-directory staging and the exe-only default.
        assert!(
            help.contains("--app-dir"),
            "documents the --app-dir flag: {help}"
        );
        assert!(
            help.contains("complete host folder"),
            "documents --app-dir folder staging: {help}"
        );
        assert!(
            help.contains("instead of just the executable"),
            "documents the exe-only default: {help}"
        );
        // Guest argv after `--`.
        assert!(
            help.contains("-- -n 3 -m hi"),
            "documents guest argv after --: {help}"
        );
    }

    /// Everything after `--` is guest argv, verbatim (leading hyphens kept).
    #[test]
    fn guest_args_after_double_dash_pass_verbatim() {
        assert!(matches!(
            parse_run(&["--", "-n", "3", "-m", "hi"]),
            Command::Run { guest_args, .. } if guest_args == ["-n", "3", "-m", "hi"]
        ));
    }

    /// Flags before `--`, guest argv after it — the micro invocation shape.
    #[test]
    fn micro_flags_then_guest_args_parse() {
        assert!(matches!(
            parse_run(&["--root", "/tmp/b", "--max-api", "1000", "--", "-n", "3"]),
            Command::Run { guest_args, .. } if guest_args == ["-n", "3"]
        ));
    }

    /// The named-bottle shape: `--bottle <name>` names the bottle while
    /// everything after `--` is guest argv verbatim (the
    /// `run --bottle games app.exe -- -n 3` contract).
    #[test]
    fn named_bottle_guest_args_parse() {
        assert!(matches!(
            parse_run(&["--bottle", "games", "--", "-n", "3", r"C:\DOOM2.WAD"]),
            Command::Run {
                bottle: Some(ref name),
                guest_args,
                ..
            } if name.as_str() == "games"
                && guest_args == ["-n", "3", r"C:\DOOM2.WAD"]
        ));
        // The `bottle run <name> <exe> -- <args>` shape parses the same way.
        assert!(matches!(
            Cli::try_parse_from([
                "wie",
                "bottle",
                "run",
                "doomretro",
                "doomretro.exe",
                "--",
                r"C:\DOOM2.WAD",
            ])
            .expect("bottle run argv parses")
            .command,
            Command::Bottle {
                command: BottleCommand::Run { name, exe, guest_args },
            } if name == "doomretro"
                && exe == "doomretro.exe"
                && guest_args == [r"C:\DOOM2.WAD"]
        ));
    }

    /// `--console` + `--persistent` and `--gui` + `--persistent` all parse:
    /// the mutual exclusion and GUI precedence are enforced in `main`, not
    /// clap — this pins that contract.
    #[test]
    fn run_mode_conflicts_are_runtime_not_parse() {
        assert!(matches!(
            parse_run(&["--console", "--persistent"]),
            Command::Run {
                console: true,
                persistent: true,
                ..
            }
        ));
        assert!(matches!(
            parse_run(&["--gui", "--persistent"]),
            Command::Run {
                gui: true,
                persistent: true,
                ..
            }
        ));
    }
}
