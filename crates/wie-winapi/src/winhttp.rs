//! Handles `WINHTTP.dll` — the WinHTTP surface SDL2 uses for its online /
//! Steam integration probes.
//!
//! Honest minimal model: `WinHttpOpen` / `WinHttpConnect` / `WinHttpOpenRequest`
//! hand out fake handles so the guest's session setup succeeds; the request
//! I/O operations (`SendRequest` / `ReceiveResponse` / `ReadData` /
//! `QueryDataAvailable`) fail cleanly with `ERROR_WINHTTP_CANNOT_CONNECT`,
//! so SDL2 treats the network as unavailable and continues offline.
//! `WinHttpAddRequestHeaders` accepts and ignores the headers.
//!
//! The wininet lane has a real host HTTP client; wiring WinHttp to it is a
//! later wave if a guest actually exercises the request path.

use anyhow::{Context, Result};

use crate::{HandlerContext, WinApiHandlerResult};

/// Fake `HINTERNET` handle base for WinHTTP (clear of wininet's `0x5100_0000`).
const WINHTTP_HANDLE_BASE: u64 = 0x5101_0000;
/// `ERROR_WINHTTP_CANNOT_CONNECT` (winhttp.h) — the request I/O failure code.
const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12029;

/// Live WinHTTP handle table (sessions, connections, requests share one
/// namespace — `WinHttpCloseHandle` frees any of them).
#[derive(Debug, Default)]
pub struct WinhttpState {
    next_handle: u64,
    live: Vec<u64>,
}

impl WinhttpState {
    fn alloc(&mut self) -> u64 {
        if self.next_handle == 0 {
            self.next_handle = WINHTTP_HANDLE_BASE;
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.saturating_add(1);
        self.live.push(handle);
        handle
    }

    fn free(&mut self, handle: u64) -> bool {
        let before = self.live.len();
        self.live.retain(|h| *h != handle);
        self.live.len() != before
    }
}

crate::soft_dispatch::soft_exports! {
    ("winhttpaddrequestheaders", handle_win_http_add_request_headers),
    ("winhttpclosehandle", handle_win_http_close_handle),
    ("winhttpconnect", handle_win_http_connect),
    ("winhttpopen", handle_win_http_open),
    ("winhttpopenrequest", handle_win_http_open_request),
    ("winhttpquerydataavailable", handle_win_http_request_io),
    ("winhttpreaddata", handle_win_http_request_io),
    ("winhttpreceiveresponse", handle_win_http_request_io),
    ("winhttpsendrequest", handle_win_http_request_io),
}

/// Dispatch a `WINHTTP.dll` export by name (case-insensitive).
pub fn dispatch_winhttp(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    crate::soft_dispatch::dispatch_soft_table(ctx, DISPATCH, name)
}

/// `HINTERNET WinHttpOpen(LPCWSTR, DWORD, LPCWSTR*, LPCWSTR*, DWORD)`.
fn handle_win_http_open(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _agent = ctx.engine.read_rcx()?;
    let _access_type = ctx.engine.read_rdx()?;
    let _proxy = ctx.engine.read_r8()?;
    let _proxy_bypass = ctx.engine.read_r9()?;
    let handle = ctx.state.winhttp().alloc();
    ctx.finish(handle)
}

/// `HINTERNET WinHttpConnect(HINTERNET, LPCWSTR, INTERNET_PORT, DWORD)`.
fn handle_win_http_connect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _session = ctx.engine.read_rcx()?;
    let _server = ctx.engine.read_rdx()?;
    let _port = ctx.engine.read_r8()?;
    let _reserved = ctx.engine.read_r9()?;
    let handle = ctx.state.winhttp().alloc();
    ctx.finish(handle)
}

/// `HINTERNET WinHttpOpenRequest(HINTERNET, LPCWSTR, LPCWSTR, LPCWSTR,
/// LPCWSTR*, LPCWSTR*, DWORD)`.
fn handle_win_http_open_request(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _connect = ctx.engine.read_rcx()?;
    let _verb = ctx.engine.read_rdx()?;
    let _object = ctx.engine.read_r8()?;
    let _version = ctx.engine.read_r9()?;
    let handle = ctx.state.winhttp().alloc();
    ctx.finish(handle)
}

/// `BOOL WinHttpCloseHandle(HINTERNET)` — frees the handle when it was live.
fn handle_win_http_close_handle(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for WinHttpCloseHandle")?;
    let freed = state.winhttp().free(handle);
    ctx.finish(u64::from(freed))
}

/// `BOOL WinHttpAddRequestHeaders(HINTERNET, LPCWSTR, DWORD, DWORD)` — accepts
/// the headers (they are ignored).
fn handle_win_http_add_request_headers(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let _request = ctx.engine.read_rcx()?;
    let _headers = ctx.engine.read_rdx()?;
    let _length = ctx.engine.read_r8()?;
    let _modifiers = ctx.engine.read_r9()?;
    ctx.finish(1)
}

/// Shared FALSE for the request I/O operations — the network is unavailable
/// (`ERROR_WINHTTP_CANNOT_CONNECT`).
fn handle_win_http_request_io(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let _request = ctx.engine.read_rcx()?;
    let _arg1 = ctx.engine.read_rdx()?;
    let _arg2 = ctx.engine.read_r8()?;
    let _arg3 = ctx.engine.read_r9()?;
    ctx.state.process.last_error = ERROR_WINHTTP_CANNOT_CONNECT;
    ctx.finish(0)
}
