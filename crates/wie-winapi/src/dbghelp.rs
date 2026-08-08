//! Handles `DBGHELP.dll` + `IMAGEHLP.dll` — debug helpers (string dispatch).
//!
//! Minimal: initialization succeeds, symbol lookups fail gracefully.
//! Stateless, so no `DllStateMap` slot is needed.

use anyhow::{Context, Result};

use crate::{HandlerContext, WinApiHandlerResult};

/// Win32 `ERROR_INVALID_ADDRESS` (487) — SymFromAddr lookup miss.
const ERROR_INVALID_ADDRESS: u32 = 487;

/// Dispatch a `DBGHELP.dll` / `IMAGEHLP.dll` export by name.
///
/// IMAGEHLP contributes `MapFileAndCheckSum*`; everything else lives in
/// DBGHELP proper.
pub fn dispatch_dbghelp(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "syminitializew" | "syminitialize" => Ok(Some(handle_sym_initialize(ctx)?)),
        "symcleanup" => Ok(Some(handle_sym_cleanup(ctx)?)),
        "symfromaddrw" | "symfromaddr" => Ok(Some(handle_sym_from_addr(ctx)?)),
        "mapfileandchecksumw" | "mapfileandchecksuma" => {
            Ok(Some(handle_map_file_and_checksum(ctx)?))
        }
        _ => Ok(None),
    }
}

/// `BOOL SymInitializeW/A(HANDLE hProcess, PCSTR/W UserSearchPath, BOOL fInvadeProcess)`.
///
/// No real symbol engine exists, but initialization "succeeds" so callers
/// proceed to the lookup path (which fails gracefully).
fn handle_sym_initialize(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process = engine
        .read_rcx()
        .context("failed to read RCX for SymInitialize")?;
    let _search_path = engine
        .read_rdx()
        .context("failed to read RDX for SymInitialize")?;
    let _invade = engine
        .read_r8()
        .context("failed to read R8 for SymInitialize")?;
    ctx.finish(1)
}

/// `BOOL SymCleanup(HANDLE hProcess)` — TRUE.
fn handle_sym_cleanup(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process = engine
        .read_rcx()
        .context("failed to read RCX for SymCleanup")?;
    ctx.finish(1)
}

/// `BOOL SymFromAddrW/A(HANDLE, DWORD64 Address, PDWORD64 Displacement, PSYMBOL_INFOW/A)`.
///
/// No symbols are ever loaded — FALSE with `ERROR_INVALID_ADDRESS`.
fn handle_sym_from_addr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _process = engine
        .read_rcx()
        .context("failed to read RCX for SymFromAddr")?;
    let _address = engine
        .read_rdx()
        .context("failed to read RDX for SymFromAddr")?;
    let _symbol = engine
        .read_r8()
        .context("failed to read R8 for SymFromAddr")?;
    let _displacement = engine
        .read_r9()
        .context("failed to read R9 for SymFromAddr")?;
    ctx.state.process.last_error = ERROR_INVALID_ADDRESS;
    ctx.finish(0)
}

/// `DWORD MapFileAndCheckSumW/A(LPCSTR/W Filename, LPDWORD HeaderSum, LPDWORD CheckSum)`.
///
/// The sums are unknown — write 0 to both out-params and return
/// `CHECKSUM_SUCCESS` (0) so callers proceed with a zero checksum.
fn handle_map_file_and_checksum(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _file_name = engine
        .read_rcx()
        .context("failed to read RCX for MapFileAndCheckSum")?;
    let header_sum = engine
        .read_rdx()
        .context("failed to read RDX for MapFileAndCheckSum")?;
    let checksum = engine
        .read_r8()
        .context("failed to read R8 for MapFileAndCheckSum")?;
    let zeros = 0_u32.to_le_bytes();
    if header_sum != 0 {
        engine.mem_write(header_sum, &zeros)?;
    }
    if checksum != 0 {
        engine.mem_write(checksum, &zeros)?;
    }
    ctx.finish(0)
}
