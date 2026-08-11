# `wie bottle` CLI Command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `bottle` subcommand to the `wie` CLI that manages named bottles — create, list, inspect, delete, copy host files/folders into a bottle's `drive_c`, print the bottle path, and run a guest exe inside a bottle.

**Architecture:** Named bottles live as plain directories under `~/Library/Application Support/WIE/bottles/<name>/drive_c/` — the same on-disk layout the VFS already maps (guest `C:\…` → `{root}/drive_c/…`), so no emulator-core changes are needed. The CLI command wraps existing primitives: `wie_winapi::bottle::guest_path_to_host` for guest→host mapping, `run_micro` for `bottle run` delegation, and the repo's broken-pipe-safe `util::write_line` for output. Bottles are plain directories (KISS): no manifest file, no registry — `drive_c/` is the bottle.

**Tech Stack:** Rust, clap 4 (derive), `std::fs` (create/delete/copy/recursive dir copy), existing `wie-winapi` bottle/VFS helpers, `anyhow`.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/wie-cli/src/commands/bottle.rs` (new) | All `bottle` subcommand logic: bottles-dir resolution, create/list/info/delete/add/path/run, recursive copy + size helpers, `#[cfg(test)]` unit tests |
| `crates/wie-cli/src/commands/mod.rs` (modify) | Register `pub(crate) mod bottle;` + re-export its entry fn |
| `crates/wie-cli/src/main.rs` (modify) | Add `Bottle` variant to the clap `Command` enum + a `BottleCommand` subcommand enum + dispatch arm |

No changes to `wie-winapi` or `wie-runtime`: the bottle dir layout is already what the VFS consumes (`global_bottle_root()` is the *default* single bottle; named bottles are siblings under `WIE/bottles/`).

---

### Task 1: Bottles directory + `create` + `list`

**Files:**
- Create: `crates/wie-cli/src/commands/bottle.rs`
- Modify: `crates/wie-cli/src/commands/mod.rs`

- [ ] **Step 1: Write the failing tests** (module skeleton + tests first)

```rust
//! `wie bottle` — manage named bottles (guest `C:\` roots).
//!
//! A bottle is a plain directory `{bottles_dir}/{name}/drive_c/` in the same
//! layout the VFS maps: guest `C:\App\x` → `{root}/drive_c/App/x`. The
//! default single bottle (`WIE/bottle`) is untouched — named bottles are
//! siblings under `WIE/bottles/`.

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

/// Create a named bottle (fails if it already exists).
fn create(bottles_dir: &Path, name: &str) -> Result<()> {
    let root = bottle_root(bottles_dir, name);
    if root.exists() {
        bail!("bottle '{name}' already exists at {}", root.display());
    }
    fs::create_dir_all(drive_c_dir(&root))
        .with_context(|| format!("failed to create bottle '{name}'"))?;
    Ok(())
}

/// Names of all existing bottles, sorted.
fn list(bottles_dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(bottles_dir).with_context(|| {
        format!("failed to read bottles directory {}", bottles_dir.display())
    })? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && drive_c_dir(&path).is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Fresh temp dir unique per test call (parallel tests must not collide).
    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wie-bottle-test-{n}"));
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
    fn create_refuses_existing() {
        let dir = temp_dir();
        create(&dir, "dup").unwrap();
        assert!(create(&dir, "dup").is_err());
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
        assert_eq!(
            dir,
            wie_winapi::global_bottle_root()
                .parent()
                .unwrap()
                .join("bottles")
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p wie-cli bottle::`
Expected: FAIL — `could not compile` (`bottle` module not registered).

- [ ] **Step 3: Register the module** in `crates/wie-cli/src/commands/mod.rs`

```rust
mod bottle;
mod inspect;
mod run;
mod trace;
mod util;

pub(crate) use bottle::bottle;
pub(crate) use inspect::{image, imports, inspect, sections, winapi_map};
pub(crate) use run::{ensure_exe_in_bottle, run_console_interactive, run_micro, run_until_yield};
pub(crate) use trace::entry_trace;
```

Note: `bottle` (the dispatcher fn) is defined in Task 6 — until then the re-export line will not compile. To keep this commit green, add the re-export in Task 6 and only `mod bottle;` here (tests use `super::` paths, so `pub(crate) use` is not needed for Task 1's tests).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p wie-cli bottle::`
Expected: PASS — 4 tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/wie-cli/src/commands/bottle.rs crates/wie-cli/src/commands/mod.rs
git commit -m "cli: bottle create/list — named bottles under WIE/bottles/<name>/drive_c"
```

---

### Task 2: `info` + `path` + `delete`

**Files:**
- Modify: `crates/wie-cli/src/commands/bottle.rs`

- [ ] **Step 1: Write the failing tests** (append to the `tests` module in `bottle.rs`)

```rust
    /// Sum of all file sizes under `root`, recursively (bytes).
    fn dir_size(root: &Path) -> u64 {
        let mut total = 0;
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    total += dir_size(&path);
                } else if let Ok(meta) = fs::metadata(&path) {
                    total += meta.len();
                }
            }
        }
        total
    }

    /// Remove a bottle (fails if it does not exist).
    fn delete(bottles_dir: &Path, name: &str) -> Result<()> {
        let root = bottle_root(bottles_dir, name);
        if !root.is_dir() {
            bail!("bottle '{name}' does not exist");
        }
        fs::remove_dir_all(&root)
            .with_context(|| format!("failed to delete bottle '{name}'"))?;
        Ok(())
    }

    #[test]
    fn info_reports_path_and_size() {
        let dir = temp_dir();
        create(&dir, "apps").unwrap();
        // A 10-byte file inside drive_c.
        let f = drive_c_dir(&bottle_root(&dir, "apps")).join("hello.txt");
        fs::write(&f, b"0123456789").unwrap();
        let root = bottle_root(&dir, "apps");
        assert_eq!(dir_size(&root), 10);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_removes_and_missing_fails() {
        let dir = temp_dir();
        create(&dir, "gone").unwrap();
        delete(&dir, "gone").unwrap();
        assert!(!bottle_root(&dir, "gone").exists());
        assert!(delete(&dir, "gone").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p wie-cli bottle::`
Expected: FAIL — `cannot find function dir_size/delete` in the test module scope (helpers are defined below the tests module).

- [ ] **Step 3: Add the two helper fns above the `#[cfg(test)]` module** (the code shown in Step 1, placed after `list` in the main module body).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p wie-cli bottle::`
Expected: PASS — 6 tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/wie-cli/src/commands/bottle.rs
git commit -m "cli: bottle info/delete — dir-size reporting and removal"
```

---

### Task 3: `add` — copy a host file or folder into `drive_c`

**Files:**
- Modify: `crates/wie-cli/src/commands/bottle.rs`

- [ ] **Step 1: Write the failing tests**

```rust
    /// Recursively copy `src` to `dst` (file or directory tree).
    fn copy_into(src: &Path, dst: &Path) -> Result<()> {
        if src.is_dir() {
            fs::create_dir_all(dst)
                .with_context(|| format!("failed to create {}", dst.display()))?;
            for entry in fs::read_dir(src)
                .with_context(|| format!("failed to read {}", src.display()))?
            {
                let entry = entry?;
                copy_into(&entry.path(), &dst.join(entry.file_name()))?;
            }
            Ok(())
        } else {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::copy(src, dst)
                .with_context(|| {
                    format!("failed to copy {} → {}", src.display(), dst.display())
                })?;
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p wie-cli bottle::`
Expected: FAIL — `cannot find function copy_into/resolve_guest_path`.

- [ ] **Step 3: Add the two helper fns above the `#[cfg(test)]` module**

Place after `delete` in the main module body (exact code in Step 1).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p wie-cli bottle::`
Expected: PASS — 9 tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/wie-cli/src/commands/bottle.rs
git commit -m "cli: bottle add — recursive host file/folder copy into drive_c with C:\\ confinement"
```

---

### Task 4: `run` — delegate to `run_micro` with the bottle as `--root`

**Files:**
- Modify: `crates/wie-cli/src/commands/bottle.rs`

- [ ] **Step 1: Write the failing test** (guest-path resolution only — running a PE is covered by the existing micro-suite)

```rust
    /// Host path of a guest exe inside the bottle, e.g. `C:\App\app.exe`.
    fn resolve_exe(bottle_root: &Path, guest_exe: &str) -> Result<PathBuf> {
        resolve_guest_path(bottle_root, guest_exe)
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p wie-cli bottle::resolve_exe`
Expected: FAIL — `cannot find function resolve_exe`.

- [ ] **Step 3: Add `resolve_exe` above the test module** (exact code in Step 1).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p wie-cli bottle::resolve_exe`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/wie-cli/src/commands/bottle.rs
git commit -m "cli: bottle run — resolve guest exe inside the bottle root"
```

---

### Task 5: Wire the `bottle` subcommand into clap (`main.rs`)

**Files:**
- Modify: `crates/wie-cli/src/main.rs`

- [ ] **Step 1: Add the `BottleCommand` subcommand enum + `Bottle` variant**

In `crates/wie-cli/src/main.rs`, after the `Command` enum's `Trace` variant, add the enum and extend `Command`:

```rust
/// Named-bottle management (`Bottle` subcommand surface).
#[derive(Debug, Subcommand)]
enum BottleCommand {
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
        /// Guest path of the exe inside the bottle, e.g. `C:\App\app.exe`.
        exe: String,
        /// Guest argv after the exe.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        guest_args: Vec<String>,
    },
}
```

Then add the variant to `Command` (after `Trace`):

```rust
    /// Named-bottle management (create/list/info/delete/add/run).
    Bottle {
        #[command(subcommand)]
        command: BottleCommand,
    },
```

- [ ] **Step 2: Add the dispatch arm** in `main()`'s match, after the `Command::Trace` arm

```rust
        Command::Bottle { command } => commands::bottle(command)?,
```

- [ ] **Step 3: Compile check**

Run: `cargo check -p wie-cli`
Expected: FAIL — `bottle` is not exported from `commands` yet (that is Task 6). If the failure is only the missing `commands::bottle`, proceed to Task 6 before verifying.

- [ ] **Step 4: Commit**

```bash
git add crates/wie-cli/src/main.rs
git commit -m "cli: wire bottle subcommand surface into clap (create/list/info/path/delete/add/run)"
```

---

### Task 6: The `bottle` dispatcher + `add`/`run` host-side glue

**Files:**
- Modify: `crates/wie-cli/src/commands/bottle.rs`
- Modify: `crates/wie-cli/src/commands/mod.rs`

- [ ] **Step 1: Write the dispatcher and wire the re-export**

Add to `bottle.rs` (before the test module):

```rust
use crate::commands::util::write_line;
use crate::commands::BottleCommand;
use std::io::Write;

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
            let root = bottle_root(&bottles_dir(), &name);
            if !root.is_dir() {
                bail!("bottle '{name}' does not exist");
            }
            let mut out = std::io::stdout().lock();
            write_line(
                &mut out,
                &format!("name: {name}\nroot: {}\ndrive_c: {}\nsize: {} bytes",
                    root.display(),
                    drive_c_dir(&root).display(),
                    dir_size(&root)),
            )?;
        }
        BottleCommand::Path { name } => {
            let root = bottle_root(&bottles_dir(), &name);
            if !root.is_dir() {
                bail!("bottle '{name}' does not exist");
            }
            let mut out = std::io::stdout().lock();
            write_line(&mut out, &root.display().to_string())?;
        }
        BottleCommand::Delete { name, yes } => {
            if !yes {
                eprintln!("delete bottle '{name}' and ALL its contents? [y/N]");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if line.trim().to_ascii_lowercase() != "y" {
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
            let root = bottle_root(&bottles_dir(), &name);
            if !root.is_dir() {
                bail!("bottle '{name}' does not exist");
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
            guest_args,
        } => {
            let root = bottle_root(&bottles_dir(), &name);
            if !root.is_dir() {
                bail!("bottle '{name}' does not exist");
            }
            let host_exe = resolve_exe(&root, &exe)?;
            crate::commands::run_micro(
                &host_exe,
                crate::MICRO_MAX_API_DEFAULT,
                0,
                Some(&root),
                None,
                None,
                &guest_args,
            )?;
        }
    }
    Ok(())
}
```

Note: `use crate::commands::BottleCommand;` requires `BottleCommand` to be re-exported from `commands/mod.rs` — add `pub(crate) use crate::BottleCommand;`? No: `BottleCommand` is defined in `main.rs`, and `commands` is a sibling module. The clean way: move the enum into `commands/bottle.rs` instead, or reference it via `crate::BottleCommand`. To avoid a circular module dependency, change the import to `use crate::BottleCommand;` and in `commands/mod.rs` re-export the dispatcher:

```rust
pub(crate) use bottle::bottle;
```

(`main.rs` already has `use crate::commands::…`; the dispatcher's `crate::BottleCommand` path works because the enum is `pub`-in-crate via the `enum` in `main.rs` — check: the `Command`/`BottleCommand` enums are private to `main.rs`. To make `crate::BottleCommand` visible, add `pub(crate) enum BottleCommand` — the enum is only used inside the crate, so `pub(crate)` is correct and matches `pub use` visibility needs.)

- [ ] **Step 2: Adjust visibility in `main.rs`** — change `enum BottleCommand` to `pub(crate) enum BottleCommand`.

- [ ] **Step 3: Compile + full unit test pass**

Run: `cargo check -p wie-cli && cargo test -p wie-cli bottle::`
Expected: PASS — all bottle unit tests green, crate compiles.

- [ ] **Step 4: Manual smoke — create, add, list, path, run**

```bash
cargo build -p wie-cli 2>&1 | tail -1
./target/debug/wie bottle create smoke
./target/debug/wie bottle list
./target/debug/wie bottle path smoke
# Copy a micro exe into the bottle and run it there.
./target/debug/wie bottle add smoke micro-exes/out/gl_quad.exe --target "C:\\smoke\\gl_quad.exe"
WIE_SELFTEST=1 ./target/debug/wie bottle run smoke "C:\\smoke\\gl_quad.exe"
```

Expected: `create` prints nothing (or a line), `list` prints `smoke`, `path` prints the host root, `run` runs gl_quad in self-test and exits 0 (observe `guest exited code=0`).

- [ ] **Step 5: Commit**

```bash
git add crates/wie-cli/src/commands/bottle.rs crates/wie-cli/src/commands/mod.rs crates/wie-cli/src/main.rs
git commit -m "cli: bottle dispatcher — create/list/info/path/delete/add/run wired end-to-end"
```

---

### Task 7: `add` default-target edge case + docs

**Files:**
- Modify: `crates/wie-cli/src/commands/bottle.rs`
- Modify: `docs/RUNBOOK.md` (bottle section)

- [ ] **Step 1: Write the failing test for the default-target path**

```rust
    #[test]
    fn add_defaults_target_to_c_basename() {
        let dir = temp_dir();
        create(&dir, "b").unwrap();
        let root = bottle_root(&dir, "b");
        let file = dir.join("tool.exe");
        fs::write(&file, b"x").unwrap();
        copy_into(&file, &resolve_guest_path(&root, r"C:\tool.exe").unwrap()).unwrap();
        assert!(drive_c_dir(&root).join("tool.exe").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Run test to verify it passes** (the behavior already exists — this pins the default)

Run: `cargo test -p wie-cli bottle::add_defaults_target_to_c_basename`
Expected: PASS.

- [ ] **Step 3: Document the command** in `docs/RUNBOOK.md`, in the CLI section

```markdown
## Bottles

Named bottles live under `~/Library/Application Support/WIE/bottles/<name>/`
(each with a `drive_c/` — the guest `C:\` volume; the default single bottle
at `WIE/bottle` is unchanged). Manage them with:

    wie bottle create <name>                 # new empty bottle
    wie bottle list                          # existing bottles
    wie bottle info <name>                   # path, drive_c, size
    wie bottle path <name>                   # host root (for --root scripting)
    wie bottle delete <name> [--yes]         # remove a bottle
    wie bottle add <name> <host-path> [--target C:\Apps\Foo]   # copy file/folder in
    wie bottle run <name> C:\App\app.exe [args...]             # run inside the bottle

`bottle run` is `run --root <bottle>` with the exe resolved inside `drive_c`.
```

- [ ] **Step 4: Commit**

```bash
git add crates/wie-cli/src/commands/bottle.rs docs/RUNBOOK.md
git commit -m "docs: bottle command in RUNBOOK; pin add default-target behavior"
```

---

### Task 8: Full gate

**Files:** none (verification only)

- [ ] **Step 1: Workspace gate**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all green (fmt clean, clippy clean, all tests pass — bottle unit tests included).

- [ ] **Step 2: Micro-suite regression**

Run: `make -C micro-exes && ./scripts/run-micro-suite.sh gl_quad`
Expected: PASS — the existing GL micro still runs under the default bottle (named-bottle support must not change `run --root` / default behavior).

- [ ] **Step 3: Final smoke with `--version` intact**

Run: `./target/debug/wie --version && ./target/debug/wie bottle --help`
Expected: `wie 0.1.0`, then the bottle help listing all 7 subcommands.

- [ ] **Step 4: Commit (if anything drifted)**

```bash
git status --short
git add -u
git commit -m "chore: post-bottle-command gate fixes"
```

---

## Self-Review

**Spec coverage:** create/list/info/delete/add/path/run cover "manage bottles, move apps/folders into them, everything a user could need" — no `rename`/`export`/`clone` in v1 (YAGNI; `add` + `path` compose them). The default single bottle and `run --root` behavior are untouched (backward compatible). Guest-path confinement (`C:\` only) prevents escapes; delete requires confirmation.

**Placeholders:** none — every task has exact code, paths, commands, expected output.

**Type consistency:** `bottle_root(&bottles_dir, name)` and `drive_c_dir(&root)` are used consistently across create/list/info/add/run; `resolve_guest_path`/`resolve_exe` share the `drive_c_dir` confinement; `BottleCommand` is `pub(crate)` in `main.rs` and dispatched via `commands::bottle` re-exported from `commands/mod.rs`.
