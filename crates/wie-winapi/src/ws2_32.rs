//! Handles `WS2_32.dll` — Winsock / network APIs (string dispatch).
//!
//! Host strategy: real sockets via `std::net` on loopback (no FFI — `unsafe`
//! is denied in this crate). `Ws2State` owns the guest `SOCKET` → host socket
//! table and lives in a `DllStateMap` slot, heap-allocated on first load.
//!
//! Blocking semantics: handlers run synchronously on the guest thread, so a
//! blocking `recv`/`accept`/`connect` blocks the guest thread exactly like a
//! parked wait (`WaitForSingleObject`). The WinAPI mutex stays held — the same
//! model the spec prescribes for this lane.
//!
//! Known limits (documented, never fake success):
//! * `AF_INET` only; `SOCK_STREAM` only (no UDP/raw sockets).
//! * `connect` on an already-`bind`-ed socket fails with `WSAEINVAL` —
//!   `std::net` cannot connect a socket that was materialised as a listener.
//! * `select` is a KISS non-blocking poll loop; see its handler for the
//!   documented probing semantics.
//! * `getaddrinfo`/`gethostbyname` are minimal IPv4 resolvers (single-result
//!   chains written into guest memory with the null-terminated layouts the
//!   Windows headers define).
use std::io::{Read, Write};
use std::net::{
    Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs,
};
use std::time::{Duration, Instant};

use ahash::HashMapExt;
use anyhow::{Context, Result};

use crate::guest_memory::{
    read_i32, read_u16, read_u32, read_u64, write_u16, write_u32, write_u64,
};
use crate::kernel32::{low_u32, read_ansi_string_from_cpu, read_stack_u64};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

/// `AF_INET` (ws2def.h).
const AF_INET: u16 = 2;
/// `SOCK_STREAM` (winsock2.h).
const SOCK_STREAM: i32 = 1;
/// `IPPROTO_TCP` (winsock2.h).
const IPPROTO_TCP: i32 = 6;
/// `FD_SETSIZE` (winsock2.h).
const FD_SETSIZE: usize = 64;
/// `INVALID_SOCKET` — `(SOCKET)(~0)` (winsock2.h).
const INVALID_SOCKET: u64 = u64::MAX;
/// `SOCKET_ERROR` — `-1`, sign-extended to the register width.
const SOCKET_ERROR: u64 = u64::MAX;
/// Fake `SOCKET` handle base — clear of every other handle namespace.
const SOCKET_HANDLE_BASE: u64 = 0x5000_0000;
/// Guest `sockaddr_in` size (16 bytes).
const SOCKADDR_IN_SIZE: u64 = 16;
/// Cap for one `send`/`recv` copy (real Winsock buffers are far smaller).
const MAX_IO_COPY: usize = 64 * 1024 * 1024;
/// `FIONBIO` (winsock2.h), 32-bit `u_long` width.
const FIONBIO_32: u32 = 0x8004_667E;
/// `FIONBIO` (winsock2.h), 64-bit `u_long` width — what a Win64 guest passes.
const FIONBIO_64: u32 = 0x8008_667E;
/// Poll period of the `select` loop.
const SELECT_POLL_PERIOD: Duration = Duration::from_millis(1);

// Winsock error codes (winsock2.h). Only the ones this module produces.
const WSA_NOT_ENOUGH_MEMORY: u32 = 10008;
const WSAEINTR: u32 = 10004;
const WSAEACCES: u32 = 10013;
const WSAEFAULT: u32 = 10014;
const WSAEINVAL: u32 = 10022;
const WSAEWOULDBLOCK: u32 = 10035;
const WSAEISCONN: u32 = 10056;
const WSAENOTSOCK: u32 = 10038;
const WSAEOPNOTSUPP: u32 = 10045;
const WSAEAFNOSUPPORT: u32 = 10047;
const WSAESOCKTNOSUPPORT: u32 = 10044;
const WSAEPROTONOSUPPORT: u32 = 10043;
const WSAEADDRINUSE: u32 = 10048;
const WSAEADDRNOTAVAIL: u32 = 10049;
const WSAENETUNREACH: u32 = 10051;
const WSAECONNABORTED: u32 = 10053;
const WSAECONNRESET: u32 = 10054;
const WSAENOTCONN: u32 = 10057;
const WSAETIMEDOUT: u32 = 10060;
const WSAECONNREFUSED: u32 = 10061;
const WSAEHOSTUNREACH: u32 = 10065;
const WSASYSCALLFAILURE: u32 = 10107;
const WSAHOST_NOT_FOUND: u32 = 11001;

/// Guest `SOCKET` → host socket state, owned by this module.
#[derive(Debug)]
pub struct Ws2State {
    /// Live guest sockets, keyed by the fake `SOCKET` handle.
    sockets: ahash::HashMap<u64, HostSocket>,
    /// Connections already pulled out of a listener by the `select` probe,
    /// keyed by the listener handle; drained by the next `accept`.
    pending_accepts: ahash::HashMap<u64, Vec<TcpStream>>,
    /// Monotonic fake-`SOCKET` allocator.
    next_handle: u64,
    /// The `inet_ntoa` static buffer (16 bytes, allocated on first use).
    ntoa_buf: u64,
}

impl Default for Ws2State {
    fn default() -> Self {
        Self {
            sockets: ahash::HashMap::new(),
            pending_accepts: ahash::HashMap::new(),
            next_handle: SOCKET_HANDLE_BASE,
            ntoa_buf: 0,
        }
    }
}

impl Ws2State {
    /// Allocate a fake `SOCKET` for `sock` and hand back the handle.
    fn insert_socket(&mut self, sock: HostSocket) -> u64 {
        let handle = self.next_handle;
        self.next_handle = handle.wrapping_add(1);
        self.sockets.insert(handle, sock);
        handle
    }
}

/// Host side of a guest `SOCKET`.
#[derive(Debug)]
enum HostSocket {
    /// `socket()`-created but not yet `bind()`-ed. `bind` materialises the
    /// host listener; `connect` materialises the host stream.
    Unbound,
    /// Listening TCP endpoint (std::net binds and listens in one step).
    TcpListener(TcpListener),
    /// Connected TCP stream.
    TcpStream(TcpStream),
}

/// Canonical handler tail (mirrors `ole32::finish`).
fn finish(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("ws2_32 return")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// Finish with a Winsock last-error recorded and `SOCKET_ERROR` returned.
fn finish_wsa_error(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    code: u32,
) -> Result<WinApiHandlerResult> {
    state.process.last_error = code;
    finish(engine, SOCKET_ERROR)
}

/// Map a host `io::Error` to the closest Winsock code.
fn wsa_error_from_io(err: &std::io::Error) -> u32 {
    match err.kind() {
        std::io::ErrorKind::ConnectionRefused => WSAECONNREFUSED,
        std::io::ErrorKind::ConnectionReset => WSAECONNRESET,
        std::io::ErrorKind::ConnectionAborted => WSAECONNABORTED,
        std::io::ErrorKind::NotConnected => WSAENOTCONN,
        std::io::ErrorKind::AddrInUse => WSAEADDRINUSE,
        std::io::ErrorKind::AddrNotAvailable => WSAEADDRNOTAVAIL,
        std::io::ErrorKind::TimedOut => WSAETIMEDOUT,
        std::io::ErrorKind::WouldBlock => WSAEWOULDBLOCK,
        std::io::ErrorKind::PermissionDenied => WSAEACCES,
        std::io::ErrorKind::Interrupted => WSAEINTR,
        std::io::ErrorKind::InvalidInput => WSAEINVAL,
        std::io::ErrorKind::Unsupported => WSAEOPNOTSUPP,
        std::io::ErrorKind::NetworkUnreachable => WSAENETUNREACH,
        std::io::ErrorKind::HostUnreachable => WSAEHOSTUNREACH,
        _ => WSASYSCALLFAILURE,
    }
}

/// Read a guest `sockaddr_in` (family u16, port u16 network-order, addr 4
/// bytes). `None` for a NULL pointer, a too-short buffer, or a non-`AF_INET`
/// family — the callers turn that into `WSAEINVAL`.
fn read_guest_sockaddr(
    engine: &mut dyn wie_cpu::CpuEngine,
    name_va: u64,
    namelen: u64,
) -> Result<Option<SocketAddrV4>> {
    if name_va == 0 || namelen < SOCKADDR_IN_SIZE {
        return Ok(None);
    }
    let family = read_u16(engine, name_va)?;
    if family != AF_INET {
        return Ok(None);
    }
    let port_raw = read_u16(engine, name_va.wrapping_add(2))?;
    let mut octets = [0_u8; 4];
    engine.mem_read(name_va.wrapping_add(4), &mut octets)?;
    Ok(Some(SocketAddrV4::new(
        Ipv4Addr::from(octets),
        port_raw.swap_bytes(),
    )))
}

/// Write a host address into a guest `sockaddr_in`, updating `*namelen` to
/// the full 16 bytes. `sin_zero` is left untouched (Windows zeroes it, but
/// bind/connect callers never read it).
fn write_guest_sockaddr(
    engine: &mut dyn wie_cpu::CpuEngine,
    name_va: u64,
    namelen_va: u64,
    addr: SocketAddrV4,
) -> Result<()> {
    if name_va != 0 {
        write_u16(engine, name_va, AF_INET)?;
        write_u16(engine, name_va.wrapping_add(2), addr.port().swap_bytes())?;
        engine.mem_write(name_va.wrapping_add(4), &addr.ip().octets())?;
    }
    if namelen_va != 0 {
        write_u32(
            engine,
            namelen_va,
            u32::try_from(SOCKADDR_IN_SIZE).unwrap_or(0),
        )?;
    }
    Ok(())
}

/// Read the socket handles of a guest `fd_set` (u32 count + u64 array).
/// A NULL pointer is an empty set (Windows accepts NULL `fd_set` pointers).
fn read_fd_set(engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Result<Vec<u64>> {
    if va == 0 {
        return Ok(Vec::new());
    }
    let count = usize::try_from(u64::from(read_u32(engine, va)?))
        .context("fd_set count does not fit usize")?
        .min(FD_SETSIZE);
    let mut socks = Vec::with_capacity(count);
    for i in 0..count {
        let entry_va = va
            .wrapping_add(4)
            .wrapping_add(u64::try_from(i).unwrap_or(0).wrapping_mul(8));
        socks.push(read_u64(engine, entry_va)?);
    }
    Ok(socks)
}

/// Write a socket list back into a guest `fd_set` (Windows updates the sets
/// in place to contain only the ready sockets).
fn write_fd_set(engine: &mut dyn wie_cpu::CpuEngine, va: u64, socks: &[u64]) -> Result<()> {
    if va == 0 {
        return Ok(());
    }
    write_u32(engine, va, u32::try_from(socks.len()).unwrap_or(0))?;
    for (i, s) in socks.iter().enumerate() {
        let entry_va = va
            .wrapping_add(4)
            .wrapping_add(u64::try_from(i).unwrap_or(0).wrapping_mul(8));
        write_u64(engine, entry_va, *s)?;
    }
    Ok(())
}

/// Read the `struct timeval` timeout of `select` (two 32-bit `long`s on
/// Windows, even for Win64). NULL timeout = wait forever.
fn read_select_timeout(engine: &mut dyn wie_cpu::CpuEngine, va: u64) -> Result<Option<Duration>> {
    if va == 0 {
        return Ok(None);
    }
    let secs = u64::from(read_u32(engine, va)?);
    let micros =
        u32::try_from(u64::from(read_u32(engine, va.wrapping_add(4))?).saturating_mul(1000))
            .unwrap_or(u32::MAX);
    Ok(Some(Duration::new(secs, micros)))
}

/// Probe a socket for readability without consuming queued data.
///
/// A `TcpStream` is probed with `peek` (data stays queued); a `TcpListener`
/// with a non-blocking `accept`, whose accepted connection is parked in
/// [`Ws2State::pending_accepts`] and served by the next `accept` call. Both
/// are restored to blocking mode after the probe. Unknown handles and
/// genuine errors report "not ready" — the app's own call surfaces them.
fn socket_readable(ws2: &mut Ws2State, handle: u64) -> bool {
    let Some(slot) = ws2.sockets.get_mut(&handle) else {
        return false;
    };
    match slot {
        HostSocket::TcpStream(stream) => {
            if stream.set_nonblocking(true).is_err() {
                return false;
            }
            let mut probe = [0_u8; 1];
            // Ok(0) = peer closed (readable, EOF); Ok(_) = data queued (readable).
            let readable = stream.peek(&mut probe).is_ok();
            drop(stream.set_nonblocking(false));
            readable
        }
        HostSocket::TcpListener(listener) => {
            if listener.set_nonblocking(true).is_err() {
                return false;
            }
            let readable = match listener.accept() {
                Ok((stream, _peer)) => {
                    ws2.pending_accepts.entry(handle).or_default().push(stream);
                    true
                }
                Err(_) => false,
            };
            drop(listener.set_nonblocking(false));
            readable
        }
        HostSocket::Unbound => false,
    }
}

/// Probe a socket for writability with a zero-length write (non-consuming).
///
/// A zero-length write succeeds whenever the send side is open, so this
/// reports a shutdown/closed stream as not-ready and everything else as
/// ready — TCP send buffers are normally available and a true check needs
/// host `poll(2)`, which this crate denies (no FFI).
fn socket_writable(ws2: &mut Ws2State, handle: u64) -> bool {
    let Some(slot) = ws2.sockets.get_mut(&handle) else {
        return false;
    };
    match slot {
        HostSocket::TcpStream(stream) => {
            if stream.set_nonblocking(true).is_err() {
                return false;
            }
            let writable = stream.write(&[]).is_ok();
            drop(stream.set_nonblocking(false));
            writable
        }
        HostSocket::TcpListener(_) | HostSocket::Unbound => false,
    }
}

/// Dispatch a `WS2_32.dll` export by name (case-insensitive).
pub fn dispatch_ws2(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "wsastartup" => Ok(Some(handle_wsastartup(ctx)?)),
        "wsacleanup" => Ok(Some(handle_wsacleanup(ctx)?)),
        "wsagetlasterror" => Ok(Some(handle_wsagetlasterror(ctx)?)),
        "wsasetlasterror" => Ok(Some(handle_wsasetlasterror(ctx)?)),
        "socket" => Ok(Some(handle_socket(ctx)?)),
        "closesocket" => Ok(Some(handle_closesocket(ctx)?)),
        "bind" => Ok(Some(handle_bind(ctx)?)),
        "listen" => Ok(Some(handle_listen(ctx)?)),
        "accept" => Ok(Some(handle_accept(ctx)?)),
        "connect" => Ok(Some(handle_connect(ctx)?)),
        "send" => Ok(Some(handle_send(ctx)?)),
        "recv" => Ok(Some(handle_recv(ctx)?)),
        "select" => Ok(Some(handle_select(ctx)?)),
        "getaddrinfo" => Ok(Some(handle_getaddrinfo(ctx)?)),
        "freeaddrinfo" => Ok(Some(handle_freeaddrinfo(ctx)?)),
        "gethostbyname" => Ok(Some(handle_gethostbyname(ctx)?)),
        "inet_addr" => Ok(Some(handle_inet_addr(ctx)?)),
        "inet_ntoa" => Ok(Some(handle_inet_ntoa(ctx)?)),
        "htons" => Ok(Some(handle_htons(ctx)?)),
        "ntohs" => Ok(Some(handle_ntohs(ctx)?)),
        "getsockname" => Ok(Some(handle_getsockname(ctx)?)),
        "getpeername" => Ok(Some(handle_getpeername(ctx)?)),
        "setsockopt" => Ok(Some(handle_setsockopt(ctx)?)),
        "getsockopt" => Ok(Some(handle_getsockopt(ctx)?)),
        "shutdown" => Ok(Some(handle_shutdown(ctx)?)),
        "ioctlsocket" => Ok(Some(handle_ioctlsocket(ctx)?)),
        _ => Ok(None),
    }
}

/// `int WSAStartup(WORD wVersionRequested, LPWSADATA lpWSAData)`
///
/// Always succeeds (a real Winsock stack is available via `std::net`). The
/// `WSADATA` version fields echo the request; `wHighVersion` is 2.2.
fn handle_wsastartup(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let version_requested = low_u32(engine.read_rcx()?, "WSAStartup wVersionRequested")?;
    let wsadata_va = engine.read_rdx()?;
    if wsadata_va != 0 {
        // WSADATA layout (winsock2.h): wVersion, wHighVersion, iMaxSockets,
        // iMaxUdpDg, lpVendorInfo, then the two fixed ANSI strings.
        write_u16(
            engine,
            wsadata_va,
            u16::try_from(version_requested & 0xffff).unwrap_or(0),
        )?;
        write_u16(engine, wsadata_va.wrapping_add(2), 0x0202)?; // wHighVersion
        write_u16(engine, wsadata_va.wrapping_add(4), 0)?; // iMaxSockets
        write_u16(engine, wsadata_va.wrapping_add(6), 0)?; // iMaxUdpDg
        write_u64(engine, wsadata_va.wrapping_add(8), 0)?; // lpVendorInfo
        // szDescription is at offset 16 (257 chars), szSystemStatus at 273.
        crate::guest_string::write_fixed_ansi(
            engine,
            wsadata_va.wrapping_add(16),
            257,
            b"WIE Winsock",
        )?;
        crate::guest_string::write_fixed_ansi(
            engine,
            wsadata_va.wrapping_add(273),
            129,
            b"Running",
        )?;
    }
    finish(engine, 0)
}

/// `int WSACleanup(void)` — no per-process bookkeeping to tear down.
fn handle_wsacleanup(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    finish(engine, 0)
}

/// `int WSAGetLastError(void)` — routes through the process last-error slot,
/// mirroring how the `KERNEL32` `GetLastError` handler works.
fn handle_wsagetlasterror(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let state = &mut *ctx.state;
    let value = u64::from(state.process.last_error);
    finish(ctx.engine, value)
}

/// `int WSASetLastError(int iError)`
fn handle_wsasetlasterror(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let error_raw = engine.read_rcx()?;
    let error = u32::try_from(error_raw & 0xffff_ffff).context("WSASetLastError value")?;
    state.process.last_error = error;
    finish(engine, 0)
}

/// `SOCKET socket(int af, int type, int protocol)`
///
/// Only `AF_INET` + `SOCK_STREAM` is backed by a host socket. The handle
/// starts as [`HostSocket::Unbound`]; `bind`/`connect` materialise the host
/// object.
fn handle_socket(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let af = low_u32(engine.read_rcx()?, "socket af")?;
    let ty = low_u32(engine.read_rdx()?, "socket type")?;
    let proto = low_u32(engine.read_r8()?, "socket protocol")?;
    if af != u32::from(AF_INET) {
        return finish_wsa_error(engine, state, WSAEAFNOSUPPORT);
    }
    if ty != u32::try_from(SOCK_STREAM).unwrap_or(0) {
        return finish_wsa_error(engine, state, WSAESOCKTNOSUPPORT);
    }
    // A non-zero protocol must be TCP for a stream socket.
    if proto != 0 && proto != u32::try_from(IPPROTO_TCP).unwrap_or(0) {
        return finish_wsa_error(engine, state, WSAEPROTONOSUPPORT);
    }
    let handle = {
        let ws2 = state.ws2();
        ws2.insert_socket(HostSocket::Unbound)
    };
    finish(engine, handle)
}

/// `int closesocket(SOCKET s)` — dropping the host object closes it.
fn handle_closesocket(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let (value, err) = {
        let ws2 = state.ws2();
        let removed = ws2.sockets.remove(&s).is_some();
        ws2.pending_accepts.remove(&s);
        if removed {
            (0, 0)
        } else {
            (SOCKET_ERROR, WSAENOTSOCK)
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int bind(SOCKET s, const struct sockaddr *name, int namelen)`
///
/// Materialises the host listener on the guest address. `std::net` binds and
/// listens in one step, so `listen` below is then a no-op.
fn handle_bind(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let name_va = engine.read_rdx()?;
    let namelen = engine.read_r8()?;
    let Some(addr) = read_guest_sockaddr(engine, name_va, namelen)? else {
        return finish_wsa_error(engine, state, WSAEINVAL);
    };
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(slot) => match slot {
                HostSocket::Unbound => match TcpListener::bind(SocketAddr::V4(addr)) {
                    Ok(listener) => {
                        *slot = HostSocket::TcpListener(listener);
                        (0, 0)
                    }
                    Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
                },
                _ => (SOCKET_ERROR, WSAEINVAL),
            },
            None => (SOCKET_ERROR, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int listen(SOCKET s, int backlog)` — a no-op on a bound socket (std::net
/// already listens); `WSAEINVAL` before `bind`, like Windows.
fn handle_listen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let _backlog = engine.read_rdx()?;
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(slot) => match slot {
                HostSocket::TcpListener(_) => (0, 0),
                HostSocket::Unbound | HostSocket::TcpStream(_) => (SOCKET_ERROR, WSAEINVAL),
            },
            None => (SOCKET_ERROR, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `SOCKET accept(SOCKET s, struct sockaddr *addr, int *addrlen)`
///
/// Drains a connection already pulled from the listener by the `select`
/// probe; otherwise blocks on the host listener.
fn handle_accept(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let addr_va = engine.read_rdx()?;
    let addrlen_va = engine.read_r8()?;
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(HostSocket::TcpListener(listener)) => {
                let pending = ws2
                    .pending_accepts
                    .get_mut(&s)
                    .and_then(|queue| queue.pop());
                match pending {
                    Some(stream) => {
                        let peer = stream.peer_addr().ok();
                        let handle = ws2.insert_socket(HostSocket::TcpStream(stream));
                        if let Some(SocketAddr::V4(peer_v4)) = peer {
                            write_guest_sockaddr(engine, addr_va, addrlen_va, peer_v4)?;
                        }
                        (handle, 0)
                    }
                    None => match listener.accept() {
                        Ok((stream, SocketAddr::V4(peer))) => {
                            let handle = ws2.insert_socket(HostSocket::TcpStream(stream));
                            write_guest_sockaddr(engine, addr_va, addrlen_va, peer)?;
                            (handle, 0)
                        }
                        Ok((_stream, _)) => (INVALID_SOCKET, WSAEAFNOSUPPORT),
                        Err(e) => (INVALID_SOCKET, wsa_error_from_io(&e)),
                    },
                }
            }
            _ => (INVALID_SOCKET, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int connect(SOCKET s, const struct sockaddr *name, int namelen)`
///
/// Materialises the host stream. A refused loopback connect surfaces
/// `WSAECONNREFUSED` through the last-error slot.
fn handle_connect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let name_va = engine.read_rdx()?;
    let namelen = engine.read_r8()?;
    let Some(addr) = read_guest_sockaddr(engine, name_va, namelen)? else {
        return finish_wsa_error(engine, state, WSAEINVAL);
    };
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(slot) => match slot {
                HostSocket::Unbound => match TcpStream::connect(SocketAddr::V4(addr)) {
                    Ok(stream) => {
                        *slot = HostSocket::TcpStream(stream);
                        (0, 0)
                    }
                    Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
                },
                HostSocket::TcpStream(_) => (SOCKET_ERROR, WSAEISCONN),
                HostSocket::TcpListener(_) => (SOCKET_ERROR, WSAEINVAL),
            },
            None => (SOCKET_ERROR, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int send(SOCKET s, const char *buf, int len, int flags)`
///
/// Blocks until the host accepts the bytes (the guest thread blocks, like a
/// park). Flags are ignored — `MSG_DONTROUTE` has no loopback effect.
fn handle_send(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let buf_va = engine.read_rdx()?;
    let len = low_u32(engine.read_r8()?, "send len")?;
    let _flags = engine.read_r9()?;
    let len_usize = usize::try_from(len).context("send len does not fit usize")?;
    if len_usize > MAX_IO_COPY {
        return finish_wsa_error(engine, state, WSAEINVAL);
    }
    let mut bytes = vec![0_u8; len_usize];
    engine.mem_read(buf_va, &mut bytes)?;
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(HostSocket::TcpStream(stream)) => match stream.write(&bytes) {
                Ok(n) => (u64::try_from(n).unwrap_or(0), 0),
                Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
            },
            _ => (SOCKET_ERROR, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int recv(SOCKET s, char *buf, int len, int flags)`
fn handle_recv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let buf_va = engine.read_rdx()?;
    let len = low_u32(engine.read_r8()?, "recv len")?;
    let _flags = engine.read_r9()?;
    let len_usize = usize::try_from(len).context("recv len does not fit usize")?;
    if len_usize > MAX_IO_COPY {
        return finish_wsa_error(engine, state, WSAEINVAL);
    }
    let mut bytes = vec![0_u8; len_usize];
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(HostSocket::TcpStream(stream)) => match stream.read(&mut bytes) {
                Ok(0) => (0, 0), // graceful peer close → recv returns 0
                Ok(n) => {
                    let written = bytes.get(..n).context("recv slice out of range")?;
                    engine.mem_write(buf_va, written)?;
                    (u64::try_from(n).unwrap_or(0), 0)
                }
                Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
            },
            _ => (SOCKET_ERROR, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int getsockname(SOCKET s, struct sockaddr *name, int *namelen)`
fn handle_getsockname(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let name_va = engine.read_rdx()?;
    let namelen_va = engine.read_r8()?;
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(slot) => match slot {
                HostSocket::TcpListener(listener) => match listener.local_addr() {
                    Ok(SocketAddr::V4(addr)) => {
                        write_guest_sockaddr(engine, name_va, namelen_va, addr)?;
                        (0, 0)
                    }
                    Ok(_) => (SOCKET_ERROR, WSAEAFNOSUPPORT),
                    Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
                },
                HostSocket::TcpStream(stream) => match stream.local_addr() {
                    Ok(SocketAddr::V4(addr)) => {
                        write_guest_sockaddr(engine, name_va, namelen_va, addr)?;
                        (0, 0)
                    }
                    Ok(_) => (SOCKET_ERROR, WSAEAFNOSUPPORT),
                    Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
                },
                HostSocket::Unbound => (SOCKET_ERROR, WSAEINVAL),
            },
            None => (SOCKET_ERROR, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int getpeername(SOCKET s, struct sockaddr *name, int *namelen)`
fn handle_getpeername(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let name_va = engine.read_rdx()?;
    let namelen_va = engine.read_r8()?;
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(HostSocket::TcpStream(stream)) => match stream.peer_addr() {
                Ok(SocketAddr::V4(addr)) => {
                    write_guest_sockaddr(engine, name_va, namelen_va, addr)?;
                    (0, 0)
                }
                Ok(_) => (SOCKET_ERROR, WSAEAFNOSUPPORT),
                Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
            },
            // Unbound / listening sockets are not connected.
            Some(HostSocket::Unbound) | Some(HostSocket::TcpListener(_)) => {
                (SOCKET_ERROR, WSAENOTCONN)
            }
            None => (SOCKET_ERROR, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int shutdown(SOCKET s, int how)` — SD_RECEIVE=0, SD_SEND=1, SD_BOTH=2.
fn handle_shutdown(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let how = low_u32(engine.read_rdx()?, "shutdown how")?;
    let how = match how {
        0 => Shutdown::Read,
        1 => Shutdown::Write,
        2 => Shutdown::Both,
        _ => return finish_wsa_error(engine, state, WSAEINVAL),
    };
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(HostSocket::TcpStream(stream)) => match stream.shutdown(how) {
                Ok(()) => (0, 0),
                Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
            },
            _ => (SOCKET_ERROR, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int setsockopt(SOCKET s, int level, int optname, const char *optval,
///                 int optlen)`
///
/// Accepts-and-ignores the common options (`SO_REUSEADDR`, `TCP_NODELAY`, …).
/// The socket must exist and the option bytes are read (validating the guest
/// buffer) but not interpreted — their effects are not observable on the
/// loopback single-process model this lane implements.
fn handle_setsockopt(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let _level = engine.read_rdx()?;
    let _optname = engine.read_r8()?;
    let optval_va = engine.read_r9()?;
    let optlen = low_u32(read_stack_u64(engine, 0x28)?, "setsockopt optlen")?;
    let probe_len = usize::try_from(optlen).unwrap_or(0).min(64);
    let mut dummy = vec![0_u8; probe_len];
    if optval_va != 0 && !dummy.is_empty() {
        engine.mem_read(optval_va, &mut dummy)?;
    }
    let exists = {
        let ws2 = state.ws2();
        ws2.sockets.contains_key(&s)
    };
    if !exists {
        return finish_wsa_error(engine, state, WSAENOTSOCK);
    }
    finish(engine, 0)
}

/// `int getsockopt(SOCKET s, int level, int optname, char *optval,
///                 int *optlen)`
///
/// Reports the option defaults the echo model can stand behind: a zero int
/// and length 4 (SO_ERROR is 0 — no pending error; TCP_NODELAY is off).
fn handle_getsockopt(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let _level = engine.read_rdx()?;
    let _optname = engine.read_r8()?;
    let optval_va = engine.read_r9()?;
    let optlen_va = read_stack_u64(engine, 0x28)?;
    let exists = {
        let ws2 = state.ws2();
        ws2.sockets.contains_key(&s)
    };
    if !exists {
        return finish_wsa_error(engine, state, WSAENOTSOCK);
    }
    if optval_va != 0 {
        write_u32(engine, optval_va, 0)?;
    }
    if optlen_va != 0 {
        write_u32(engine, optlen_va, 4)?;
    }
    finish(engine, 0)
}

/// `int ioctlsocket(SOCKET s, long cmd, u_long *argp)`
///
/// `FIONBIO` flips non-blocking mode (1 = non-blocking, 0 = blocking) on the
/// host socket. Other commands fail with `WSAEINVAL` — honest, not faked.
fn handle_ioctlsocket(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let s = engine.read_rcx()?;
    let cmd = low_u32(engine.read_rdx()?, "ioctlsocket cmd")?;
    let argp_va = engine.read_r8()?;
    if (cmd != FIONBIO_32 && cmd != FIONBIO_64) || argp_va == 0 {
        return finish_wsa_error(engine, state, WSAEINVAL);
    }
    let enable = read_u32(engine, argp_va)? != 0;
    let (value, err) = {
        let ws2 = state.ws2();
        match ws2.sockets.get_mut(&s) {
            Some(slot) => match slot {
                HostSocket::TcpStream(stream) => match stream.set_nonblocking(enable) {
                    Ok(()) => (0, 0),
                    Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
                },
                HostSocket::TcpListener(listener) => match listener.set_nonblocking(enable) {
                    Ok(()) => (0, 0),
                    Err(e) => (SOCKET_ERROR, wsa_error_from_io(&e)),
                },
                HostSocket::Unbound => (SOCKET_ERROR, WSAEINVAL),
            },
            None => (SOCKET_ERROR, WSAENOTSOCK),
        }
    };
    if err != 0 {
        return finish_wsa_error(engine, state, err);
    }
    finish(engine, value)
}

/// `int select(int nfds, fd_set *readfds, fd_set *writefds, fd_set *exceptfds,
///             const struct timeval *timeout)`
///
/// KISS poll loop: every candidate socket is set non-blocking, probed, and
/// restored to blocking. The probes are non-consuming — see
/// [`socket_readable`]. `writefds` reports its streams ready (zero-length
/// write probe). One probe round per millisecond until the timeout elapses;
/// a NULL timeout waits forever. The result `fd_set`s are rewritten in place
/// with the ready handles and the ready count returned.
fn handle_select(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _nfds = engine.read_rcx()?;
    let readfds_va = engine.read_rdx()?;
    let writefds_va = engine.read_r8()?;
    let _exceptfds_va = engine.read_r9()?;
    let timeout_va = read_stack_u64(engine, 0x28)?;

    let read_candidates = read_fd_set(engine, readfds_va)?;
    let write_candidates = read_fd_set(engine, writefds_va)?;
    let timeout = read_select_timeout(engine, timeout_va)?;
    let deadline = timeout.and_then(|t| Instant::now().checked_add(t));

    let ready_reads;
    let ready_writes;
    loop {
        let (rr, rw) = {
            let ws2 = state.ws2();
            (
                read_candidates
                    .iter()
                    .copied()
                    .filter(|h| socket_readable(ws2, *h))
                    .collect::<Vec<u64>>(),
                write_candidates
                    .iter()
                    .copied()
                    .filter(|h| socket_writable(ws2, *h))
                    .collect::<Vec<u64>>(),
            )
        };
        if !rr.is_empty() || !rw.is_empty() {
            ready_reads = rr;
            ready_writes = rw;
            break;
        }
        match deadline {
            None => std::thread::sleep(SELECT_POLL_PERIOD),
            Some(d) => {
                if Instant::now() >= d {
                    ready_reads = Vec::new();
                    ready_writes = Vec::new();
                    break;
                }
                std::thread::sleep(SELECT_POLL_PERIOD);
            }
        }
    }

    write_fd_set(engine, readfds_va, &ready_reads)?;
    write_fd_set(engine, writefds_va, &ready_writes)?;
    // A socket ready in both sets counts once (Windows semantics).
    let mut count = u64::try_from(ready_writes.len()).unwrap_or(0);
    for h in &ready_reads {
        if !ready_writes.contains(h) {
            count = count.saturating_add(1);
        }
    }
    finish(engine, count)
}

/// `int getaddrinfo(const char *node, const char *service,
///                  const struct addrinfo *hints, struct addrinfo **result)`
///
/// Minimal resolver: "localhost", IP literals, and hostnames resolve through
/// the host `ToSocketAddrs`; IPv4 only; single-result chain. The chain — one
/// 48-byte `addrinfo` plus its 16-byte `sockaddr_in` and NUL-terminated
/// canonical name — lives in one guest-heap block so `freeaddrinfo` frees a
/// single chunk. A service *name* ("http") is not resolved (numeric ports
/// only); a NULL node binds `INADDR_ANY`.
fn handle_getaddrinfo(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let node_va = engine.read_rcx()?;
    let service_va = engine.read_rdx()?;
    let hints_va = engine.read_r8()?;
    let result_va = engine.read_r9()?;

    // Clear *result up front — callers rely on it on failure.
    if result_va == 0 {
        return finish(engine, u64::from(WSAEFAULT));
    }
    write_u64(engine, result_va, 0)?;

    let node = if node_va == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, node_va, 1024)?
    };
    let service = if service_va == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, service_va, 1024)?
    };

    // Hints (ai_flags and ai_canonname are ignored): family at +4, socktype
    // at +8, protocol at +12.
    let mut hint_family = 0_i32;
    let mut hint_socktype = 0_i32;
    let mut hint_protocol = 0_i32;
    if hints_va != 0 {
        hint_family = read_i32(engine, hints_va.wrapping_add(4))?;
        hint_socktype = read_i32(engine, hints_va.wrapping_add(8))?;
        hint_protocol = read_i32(engine, hints_va.wrapping_add(12))?;
    }
    if hint_family != 0 && hint_family != i32::from(AF_INET) {
        return finish(engine, u64::from(WSAEAFNOSUPPORT));
    }

    let port = if service.is_empty() {
        0_u16
    } else {
        match service.parse::<u16>() {
            Ok(p) => p,
            Err(_) => return finish(engine, u64::from(WSAEINVAL)),
        }
    };

    let ip = if node.is_empty() {
        Ipv4Addr::new(0, 0, 0, 0)
    } else {
        let mut found = None;
        if let Ok(mut addrs) = (node.as_str(), port).to_socket_addrs() {
            for a in addrs.by_ref() {
                if let SocketAddr::V4(v4) = a {
                    found = Some(*v4.ip());
                    break;
                }
            }
        }
        let Some(ip) = found else {
            return finish(engine, u64::from(WSAHOST_NOT_FOUND));
        };
        ip
    };

    // Chain layout: addrinfo(48) | sockaddr_in(16) | canon name(NUL-terminated).
    let canon = if node.is_empty() {
        "localhost".to_owned()
    } else {
        node.clone()
    };
    let name_len = u64::try_from(canon.len().saturating_add(1)).unwrap_or(0);
    let total = 48_u64
        .saturating_add(SOCKADDR_IN_SIZE)
        .saturating_add(name_len);
    let base = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, total);
    if base == 0 {
        return finish(engine, u64::from(WSA_NOT_ENOUGH_MEMORY));
    }
    let sockaddr_va = base.wrapping_add(48);
    let canon_va = base.wrapping_add(48).wrapping_add(SOCKADDR_IN_SIZE);

    let socktype = if hint_socktype == 0 {
        SOCK_STREAM
    } else {
        hint_socktype
    };
    let protocol = if hint_protocol == 0 {
        if socktype == 2 { 17 } else { IPPROTO_TCP }
    } else {
        hint_protocol
    };
    write_u32(engine, base, 0)?; // ai_flags
    write_u32(engine, base.wrapping_add(4), u32::from(AF_INET))?; // ai_family
    write_u32(
        engine,
        base.wrapping_add(8),
        u32::try_from(socktype).unwrap_or(0),
    )?; // ai_socktype
    write_u32(
        engine,
        base.wrapping_add(12),
        u32::try_from(protocol).unwrap_or(0),
    )?; // ai_protocol
    write_u64(engine, base.wrapping_add(16), SOCKADDR_IN_SIZE)?; // ai_addrlen
    write_u64(engine, base.wrapping_add(24), canon_va)?; // ai_canonname
    write_u64(engine, base.wrapping_add(32), sockaddr_va)?; // ai_addr
    write_u64(engine, base.wrapping_add(40), 0)?; // ai_next
    write_u16(engine, sockaddr_va, AF_INET)?;
    write_u16(engine, sockaddr_va.wrapping_add(2), port.swap_bytes())?;
    engine.mem_write(sockaddr_va.wrapping_add(4), &ip.octets())?;
    let mut name_bytes = Vec::with_capacity(canon.len().saturating_add(1));
    name_bytes.extend_from_slice(canon.as_bytes());
    name_bytes.push(0);
    engine.mem_write(canon_va, &name_bytes)?;

    write_u64(engine, result_va, base)?;
    finish(engine, 0)
}

/// `void freeaddrinfo(struct addrinfo *ai)` — frees the single guest-heap
/// block `getaddrinfo` allocated.
fn handle_freeaddrinfo(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let ai = engine.read_rcx()?;
    if ai != 0 {
        let _ = state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .free_coherent(engine, ai);
    }
    finish(engine, 0)
}

/// `struct hostent *gethostbyname(const char *name)`
///
/// Minimal: "localhost" resolves to 127.0.0.1; an IP literal resolves to
/// itself; anything else fails with `WSAHOST_NOT_FOUND`. The `hostent` (one
/// guest-heap block) mirrors the Windows layout: h_name, a single NULL
/// alias, `AF_INET`/length 4, and a NULL-terminated `h_addr_list` of one
/// address.
fn handle_gethostbyname(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let name_va = engine.read_rcx()?;
    let name = if name_va == 0 {
        String::new()
    } else {
        read_ansi_string_from_cpu(engine, name_va, 1024)?
    };
    let ip = if name.eq_ignore_ascii_case("localhost") {
        Ipv4Addr::new(127, 0, 0, 1)
    } else {
        match name.parse::<Ipv4Addr>() {
            Ok(ip) => ip,
            Err(_) => return finish_wsa_error(engine, state, WSAHOST_NOT_FOUND),
        }
    };
    // Layout: hostent(32) | aliases(8) | addr_list(16) | addr(4) | name.
    let name_len = u64::try_from(name.len().saturating_add(1)).unwrap_or(0);
    let total = 32_u64
        .saturating_add(8)
        .saturating_add(16)
        .saturating_add(4)
        .saturating_add(name_len);
    let base = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, total);
    if base == 0 {
        return finish_wsa_error(engine, state, WSA_NOT_ENOUGH_MEMORY);
    }
    let aliases_va = base.wrapping_add(32);
    let addr_list_va = base.wrapping_add(40);
    let addr_va = base.wrapping_add(56);
    let host_name_va = base.wrapping_add(60);
    write_u64(engine, base, host_name_va)?; // h_name
    write_u64(engine, base.wrapping_add(8), aliases_va)?; // h_aliases
    write_u16(engine, base.wrapping_add(16), AF_INET)?; // h_addrtype
    write_u16(engine, base.wrapping_add(18), 4)?; // h_length
    write_u64(engine, base.wrapping_add(24), addr_list_va)?; // h_addr_list
    write_u64(engine, aliases_va, 0)?; // one NULL alias
    write_u64(engine, addr_list_va, addr_va)?;
    write_u64(engine, addr_list_va.wrapping_add(8), 0)?;
    engine.mem_write(addr_va, &ip.octets())?;
    let mut name_bytes = Vec::with_capacity(name.len().saturating_add(1));
    name_bytes.extend_from_slice(name.as_bytes());
    name_bytes.push(0);
    engine.mem_write(host_name_va, &name_bytes)?;
    finish(engine, base)
}

/// `unsigned long inet_addr(const char *cp)` — dotted quad → `in_addr` value.
///
/// The returned u32, stored into `in_addr::S_addr`, must reproduce the quad
/// bytes in guest memory — on the little-endian guest that is
/// `a | b<<8 | c<<16 | d<<24`. Malformed input returns `INADDR_NONE`
/// (0xFFFFFFFF), the documented failure value.
fn handle_inet_addr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let cp_va = engine.read_rcx()?;
    let cp = read_ansi_string_from_cpu(engine, cp_va, 64)?;
    let mut parts = cp.split('.');
    let a = parts.next().and_then(|s| s.parse::<u32>().ok());
    let b = parts.next().and_then(|s| s.parse::<u32>().ok());
    let c = parts.next().and_then(|s| s.parse::<u32>().ok());
    let d = parts.next().and_then(|s| s.parse::<u32>().ok());
    let trailing = parts.next();
    let (Some(a), Some(b), Some(c), Some(d), None) = (a, b, c, d, trailing) else {
        return finish(engine, u64::MAX);
    };
    if a > 255 || b > 255 || c > 255 || d > 255 {
        return finish(engine, u64::MAX);
    }
    let value = a | (b << 8) | (c << 16) | (d << 24);
    finish(engine, u64::from(value))
}

/// `char *inet_ntoa(struct in_addr in)` — address (passed by value in RCX)
/// → dotted quad in a static guest buffer, reused on the next call like the
/// real per-thread buffer.
fn handle_inet_ntoa(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    // The 4-byte struct travels in the low 32 bits of RCX; on the LE guest
    // its byte order equals the address octets in order.
    let raw = low_u32(engine.read_rcx()?, "inet_ntoa in")?;
    let text = Ipv4Addr::from(raw.to_le_bytes()).to_string();
    let mut buf = {
        let ws2 = state.ws2();
        ws2.ntoa_buf
    };
    if buf == 0 {
        buf = state
            .heap_state
            .heap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .alloc_coherent(engine, 16);
        let ws2 = state.ws2();
        ws2.ntoa_buf = buf;
    }
    if buf == 0 {
        return finish(engine, 0); // NULL — no heap for the static buffer
    }
    let mut bytes = Vec::with_capacity(text.len().saturating_add(1));
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(0);
    engine.mem_write(buf, &bytes)?;
    finish(engine, buf)
}

/// `u_short htons(u_short hostshort)` — host → network byte order.
fn handle_htons(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let v = low_u32(engine.read_rcx()?, "htons hostshort")?;
    let swapped = u16::try_from(v & 0xffff).unwrap_or(0).swap_bytes();
    finish(engine, u64::from(swapped))
}

/// `u_short ntohs(u_short netshort)` — network → host byte order. The same
/// byte swap on the little-endian guest.
fn handle_ntohs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    handle_htons(ctx)
}
