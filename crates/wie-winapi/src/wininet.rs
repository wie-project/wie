//! Handles `WININET.dll` — Internet / HTTP surface (string dispatch).
//!
//! Host strategy: real HTTP/1.1 over `std::net::TcpStream` (no FFI — `unsafe`
//! is denied in this crate), mirroring the `ws2_32` lane. [`WininetState`]
//! owns the guest `HINTERNET` handle tables (sessions / connections /
//! requests) and lives in a `DllStateMap` slot, heap-allocated on first load.
//!
//! Blocking semantics: the HTTP exchange in `HttpSendRequest` (and the
//! implicit one in `InternetOpenUrl`) runs synchronously on the guest thread
//! while holding the WinAPI mutex — a blocked request blocks the guest thread
//! exactly like a parked wait, the same model `ws2_32` uses for `recv`.
//!
//! Protocol surface (documented limits — never fake success):
//! * `http://` only; `https://` / `ftp://` are rejected with
//!   `ERROR_INTERNET_UNRECOGNIZED_SCHEME`.
//! * The response (head + body, chunked transfer-decoding applied) is fully
//!   buffered on the host before `InternetReadFile` serves it. Response head
//!   capped at 1 MiB, body at 64 MiB (a `Result` error beyond that).
//! * `InternetReadFile` serves head + body as one byte stream (the raw
//!   response head is not stripped) and returns FALSE once the buffer is
//!   exhausted — the exhaustion contract of this lane. The Windows contract
//!   returns TRUE with a zero byte count instead; real apps that loop on
//!   `read > 0` are unaffected.
//! * `InternetQueryOptionW/A` always fails with `ERROR_INVALID_PARAMETER` —
//!   no option data is faked.
//! * No TLS, no redirects, no proxy handling (a direct connection is always
//!   attempted, per `INTERNET_OPEN_TYPE_DIRECT`).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use ahash::HashMapExt;
use anyhow::{Context, Result, anyhow};

use crate::guest_memory::write_u32;
use crate::kernel32::{
    low_u32, read_ansi_string_from_cpu, read_stack_u64, read_wide_string_from_cpu,
};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// Fake `HINTERNET` handle base — clear of every other handle namespace
/// (`ws2_32` uses `0x5000_0000`, console `0x6000_0010`).
const HINTERNET_HANDLE_BASE: u64 = 0x5100_0000;
/// Default HTTP port when `InternetConnect`/URL parsing gives `0`.
const DEFAULT_HTTP_PORT: u16 = 80;
/// Cap for the buffered response head (status line + headers).
const MAX_RESPONSE_HEAD: usize = 1024 * 1024;
/// Cap for the buffered response body.
const MAX_RESPONSE_BODY: usize = 64 * 1024 * 1024;
/// Cap for a guest URL string.
const MAX_URL_LEN: usize = 4096;
/// `INTERNET_SERVICE_HTTP` (wininet.h).
const INTERNET_SERVICE_HTTP: u32 = 3;
/// `INTERNET_SERVICE_FTP` (wininet.h) — rejected, this lane is HTTP only.
const INTERNET_SERVICE_FTP: u32 = 1;
/// `INTERNET_CONNECTION_LAN` (wininet.h).
const INTERNET_CONNECTION_LAN: u32 = 0x2;

// Error codes (winerror.h / wininet.h).
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_INTERNET_CANNOT_CONNECT: u32 = 12029;
const ERROR_INTERNET_TIMEOUT: u32 = 12002;
const ERROR_INTERNET_INVALID_URL: u32 = 12004;
const ERROR_INTERNET_UNRECOGNIZED_SCHEME: u32 = 12003;

/// Guest `HINTERNET` handle tables, owned by this module.
#[derive(Debug)]
pub struct WininetState {
    /// Live session handles (`InternetOpen`).
    sessions: ahash::HashMap<u64, WininetSession>,
    /// Live connection handles (`InternetConnect`).
    connections: ahash::HashMap<u64, WininetConnection>,
    /// Live request handles (`InternetOpenUrl` / `HttpOpenRequest`).
    requests: ahash::HashMap<u64, WininetRequest>,
    /// Monotonic fake-`HINTERNET` allocator.
    next_handle: u64,
}

impl Default for WininetState {
    fn default() -> Self {
        Self {
            sessions: ahash::HashMap::new(),
            connections: ahash::HashMap::new(),
            requests: ahash::HashMap::new(),
            next_handle: HINTERNET_HANDLE_BASE,
        }
    }
}

impl WininetState {
    /// Allocate the next fake `HINTERNET` value.
    fn alloc_handle(&mut self) -> u64 {
        let handle = self.next_handle;
        self.next_handle = handle.wrapping_add(1);
        handle
    }
}

/// A session handle's host-side identity: the user-agent string only.
#[derive(Debug, Default)]
struct WininetSession {
    agent: String,
}

/// A connection handle's host-side identity: the server endpoint plus the
/// session's user-agent (propagated from `InternetOpen`).
#[derive(Debug)]
struct WininetConnection {
    host: String,
    port: u16,
    agent: String,
}

/// A request handle: the HTTP request description plus the buffered response.
#[derive(Debug)]
struct WininetRequest {
    verb: String,
    path: String,
    host: String,
    port: u16,
    version: String,
    /// Extra headers merged from `HttpOpenRequest` + `HttpSendRequest`
    /// (raw bytes, sent verbatim after the request line).
    headers: Vec<u8>,
    /// Filled by the perform step (`HttpSendRequest` / `InternetOpenUrl`).
    status_code: u16,
    status_line: String,
    /// Buffered response: raw head bytes followed by the (chunked-decoded)
    /// body. `consumed` marks how much `InternetReadFile` has served.
    data: Vec<u8>,
    consumed: usize,
}

impl WininetRequest {
    fn unperformed(verb: String, path: String, host: String, port: u16) -> Self {
        Self {
            verb,
            path,
            host,
            port,
            version: "HTTP/1.1".to_owned(),
            headers: Vec::new(),
            status_code: 0,
            status_line: String::new(),
            data: Vec::new(),
            consumed: 0,
        }
    }
}

/// Parsed `http://` URL parts shared with `urlmon`.
pub(crate) struct HttpUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// The fully-buffered host response shared with `urlmon`.
pub(crate) struct HttpResult {
    pub status_code: u16,
    pub status_line: String,
    pub head: Vec<u8>,
    pub body: Vec<u8>,
}

/// Canonical handler tail (mirrors `ws2_32::finish`).
fn finish(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("wininet return")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// Finish with the process last-error recorded and `FALSE` returned.
fn finish_false(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    code: u32,
) -> Result<WinApiHandlerResult> {
    state.process.last_error = code;
    finish(engine, 0)
}

/// Dispatch a `wininet.dll` export by name (case-insensitive).
pub fn dispatch_wininet(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    dispatch_export(ctx, WININET_EXPORTS, name)
}

/// Census oracle: which `wininet.dll` exports are implemented.
pub fn is_export(name: &str) -> bool {
    export_listed(WININET_EXPORTS, name)
}

/// One implemented export of a string-dispatched DLL: the census name
/// (lowercase) plus its handler.
type ExportHandler = fn(&mut HandlerContext<'_>) -> Result<WinApiHandlerResult>;

/// Every implemented `wininet.dll` export — the single source shared by
/// [`dispatch_wininet`] and the census oracle [`is_export`].
const WININET_EXPORTS: &[(&str, ExportHandler)] = &[
    ("internetopenw", handle_internet_open_w),
    ("internetopena", handle_internet_open_a),
    ("internetclosehandle", handle_internet_close_handle),
    ("internetconnectw", handle_internet_connect_w),
    ("internetconnecta", handle_internet_connect_a),
    ("internetopenurlw", handle_internet_open_url_w),
    ("internetopenurla", handle_internet_open_url_a),
    ("httpopenrequestw", handle_http_open_request_w),
    ("httpopenrequesta", handle_http_open_request_a),
    ("httpsendrequestw", handle_http_send_request_w),
    ("httpsendrequesta", handle_http_send_request_a),
    ("internetreadfile", handle_internet_read_file),
    ("internetsetoptionw", handle_internet_set_option_w),
    ("internetsetoptiona", handle_internet_set_option_a),
    (
        "internetgetconnectedstate",
        handle_internet_get_connected_state,
    ),
    ("internetqueryoptionw", handle_internet_query_option_w),
    ("internetqueryoptiona", handle_internet_query_option_a),
];

/// Route `name` through an export table; `None` when it is not implemented.
fn dispatch_export(
    ctx: &mut HandlerContext<'_>,
    exports: &[(&str, ExportHandler)],
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let wanted = name.to_ascii_lowercase();
    let Some((_, handler)) = exports
        .iter()
        .find(|(export, _)| *export == wanted.as_str())
    else {
        return Ok(None);
    };
    handler(ctx).map(Some)
}

/// Whether a lowercased `name` appears in an export table.
fn export_listed(exports: &[(&str, ExportHandler)], name: &str) -> bool {
    let wanted = name.to_ascii_lowercase();
    exports.iter().any(|(export, _)| *export == wanted.as_str())
}

/// Parse `http://host[:port]/path` — the only scheme the lane supports.
pub(crate) fn parse_http_url(url: &str) -> Result<HttpUrl> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("unsupported URL scheme (only `http://` is implemented)"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_owned()),
    };
    let (host, port) = match authority.split_once(':') {
        Some((host, port_raw)) => {
            let port = port_raw
                .parse::<u16>()
                .with_context(|| format!("invalid URL port {port_raw:?}"))?;
            (host, port)
        }
        None => (authority, DEFAULT_HTTP_PORT),
    };
    if host.is_empty() {
        anyhow::bail!("URL has no host");
    }
    Ok(HttpUrl {
        host: host.to_owned(),
        port,
        path,
    })
}

/// Read the response head: everything up to and including the first
/// CRLFCRLF, capped at [`MAX_RESPONSE_HEAD`]. Bytes beyond the terminator
/// stay buffered in `reader`.
fn read_response_head(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut head = Vec::new();
    loop {
        // Scope the `fill_buf` borrow: `consume` below needs `reader` mutable.
        let probe = {
            let window = reader
                .fill_buf()
                .context("wininet: failed reading response head")?;
            if window.is_empty() {
                anyhow::bail!("wininet: connection closed before the response head");
            }
            let prior_len = head.len();
            head.extend_from_slice(window);
            let found = head.windows(4).position(|w| w == b"\r\n\r\n");
            (found, prior_len, window.len())
        };
        let (found, prior_len, window_len) = probe;
        if let Some(head_end) = found {
            let head_end = head_end.wrapping_add(4);
            reader.consume(head_end.saturating_sub(prior_len));
            head.truncate(head_end);
            return Ok(head);
        }
        if head.len() > MAX_RESPONSE_HEAD {
            anyhow::bail!("wininet: response head exceeds {MAX_RESPONSE_HEAD} bytes");
        }
        reader.consume(window_len);
    }
}

/// Read a chunked-transfer-encoded body until the `0` chunk, appending each
/// decoded chunk to `body`. Trailing headers are drained to the empty line.
fn read_chunked_body(reader: &mut impl BufRead, body: &mut Vec<u8>) -> Result<()> {
    loop {
        let mut size_line = Vec::new();
        reader
            .read_until(b'\n', &mut size_line)
            .context("wininet: failed reading chunk size")?;
        let size_text = String::from_utf8_lossy(&size_line);
        let size_text = size_text.trim();
        // Chunk extensions (`size;ext=val`) end the size token.
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .with_context(|| format!("wininet: invalid chunk size {size_text:?}"))?;
        if size == 0 {
            break;
        }
        if body.len().saturating_add(size) > MAX_RESPONSE_BODY {
            anyhow::bail!("wininet: chunked body exceeds {MAX_RESPONSE_BODY} bytes");
        }
        let mut chunk = vec![0_u8; size];
        reader
            .read_exact(&mut chunk)
            .context("wininet: failed reading chunk payload")?;
        body.extend_from_slice(&chunk);
        // Each chunk payload is followed by CRLF.
        let mut crlf = [0_u8; 2];
        reader
            .read_exact(&mut crlf)
            .context("wininet: missing chunk trailer")?;
    }
    // Trailing headers end with an empty line (EOF before it is tolerated).
    loop {
        let mut trailer = Vec::new();
        reader
            .read_until(b'\n', &mut trailer)
            .context("wininet: failed reading chunk trailers")?;
        if trailer == b"\r\n" || trailer == b"\n" || trailer.is_empty() {
            return Ok(());
        }
    }
}

/// Read the remaining response body to EOF, capped at [`MAX_RESPONSE_BODY`].
fn read_body_to_eof(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut tmp = [0_u8; 16 * 1024];
    loop {
        let n = reader
            .read(&mut tmp)
            .context("wininet: failed reading body")?;
        if n == 0 {
            break;
        }
        let chunk = tmp.get(..n).context("wininet: body slice out of range")?;
        body.extend_from_slice(chunk);
        if body.len() > MAX_RESPONSE_BODY {
            anyhow::bail!("wininet: response body exceeds {MAX_RESPONSE_BODY} bytes");
        }
    }
    Ok(body)
}

/// Extract the 3-digit status code from a status line like
/// `HTTP/1.1 200 OK` (`0` when the line is malformed).
fn parse_status_code(status_line: &str) -> u16 {
    status_line
        .split(' ')
        .nth(1)
        .and_then(|token| token.parse::<u16>().ok())
        .unwrap_or(0)
}

/// Whether `headers` already contains a line whose name (the part before
/// `:`) equals `name` (case-insensitive) — guards against emitting
/// duplicate headers such as `Host:` / `User-Agent:`.
fn headers_contain_named(headers: &[u8], name: &str) -> bool {
    String::from_utf8_lossy(headers)
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .any(|line| {
            line.get(..name.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
        })
}

/// Whether `headers` already contains a `Host:` line.
fn headers_contain_host(headers: &[u8]) -> bool {
    headers_contain_named(headers, "host:")
}

/// Prepend `User-Agent: <agent>` unless the headers already carry one.
///
/// The agent string from `InternetOpen` is what WinInet sends as the
/// `User-Agent` header; a caller-supplied header wins over it.
fn apply_user_agent(headers: &mut Vec<u8>, agent: &str) {
    if agent.is_empty() || headers_contain_named(headers, "user-agent:") {
        return;
    }
    let mut with_agent =
        Vec::with_capacity(headers.len().saturating_add(agent.len()).saturating_add(24));
    with_agent.extend_from_slice(b"User-Agent: ");
    with_agent.extend_from_slice(agent.as_bytes());
    with_agent.extend_from_slice(b"\r\n");
    with_agent.extend_from_slice(headers);
    *headers = with_agent;
}

/// Perform one HTTP/1.1 exchange over a fresh `TcpStream` and buffer the
/// whole response (head + body, chunked decoding applied) on the host.
pub(crate) fn perform_http_request(
    verb: &str,
    host: &str,
    port: u16,
    path: &str,
    version: &str,
    extra_headers: &[u8],
    body: &[u8],
) -> Result<HttpResult> {
    let mut stream = TcpStream::connect((host, port))
        .with_context(|| format!("wininet: connect to {host}:{port} failed"))?;

    let mut request = Vec::new();
    request.extend_from_slice(verb.as_bytes());
    request.push(b' ');
    request.extend_from_slice(path.as_bytes());
    request.push(b' ');
    request.extend_from_slice(version.as_bytes());
    request.extend_from_slice(b"\r\n");
    if !headers_contain_host(extra_headers) {
        request.extend_from_slice(b"Host: ");
        request.extend_from_slice(host.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(extra_headers);
    // A close-terminated exchange matches the full-buffer read below.
    request.extend_from_slice(b"Connection: close\r\n");
    if !body.is_empty() {
        request.extend_from_slice(b"Content-Length: ");
        request.extend_from_slice(body.len().to_string().as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(body);
    stream
        .write_all(&request)
        .context("wininet: failed writing request")?;

    let mut reader = BufReader::new(stream);
    let head = read_response_head(&mut reader)?;
    let head_text = String::from_utf8_lossy(&head);
    let mut head_lines = head_text.split("\r\n");
    let status_line = head_lines.next().unwrap_or("").to_owned();
    let status_code = parse_status_code(&status_line);

    let mut content_length = None;
    let mut chunked = false;
    for line in head_lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse::<usize>().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }

    let body = if chunked {
        let mut body = Vec::new();
        read_chunked_body(&mut reader, &mut body)?;
        body
    } else if let Some(len) = content_length {
        if len > MAX_RESPONSE_BODY {
            anyhow::bail!("wininet: response body exceeds {MAX_RESPONSE_BODY} bytes");
        }
        let mut body = vec![0_u8; len];
        reader
            .read_exact(&mut body)
            .context("wininet: failed reading response body")?;
        body
    } else {
        read_body_to_eof(&mut reader)?
    };

    Ok(HttpResult {
        status_code,
        status_line,
        head,
        body,
    })
}

/// Run the HTTP exchange against `request` and store the buffered result.
///
/// The response bytes served by `InternetReadFile` are the raw head followed
/// by the body — the status line stays visible in the data stream (the micro
/// suite scans it for the status code).
fn perform_and_store(request: &mut WininetRequest, body: &[u8]) -> Result<()> {
    let result = perform_http_request(
        &request.verb,
        &request.host,
        request.port,
        &request.path,
        &request.version,
        &request.headers,
        body,
    )?;
    request.status_code = result.status_code;
    request.status_line = result.status_line;
    let mut data = result.head;
    data.extend_from_slice(&result.body);
    request.data = data;
    request.consumed = 0;
    Ok(())
}

/// Map a request-perform failure to a graceful `FALSE` + last error.
fn finish_perform_failure(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    err: &anyhow::Error,
) -> Result<WinApiHandlerResult> {
    tracing::debug!("wininet request failed: {err:#}");
    let code = if err.to_string().contains("timed out") {
        ERROR_INTERNET_TIMEOUT
    } else {
        ERROR_INTERNET_CANNOT_CONNECT
    };
    finish_false(engine, state, code)
}

/// Which character encoding a WinInet A/W pair uses for its string
/// parameters.
#[derive(Debug, Clone, Copy)]
enum StringEncoding {
    /// UTF-16LE (`…W` exports).
    Wide,
    /// The ANSI code page (`…A` exports).
    Ansi,
}

impl StringEncoding {
    /// Read a NUL-terminated guest string of this encoding, capped at `max`
    /// units.
    fn read_string(
        self,
        engine: &mut dyn wie_cpu::CpuEngine,
        va: u64,
        max: usize,
    ) -> Result<String> {
        match self {
            Self::Wide => read_wide_string_from_cpu(engine, va, max),
            Self::Ansi => read_ansi_string_from_cpu(engine, va, max),
        }
    }

    /// Read a header string of this encoding from guest memory as raw wire
    /// bytes (CP1252, the ACP — the format headers are sent in).
    fn read_header_bytes(self, engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Result<Vec<u8>> {
        if va == 0 {
            return Ok(Vec::new());
        }
        let text = self.read_string(engine, va, MAX_URL_LEN)?;
        Ok(crate::guest_string::encode_cp1252(&text))
    }
}

/// `HINTERNET InternetOpenW(LPCWSTR lpszAgent, DWORD dwAccessType,
///                          LPCWSTR lpszProxyName, LPCWSTR lpszProxyBypass,
///                          DWORD dwFlags)` shared body.
///
/// Creates a session handle. The access type / proxy arguments are accepted
/// and ignored — this lane always connects directly.
fn handle_internet_open(
    ctx: &mut HandlerContext<'_>,
    enc: StringEncoding,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let agent_va = engine.read_rcx()?;
    let _access_type = engine.read_rdx()?;
    let _proxy = engine.read_r8()?;
    let _proxy_bypass = engine.read_r9()?;
    let _flags = read_stack_u64(engine, 0x28)?;
    let agent = if agent_va == 0 {
        String::new()
    } else {
        enc.read_string(engine, agent_va, 256)?
    };
    let handle = {
        let wininet = state.wininet();
        let handle = wininet.alloc_handle();
        wininet.sessions.insert(handle, WininetSession { agent });
        handle
    };
    finish(engine, handle)
}

/// `HINTERNET InternetOpenW(...)` — the wide variant.
fn handle_internet_open_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_internet_open(ctx, StringEncoding::Wide)
}

/// `HINTERNET InternetOpenA(LPCSTR lpszAgent, ...)` — ANSI variant.
fn handle_internet_open_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_internet_open(ctx, StringEncoding::Ansi)
}

/// `BOOL InternetCloseHandle(HINTERNET hInternet)` — frees any session /
/// connection / request handle.
fn handle_internet_close_handle(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let h = engine.read_rcx()?;
    let removed = {
        let wininet = state.wininet();
        wininet.sessions.remove(&h).is_some()
            || wininet.connections.remove(&h).is_some()
            || wininet.requests.remove(&h).is_some()
    };
    finish(engine, u64::from(removed))
}

/// `HINTERNET InternetConnect*` shared body.
fn handle_internet_connect(
    ctx: &mut HandlerContext<'_>,
    enc: StringEncoding,
    api: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let session_handle = engine.read_rcx()?;
    let server_va = engine.read_rdx()?;
    let port_raw = low_u32(engine.read_r8()?, &format!("{api} port"))?;
    let _user_va = engine.read_r9()?;
    let _password_va = read_stack_u64(engine, 0x28)?;
    let service = low_u32(read_stack_u64(engine, 0x30)?, &format!("{api} service"))?;
    let _flags = read_stack_u64(engine, 0x38)?;
    let _context = read_stack_u64(engine, 0x40)?;
    let agent = {
        let wininet = state.wininet();
        match wininet.sessions.get(&session_handle) {
            Some(session) => session.agent.clone(),
            None => return finish_false(engine, state, ERROR_INVALID_HANDLE),
        }
    };
    // HTTP only; FTP connections fail honestly instead of being served HTTP.
    if service != INTERNET_SERVICE_HTTP && service != 0 {
        if service == INTERNET_SERVICE_FTP {
            return finish_false(engine, state, ERROR_INTERNET_CANNOT_CONNECT);
        }
        return finish_false(engine, state, ERROR_INVALID_PARAMETER);
    }
    let server = enc.read_string(engine, server_va, 1024)?;
    // A zero port means "the service default" — HTTP default is 80.
    let port = if port_raw == 0 {
        DEFAULT_HTTP_PORT
    } else {
        u16::try_from(port_raw & 0xffff).unwrap_or(0)
    };
    let handle = {
        let wininet = state.wininet();
        let handle = wininet.alloc_handle();
        wininet.connections.insert(
            handle,
            WininetConnection {
                host: server,
                port,
                agent,
            },
        );
        handle
    };
    finish(engine, handle)
}

/// `HINTERNET InternetConnectW(HINTERNET hInternet, LPCWSTR lpszServerName,
///                             INTERNET_PORT nServerPort, LPCWSTR lpszUserName,
///                             LPCWSTR lpszPassword, DWORD dwService,
///                             DWORD dwFlags, DWORD_PTR dwContext)`
fn handle_internet_connect_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_internet_connect(ctx, StringEncoding::Wide, "InternetConnectW")
}

/// `HINTERNET InternetConnectA(...)` — ANSI variant.
fn handle_internet_connect_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_internet_connect(ctx, StringEncoding::Ansi, "InternetConnectA")
}

/// `HINTERNET InternetOpenUrl*` shared body: create a request handle and
/// perform the implicit `GET` immediately.
fn handle_internet_open_url(
    ctx: &mut HandlerContext<'_>,
    enc: StringEncoding,
    api: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let session_handle = engine.read_rcx()?;
    let url_va = engine.read_rdx()?;
    let headers_va = engine.read_r8()?;
    let _headers_len = engine.read_r9()?;
    let _flags = read_stack_u64(engine, 0x28)?;
    let _context = read_stack_u64(engine, 0x30)?;
    let agent = {
        let wininet = state.wininet();
        match wininet.sessions.get(&session_handle) {
            Some(session) => session.agent.clone(),
            None => return finish_false(engine, state, ERROR_INVALID_HANDLE),
        }
    };
    let url = enc.read_string(engine, url_va, MAX_URL_LEN)?;
    let parsed = match parse_http_url(&url) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::debug!(%url, "{api} URL parse failed: {err:#}");
            let code = if url.starts_with("https://") || url.starts_with("ftp://") {
                ERROR_INTERNET_UNRECOGNIZED_SCHEME
            } else {
                ERROR_INTERNET_INVALID_URL
            };
            return finish_false(engine, state, code);
        }
    };
    let headers = enc.read_header_bytes(engine, headers_va)?;
    let handle = {
        let wininet = state.wininet();
        let handle = wininet.alloc_handle();
        let mut request =
            WininetRequest::unperformed("GET".to_owned(), parsed.path, parsed.host, parsed.port);
        request.headers = headers;
        apply_user_agent(&mut request.headers, &agent);
        wininet.requests.insert(handle, request);
        handle
    };
    let outcome = {
        let wininet = state.wininet();
        match wininet.requests.get_mut(&handle) {
            Some(request) => perform_and_store(request, &[]),
            None => anyhow::bail!("wininet: request handle vanished"),
        }
    };
    match outcome {
        Ok(()) => finish(engine, handle),
        Err(err) => finish_perform_failure(engine, state, &err),
    }
}

/// `HINTERNET InternetOpenUrlW(HINTERNET hInternet, LPCWSTR lpszUrl,
///                             LPCWSTR lpszHeaders, DWORD dwHeadersLength,
///                             DWORD dwFlags, DWORD_PTR dwContext)`
fn handle_internet_open_url_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_internet_open_url(ctx, StringEncoding::Wide, "InternetOpenUrlW")
}

/// `HINTERNET InternetOpenUrlA(...)` — ANSI variant.
fn handle_internet_open_url_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_internet_open_url(ctx, StringEncoding::Ansi, "InternetOpenUrlA")
}

/// `HINTERNET HttpOpenRequest*` shared body.
fn handle_http_open_request(
    ctx: &mut HandlerContext<'_>,
    enc: StringEncoding,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let conn_handle = engine.read_rcx()?;
    let verb_va = engine.read_rdx()?;
    let object_va = engine.read_r8()?;
    let version_va = engine.read_r9()?;
    let _referrer_va = read_stack_u64(engine, 0x28)?;
    let _accept_types_va = read_stack_u64(engine, 0x30)?;
    let _flags = read_stack_u64(engine, 0x38)?;
    let _context = read_stack_u64(engine, 0x40)?;
    let (host, port, agent) = {
        let wininet = state.wininet();
        match wininet.connections.get(&conn_handle) {
            Some(conn) => (conn.host.clone(), conn.port, conn.agent.clone()),
            None => return finish_false(engine, state, ERROR_INVALID_HANDLE),
        }
    };
    // A NULL verb defaults to GET (WinInet semantics).
    let verb = if verb_va == 0 {
        "GET".to_owned()
    } else {
        enc.read_string(engine, verb_va, 64)?
    };
    let path = enc.read_string(engine, object_va, MAX_URL_LEN)?;
    let version = if version_va == 0 {
        "HTTP/1.1".to_owned()
    } else {
        enc.read_string(engine, version_va, 32)?
    };
    let handle = {
        let wininet = state.wininet();
        let handle = wininet.alloc_handle();
        let mut request = WininetRequest::unperformed(verb, path, host, port);
        request.version = version;
        apply_user_agent(&mut request.headers, &agent);
        wininet.requests.insert(handle, request);
        handle
    };
    finish(engine, handle)
}

/// `HINTERNET HttpOpenRequestW(HINTERNET hConnect, LPCWSTR lpszVerb,
///                             LPCWSTR lpszObjectName, LPCWSTR lpszVersion,
///                             LPCWSTR lpszReferrer, LPCWSTR *lplpszAcceptTypes,
///                             DWORD dwFlags, DWORD_PTR dwContext)`
fn handle_http_open_request_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_http_open_request(ctx, StringEncoding::Wide)
}

/// `HINTERNET HttpOpenRequestA(...)` — ANSI variant.
fn handle_http_open_request_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_http_open_request(ctx, StringEncoding::Ansi)
}

/// `BOOL HttpSendRequest*` shared body.
///
/// Performs the HTTP exchange over a fresh `TcpStream`: the send-time headers
/// are merged into the request and `lpOptional` becomes the request body.
fn handle_http_send_request(
    ctx: &mut HandlerContext<'_>,
    enc: StringEncoding,
    api: &str,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let req_handle = engine.read_rcx()?;
    let headers_va = engine.read_rdx()?;
    let _headers_len = engine.read_r8()?;
    let optional_va = engine.read_r9()?;
    let optional_len = low_u32(
        read_stack_u64(engine, 0x28)?,
        &format!("{api} optional length"),
    )?;
    let send_headers = enc.read_header_bytes(engine, headers_va)?;
    let body_len = usize::try_from(optional_len).context("optional length does not fit usize")?;
    let mut body = vec![0_u8; body_len];
    if optional_va != 0 && !body.is_empty() {
        engine.mem_read(optional_va, &mut body)?;
    }
    let outcome = {
        let wininet = state.wininet();
        match wininet.requests.get_mut(&req_handle) {
            Some(request) => {
                request.headers.extend_from_slice(&send_headers);
                perform_and_store(request, &body)
            }
            None => anyhow::bail!("wininet: request handle not found"),
        }
    };
    match outcome {
        Ok(()) => finish(engine, 1),
        Err(err) => finish_perform_failure(engine, state, &err),
    }
}

/// `BOOL HttpSendRequestW(HINTERNET hRequest, LPCWSTR lpszHeaders,
///                        DWORD dwHeadersLength, LPVOID lpOptional,
///                        DWORD dwOptionalLength)`
fn handle_http_send_request_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_http_send_request(ctx, StringEncoding::Wide, "HttpSendRequestW")
}

/// `BOOL HttpSendRequestA(...)` — ANSI variant.
fn handle_http_send_request_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_http_send_request(ctx, StringEncoding::Ansi, "HttpSendRequestA")
}

/// `BOOL InternetReadFile(HINTERNET hFile, LPVOID lpBuffer,
///                        DWORD dwNumberOfBytesToRead,
///                        LPDWORD lpdwNumberOfBytesRead)`
///
/// Serves the buffered response (head + body). Returns TRUE while bytes were
/// served (or remain to be served); FALSE once the buffer is exhausted, with
/// the byte count written as zero — the exhaustion contract of this lane.
fn handle_internet_read_file(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let h = engine.read_rcx()?;
    let buffer_va = engine.read_rdx()?;
    let bytes_to_read = low_u32(engine.read_r8()?, "InternetReadFile size")?;
    let bytes_read_va = engine.read_r9()?;
    let outcome: Result<(usize, bool), anyhow::Error> = {
        let wininet = state.wininet();
        match wininet.requests.get_mut(&h) {
            Some(request) => {
                let available = request.data.len().saturating_sub(request.consumed);
                let want = usize::try_from(bytes_to_read).unwrap_or(0).min(available);
                if want > 0 {
                    let end = request.consumed.wrapping_add(want);
                    let slice = request
                        .data
                        .get(request.consumed..end)
                        .context("wininet: read slice out of range")?;
                    engine.mem_write(buffer_va, slice)?;
                    request.consumed = end;
                }
                // TRUE while data remains (a zero-length read is not the
                // end); FALSE only once the buffer is fully consumed.
                Ok((want, available > 0))
            }
            None => anyhow::bail!("wininet: request handle not found"),
        }
    };
    match outcome {
        Ok((copied, more)) => {
            if bytes_read_va != 0 {
                write_u32(engine, bytes_read_va, u32::try_from(copied).unwrap_or(0))?;
            }
            finish(engine, u64::from(more))
        }
        Err(err) => {
            tracing::debug!("InternetReadFile failed: {err:#}");
            finish_false(engine, state, ERROR_INVALID_HANDLE)
        }
    }
}

/// Whether `option` is a `InternetSetOption` option this lane accepts and
/// ignores. These are the tuning knobs whose effects are not observable in
/// the single-process direct-connect model (timeouts, buffer sizes, keep
/// alive, user agent override, …).
fn is_ignored_option(option: u32) -> bool {
    matches!(
        option,
        2  // INTERNET_OPTION_CONNECT_TIMEOUT
        | 5  // INTERNET_OPTION_SEND_TIMEOUT
        | 6  // INTERNET_OPTION_RECEIVE_TIMEOUT
        | 7  // INTERNET_OPTION_DATA_SEND_TIMEOUT
        | 8  // INTERNET_OPTION_DATA_RECEIVE_TIMEOUT
        | 11 // INTERNET_OPTION_CONNECT_RETRIES
        | 31 // INTERNET_OPTION_SECURITY_FLAGS
        | 32 // INTERNET_OPTION_KEEP_CONNECTION
        | 33 // INTERNET_OPTION_DISABLE_COOKIES
        | 37 // INTERNET_OPTION_REFRESH
        | 39 // INTERNET_OPTION_SETTINGS_CHANGED
        | 41 // INTERNET_OPTION_USER_AGENT
        | 43 // INTERNET_OPTION_READ_BUFFER_SIZE
        | 44 // INTERNET_OPTION_WRITE_BUFFER_SIZE
        | 48 // INTERNET_OPTION_HTTP_VERSION
        | 70 // INTERNET_OPTION_DISABLE_AUTODIAL
    )
}

/// `BOOL InternetSetOptionW(HINTERNET hInternet, DWORD dwOption,
///                          LPVOID lpBuffer, DWORD dwBufferLength)`
///
/// Accepts-and-ignores the common tuning options; unknown options fail with
/// `ERROR_INVALID_PARAMETER` instead of faking success. The option values are
/// encoding-independent, so the ANSI variant delegates here.
fn handle_internet_set_option_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _h = engine.read_rcx()?;
    let option = low_u32(engine.read_rdx()?, "InternetSetOptionW option")?;
    let _buffer_va = engine.read_r8()?;
    let _buffer_len = engine.read_r9()?;
    if is_ignored_option(option) {
        finish(engine, 1)
    } else {
        finish_false(engine, state, ERROR_INVALID_PARAMETER)
    }
}

/// `BOOL InternetSetOptionA(...)` — ANSI variant (same option values).
fn handle_internet_set_option_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_internet_set_option_w(ctx)
}

/// `BOOL InternetGetConnectedState(LPDWORD lpdwFlags, DWORD dwReserved)`
///
/// Reports a LAN connection (this lane reaches the network directly).
fn handle_internet_get_connected_state(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let flags_va = engine.read_rcx()?;
    let _reserved = engine.read_rdx()?;
    if flags_va != 0 {
        write_u32(engine, flags_va, INTERNET_CONNECTION_LAN)?;
    }
    finish(engine, 1)
}

/// `BOOL InternetQueryOptionW(HINTERNET hInternet, DWORD dwOption,
///                            LPVOID lpBuffer, LPDWORD lpdwBufferLength)`
///
/// Always fails with `ERROR_INVALID_PARAMETER` — no option data is faked.
fn handle_internet_query_option_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _h = engine.read_rcx()?;
    let _option = engine.read_rdx()?;
    let _buffer_va = engine.read_r8()?;
    let _buffer_len_va = engine.read_r9()?;
    finish_false(engine, state, ERROR_INVALID_PARAMETER)
}

/// `BOOL InternetQueryOptionA(...)` — ANSI variant.
fn handle_internet_query_option_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_internet_query_option_w(ctx)
}
