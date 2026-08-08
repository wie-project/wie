//! Handles `CRYPT32.dll` — cryptography (string dispatch).
//!
//! Host strategy: real hashing (sha1/sha2 crates) + host entropy.
//! `Crypt32State` owns provider/hash handle tables and lives in a
//! `DllStateMap` slot, heap-allocated on first load.

use anyhow::Result;

use crate::{HandlerContext, WinApiHandlerResult};

/// Crypt provider / hash handle state, owned by this module.
#[derive(Debug, Default)]
pub struct Crypt32State {}

/// Dispatch a `CRYPT32.dll` export by name (case-insensitive).
pub fn dispatch_crypt32(
    _ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let _ = name;
    Ok(None)
}
