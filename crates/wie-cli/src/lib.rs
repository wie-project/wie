//! `wie` — WIE PE64 userspace emulator CLI: the reusable surface behind the binary.
//!
//! The crate builds two targets from this one module tree: this library holds
//! everything the `wie` binary can do — the clap argument definitions, the
//! run-entry dispatch, the [`commands`] implementations and the [`gui`]
//! bridges — while `src/main.rs` only initializes tracing, parses argv and
//! calls [`run_command`]. Keeping the logic here makes it reachable from
//! `tests/` without spawning a process.

// Machine-checked gates for this crate's *production* code (CLAUDE.md "Lint
// policy", plan T1.2): no `unwrap`/`expect`/`panic`, no unchecked slice index.
// Host code is where a panic is a user-visible crash rather than a guest
// failure. Test builds are exempt on purpose.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
// `clippy::print_stdout` is denied workspace-wide (root `Cargo.toml`) and this
// is its one declared exception: writing the PE report, the run diagnostics and
// the profile summary to stdout IS what `wie inspect` / `wie run` are for. The
// four library crates have no such exemption.
#![expect(
    clippy::print_stdout,
    reason = "the CLI's product is host stdout output (inspect report, run diagnostics)"
)]

mod bmp;
pub mod cli;
pub mod commands;
pub mod gui;

#[cfg(test)]
mod test_support;

pub use cli::{
    BottleCommand, Cli, Command, RunArgs, VERSION_TEXT, reject_micro_only_flags, resolve_run_root,
    run_command, run_entry, version_text_if_requested,
};
