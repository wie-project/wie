//! GUI micro-test: checks that a PE with window creation + paint does not crash.
//!
//! Requires mingw-built gui_*.exe (run `make -C micro-exes gui_exes`).
//!
//! The suite is split along its natural seams: shared pump/wait/ink helpers
//! (`helpers`), the gui_*.exe micro-demos (`demo`), the control PEs
//! (`controls`), the RNotepad scenarios (`notepad`), and the RNotepad
//! font/confirm/save dialog flows (`dialogs`).

mod controls;
mod demo;
mod dialogs;
mod find;
mod helpers;
mod notepad;
