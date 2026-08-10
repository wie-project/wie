//! Handles `URLMON.dll` — URL download surface (string dispatch).
//!
//! Host strategy: `URLDownloadToFile` performs a real HTTP/1.1 GET over
//! `std::net::TcpStream` (reusing the `wininet` HTTP machinery) and writes the
//! body straight through the VFS. The destination is a guest path and must
//! resolve inside the bottle / D: bridge volumes (`crate::vfs`); a path that
//! does not map fails the download.
//!
//! The COM entry points (`CoInternetCreateSecurityManager`,
//! `CoInternetGetSession`) are documented stubs returning `E_NOTIMPL` — no
//! COM object is registered for them.
//!
//! Limits (documented, never fake success): `http://` only (no TLS), no
//! redirects, no proxy; the body is fully buffered before the file write
//! (same cap as `wininet`, 64 MiB).
use anyhow::{Context, Result};

use crate::kernel32::{read_ansi_string_from_cpu, read_stack_u64, read_wide_string_from_cpu};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// `S_OK` (winerror.h).
const S_OK: u64 = 0;
/// `E_NOTIMPL` (winerror.h).
const E_NOTIMPL: u64 = 0x8000_4001;
/// `INET_E_DOWNLOAD_FAILURE` (urlmon.h) — the download-failure HRESULT.
const INET_E_DOWNLOAD_FAILURE: u64 = 0x800C_0008;
/// Cap for a guest URL / path string.
const MAX_URL_LEN: usize = 4096;

/// Canonical handler tail (mirrors `wininet::finish`).
fn finish(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("urlmon return")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// Dispatch a `urlmon.dll` export by name (case-insensitive).
pub fn dispatch_urlmon(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "urldownloadtofilew" => Ok(Some(handle_url_download_to_file_w(ctx)?)),
        "urldownloadtofilea" => Ok(Some(handle_url_download_to_file_a(ctx)?)),
        "cointernetcreatesecuritymanager" => {
            Ok(Some(handle_co_internet_create_security_manager(ctx)?))
        }
        "cointernetgetsession" => Ok(Some(handle_co_internet_get_session(ctx)?)),
        _ => Ok(None),
    }
}

/// `HRESULT URLDownloadToFileW(LPUNKNOWN pCaller, LPCWSTR szURL,
///                             LPCWSTR szFileName, DWORD dwReserved,
///                             LPBINDSTATUSCALLBACK lpfnCB)`
fn handle_url_download_to_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _caller = engine.read_rcx()?;
    let url_va = engine.read_rdx()?;
    let file_va = engine.read_r8()?;
    let _reserved = engine.read_r9()?;
    let _callback = read_stack_u64(engine, 0x28)?;
    let url = read_wide_string_from_cpu(engine, url_va, MAX_URL_LEN)?;
    let file = read_wide_string_from_cpu(engine, file_va, MAX_URL_LEN)?;
    download_to_file(ctx.engine, state, &url, &file)
}

/// `HRESULT URLDownloadToFileA(...)` — ANSI variant.
fn handle_url_download_to_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _caller = engine.read_rcx()?;
    let url_va = engine.read_rdx()?;
    let file_va = engine.read_r8()?;
    let _reserved = engine.read_r9()?;
    let _callback = read_stack_u64(engine, 0x28)?;
    let url = read_ansi_string_from_cpu(engine, url_va, MAX_URL_LEN)?;
    let file = read_ansi_string_from_cpu(engine, file_va, MAX_URL_LEN)?;
    download_to_file(ctx.engine, state, &url, &file)
}

/// Shared `URLDownloadToFile` body: GET `url`, write the body to the host
/// file `guest_path` resolves to, return `S_OK`.
///
/// Failures (unmappable destination, unsupported scheme, network error, I/O
/// error) return `INET_E_DOWNLOAD_FAILURE` with the cause logged — graceful,
/// so the guest keeps running.
fn download_to_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    url: &str,
    guest_path: &str,
) -> Result<WinApiHandlerResult> {
    let result = do_download(state, url, guest_path);
    match result {
        Ok(()) => finish(engine, S_OK),
        Err(err) => {
            tracing::debug!(%url, %guest_path, "URLDownloadToFile failed: {err:#}");
            finish(engine, INET_E_DOWNLOAD_FAILURE)
        }
    }
}

/// The fallible part of [`download_to_file`].
fn do_download(state: &mut WinApiState, url: &str, guest_path: &str) -> Result<()> {
    // The destination must map inside the bottle / D: bridge volumes.
    let map = crate::vfs::guest_path_to_host(&state.file_io.volumes, guest_path)
        .with_context(|| format!("destination {guest_path:?} is outside every mapped volume"))?;
    let parsed = crate::wininet::parse_http_url(url)
        .with_context(|| format!("unsupported download URL {url:?}"))?;
    let result = crate::wininet::perform_http_request(
        "GET",
        &parsed.host,
        parsed.port,
        &parsed.path,
        "HTTP/1.1",
        &[],
        &[],
    )
    .context("HTTP GET failed")?;
    if let Some(parent) = map.host.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating directory {}", parent.display()))?;
    }
    std::fs::write(&map.host, &result.body)
        .with_context(|| format!("failed writing {}", map.host.display()))?;
    Ok(())
}

/// `HRESULT CoInternetCreateSecurityManager(IServiceProvider *pSP,
///                                          IInternetSecurityManager **ppISM,
///                                          DWORD dwReserved)`
///
/// Stub — no COM security-manager class is registered in this emulator.
fn handle_co_internet_create_security_manager(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _p_sp = engine.read_rcx()?;
    let _pp_ism = engine.read_rdx()?;
    let _reserved = engine.read_r8()?;
    finish(engine, E_NOTIMPL)
}

/// `HRESULT CoInternetGetSession(DWORD dwSessionMode,
///                               IInternetSession **ppIInternetSession,
///                               DWORD dwReserved)`
///
/// Stub — no `IInternetSession` object is registered in this emulator.
fn handle_co_internet_get_session(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _session_mode = engine.read_rcx()?;
    let _pp_session = engine.read_rdx()?;
    let _reserved = engine.read_r8()?;
    finish(engine, E_NOTIMPL)
}

/// Census oracle: which `urlmon.dll` exports are implemented.
pub fn is_export(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "urldownloadtofilew"
            | "urldownloadtofilea"
            | "cointernetcreatesecuritymanager"
            | "cointernetgetsession"
    )
}
