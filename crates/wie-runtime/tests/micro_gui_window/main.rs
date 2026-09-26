//! GUI micro-test: checks that a PE with window creation + paint does not crash.
//!
//! Requires mingw-built gui_*.exe (run `make -C micro-exes gui_exes`).
//!
//! The suite is split along its natural seams: shared pump/wait/ink helpers
//! (`helpers`), the gui_*.exe micro-demos (`demo`), the control PEs
//! (`controls`), the RNotepad scenarios (`notepad`), the RNotepad
//! font/confirm/save dialog flows (`dialogs`), and the Edit → Go To
//! line-jump flow (`goto`).

// Shared with the single-file integration tests next door: the guest-PE
// resolvers live in one place so the "missing fixture is a failure" rule
// cannot drift per test file. `common` is not a test target of its own — a
// `tests/<dir>/mod.rs` is only compiled when a target declares `mod common`.
#[path = "../common/mod.rs"]
mod common;

mod capture;
mod controls;
mod demo;
mod dialogs;
mod find;
mod goto;
mod helpers;
mod notepad;
