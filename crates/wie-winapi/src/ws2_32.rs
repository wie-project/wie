//! Handles `WS2_32.dll` — Winsock / network APIs (string dispatch).
//!
//! Host strategy: real sockets via `std::net` on loopback. `Ws2State` owns the
//! guest `SOCKET` → host socket table and lives in a `DllStateMap` slot,
//! heap-allocated on first load.

use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

/// Guest `SOCKET` → host socket state, owned by this module.
#[derive(Debug, Default)]
pub struct Ws2State {}

/// Dispatch a `WS2_32.dll` export by name (case-insensitive).
pub fn dispatch_ws2(
    _ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let _ = name;
    Ok(None)
}
