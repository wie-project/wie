//! `wie` — WIE PE64 userspace emulator CLI.
//!
//! Thin shim: initialize tracing, parse argv, hand the parsed subcommand to
//! [`wie_cli::run_command`]. The argument tree and the dispatch rules live in
//! the library (`wie_cli::cli`), so they are reachable from `tests/` — the
//! tests below drive them through that public surface.

use anyhow::{Result, bail};
use clap::Parser;
use wie_cli::{Cli, run_command};

fn main() -> Result<()> {
    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_owned());
    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init()
    {
        bail!("failed to initialize tracing subscriber: {error}");
    }

    run_command(Cli::parse().command)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use wie_cli::{
        BottleCommand, Command, RunArgs, commands, reject_micro_only_flags, resolve_run_root,
        run_entry,
    };

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
                args: RunArgs {
                    app_dir: Some(ref dir),
                    ..
                },
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
                args: RunArgs {
                    gui: true,
                    app_dir: Some(ref dir),
                    ..
                },
                ..
            } if dir == std::path::Path::new("/tmp/MyApp")
        ));
        assert!(matches!(
            parse_run(&["--screenshot", "out.bmp", "--app-dir", "/tmp/MyApp"]),
            Command::Run {
                args:
                    RunArgs {
                        screenshot: Some(ref out),
                        app_dir: Some(ref dir),
                        ..
                    },
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
        // It still parses — the rejection happens in `run_command`, not clap
        // (the same contract as `--root` on these entries).
        assert!(matches!(
            parse_run(&["--console", "--app-dir", "/tmp/MyApp"]),
            Command::Run {
                args: RunArgs {
                    console: true,
                    app_dir: Some(_),
                    ..
                },
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
                args: RunArgs { console: true, .. },
                bottle: Some(ref name),
                root: None,
                ..
            } if name.as_str() == "games"
        ));
        // Screenshot mode.
        assert!(matches!(
            parse_run(&["--screenshot", "out.bmp", "--bottle", "games"]),
            Command::Run {
                args: RunArgs {
                    screenshot: Some(ref out),
                    ..
                },
                bottle: Some(ref name),
                ..
            } if out == std::path::Path::new("out.bmp") && name.as_str() == "games"
        ));
        // GUI mode.
        assert!(matches!(
            parse_run(&["--gui", "--bottle", "games"]),
            Command::Run {
                args: RunArgs { gui: true, .. },
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
                command: BottleCommand::Run { name, exe, guest_args, .. },
            } if name == "doomretro"
                && exe == "doomretro.exe"
                && guest_args == [r"C:\DOOM2.WAD"]
        ));
    }

    /// `--screenshot` with guest argv after `--` parses: the path becomes the
    /// screenshot output while the hyphens stay guest argv (`-iwad` etc.), so a
    /// headless screenshot can drive apps that need argv (e.g. Doom Retro's
    /// IWAD selection).
    #[test]
    fn screenshot_with_guest_args_parse() {
        assert!(matches!(
            parse_run(&[
                "--bottle",
                "doomretro",
                "--screenshot",
                "/tmp/doom.bmp",
                "--",
                "-iwad",
                "freedoom1.wad",
            ]),
            Command::Run {
                args: RunArgs {
                    screenshot: Some(ref out),
                    ..
                },
                bottle: Some(ref name),
                guest_args,
                ..
            } if name.as_str() == "doomretro"
                && out == std::path::Path::new("/tmp/doom.bmp")
                && guest_args == ["-iwad", "freedoom1.wad"]
        ));
    }

    /// Parse `wie bottle run <name> <exe> <args>` and return the parsed
    /// `BottleCommand`.
    fn parse_bottle_run(name: &str, exe: &str, args: &[&str]) -> BottleCommand {
        let mut argv = vec!["wie", "bottle", "run", name, exe];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv)
            .expect("parse bottle run argv")
            .command
        {
            Command::Bottle { command } => command,
            other => panic!("expected BottleCommand, got {other:?}"),
        }
    }

    /// `bottle run` parses every mode flag as a flag, not as guest argv. The
    /// old bug swallowed `--gui` and friends into `guest_args`; the shape
    /// must now mirror `run --bottle` exactly.
    #[test]
    fn bottle_run_parses_mode_flags_as_flags() {
        assert!(matches!(
            parse_bottle_run(
                "doom",
                "doom.exe",
                &["--gui", "--app-dir", "/tmp/MyApp", "--max-api", "500", "--", "-n", "3"],
            ),
            BottleCommand::Run {
                args: RunArgs {
                    gui: true,
                    app_dir: Some(ref dir),
                    max_api: Some(500),
                    ..
                },
                guest_args,
                ..
            } if dir == std::path::Path::new("/tmp/MyApp")
                && guest_args == ["-n", "3"]
        ));
    }

    /// `bottle run <name> <exe> --gui` must NOT swallow `--gui` into
    /// `guest_args` (the original bug), and parses as the GUI mode.
    #[test]
    fn bottle_run_gui_flag_is_not_a_guest_arg() {
        assert!(matches!(
            parse_bottle_run("doom", "doom.exe", &["--gui"]),
            BottleCommand::Run {
                args: RunArgs { gui: true, .. },
                guest_args,
                ..
            } if guest_args.is_empty()
        ));
    }

    /// The remaining mode flags parse on `bottle run` like `run --bottle`.
    #[test]
    fn bottle_run_parses_persistent_screenshot_expect_code() {
        assert!(matches!(
            parse_bottle_run("doom", "doom.exe", &["--persistent", "--max-api", "1000"]),
            BottleCommand::Run {
                args: RunArgs {
                    persistent: true,
                    max_api: Some(1000),
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            parse_bottle_run("doom", "doom.exe", &["--screenshot", "out.bmp"]),
            BottleCommand::Run {
                args: RunArgs {
                    screenshot: Some(ref out),
                    ..
                },
                ..
            } if out == std::path::Path::new("out.bmp")
        ));
        assert!(matches!(
            parse_bottle_run("doom", "doom.exe", &["--expect-code", "7"]),
            BottleCommand::Run {
                args: RunArgs { expect_code: 7, .. },
                ..
            }
        ));
    }

    /// `bottle run <name> <exe>` with no mode flag is the micro default — the
    /// exact behavior preserved from before the shared dispatch refactor.
    #[test]
    fn bottle_run_default_is_micro() {
        assert!(matches!(
            parse_bottle_run("doom", "doom.exe", &[]),
            BottleCommand::Run {
                args: RunArgs {
                    gui: false,
                    console: false,
                    persistent: false,
                    screenshot: None,
                    max_api: None,
                    expect_code: 0,
                    ..
                },
                ..
            }
        ));
    }

    /// The shared dispatch gives GUI precedence over the lower modes and
    /// enforces the micro-only flag rules — the same contract for `bottle run`
    /// and `run --bottle`.
    #[test]
    fn run_entry_gui_precedence_and_micro_only_rejections() {
        // GUI is selected over the (also-set) micro flags, and an input
        // script that is not a file fails up front in the GUI branch.
        let script = std::env::temp_dir().join("wie-no-such-script-0.txt");
        let err = run_entry(
            std::path::Path::new("app.exe"),
            None,
            None,
            RunArgs {
                max_api: Some(100),
                gui: true,
                input_script: Some(script.clone()),
                ..RunArgs::default()
            },
            Vec::new(),
        )
        .expect_err("gui with a missing input script must fail");
        assert!(
            err.to_string().contains("input script not found"),
            "took the GUI branch: {err}"
        );
        // A script without --gui is rejected regardless of mode.
        let err = run_entry(
            std::path::Path::new("app.exe"),
            None,
            None,
            RunArgs {
                max_api: Some(100),
                input_script: Some(script),
                ..RunArgs::default()
            },
            Vec::new(),
        )
        .expect_err("--input-script without --gui must fail");
        assert!(
            err.to_string().contains("--input-script requires --gui"),
            "rejects input script outside gui: {err}"
        );
        // Console mode rejects the micro-only --drive-d flag.
        let err = run_entry(
            std::path::Path::new("app.exe"),
            None,
            None,
            RunArgs {
                console: true,
                drive_d: Some(std::path::PathBuf::from("/tmp/d")),
                ..RunArgs::default()
            },
            Vec::new(),
        )
        .expect_err("console mode must reject --drive-d");
        assert!(
            err.to_string().contains("micro mode"),
            "console rejects drive-d: {err}"
        );
        // The bottle-derived root is allowed on console (no raw --root flag).
        let root = std::env::temp_dir().join("wie-run-entry-root");
        let err = run_entry(
            std::path::Path::new(&root.join("app.exe")),
            Some(root.clone()),
            None, // no raw --root flag
            RunArgs {
                console: true,
                ..RunArgs::default()
            },
            Vec::new(),
        )
        .expect_err("console run with a missing exe must fail at staging");
        assert!(
            err.to_string().contains("stat run source"),
            "console reached the staging/run step (bottle root allowed): {err}"
        );
    }

    /// `--console` + `--persistent` and `--gui` + `--persistent` all parse:
    /// the mutual exclusion and GUI precedence are enforced in `run_command`,
    /// not clap — this pins that contract.
    #[test]
    fn run_mode_conflicts_are_runtime_not_parse() {
        assert!(matches!(
            parse_run(&["--console", "--persistent"]),
            Command::Run {
                args: RunArgs {
                    console: true,
                    persistent: true,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            parse_run(&["--gui", "--persistent"]),
            Command::Run {
                args: RunArgs {
                    gui: true,
                    persistent: true,
                    ..
                },
                ..
            }
        ));
    }
}
