//! Win64 struct layouts that cross the guest boundary, verified at compile
//! time against mingw-verified constants.
//!
//! Every struct here derives zerocopy's `KnownLayout`, `Immutable`,
//! `FromBytes`, and `IntoBytes` on a `#[repr(C)]` definition. Deliberately
//! **not** `Unaligned`: `u16`/`u32`/`u64`/`usize` fields are not
//! `Unaligned`, so no Win32 struct can derive it. Alignment is instead
//! resolved at runtime by the staging fallback in `crate::guest_memory`
//! (an odd guest VA stages into an aligned host buffer instead of faulting).
//!
//! `FromBytes` permits padding gaps — the Win64 `MSG` has one at offset 12 —
//! which is exactly what these layouts need (bytemuck-style `Pod` would
//! reject the type outright).
//!
//! The module is split per Windows source module (`core`, `gdi32`, `user32`,
//! `kernel32`, `comdlg32`) plus the Z1 print lane (`print_lane`). Each
//! submodule carries its own const-assert drift tables and round-trip tests;
//! this file only re-exports so the `crate::guest_layout::*` paths the
//! handlers use stay unchanged.

mod comdlg32;
mod core;
mod gdi32;
mod kernel32;
mod print_lane;
mod user32;

pub(crate) use comdlg32::*;
pub(crate) use core::*;
pub(crate) use gdi32::*;
pub(crate) use kernel32::*;
pub(crate) use print_lane::*;
pub(crate) use user32::*;
