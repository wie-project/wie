//! In-guest machine-code stubs for trivial WinAPI entries.
//!
//! These run entirely inside the guest without a host stop when the code hook
//! treats their instruction bytes as passthrough (see `install_runtime_hooks`).
//!
//! # Correctness policy (Microsoft Learn)
//!
//! Only plant stubs when the in-guest body can honour the documented API contract
//! for the subset of behaviour WIE models (fixed guest environment, published
//! guest memory). **Do not** accelerate APIs with simplified “always success”
//! answers that diverge from Learn (e.g. `VirtualProtect` with NULL
//! `lpflOldProtect` must fail; `VirtualQuery` must describe real regions —
//! those stay on the host until RegionTable-backed handlers exist).
//!
//! `LocalAlloc` / `GlobalAlloc` with `LMEM_MOVEABLE` / lock semantics also stay
//! on the host — a thin `HeapAlloc` wrapper would break real apps.
//!
//! Module layout: `config` (guest VA layout + offsets) · `kind` (stub kinds
//! and their classification metadata) · `encode` (machine-code bodies) ·
//! `classify` (library/name → kind) · `data` (data-page builders) · `plant`
//! (fake-API planting).

mod classify;
mod config;
mod data;
mod encode;
mod kind;
mod plant;

#[cfg(test)]
mod tests;

pub use classify::TEB_LAST_ERROR_VA;

pub(crate) use classify::classify_guest_stub;
pub(crate) use config::GuestStubConfig;
pub(crate) use data::{build_stub_data_page, publish_cwd_wide, refresh_clock_table};
pub(crate) use kind::GuestStubKind;
pub(crate) use plant::plant_guest_stubs;
