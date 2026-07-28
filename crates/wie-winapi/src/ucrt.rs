//! Host-side UCRT / `api-ms-win-crt-*` API set for PE64 CRT-linked programs.
//!
//! The UCRT emulation handles printf/scanf-style format parsing, locale helpers,
//! and ctype operations with intentional integer arithmetic, indexing, and casts.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::integer_division,
    clippy::cast_sign_loss,
    clippy::items_after_statements,
    clippy::cast_possible_wrap,
    clippy::unnecessary_wraps
)]
//!
//! Clean room: implement enough of the Universal CRT surface that a normal
//! mingw/MSVC CRT startup + `main` can run. Not a port of Wine/ReactOS UCRT.
//!
//! API set DLLs (`api-ms-win-crt-stdio-l1-1-0.dll`, …) are Windows forwarders to
//! `ucrtbase.dll`; we treat them as one dispatch namespace by export name.

use crate::guest_memory::read_u64 as read_guest_u64;
use crate::kernel32::create_guest_thread;
use crate::seh::{self, ThrowPayload};
use crate::sync_obj::KernelObject;
use crate::{GuestStdinMode, HandlerContext, WinApiControlSignal, WinApiHandlerResult};
use anyhow::{Context, Result};

/// Guest VA base for synthetic CRT objects (FILE cookies, env pointers, etc.).
const ACMDLN_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x328;
const CRT_GUEST_BASE: u64 = 0x0000_0000_6800_0000;
const FILE_STDIN: u64 = CRT_GUEST_BASE;
const FILE_STDOUT: u64 = CRT_GUEST_BASE + 0x100;
const FILE_STDERR: u64 = CRT_GUEST_BASE + 0x200;
const ENVIRON_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x300;
const ARGV_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x308;
const ARGC_SLOT: u64 = CRT_GUEST_BASE + 0x310;
const COMMODE_SLOT: u64 = CRT_GUEST_BASE + 0x318;
const FMODE_SLOT: u64 = CRT_GUEST_BASE + 0x320;

/// Return an ASCII-lowercased view of `name` without allocating when possible.
///
/// The vast majority of UCRT / msvcrt exports arrive already lowercase from PE
/// import tables. Detecting that lets dispatch skip a per-call `String` alloc.
/// Names that do contain uppercase are lowered into `scratch` (stack buffer);
/// names longer than the scratch buffer fall back to a heap allocation returned
/// via the `owned` out-parameter.
///
/// Returns a `&str` borrowing from either `name`, `scratch`, or `*owned`.
#[inline]
fn ascii_lower<'a>(name: &'a str, scratch: &'a mut [u8], owned: &'a mut Option<String>) -> &'a str {
    let bytes = name.as_bytes();
    if bytes.iter().all(|b| !b.is_ascii_uppercase()) {
        return name;
    }
    if let Some(dst) = scratch.get_mut(..bytes.len()) {
        for (d, &b) in dst.iter_mut().zip(bytes.iter()) {
            *d = b.to_ascii_lowercase();
        }
        // ASCII lowering only rewrites bytes < 0x80, so the result is still
        // valid UTF-8 and this check always succeeds. It is kept rather than
        // using `from_utf8_unchecked` because validating ≤96 bytes is a short
        // vectorised scan — not worth an `unsafe` block to skip.
        if let Ok(s) = std::str::from_utf8(dst) {
            return s;
        }
    }
    // Very long export name (>SCRATCH) — fall back to a heap-owned lowercase.
    *owned = Some(name.to_ascii_lowercase());
    owned.as_deref().unwrap_or(name)
}

/// Stack buffer size for ASCII-lower of dispatch names.
///
/// UCRT / msvcrt export names top out around ~30 bytes; the longest MSVC C++
/// mangled name we currently match is `??1type_info@@ueaa@xz` (21 bytes).
/// 96 gives comfortable headroom without cache-line waste.
const ASCII_LOWER_SCRATCH: usize = 96;

/// Whether `library` is a UCRT API-set or `ucrtbase`.
#[must_use]
pub fn is_ucrt_library(library: &str) -> bool {
    let mut scratch = [0_u8; ASCII_LOWER_SCRATCH];
    let mut owned: Option<String> = None;
    let l = ascii_lower(library, &mut scratch, &mut owned);
    l.starts_with("api-ms-win-crt-") || l == "ucrtbase.dll" || l == "msvcrt.dll"
}

/// Guest VA for legacy `msvcrt` **data** imports (`_fmode`, `_commode`, `_acmdln`).
///
/// These are not callable: the IAT slot must hold the address of the variable so
/// guest code can `mov` through it. Returns `None` for ordinary function exports.
#[must_use]
pub fn crt_data_import_va(name: &str) -> Option<u64> {
    let mut scratch = [0_u8; ASCII_LOWER_SCRATCH];
    let mut owned: Option<String> = None;
    let n = ascii_lower(name, &mut scratch, &mut owned);
    match n {
        "_fmode" => Some(FMODE_SLOT),
        "_commode" => Some(COMMODE_SLOT),
        "_acmdln" => Some(ACMDLN_PTR_SLOT),
        // Legacy msvcrt: `FILE _iob[]` / `char **__initenv`.
        // Point `_iob` at stdin cookie; fputs/fputc treat nearby streams as console.
        "_iob" => Some(FILE_STDIN),
        "__initenv" => Some(ENVIRON_PTR_SLOT),
        _ => None,
    }
}

/// Dispatch a UCRT export by name (case-insensitive).
pub fn dispatch_ucrt(ctx: &mut HandlerContext<'_>, name: &str) -> Result<WinApiHandlerResult> {
    let mut scratch = [0_u8; ASCII_LOWER_SCRATCH];
    let mut owned: Option<String> = None;
    let n = ascii_lower(name, &mut scratch, &mut owned);
    match n {
        "__acrt_iob_func" => handle_acrt_iob_func(ctx),
        "fwrite" => handle_fwrite(ctx),
        "fflush" => handle_fflush(ctx),
        "setvbuf" => handle_setvbuf(ctx),
        "__stdio_common_vfprintf" => handle_stdio_common_vfprintf(ctx),
        "__stdio_common_vsprintf" => handle_stdio_common_vsprintf(ctx),
        "__stdio_common_vsscanf" => handle_stdio_common_vsscanf(ctx),
        "malloc" => handle_malloc(ctx),
        "calloc" => handle_calloc(ctx),
        "free" => handle_free(ctx),
        "_set_new_mode" => handle_set_new_mode(ctx),
        "__p__environ" => handle_p_environ(ctx),
        "__p__acmdln" => handle_p_acmdln(ctx),
        "__p___argc" => handle_p_argc(ctx),
        "__p___argv" => handle_p_argv(ctx),
        "__p__commode" => handle_p_commode(ctx),
        "__p__fmode" => handle_p_fmode(ctx),
        "_configthreadlocale" => handle_config_thread_locale(ctx),
        "__setusermatherr" => handle_set_user_matherr(ctx),
        "__c_specific_handler" | "__cxxframehandler" => handle_c_specific_handler(ctx),
        "memcpy" | "memmove" => handle_memcpy(ctx),
        "memcmp" => handle_memcmp(ctx),
        "memset" => handle_memset(ctx),
        "strlen" => handle_strlen(ctx),
        "strncmp" => handle_strncmp(ctx),
        "_initterm" => handle_initterm(ctx),
        "_initterm_e" => handle_initterm_e(ctx),
        "_configure_narrow_argv" => handle_configure_narrow_argv(ctx),
        "_initialize_narrow_environment" => handle_initialize_narrow_environment(ctx),
        "_crt_atexit" => handle_crt_atexit(ctx),
        // UCRT: `_set_app_type`; legacy msvcrt: `__set_app_type`.
        "_set_app_type" | "__set_app_type" => handle_set_app_type(ctx),
        "_time64" => handle_time64(ctx),
        "_localtime64" => handle_localtime64(ctx),
        "_set_invalid_parameter_handler" => handle_set_invalid_parameter_handler(ctx),
        // Legacy msvcrt CRT startup / teardown.
        "getenv" => handle_getenv(ctx),
        "__getmainargs" => handle_getmainargs(ctx),
        "_xcptfilter" => handle_xcpt_filter(ctx),
        "_cexit" | "_c_exit" => handle_cexit(ctx),
        "_errno" => handle_errno(ctx),
        "strerror" => handle_strerror(ctx),
        "setlocale" => handle_setlocale(ctx),
        "signal" => handle_signal(ctx),
        // exit / _exit / abort: marked exit_process in hooks; still provide handler body
        // in case traits path misses API-set library names.
        "exit" | "_exit" => handle_exit_like(ctx),
        "abort" => handle_abort(ctx),
        // Legacy msvcrt used heavily by standalone 7za / MSVC CRT apps.
        "realloc" => handle_realloc(ctx),
        "_isatty" => handle_isatty(ctx),
        "_get_osfhandle" => handle_get_osfhandle(ctx),
        "puts" => handle_puts(ctx),
        "fputc" => handle_fputc(ctx),
        "fputs" => handle_fputs(ctx),
        "atoi" => handle_atoi(ctx),
        "atol" => handle_atol(ctx),
        "strtol" => handle_strtol(ctx),
        "strtoul" => handle_strtoul(ctx),
        "strtod" | "strtof" => handle_strtod(ctx),
        "fopen" => handle_fopen(ctx),
        "fclose" => handle_fclose(ctx),
        "fgets" => handle_fgets(ctx),
        "fgetc" => handle_fgetc(ctx),
        "strtok" => handle_strtok(ctx),
        "strcmp" => handle_strcmp(ctx),
        "isalpha" => handle_isalpha(ctx),
        "isdigit" => handle_isdigit(ctx),
        "isalnum" => handle_isalnum(ctx),
        "islower" => handle_islower(ctx),
        "isupper" => handle_isupper(ctx),
        "isspace" => handle_isspace(ctx),
        "toupper" => handle_toupper(ctx),
        "tolower" => handle_tolower(ctx),
        "wcscmp" => handle_wcscmp(ctx),
        "wcsstr" => handle_wcsstr(ctx),
        "_onexit" | "__dllonexit" => handle_onexit(ctx),
        "_beginthreadex" => handle_begin_thread_ex(ctx),
        "_endthreadex" => handle_end_thread_ex(ctx),
        "_purecall" => handle_purecall(ctx),
        // MSVC C++ mangled names (matched after to_ascii_lowercase).
        "?terminate@@yaxxz" => handle_terminate_cxx(ctx),
        "??1type_info@@ueaa@xz" => handle_type_info_dtor(ctx),
        "_cxxthrowexception" => handle_cxx_throw_exception(ctx),
        "srand" => handle_srand(ctx),
        "rand" => handle_rand(ctx),
        "_kbhit" => handle_kbhit(ctx),
        "_getch" => handle_getch(ctx),
        "system" => handle_system(ctx),
        _ => anyhow::bail!("unsupported UCRT export: {name}"),
    }
}

fn ret(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine.return_from_win64_api(value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// Bitcast a CRT `int` status into RAX without `as` (sign-preserving via `i64`).
#[inline]
fn i32_status_to_u64(v: i32) -> u64 {
    // i32 → i64 sign-extends; `from_ne_bytes` reinterprets bits (same as `as u64` on two's complement).
    u64::from_ne_bytes(i64::from(v).to_ne_bytes())
}

/// `__acrt_iob_func(ix)` → `FILE*` for stdin/stdout/stderr.
fn handle_acrt_iob_func(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ix = engine.read_rcx()? & 0xffff_ffff;
    let ptr = match ix {
        0 => FILE_STDIN,
        1 => FILE_STDOUT,
        2 => FILE_STDERR,
        _ => 0,
    };
    ret(engine, ptr)
}

/// Cap output at 64 KiB per call (matches JIT fast path guard).
const MAX_FWRITE_OUTPUT: usize = 64 * 1024;

fn handle_fwrite(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let buf = engine.read_rcx()?;
    let size = engine.read_rdx()?;
    let count = engine.read_r8()?;
    let stream = engine.read_r9()?;

    if size == 0 || count == 0 {
        return ret(engine, 0);
    }
    let total = size.saturating_mul(count);
    let total_usize = usize::try_from(total).unwrap_or(0);
    if total_usize == 0 {
        return ret(engine, 0);
    }
    // Probe-read first byte to validate buffer is readable.
    if buf != 0 {
        let mut probe = [0_u8; 1];
        if engine.mem_read(buf, &mut probe).is_err() {
            return ret(engine, count); // skip silently
        }
    }
    let capped = total_usize.min(MAX_FWRITE_OUTPUT);
    let mut bytes = vec![0_u8; capped];
    if capped > 0 && buf != 0 {
        engine
            .mem_read(buf, &mut bytes)
            .context("fwrite guest buffer")?;
    }

    // Host stdout/stderr for console programs (independent CRT expects console I/O).
    if stream == FILE_STDOUT || stream == FILE_STDERR {
        write_host_console(stream, &bytes);
    }

    ret(engine, count)
}

fn handle_fflush(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _stream = engine.read_rcx()?;
    // Don't flush the console buffer here — it waits for Sleep so the
    // terminal receives the frame atomically rather than per-write.
    ret(engine, 0)
}

/// Host console write without `std::io::{stdout,stderr}` lock (hot CRT path).
#[cfg(unix)]
fn write_host_console(stream: u64, bytes: &[u8]) {
    let fd = if stream == FILE_STDOUT {
        libc::STDOUT_FILENO
    } else {
        libc::STDERR_FILENO
    };
    write_all_fd(fd, bytes);
}

/// Write the full buffer to `fd`, retrying EINTR; give up on other errors.
///
/// Deliberately raw rather than `std::io::Stdout`: that would take a reentrant
/// lock and buffer on every guest `fwrite`, and Rust's buffers are not flushed
/// when the guest's `ExitProcess` terminates the host process, which would lose
/// trailing output. This is the single console-write path for the crate —
/// `kernel32`'s console handles delegate here rather than duplicating the loop.
#[cfg(unix)]
pub(crate) fn write_all_fd(fd: libc::c_int, bytes: &[u8]) {
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let Some(chunk) = bytes.get(offset..) else {
            break;
        };
        // SAFETY: `chunk` is a valid contiguous slice; write does not retain the pointer.
        // Hot path: avoid `std::io` mutex on every guest `fwrite` to stdout/stderr.
        #[expect(unsafe_code)]
        let n = unsafe { libc::write(fd, chunk.as_ptr().cast::<libc::c_void>(), chunk.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        offset = offset.saturating_add(usize::try_from(n).unwrap_or(0));
    }
}

#[cfg(not(unix))]
fn write_host_console(stream: u64, bytes: &[u8]) {
    use std::io::Write;
    if stream == FILE_STDOUT {
        drop(std::io::stdout().write_all(bytes));
    } else if stream == FILE_STDERR {
        drop(std::io::stderr().write_all(bytes));
    }
}

fn handle_setvbuf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _stream = engine.read_rcx()?;
    let _buf = engine.read_rdx()?;
    let _mode = engine.read_r8()?;
    let _size = engine.read_r9()?;
    ret(engine, 0)
}

/// `__stdio_common_vfprintf(options, FILE*, format, locale, va_list)`.
/// Formats the string and writes it to the host console.
fn handle_stdio_common_vfprintf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let (out, is_stderr) = {
        let engine = &mut *ctx.engine;
        let _options = engine.read_rcx()?;
        let file_ptr = engine.read_rdx()?; // FILE* (0=stdin, 1=stdout, 2=stderr)
        let fmt_ptr = engine.read_r8()?;
        let _locale = engine.read_r9()?;
        if fmt_ptr == 0 {
            return ret(engine, 0);
        }
        let fmt = read_guest_str(engine, fmt_ptr, 4096)?;
        let rsp = engine.read_rsp()?;
        let mut va = read_guest_u64(engine, rsp.wrapping_add(0x28)).unwrap_or(0);

        const MAX_OUTPUT: usize = 4096;
        let mut out = Vec::with_capacity(256);
        let bytes = fmt.as_bytes();
        let mut i = 0;
        while i < bytes.len() && out.len() < MAX_OUTPUT {
            if bytes[i] == b'%' && i + 1 < bytes.len() {
                i += 1;
                match bytes[i] {
                    b'd' | b'i' | b'u' | b'X' | b'x' => {
                        let v = read_guest_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        out.extend_from_slice(format!("{}", v as i64).as_bytes());
                    }
                    b's' => {
                        let p = read_guest_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        out.extend_from_slice(read_guest_str(engine, p, 1024).unwrap_or_default().as_bytes());
                    }
                    b'c' => {
                        let v = read_guest_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        out.push(v as u8);
                    }
                    _ => { va = va.wrapping_add(8); }
                }
            } else {
                out.push(bytes[i]);
            }
            i += 1;
        }
        (out, file_ptr == 2)
    };
    // Engine borrow is dropped — now we can use ctx.
    if is_stderr {
        write_host_console(FILE_STDERR, &out);
    } else {
        crate::kernel32::console::emit_text_from_bytes(ctx, &out);
    }
    ret(&mut *ctx.engine, out.len() as u64)
}

/// `__stdio_common_vsprintf(options, buf, count, format, locale, va_list)`.
fn handle_stdio_common_vsprintf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let buf = engine.read_rdx()?;
    let fmt_ptr = engine.read_r9()?;
    if buf == 0 || fmt_ptr == 0 {
        return ret(engine, 0);
    }
    let fmt = read_guest_str(engine, fmt_ptr, 4096)?;
    let rsp = engine.read_rsp()?;
    let mut va = read_guest_u64(engine, rsp.wrapping_add(0x30)).unwrap_or(0);

    const MAX_OUTPUT: usize = 4096;
    let mut out = Vec::with_capacity(256);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() && out.len() < MAX_OUTPUT {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            i += 1;
            match bytes[i] {
                b'd' | b'i' | b'u' => {
                    let v = read_guest_u64(engine, va).unwrap_or(0);
                    va = va.wrapping_add(8);
                    out.extend_from_slice(
                        if bytes[i] == b'u' {
                            format!("{v}")
                        } else {
                            format!("{}", v as i64)
                        }
                        .as_bytes(),
                    );
                }
                b'x' | b'X' => {
                    let v = read_guest_u64(engine, va).unwrap_or(0);
                    va = va.wrapping_add(8);
                    out.extend_from_slice(format!("{v:x}").as_bytes());
                }
                b's' => {
                    let p = read_guest_u64(engine, va).unwrap_or(0);
                    va = va.wrapping_add(8);
                    out.extend_from_slice(
                        read_guest_str(engine, p, 1024)
                            .unwrap_or_default()
                            .as_bytes(),
                    );
                }
                b'c' => {
                    let v = read_guest_u64(engine, va).unwrap_or(0);
                    va = va.wrapping_add(8);
                    out.push(v as u8);
                }
                b'%' => out.push(b'%'),
                _ => {
                    out.push(b'%');
                    out.push(bytes[i]);
                }
            }
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    out.push(0);
    drop(engine.mem_write(buf, &out));
    ret(engine, (out.len().saturating_sub(1)) as u64)
}

/// `__stdio_common_vsscanf(options, buf, count, format, locale, va_list)`.
fn handle_stdio_common_vsscanf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let src_ptr = engine.read_rdx()?;
    let fmt_ptr = engine.read_r9()?;
    if src_ptr == 0 || fmt_ptr == 0 {
        return ret(engine, 0);
    }
    let src = read_guest_str(engine, src_ptr, 4096)?;
    let fmt = read_guest_str(engine, fmt_ptr, 4096)?;
    let rsp = engine.read_rsp()?;
    let mut va = read_guest_u64(engine, rsp.wrapping_add(0x30)).unwrap_or(0);
    let sb = src.as_bytes();
    let fb = fmt.as_bytes();
    let mut si = 0;
    let mut fi = 0;
    let mut items = 0;
    while fi < fb.len() && si < sb.len() {
        if fb[fi] == b'%' && fi + 1 < fb.len() {
            fi += 1;
            match fb[fi] {
                b'd' | b'i' | b'u' => {
                    while si < sb.len() && sb[si].is_ascii_whitespace() {
                        si += 1;
                    }
                    let neg = si < sb.len() && sb[si] == b'-';
                    if neg || (si < sb.len() && sb[si] == b'+') {
                        si += 1;
                    }
                    let start = si;
                    while si < sb.len() && sb[si].is_ascii_digit() {
                        si += 1;
                    }
                    if si > start {
                        let s = std::str::from_utf8(&sb[start..si]).unwrap_or("0");
                        let val: u64 = (if neg {
                            -(s.parse::<i64>().unwrap_or(0))
                        } else {
                            s.parse::<i64>().unwrap_or(0)
                        }) as u64;
                        let out = read_guest_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        if out != 0 {
                            drop(engine.mem_write(out, &val.to_le_bytes()));
                        }
                        items += 1;
                    }
                }
                b's' => {
                    while si < sb.len() && sb[si].is_ascii_whitespace() {
                        si += 1;
                    }
                    let start = si;
                    while si < sb.len() && !sb[si].is_ascii_whitespace() {
                        si += 1;
                    }
                    let out = read_guest_u64(engine, va).unwrap_or(0);
                    va = va.wrapping_add(8);
                    if out != 0 {
                        let mut w = sb[start..si].to_vec();
                        w.push(0);
                        drop(engine.mem_write(out, &w));
                    }
                    items += 1;
                }
                _ => {}
            }
        } else if fb[fi].is_ascii_whitespace() {
            while si < sb.len() && sb[si].is_ascii_whitespace() {
                si += 1;
            }
        } else if si < sb.len() && sb[si] == fb[fi] {
            si += 1;
        }
        fi += 1;
    }
    ret(engine, items)
}

/// Shared RNG state between `srand` and `rand`.
/// Uses a host `AtomicU32` so seeding and reading are properly ordered
/// even if the guest remains single-threaded through the emulator.
static CRT_RNG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Convert a Unix timestamp to `struct tm` fields.
fn unix_ts_to_tm(ts: i64) -> [i32; 9] {
    let mut days = ts / 86400;
    if ts < 0 && ts % 86400 != 0 {
        days -= 1;
    }
    let mut y = 1970_i64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        let yd = if leap { 366 } else { 365 };
        if days < yd {
            break;
        }
        days -= yd;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mon = 0_i64;
    while mon < 12 && days >= mdays[mon as usize] {
        days -= mdays[mon as usize];
        mon += 1;
    }
    let day = days + 1;
    let rem = ts.rem_euclid(86400);
    let sec = rem % 60;
    let min = (rem / 60) % 60;
    let hr = rem / 3600;
    let y_adj = if mon < 2 { y - 1 } else { y };
    let m_adj = if mon < 2 { mon + 13 } else { mon + 1 };
    let wd = ((day + (13 * m_adj) / 5 + y_adj % 100 + (y_adj % 100) / 4 + (y_adj / 100) / 4
        - 2 * (y_adj / 100))
        % 7
        + 7)
        % 7;
    [
        sec as i32,
        min as i32,
        hr as i32,
        day as i32,
        mon as i32,
        (y - 1900) as i32,
        wd as i32,
        0,
        0,
    ]
}

/// `_localtime64(t)` — convert time_t to local struct tm.
fn handle_localtime64(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let t_ptr = engine.read_rcx()?;
    if t_ptr == 0 {
        return ret(engine, 0);
    }
    let mut buf = [0_u8; 8];
    engine.mem_read(t_ptr, &mut buf)?;
    let ts = i64::from_le_bytes(buf);
    let tm = unix_ts_to_tm(ts);
    // x64 struct tm layout: tm_sec(4), tm_min(4), tm_hour(4), tm_mday(4),
    // tm_mon(4), tm_year(4), tm_wday(4), tm_yday(4), tm_isdst(4) = 36 bytes.
    // Allocate and write from the heap.
    let va = state.heap_state.heap.alloc_coherent(engine, 36);
    if va == 0 {
        return ret(engine, 0);
    }
    for (i, &v) in tm.iter().enumerate() {
        let off = u64::try_from(i * 4).unwrap_or(0);
        drop(engine.mem_write(va.wrapping_add(off), &(v as u32).to_le_bytes()));
    }
    ret(engine, va)
}

/// `_time64(t)` — get current time in seconds since epoch.
fn handle_time64(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let t_ptr = engine.read_rcx()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if t_ptr != 0 {
        drop(engine.mem_write(t_ptr, &now.to_le_bytes()));
    }
    ret(engine, now)
}

/// `srand(seed)` — seed the CRT random number generator.
///
/// Uses the Windows UCRT algorithm (MSVC CRT compatible):
/// `state = state * 214013 + 2531011`, return `(state >> 16) & 0x7FFF`.
/// The constants differ from BSD/glibc (`1103515245, 12345`), so a mingw
/// program linked against `api-ms-win-crt-utility-l1-1-0.dll` gets the
/// same sequence as MSVC.
fn handle_srand(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let seed = engine.read_rcx()?;
    CRT_RNG.store(seed as u32, std::sync::atomic::Ordering::Relaxed);
    ret(engine, 0)
}

/// `rand()` → pseudo-random integer between 0 and RAND_MAX (0x7FFF).
fn handle_rand(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let prev = CRT_RNG.load(std::sync::atomic::Ordering::Relaxed);
    let next = prev.wrapping_mul(214_013).wrapping_add(2_531_011);
    CRT_RNG.store(next, std::sync::atomic::Ordering::Relaxed);
    let val = (next >> 16) & 0x7FFF;
    ret(engine, u64::from(val))
}

/// `_kbhit()` — non-blocking key-press check (peek, does NOT consume).
fn handle_kbhit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.state.flush_console();
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::console::pump::ensure_input_ready(state);
    let ready = crate::console::pump::peek_key_press(state);
    ret(engine, u64::from(ready))
}

// Map a Windows VK code to the scan code MSVC `_getch` returns for
// extended keys (arrows, F-keys, etc.) — the two-call protocol.
const fn vk_to_scan(vk: u16) -> Option<u8> {
    Some(match vk {
        0x25 => 75,   // VK_LEFT
        0x26 => 72,   // VK_UP
        0x27 => 77,   // VK_RIGHT
        0x28 => 80,   // VK_DOWN
        0x24 => 71,   // VK_HOME
        0x23 => 79,   // VK_END
        0x2D => 82,   // VK_INSERT
        0x2E => 83,   // VK_DELETE
        0x21 => 73,   // VK_PRIOR (PgUp)
        0x22 => 81,   // VK_NEXT (PgDn)
        0x70..=0x7B => 59 + (vk - 0x70) as u8, // VK_F1..VK_F12 → 59..68, 133..134
        _ => return None,
    })
}

// Per-thread state for the two-call extended-key protocol.
std::thread_local! {
    static PENDING_SCAN: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

/// `_getch()` — blocking key read (no echo).
///
/// Extended keys (arrows, F-keys, etc.) use a two-call protocol:
/// 1. First call returns 0 (signals an extended key).
/// 2. Second call returns the scan code.
fn handle_getch(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.state.flush_console();
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    // If a scan code is pending from a previous extended-key prefix, return it.
    if let Some(scan) = PENDING_SCAN.get() {
        PENDING_SCAN.set(None);
        return ret(engine, u64::from(scan));
    }

    crate::console::pump::ensure_input_ready(state);
    let Some(key) = crate::console::pump::next_key_press(state, true) else {
        return ret(engine, 0);
    };

    if key.unit != 0 {
        // Regular key: return the character directly.
        return ret(engine, u64::from(key.unit));
    }

    // Extended key (no character): return 0 now, save scan code for next call.
    if let Some(scan) = vk_to_scan(key.virtual_key_code) {
        PENDING_SCAN.set(Some(scan));
    }
    ret(engine, 0)
}

/// `system(command)` — run a shell command on the host.
///
/// Reads the command string from guest memory, executes it via the host
/// shell, and returns the exit code. When `command` is NULL, returns
/// non-zero to indicate a command processor is available (per spec).
fn handle_system(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let cmd_ptr = ctx.engine.read_rcx()?;
    if cmd_ptr == 0 {
        // MSDN: passing NULL queries whether a command processor exists.
        let eng = &mut *ctx.engine;
        #[cfg(not(target_os = "windows"))]
        return ret(eng, 1);
        #[cfg(target_os = "windows")]
        return ret(eng, 0);
    }
    // Read the command string from guest memory (null-terminated).
    let mut cmd_bytes = Vec::new();
    let mut addr = cmd_ptr;
    loop {
        let mut byte = [0_u8];
        ctx.engine.mem_read(addr, &mut byte)?;
        if byte[0] == 0 {
            break;
        }
        cmd_bytes.push(byte[0]);
        addr = addr.wrapping_add(1);
        if cmd_bytes.len() > 4096 {
            break; // safety cap
        }
    }
    let cmd = String::from_utf8_lossy(&cmd_bytes);
    // Handle cls directly — this is the most common system() call and
    // shelling it on macOS/Linux would fail (cls is a Windows command).
    if cmd.trim().eq_ignore_ascii_case("cls") {
        // Route through the console buffer so the clear and the
        // subsequent fputs(frame) arrive at the terminal as one
        // atomic write on Sleep.
        crate::kernel32::console::emit_text_from_bytes(ctx, b"\x1b[H\x1b[J");
        let eng = &mut *ctx.engine;
        return ret(eng, 0);
    }
    // Other commands are passed to the host shell.
    let eng = &mut *ctx.engine;
    #[cfg(not(target_os = "windows"))]
    {
        let result = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd.as_ref())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status();
        let code = result.ok().and_then(|s| s.code()).unwrap_or(-1);
        ret(eng, code as u64)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = cmd;
        ret(eng, 0)
    }
}

fn handle_malloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let size = engine.read_rcx()?;
    let ptr = if size == 0 {
        0
    } else {
        state.heap_state.heap.alloc_coherent(engine, size)
    };
    ret(engine, ptr)
}

fn handle_calloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let n = engine.read_rcx()?;
    let size = engine.read_rdx()?;
    let total = n.saturating_mul(size);
    let ptr = if total == 0 {
        0
    } else {
        let p = state.heap_state.heap.alloc_coherent(engine, total);
        if p != 0 {
            let len = usize::try_from(total).unwrap_or(0);
            let zeros = vec![0_u8; len];
            engine.mem_write(p, &zeros)?;
        }
        p
    };
    ret(engine, ptr)
}

fn handle_free(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let ptr = engine.read_rcx()?;
    if ptr != 0 {
        let _ = state.heap_state.heap.free_coherent(engine, ptr);
    }
    ret(engine, 0)
}

fn handle_set_new_mode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _mode = engine.read_rcx()?;
    ret(engine, 0)
}

fn handle_p_environ(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // char*** — point at a slot holding NULL (empty environment block list).
    engine.mem_write(ENVIRON_PTR_SLOT, &0_u64.to_le_bytes())?;
    ret(engine, ENVIRON_PTR_SLOT)
}

fn handle_p_acmdln(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Slot is filled at session start (points at GetCommandLineA buffer).
    ret(engine, ACMDLN_PTR_SLOT)
}

fn handle_p_argc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Slot is filled at session start from guest argv.
    ret(engine, ARGC_SLOT)
}

fn handle_p_argv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Slot holds char** filled at session start.
    ret(engine, ARGV_PTR_SLOT)
}

fn handle_p_commode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    engine.mem_write(COMMODE_SLOT, &0_u32.to_le_bytes())?;
    ret(engine, COMMODE_SLOT)
}

fn handle_p_fmode(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    engine.mem_write(FMODE_SLOT, &0_u32.to_le_bytes())?;
    ret(engine, FMODE_SLOT)
}

fn handle_config_thread_locale(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _ = engine.read_rcx()?;
    ret(engine, 0)
}

fn handle_set_user_matherr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _ = engine.read_rcx()?;
    ret(engine, 0)
}

fn handle_c_specific_handler(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Exception filter: continue search.
    ret(engine, 1)
}

fn handle_memcpy(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let src = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || dest == 0 || src == 0 {
        return ret(engine, dest);
    }
    // `mem_copy` resolves both spans inside wie-cpu and uses memmove
    // semantics, so overlapping ranges are handled correctly rather than
    // being punted to a host bounce buffer. Returns false only when a side
    // is not a single mapped span.
    if engine.mem_copy(dest, src, n_usize) {
        return ret(engine, dest);
    }
    // Fallback: cross-arena or SPC-denied.
    let mut buf = vec![0_u8; n_usize];
    engine.mem_read(src, &mut buf)?;
    engine.mem_write(dest, &buf)?;
    ret(engine, dest)
}

fn handle_memcmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || a == 0 || b == 0 {
        return ret(engine, 0);
    }
    // Fast path: both spans in host-contiguous arenas → direct slice compare.
    // Both slices borrow `&engine`, so they can coexist; the borrow ends
    // before `ret` needs `&mut engine`.
    let direct = match (engine.host_slice(a, n_usize), engine.host_slice(b, n_usize)) {
        (Some(sa), Some(sb)) => {
            let mut result: i32 = 0;
            for (xa, xb) in sa.iter().zip(sb.iter()) {
                if xa != xb {
                    result = i32::from(*xa).wrapping_sub(i32::from(*xb));
                    break;
                }
            }
            Some(result)
        }
        _ => None,
    };
    if let Some(result) = direct {
        return ret(engine, i32_status_to_u64(result));
    }
    let mut ba = vec![0_u8; n_usize];
    let mut bb = vec![0_u8; n_usize];
    engine.mem_read(a, &mut ba)?;
    engine.mem_read(b, &mut bb)?;
    let mut result: i32 = 0;
    for (xa, xb) in ba.iter().zip(bb.iter()) {
        if xa != xb {
            result = i32::from(*xa).wrapping_sub(i32::from(*xb));
            break;
        }
    }
    ret(engine, i32_status_to_u64(result))
}

fn handle_memset(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let dest = engine.read_rcx()?;
    let c = engine.read_rdx()? & 0xff;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 || dest == 0 {
        return ret(engine, dest);
    }
    let value = u8::try_from(c).unwrap_or(0);
    if engine.mem_fill(dest, value, n_usize) {
        return ret(engine, dest);
    }
    // Fallback: bounce through a host Vec only when the span isn't directly writable.
    let buf = vec![value; n_usize];
    engine.mem_write(dest, &buf)?;
    ret(engine, dest)
}

/// Legacy msvcrt `__getmainargs(argc*, argv**, env**, doWildcard, startupinfo*)`.
///
/// Fills caller out-params from the CRT page prepared at session start.
fn handle_getenv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // `getenv(const char* name)` → returns NULL (variable not found).
    // The C++ runtime checks for debug/env flags during startup; returning
    // NULL is safe — no deployment expects these to be set.
    ret(engine, 0)
}

fn handle_getmainargs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let argc_ptr = engine.read_rcx()?;
    let argv_ptr = engine.read_rdx()?;
    let env_ptr = engine.read_r8()?;
    let _do_wildcard = engine.read_r9()?;
    // 5th arg on stack is ignored (startupinfo*).

    if argc_ptr != 0 {
        let mut argc_bytes = [0_u8; 4];
        engine
            .mem_read(ARGC_SLOT, &mut argc_bytes)
            .context("__getmainargs read argc slot")?;
        engine
            .mem_write(argc_ptr, &argc_bytes)
            .context("__getmainargs write *argc")?;
    }
    if argv_ptr != 0 {
        // *argv = char** table (same layout as __p___argv materialization).
        const ARGV_TABLE: u64 = CRT_GUEST_BASE + 0x400;
        engine
            .mem_write(argv_ptr, &ARGV_TABLE.to_le_bytes())
            .context("__getmainargs write *argv")?;
    }
    if env_ptr != 0 {
        // Empty environment: ENVIRON_PTR_SLOT holds a single NULL char* terminator.
        engine
            .mem_write(ENVIRON_PTR_SLOT, &0_u64.to_le_bytes())
            .context("__getmainargs zero env list")?;
        engine
            .mem_write(env_ptr, &ENVIRON_PTR_SLOT.to_le_bytes())
            .context("__getmainargs write *env")?;
    }
    ret(engine, 0)
}

/// `_XcptFilter` — SEH filter; continue search (no host exception model).
fn handle_xcpt_filter(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _xcptnum = engine.read_rcx()?;
    let _info = engine.read_rdx()?;
    // EXCEPTION_CONTINUE_SEARCH
    ret(engine, 0)
}

fn handle_strlen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = engine.read_rcx()?;
    if s == 0 {
        return ret(engine, 0);
    }
    // Scan up to one guest page per host_span call; keeps a single memory-lock
    // acquisition covering ~4 KiB of scan instead of one per byte.
    const PAGE: u64 = 4096;
    const CAP: u64 = 1_000_000;
    let mut cursor = s;
    let mut total: u64 = 0;
    while total < CAP {
        let page_end = (cursor | (PAGE - 1)).wrapping_add(1);
        let remaining = CAP.saturating_sub(total);
        let span_len_u64 = page_end.saturating_sub(cursor).min(remaining);
        let span_len = usize::try_from(span_len_u64).unwrap_or(0);
        if span_len == 0 {
            break;
        }
        // Scan the page in place through a borrowed slice — no copy, and the
        // borrow ends before `ret` takes `&mut engine`.
        if let Some(found) = engine
            .host_slice(cursor, span_len)
            .map(|slice| slice.iter().position(|&b| b == 0))
        {
            if let Some(off) = found {
                total = total.saturating_add(u64::try_from(off).unwrap_or(0));
                return ret(engine, total);
            }
            total = total.saturating_add(span_len_u64);
            cursor = cursor.wrapping_add(span_len_u64);
            continue;
        }
        // Fallback (unmapped span / protect denied): scalar byte scan of this page.
        let mut buf = [0_u8; 1];
        engine.mem_read(cursor, &mut buf)?;
        if buf[0] == 0 {
            return ret(engine, total);
        }
        total = total.saturating_add(1);
        cursor = cursor.wrapping_add(1);
    }
    ret(engine, total)
}

fn handle_strncmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    let n = engine.read_r8()?;
    let n_usize = usize::try_from(n).unwrap_or(0);
    if n_usize == 0 {
        return ret(engine, 0);
    }
    // Try both spans as one contiguous host slice each; fall back to scalar.
    let direct = match (engine.host_slice(a, n_usize), engine.host_slice(b, n_usize)) {
        (Some(sa), Some(sb)) => {
            let mut result: i32 = 0;
            for (&ca, &cb) in sa.iter().zip(sb.iter()) {
                if ca != cb {
                    result = i32::from(ca).wrapping_sub(i32::from(cb));
                    break;
                }
                if ca == 0 {
                    break;
                }
            }
            Some(result)
        }
        _ => None,
    };
    if let Some(result) = direct {
        return ret(engine, i32_status_to_u64(result));
    }
    let mut result: i32 = 0;
    for i in 0..n_usize {
        let mut ba = [0_u8; 1];
        let mut bb = [0_u8; 1];
        engine.mem_read(a.wrapping_add(u64::try_from(i).unwrap_or(0)), &mut ba)?;
        engine.mem_read(b.wrapping_add(u64::try_from(i).unwrap_or(0)), &mut bb)?;
        if ba[0] != bb[0] {
            result = i32::from(ba[0]).wrapping_sub(i32::from(bb[0]));
            break;
        }
        if ba[0] == 0 {
            break;
        }
    }
    ret(engine, i32_status_to_u64(result))
}

/// `_initterm(first, last)` — call void (*)() for each non-null entry in [first, last).
///
/// v0: **no-op**. Calling guest constructors requires a full call bridge; empty/noncritical
/// `.CRT` sections still allow simple `main` programs. Tighten when a CRT PE needs ctors.
fn handle_initterm(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _first = engine.read_rcx()?;
    let _last = engine.read_rdx()?;
    ret(engine, 0)
}

/// `_initterm_e` — same as `_initterm` but entries return `int`; non-zero aborts.
/// v0: no-op success (return 0).
fn handle_initterm_e(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _first = engine.read_rcx()?;
    let _last = engine.read_rdx()?;
    ret(engine, 0)
}

fn handle_configure_narrow_argv(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _mode = engine.read_rcx()?;
    ret(engine, 0)
}

fn handle_initialize_narrow_environment(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    ret(engine, 0)
}

fn handle_crt_atexit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _fn = engine.read_rcx()?;
    ret(engine, 0)
}

fn handle_set_app_type(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _t = engine.read_rcx()?;
    ret(engine, 0)
}

fn handle_set_invalid_parameter_handler(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _h = engine.read_rcx()?;
    ret(engine, 0)
}

fn handle_cexit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    ret(engine, 0)
}

fn handle_signal(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _sig = engine.read_rcx()?;
    let _handler = engine.read_rdx()?;
    ret(engine, 0)
}

fn handle_exit_like(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Should be intercepted via exit_process trait; if not, still return.
    let code = engine.read_rcx()?;
    ret(engine, code)
}

fn handle_abort(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    ret(engine, 3)
}

/// CRT `realloc(ptr, size)`.
fn handle_realloc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let ptr = engine.read_rcx()?;
    let new_size = engine.read_rdx()?;
    if ptr == 0 {
        let p = if new_size == 0 {
            0
        } else {
            state.heap_state.heap.alloc_coherent(engine, new_size)
        };
        return ret(engine, p);
    }
    if new_size == 0 {
        let _ = state.heap_state.heap.free_coherent(engine, ptr);
        return ret(engine, 0);
    }
    if let Some(same) = state.heap_state.heap.try_realloc_in_place(ptr, new_size) {
        return ret(engine, same);
    }
    let old_size = state
        .heap_state
        .heap
        .size_of(ptr)
        .or_else(|| {
            let mut hb = [0_u8; 8];
            engine
                .mem_read(ptr.wrapping_sub(8), &mut hb)
                .ok()
                .map(|()| u64::from_le_bytes(hb))
        })
        .unwrap_or(0);
    let new_addr = state.heap_state.heap.alloc_coherent(engine, new_size);
    if new_addr == 0 {
        return ret(engine, 0);
    }
    let copy_len = usize::try_from(old_size.min(new_size)).unwrap_or(0);
    if copy_len > 0 {
        let mut bytes = vec![0_u8; copy_len];
        engine.mem_read(ptr, &mut bytes)?;
        engine.mem_write(new_addr, &bytes)?;
    }
    let _ = state.heap_state.heap.free_coherent(engine, ptr);
    ret(engine, new_addr)
}

/// `_isatty(fd)` — treat 0/1/2 as console TTYs.
fn handle_isatty(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let fd = engine.read_rcx()? & 0xffff_ffff;
    let is_tty = (0..=2).contains(&fd);
    ret(engine, u64::from(is_tty))
}

/// `_get_osfhandle(fd)` → fake console HANDLE for std streams.
fn handle_get_osfhandle(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let fd = engine.read_rcx()? & 0xffff_ffff;
    // Align with kernel32 fake std handles.
    let handle = match fd {
        0 => 0x0000_0000_6000_0001_u64, // stdin
        1 => 0x0000_0000_6000_0002_u64, // stdout
        2 => 0x0000_0000_6000_0003_u64, // stderr
        _ => u64::MAX,                  // INVALID_HANDLE_VALUE
    };
    ret(engine, handle)
}

/// `fputc(c, stream)`.
fn handle_fputc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? & 0xff;
    let stream = engine.read_rdx()?;
    let ch = u8::try_from(c).unwrap_or(0);
    if stream == FILE_STDOUT || stream == FILE_STDERR {
        write_host_console(stream, &[ch]);
        return ret(engine, u64::from(ch));
    }
    if stream == FILE_STDIN {
        return ret(engine, u64::from(u32::MAX)); // EOF
    }
    // Unknown FILE* — still echo to stdout (best-effort for &_iob[1] offsets).
    write_host_console(FILE_STDOUT, &[ch]);
    ret(engine, u64::from(ch))
}

/// `fputs(s, stream)`.
fn handle_fputs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let s = ctx.engine.read_rcx()?;
    let stream = ctx.engine.read_rdx()?;
    if s == 0 {
        return ret(&mut *ctx.engine, u64::from(u32::MAX)); // EOF
    }
    let mut bytes = Vec::new();
    let mut off = 0_u64;
    loop {
        let mut b = [0_u8; 1];
        ctx.engine.mem_read(s.wrapping_add(off), &mut b)?;
        if b[0] == 0 {
            break;
        }
        bytes.push(b[0]);
        off = off.saturating_add(1);
        if off > 1_000_000 {
            break;
        }
    }
    if stream == FILE_STDERR {
        write_host_console(FILE_STDERR, &bytes);
    } else {
        // Buffer through the console module — flushes atomically on Sleep.
        crate::kernel32::console::emit_text_from_bytes(ctx, &bytes);
    }
    ret(&mut *ctx.engine, 0)
}

/// `puts(s)` — write NUL-terminated string + newline to stdout.
fn handle_puts(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s = engine.read_rcx()?;
    if s == 0 {
        return ret(engine, u64::from(u32::MAX)); // EOF
    }
    let mut bytes = Vec::new();
    let mut off = 0_u64;
    loop {
        let mut b = [0_u8; 1];
        engine.mem_read(s.wrapping_add(off), &mut b)?;
        if b[0] == 0 {
            break;
        }
        bytes.push(b[0]);
        off = off.saturating_add(1);
        if off > 1_000_000 {
            break;
        }
    }
    bytes.push(b'\n');
    write_host_console(FILE_STDOUT, &bytes);
    ret(engine, 0) // non-negative = success
}

/// `fopen(path, mode)` — open a file for stdio access.
fn handle_fopen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let p = engine.read_rcx()?;
    let m = engine.read_rdx()?;
    drop(read_guest_str(engine, p, 1024).ok());
    drop(read_guest_str(engine, m, 16).ok());
    ret(engine, 0) // NULL = not implemented yet (needs VFS-to-CRT bridge)
}

/// `fclose(stream)` — close a stdio file handle.
fn handle_fclose(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    engine.read_rcx()?;
    ret(engine, u64::from(u32::MAX)) // EOF = not implemented
}

/// `fgets(buf, max, stream)` — read one line from stdin.
fn handle_fgets(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let buf = engine.read_rcx()?;
    let max = engine.read_rdx()?;
    let _stream = engine.read_r8()?;
    if buf == 0 || max == 0 {
        return ret(engine, 0); // NULL
    }
    let cap = usize::try_from(max).unwrap_or(0);
    // Refill from host stdin if buffer is empty and LiveHost mode.
    if state.file_io.stdin_cursor >= state.file_io.stdin_bytes.len()
        && state.file_io.stdin_mode == GuestStdinMode::LiveHost
    {
        use std::io::Read;
        let mut line = Vec::new();
        let mut byte = [0_u8; 1];
        let mut stdin = std::io::stdin().lock();
        loop {
            if line.len() >= 4096 || stdin.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        if !line.is_empty() {
            state.file_io.stdin_bytes = line;
            state.file_io.stdin_cursor = 0;
        }
    }
    // Copy from stdin buffer to guest buffer.
    let mut written = 0_usize;
    while written < cap.saturating_sub(1) {
        let idx = state.file_io.stdin_cursor;
        if idx >= state.file_io.stdin_bytes.len() {
            break;
        }
        let c = state.file_io.stdin_bytes[idx];
        state.file_io.stdin_cursor = idx.wrapping_add(1);
        let byte = [c];
        engine.mem_write(buf.wrapping_add(u64::try_from(written).unwrap_or(0)), &byte)?;
        written = written.wrapping_add(1);
        if c == b'\n' {
            break;
        }
    }
    if written == 0 {
        return ret(engine, 0); // NULL -> EOF / error
    }
    // NUL-terminate.
    let nul_byte = [0_u8];
    engine.mem_write(
        buf.wrapping_add(u64::try_from(written).unwrap_or(0)),
        &nul_byte,
    )?;
    ret(engine, buf) // returns buf on success
}

/// Read a NUL-terminated string from guest memory into a host buffer.
fn read_guest_str(engine: &mut dyn wie_cpu::CpuEngine, ptr: u64, max: usize) -> Result<String> {
    if ptr == 0 {
        return Ok(String::new());
    }
    let mut bytes = Vec::with_capacity(max.min(128));
    for i in 0..max {
        let mut b = [0_u8; 1];
        let off = u64::try_from(i).unwrap_or(0);
        if engine.mem_read(ptr.wrapping_add(off), &mut b).is_err() {
            break;
        }
        if b[0] == 0 {
            break;
        }
        bytes.push(b[0]);
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

/// `atoi(s)` — parse ASCII string to int.
fn handle_atoi(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ptr = engine.read_rcx()?;
    let s = read_guest_str(engine, ptr, 32)?;
    let val: i32 = s.trim().parse().unwrap_or(0);
    ret(engine, val as u64)
}

/// `atol(s)` — parse ASCII string to long.
fn handle_atol(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ptr = engine.read_rcx()?;
    let s = read_guest_str(engine, ptr, 32)?;
    let val: i64 = s.trim().parse().unwrap_or(0);
    ret(engine, val as u64)
}

/// `strtol(s, endptr, base)` — parse string to long.
fn handle_strtol(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_ptr = engine.read_rcx()?;
    let _endptr = engine.read_rdx()?;
    let base = engine.read_r8()?;
    let s = read_guest_str(engine, s_ptr, 64)?;
    let val = i64::from_str_radix(s.trim(), u32::try_from(base).unwrap_or(10)).unwrap_or(0);
    ret(engine, val as u64)
}

/// `strtoul(s, endptr, base)` — parse string to unsigned long.
fn handle_strtoul(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_ptr = engine.read_rcx()?;
    let _endptr = engine.read_rdx()?;
    let base = engine.read_r8()?;
    let s = read_guest_str(engine, s_ptr, 64)?;
    let val = u64::from_str_radix(s.trim(), u32::try_from(base).unwrap_or(10)).unwrap_or(0);
    ret(engine, val)
}

/// `strtod(s, endptr)` — parse string to double.
fn handle_strtod(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_ptr = engine.read_rcx()?;
    let _endptr = engine.read_rdx()?;
    let s = read_guest_str(engine, s_ptr, 64)?;
    let val: f64 = s.trim().parse().unwrap_or(0.0);
    ret(engine, val.to_bits())
}

/// `strtok(s, delim)` — tokenize string (single-threaded, static buffer).
fn handle_strtok(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_ptr = engine.read_rcx()?;
    let d_ptr = engine.read_rdx()?;
    static SAVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ptr = if s_ptr == 0 {
        SAVE.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        s_ptr
    };
    if ptr == 0 {
        return ret(engine, 0);
    }
    let delim = read_guest_str(engine, d_ptr, 32).unwrap_or_default();
    // Skip leading delimiters.
    let start = {
        let mut p = ptr;
        loop {
            let mut b = [0_u8; 1];
            if engine.mem_read(p, &mut b).is_err() {
                break;
            }
            if b[0] == 0 {
                break;
            }
            if delim.contains(b[0] as char) {
                p = p.wrapping_add(1);
                continue;
            }
            break;
        }
        p
    };
    // Find first delimiter after start.
    let mut end_off = 0_u64;
    loop {
        let mut b = [0_u8; 1];
        if engine
            .mem_read(start.wrapping_add(end_off), &mut b)
            .is_err()
        {
            break;
        }
        if b[0] == 0 {
            break;
        }
        if delim.contains(b[0] as char) {
            let nul = [0_u8];
            drop(engine.mem_write(start.wrapping_add(end_off), &nul));
            SAVE.store(
                start.wrapping_add(end_off).wrapping_add(1),
                std::sync::atomic::Ordering::Relaxed,
            );
            return ret(engine, start);
        }
        end_off += 1;
    }
    // No more delimiters — return remaining token.
    SAVE.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut b = [0_u8; 1];
    if engine.mem_read(start, &mut b).is_ok() && b[0] != 0 {
        ret(engine, start)
    } else {
        ret(engine, 0)
    }
}

/// `fgetc(stream)` — EOF for empty stdin inject.
fn handle_fgetc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _stream = engine.read_rcx()?;
    ret(engine, u64::from(u32::MAX)) // EOF
}

/// ctype helpers: isalpha, isdigit, isalnum, islower, isupper, isspace, toupper, tolower.
fn handle_isalpha(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_alphabetic()))
}
fn handle_isdigit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_digit()))
}
fn handle_isalnum(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_alphanumeric()))
}
fn handle_islower(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_lowercase()))
}
fn handle_isupper(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(engine, u64::from(c.is_ascii_uppercase()))
}
fn handle_isspace(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()? as u8;
    ret(
        engine,
        u64::from(c.is_ascii_whitespace() || c == b'\t' || c == b'\n' || c == b'\r'),
    )
}
fn handle_toupper(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()?;
    ret(engine, u64::from((c as u8).to_ascii_uppercase()))
}
fn handle_tolower(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let c = engine.read_rcx()?;
    ret(engine, u64::from((c as u8).to_ascii_lowercase()))
}

/// `strerror(errnum)` — returns a string describing the error code.
fn handle_strerror(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _code = engine.read_rcx()?;
    // Return a pointer to a static "Unknown error" string in guest memory.
    static STRERROR_VA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let va = STRERROR_VA.load(std::sync::atomic::Ordering::Relaxed);
    if va == 0 {
        let msg = b"Unknown error\0";
        // Write to a known address after errno slot.
        let addr = 0x7EFD_0080;
        drop(engine.mem_write(addr, msg));
        STRERROR_VA.store(addr, std::sync::atomic::Ordering::Relaxed);
        ret(engine, addr)
    } else {
        ret(engine, va)
    }
}

/// `setlocale(category, locale)` — set/get program locale.
fn handle_setlocale(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _cat = engine.read_rcx()?;
    let locale_ptr = engine.read_rdx()?;
    if locale_ptr == 0 {
        // Query: return "C" from a static location.
        static LOCALE_VA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let va = LOCALE_VA.load(std::sync::atomic::Ordering::Relaxed);
        if va == 0 {
            let addr = 0x7EFD_0090;
            drop(engine.mem_write(addr, b"C\0"));
            LOCALE_VA.store(addr, std::sync::atomic::Ordering::Relaxed);
            ret(engine, addr)
        } else {
            ret(engine, va)
        }
    } else {
        // Set: ignore, return the old locale.
        // For now, return "C" as the old locale.
        let old = 0x7EFD_0090;
        drop(engine.mem_write(old, b"C\0"));
        ret(engine, old)
    }
}

/// `_errno()` — returns a pointer to the thread-local errno variable.
fn handle_errno(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    static ERRNO_VA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let va = ERRNO_VA.load(std::sync::atomic::Ordering::Relaxed);
    if va == 0 {
        // Use a fixed address in the guest data area for errno.
        // The TEB page is at 0x7EFD_0000; place errno at 0x7EFD_0070
        // which is just after TEB.LastErrorValue at 0x68.
        let addr = 0x7EFD_0070; // TEB page + offset after LastErrorValue
        engine.mem_write(addr, &[0u8; 4]).ok();
        ERRNO_VA.store(addr, std::sync::atomic::Ordering::Relaxed);
        ret(engine, addr)
    } else {
        ret(engine, va)
    }
}

/// `strcmp(a, b)`.
fn handle_strcmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    if a == 0 || b == 0 {
        let r = match (a, b) {
            (0, 0) => 0_i32,
            (0, _) => -1,
            _ => 1,
        };
        return ret(engine, i32_status_to_u64(r));
    }
    let mut i = 0_u64;
    loop {
        let mut ba = [0_u8; 1];
        let mut bb = [0_u8; 1];
        engine.mem_read(a.wrapping_add(i), &mut ba)?;
        engine.mem_read(b.wrapping_add(i), &mut bb)?;
        if ba[0] != bb[0] {
            let r = i32::from(ba[0]).wrapping_sub(i32::from(bb[0]));
            return ret(engine, i32_status_to_u64(r));
        }
        if ba[0] == 0 {
            return ret(engine, 0);
        }
        i = i.saturating_add(1);
        if i > 1_000_000 {
            return ret(engine, 0);
        }
    }
}

/// `wcscmp(a, b)`.
fn handle_wcscmp(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let a = engine.read_rcx()?;
    let b = engine.read_rdx()?;
    if a == 0 || b == 0 {
        let r = match (a, b) {
            (0, 0) => 0_i32,
            (0, _) => -1,
            _ => 1,
        };
        return ret(engine, i32_status_to_u64(r));
    }
    let mut i = 0_u64;
    loop {
        let mut ba = [0_u8; 2];
        let mut bb = [0_u8; 2];
        let off = i.wrapping_mul(2);
        engine.mem_read(a.wrapping_add(off), &mut ba)?;
        engine.mem_read(b.wrapping_add(off), &mut bb)?;
        let wa = u16::from_le_bytes(ba);
        let wb = u16::from_le_bytes(bb);
        if wa != wb {
            let r = i32::from(wa).wrapping_sub(i32::from(wb));
            return ret(engine, i32_status_to_u64(r));
        }
        if wa == 0 {
            return ret(engine, 0);
        }
        i = i.saturating_add(1);
        if i > 1_000_000 {
            return ret(engine, 0);
        }
    }
}

/// `wcsstr(haystack, needle)` — return pointer to first match or NULL.
fn handle_wcsstr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hay = engine.read_rcx()?;
    let needle = engine.read_rdx()?;
    if hay == 0 || needle == 0 {
        return ret(engine, 0);
    }
    // Read needle
    let mut ndl = Vec::new();
    let mut i = 0_u64;
    loop {
        let mut b = [0_u8; 2];
        engine.mem_read(needle.wrapping_add(i.wrapping_mul(2)), &mut b)?;
        let w = u16::from_le_bytes(b);
        if w == 0 {
            break;
        }
        ndl.push(w);
        i = i.saturating_add(1);
        if i > 100_000 {
            break;
        }
    }
    if ndl.is_empty() {
        return ret(engine, hay);
    }
    // Scan haystack
    let mut hay_units = Vec::new();
    i = 0;
    loop {
        let mut b = [0_u8; 2];
        engine.mem_read(hay.wrapping_add(i.wrapping_mul(2)), &mut b)?;
        let w = u16::from_le_bytes(b);
        if w == 0 {
            break;
        }
        hay_units.push(w);
        i = i.saturating_add(1);
        if i > 1_000_000 {
            break;
        }
    }
    if let Some(pos) = hay_units
        .windows(ndl.len())
        .position(|w| w == ndl.as_slice())
    {
        let addr = hay.wrapping_add(u64::try_from(pos).unwrap_or(0).wrapping_mul(2));
        return ret(engine, addr);
    }
    ret(engine, 0)
}

/// `_onexit` / `__dllonexit` — accept callback, return it (success).
fn handle_onexit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let func = engine.read_rcx()?;
    // Return the function pointer to indicate registration success (MSVC CRT contract).
    ret(engine, func)
}

/// `_beginthreadex` — same worker spawn path as `CreateThread` (MSVC CRT).
///
/// ABI (x64): security, stack_size, start, arg, initflag, thrdaddr — identical
/// layout to `CreateThread` for the args we care about.
fn handle_begin_thread_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let _security = engine.read_rcx()?;
    let stack_size = engine.read_rdx()?;
    let start = engine.read_r8()?;
    let arg = engine.read_r9()?;
    // Stack: [rsp+0x28]=initflag, [rsp+0x30]=thrdaddr (after home space).
    let flags = read_stack_u32(engine, 0x28).unwrap_or(0);
    let tid_out = read_stack_u64(engine, 0x30).unwrap_or(0);
    let handle = create_guest_thread(engine, state, stack_size, start, arg, flags, tid_out)?;
    ret(engine, handle)
}

/// `_endthreadex` — terminate the current guest worker (like `ExitThread`).
fn handle_end_thread_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let code_raw = engine.read_rcx()?;
    let code = u32::try_from(code_raw & u64::from(u32::MAX)).unwrap_or(0);
    let tid = state.kernel.threads.current_tid();
    for obj in state.kernel.sync.objects.values() {
        if let KernelObject::Thread(t) = obj
            && t.tid == tid
        {
            t.finish(code);
            break;
        }
    }
    Err(WinApiControlSignal::ExitThread { code }.into())
}

fn read_stack_u32(engine: &mut dyn wie_cpu::CpuEngine, offset: u64) -> Result<u32> {
    let rsp = engine.read_rsp()?;
    let address = rsp
        .checked_add(offset)
        .context("_beginthreadex stack arg overflow")?;
    let mut bytes = [0_u8; 4];
    engine.mem_read(address, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_stack_u64(engine: &mut dyn wie_cpu::CpuEngine, offset: u64) -> Result<u64> {
    let rsp = engine.read_rsp()?;
    let address = rsp
        .checked_add(offset)
        .context("_beginthreadex stack arg overflow")?;
    let mut bytes = [0_u8; 8];
    engine.mem_read(address, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn handle_purecall(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Pure virtual call — abort-like.
    ret(engine, 0)
}

fn handle_terminate_cxx(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    ret(engine, 0)
}

fn handle_type_info_dtor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = engine.read_rcx()?;
    ret(engine, this)
}

/// `_CxxThrowException(pExceptionObject, pThrowInfo)` — MSVC C++ throw.
///
/// Builds the usual MSVC EH `EXCEPTION_RECORD` payload and enters the shared
/// two-pass SEH dispatcher (host FuncInfo / LSDA search + register restore).
fn handle_cxx_throw_exception(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let pexception_object = engine.read_rcx()?;
    let pthrow_info = engine.read_rdx()?;
    tracing::debug!(
        pexception_object = format_args!("{pexception_object:#x}"),
        pthrow_info = format_args!("{pthrow_info:#x}"),
        "msvcrt!_CxxThrowException → SEH dispatch"
    );
    // Scratch EXCEPTION_RECORD below the current stack (host-side only; the
    // dispatcher uses the throw payload + stack walk, not this buffer for control).
    let rsp = engine.read_rsp()?;
    let rec = rsp.saturating_sub(0x100);
    let rip = engine.read_rip()?;
    // ExceptionCode = 0xE06D7363 ('msc' | 0xE0000000)
    engine.mem_write(rec, &0xE06D_7363_u32.to_le_bytes())?;
    engine.mem_write(rec.saturating_add(4), &1_u32.to_le_bytes())?; // noncontinuable
    engine.mem_write(rec.saturating_add(8), &[0u8; 8])?;
    engine.mem_write(rec.saturating_add(16), &rip.to_le_bytes())?;
    engine.mem_write(rec.saturating_add(24), &4_u32.to_le_bytes())?; // NumberParameters
    // Parameters[0] = EH magic, [1] = object, [2] = ThrowInfo, [3] = image base (0)
    engine.mem_write(rec.saturating_add(32), &0x1993_0520_u64.to_le_bytes())?;
    engine.mem_write(rec.saturating_add(40), &pexception_object.to_le_bytes())?;
    engine.mem_write(rec.saturating_add(48), &pthrow_info.to_le_bytes())?;
    engine.mem_write(rec.saturating_add(56), &0_u64.to_le_bytes())?;
    engine.write_rcx(rec)?;

    seh::dispatch_exception_with_payload(
        engine,
        state,
        ThrowPayload {
            exception_object: pexception_object,
            throw_info: pthrow_info,
            gcc_throw: false,
        },
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "msvcrt!_CxxThrowException: {e}; pExceptionObject={pexception_object:#x} \
             pThrowInfo={pthrow_info:#x}; if this is std::bad_alloc after process-heap OOM, \
             try WIE_PROCESS_HEAP_MB=1024"
        )
    })
}
