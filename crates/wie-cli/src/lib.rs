//! `wie` — WIE PE64 userspace emulator CLI: the reusable surface behind the binary.
//!
//! The crate builds two targets from this one module tree: this library holds
//! everything the `wie` binary can do — the clap argument definitions, the
//! run-entry dispatch, the [`commands`] implementations and the [`gui`]
//! bridges — while `src/main.rs` only initializes tracing, parses argv and
//! calls [`run_command`]. Keeping the logic here makes it reachable from
//! `tests/` without spawning a process.

mod bmp;
pub mod cli;
pub mod commands;
pub mod gui;

#[cfg(test)]
mod test_support;

pub use cli::{
    BottleCommand, Cli, Command, RunArgs, reject_micro_only_flags, resolve_run_root, run_command,
    run_entry,
};
