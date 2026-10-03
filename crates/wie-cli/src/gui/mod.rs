//! Cross-thread GUI presenter: winit window + wgpu (Metal) surface.
//!
//! The modules are public so the GUI bridges (the print/page-setup panels, the
//! dialog hooks, the headless screenshot entry) can be driven — and tested —
//! from outside the binary.
//!
//! PLATFORM GATING — the subtree splits cleanly in two, so the boundary sits
//! at this `mod` list rather than inside individual files:
//!
//! * macOS-only — the *windowed presenter*: `app` (the winit
//!   `ApplicationHandler`, its `input_events` / `native_dialog` submodules,
//!   `present_wgpu`'s wgpu Metal surface, `menu_bar`'s muda/NSMenu bridge),
//!   plus `input` and `input_script`, the two modules that feed it. `input` is
//!   the Win32 message/scale vocabulary translated *from* winit events;
//!   `input_script` is the `--input-script` driver, which `--gui` requires and
//!   which posts the messages `input` defines. Their only two consumers are
//!   `app` and the `--gui` branch of `run_entry`, so gating them with the
//!   presenter costs nothing outside the windowed path and keeps a non-macOS
//!   build free of the ~50 dead-code warnings that gating `app` alone would
//!   leave behind. None of them has a non-Apple implementation — not
//!   "unfinished", absent by design — and gating at the `mod` declaration keeps
//!   winit/wgpu/muda out of a non-macOS dependency graph entirely.
//!
//! * portable — the *guest-visible* GUI surface: [`arg_preflight`],
//!   [`find_dialog`], [`font_dialog`], [`headless`], [`file_dialog`],
//!   [`print`]. No platform dependency, and all of it reachable without a
//!   window: `wie run --screenshot`, `wie trace`, `wie inspect` and the unit
//!   tests need these on any platform. Gating them would take working non-GUI
//!   functionality away from non-macOS builds, so they stay ungated.
//!
//! The two portable modules with a macOS-only half — [`file_dialog`] (the rfd
//! panel bridge) and [`print`] (the NSPrintPanel glue) — keep their own
//! per-item `#[cfg(target_os = "macos")]` behind a portable no-op stub; that
//! predates this split and was left alone.

#[cfg(target_os = "macos")]
pub mod app;
pub mod arg_preflight;
pub mod file_dialog;
pub mod find_dialog;
pub mod font_dialog;
pub mod headless;
#[cfg(target_os = "macos")]
pub mod input;
#[cfg(target_os = "macos")]
pub mod input_script;
#[cfg(target_os = "macos")]
pub mod menu_bar;
#[cfg(target_os = "macos")]
pub mod present_wgpu;
pub mod print;
