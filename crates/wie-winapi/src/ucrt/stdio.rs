//! UCRT stdio surface: `FILE` cookie streams, printf/scanf format engines, and
//! the single host console-write path shared across the crate.

use crate::guest_memory::read_u64;
use crate::{GuestStdinMode, HandlerContext, WinApiHandlerResult};
use anyhow::{Context, Result};

use super::{FILE_STDERR, FILE_STDIN, FILE_STDOUT, finish, read_guest_str};
/// `__acrt_iob_func(ix)` → `FILE*` for stdin/stdout/stderr.
pub(crate) fn handle_acrt_iob_func(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ix = engine.read_rcx()? & 0xffff_ffff;
    let ptr = match ix {
        0 => FILE_STDIN,
        1 => FILE_STDOUT,
        2 => FILE_STDERR,
        _ => 0,
    };
    finish(engine, ptr)
}
/// Cap output at 64 KiB per call (matches JIT fast path guard).
const MAX_FWRITE_OUTPUT: usize = 64 * 1024;

pub(crate) fn handle_fwrite(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let buf = engine.read_rcx()?;
    let size = engine.read_rdx()?;
    let count = engine.read_r8()?;
    let stream = engine.read_r9()?;

    if size == 0 || count == 0 {
        return finish(engine, 0);
    }
    let total = size.saturating_mul(count);
    let total_usize = usize::try_from(total).unwrap_or(0);
    if total_usize == 0 {
        return finish(engine, 0);
    }
    // Probe-read first byte to validate buffer is readable.
    if buf != 0 {
        let mut probe = [0_u8; 1];
        if engine.mem_read(buf, &mut probe).is_err() {
            return finish(engine, count); // skip silently
        }
    }
    let capped = total_usize.min(MAX_FWRITE_OUTPUT);
    // Short writes use a stack buffer; only large writes hit the heap.
    let mut stack_buf = [0_u8; 256];
    let mut heap_buf = Vec::new();
    let bytes: &mut [u8] = if capped <= stack_buf.len() {
        &mut stack_buf[..capped]
    } else {
        heap_buf.resize(capped, 0);
        &mut heap_buf
    };
    if capped > 0 && buf != 0 {
        engine.mem_read(buf, bytes).context("fwrite guest buffer")?;
    }

    // Host stdout/stderr for console programs (independent CRT expects console I/O).
    if stream == FILE_STDOUT || stream == FILE_STDERR {
        write_host_console(stream, bytes);
    }

    finish(engine, count)
}

pub(crate) fn handle_fflush(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _stream = engine.read_rcx()?;
    // Flush the console buffer so the terminal receives output immediately.
    // The previous design deferred to Sleep for atomic frame flushing,
    // but fflush is the standard C mechanism for this purpose.
    ctx.state.flush_console();
    finish(engine, 0)
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
pub(crate) fn handle_setvbuf(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _stream = engine.read_rcx()?;
    let _buf = engine.read_rdx()?;
    let _mode = engine.read_r8()?;
    let _size = engine.read_r9()?;
    finish(engine, 0)
}
/// `__stdio_common_vfprintf(options, FILE*, format, locale, va_list)`.
/// Formats the string and writes it to the host console.
pub(crate) fn handle_stdio_common_vfprintf(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let (out, is_stderr) = {
        let engine = &mut *ctx.engine;
        let _options = engine.read_rcx()?;
        let file_ptr = engine.read_rdx()?; // FILE* (0=stdin, 1=stdout, 2=stderr)
        let fmt_ptr = engine.read_r8()?;
        let _locale = engine.read_r9()?;
        if fmt_ptr == 0 {
            return finish(engine, 0);
        }
        let fmt = read_guest_str(engine, fmt_ptr, 4096)?;
        let rsp = engine.read_rsp()?;
        let mut va = read_u64(engine, rsp.wrapping_add(0x28)).unwrap_or(0);

        const MAX_OUTPUT: usize = 4096;
        let mut out = Vec::with_capacity(256);
        let bytes = fmt.as_bytes();
        let mut i = 0;
        while i < bytes.len() && out.len() < MAX_OUTPUT {
            if bytes[i] == b'%' && i + 1 < bytes.len() {
                i += 1;
                let mut field_width: Option<i32> = None;
                // Parse optional field width (digits or *).
                if bytes[i] == b'*' {
                    field_width = Some(read_u64(engine, va).unwrap_or(0) as i32);
                    va = va.wrapping_add(8);
                    i += 1;
                } else if bytes[i].is_ascii_digit() {
                    let mut w: i32 = 0;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        w = w
                            .saturating_mul(10)
                            .saturating_add(i32::from(bytes[i] - b'0'));
                        i += 1;
                    }
                    field_width = Some(w);
                }
                // Skip optional precision (`.` then digits or *).
                if i < bytes.len() && bytes[i] == b'.' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'*' {
                        let _prec: i32 = read_u64(engine, va).unwrap_or(0) as i32;
                        va = va.wrapping_add(8);
                        i += 1;
                    } else {
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                match bytes[i] {
                    b'd' | b'i' | b'u' | b'X' | b'x' => {
                        let v = read_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        let s = format!("{}", v as i64);
                        pad_or_trim(&mut out, field_width, &s);
                    }
                    b's' => {
                        let p = read_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        let s = read_guest_str(engine, p, 1024).unwrap_or_default();
                        pad_or_trim(&mut out, field_width, &s);
                    }
                    b'c' => {
                        let v = read_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        out.push(v as u8);
                    }
                    _ => {
                        va = va.wrapping_add(8);
                    }
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
    finish(&mut *ctx.engine, out.len() as u64)
}

/// `__stdio_common_vsprintf(options, buf, count, format, locale, va_list)`.
/// Apply field-width padding: if `width` is Some and > `s.len()`, pad left
/// with spaces; otherwise append `s` as-is.
pub(crate) fn pad_or_trim(out: &mut Vec<u8>, width: Option<i32>, s: &str) {
    if let Some(w) = width {
        let w_usize = w.max(0) as usize;
        if w_usize > s.len() {
            for _ in 0..(w_usize.saturating_sub(s.len())) {
                out.push(b' ');
            }
        }
    }
    out.extend_from_slice(s.as_bytes());
}

pub(crate) fn handle_stdio_common_vsprintf(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let buf = engine.read_rdx()?;
    let fmt_ptr = engine.read_r9()?;
    if buf == 0 || fmt_ptr == 0 {
        return finish(engine, 0);
    }
    let fmt = read_guest_str(engine, fmt_ptr, 4096)?;
    let rsp = engine.read_rsp()?;
    let mut va = read_u64(engine, rsp.wrapping_add(0x30)).unwrap_or(0);

    const MAX_OUTPUT: usize = 4096;
    let mut out = Vec::with_capacity(256);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() && out.len() < MAX_OUTPUT {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            i += 1;
            let mut field_width: Option<i32> = None;
            // Parse optional field width (digits or *).
            if bytes[i] == b'*' {
                field_width = Some(read_u64(engine, va).unwrap_or(0) as i32);
                va = va.wrapping_add(8);
                i += 1;
            } else if bytes[i].is_ascii_digit() {
                let mut w: i32 = 0;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    w = w
                        .saturating_mul(10)
                        .saturating_add(i32::from(bytes[i] - b'0'));
                    i += 1;
                }
                field_width = Some(w);
            }
            // Skip optional precision (`.` then digits or *).
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                if i < bytes.len() && bytes[i] == b'*' {
                    let _prec: i32 = read_u64(engine, va).unwrap_or(0) as i32;
                    va = va.wrapping_add(8);
                    i += 1;
                } else {
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            match bytes[i] {
                b'd' | b'i' | b'u' => {
                    let v = read_u64(engine, va).unwrap_or(0);
                    va = va.wrapping_add(8);
                    let s = if bytes[i] == b'u' {
                        format!("{v}")
                    } else {
                        format!("{}", v as i64)
                    };
                    crate::ucrt::pad_or_trim(&mut out, field_width, &s);
                }
                b'x' | b'X' => {
                    let v = read_u64(engine, va).unwrap_or(0);
                    va = va.wrapping_add(8);
                    let s = format!("{v:x}");
                    crate::ucrt::pad_or_trim(&mut out, field_width, &s);
                }
                b's' => {
                    let p = read_u64(engine, va).unwrap_or(0);
                    va = va.wrapping_add(8);
                    let s = read_guest_str(engine, p, 1024).unwrap_or_default();
                    crate::ucrt::pad_or_trim(&mut out, field_width, &s);
                }
                b'c' => {
                    let v = read_u64(engine, va).unwrap_or(0);
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
    finish(engine, (out.len().saturating_sub(1)) as u64)
}

/// `__stdio_common_vsscanf(options, buf, count, format, locale, va_list)`.
pub(crate) fn handle_stdio_common_vsscanf(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let src_ptr = engine.read_rdx()?;
    let fmt_ptr = engine.read_r9()?;
    if src_ptr == 0 || fmt_ptr == 0 {
        return finish(engine, 0);
    }
    let src = read_guest_str(engine, src_ptr, 4096)?;
    let fmt = read_guest_str(engine, fmt_ptr, 4096)?;
    let rsp = engine.read_rsp()?;
    let mut va = read_u64(engine, rsp.wrapping_add(0x30)).unwrap_or(0);
    let sb = src.as_bytes();
    let fb = fmt.as_bytes();
    let mut si = 0;
    let mut fi = 0;
    let mut items = 0;
    while fi < fb.len() && si < sb.len() {
        if fb[fi] == b'%' && fi + 1 < fb.len() {
            fi += 1;
            // Skip optional field-width digits (e.g. %2d → skip '2').
            while fi < fb.len() && fb[fi].is_ascii_digit() {
                fi += 1;
            }
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
                        let val_i32 = if neg {
                            s.parse::<i32>().unwrap_or(0).wrapping_neg()
                        } else {
                            s.parse::<i32>().unwrap_or(0)
                        };
                        let out = read_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        if out != 0 {
                            drop(engine.mem_write(out, &val_i32.to_le_bytes()));
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
                    let out = read_u64(engine, va).unwrap_or(0);
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
    finish(engine, items)
}
/// `__stdio_common_vfscanf(options, FILE*, format, locale, va_list)`.
/// Used by `scanf`, `fscanf`, etc. — reads from stdin via the file-io buffer.
///
/// Only advances the stdin cursor by the bytes actually consumed by format
/// parsing, so remaining input (e.g. the newline after a number) stays
/// available for subsequent `getchar` / `fgets` calls.
pub(crate) fn handle_stdio_common_vfscanf(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    ctx.state.flush_console();
    let engine = &mut *ctx.engine;
    let fmt_ptr = engine.read_r8()?;
    if fmt_ptr == 0 {
        return finish(engine, 0);
    }
    let fmt = read_guest_str(engine, fmt_ptr, 4096)?;
    let rsp = engine.read_rsp()?;
    let mut va = read_u64(engine, rsp.wrapping_add(0x28)).unwrap_or(0);
    let fb = fmt.as_bytes();
    let mut fi = 0;
    let mut items = 0_usize;

    // Refill from host stdin if needed, then snapshot a local copy.
    let (input, base) = {
        let state = &mut *ctx.state;
        let base_cursor = state.file_io.stdin_cursor;
        if base_cursor >= state.file_io.stdin_bytes.len()
            && state.file_io.stdin_mode == GuestStdinMode::LiveHost
        {
            use std::io::Read;
            let mut line = Vec::new();
            let mut byte = [0_u8; 1];
            let mut host_stdin = std::io::stdin().lock();
            loop {
                if line.len() >= 4096 || host_stdin.read(&mut byte).unwrap_or(0) == 0 {
                    break;
                }
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            if line.is_empty() {
                let c = base_cursor;
                let buf: Vec<u8> = state.file_io.stdin_bytes[c..].to_vec();
                (buf, c)
            } else {
                state.file_io.stdin_bytes = line;
                let c = 0_usize;
                let buf: Vec<u8> = state.file_io.stdin_bytes[c..].to_vec();
                (buf, c)
            }
        } else {
            let c = base_cursor;
            let buf: Vec<u8> = state.file_io.stdin_bytes[c..].to_vec();
            (buf, c)
        }
    };
    let engine = &mut *ctx.engine;

    let mut pos = 0_usize;
    while fi < fb.len() && pos < input.len() {
        if fb[fi] == b'%' && fi + 1 < fb.len() {
            fi += 1;
            // Skip optional field-width digits (e.g. %2d → skip '2').
            while fi < fb.len() && fb[fi].is_ascii_digit() {
                fi += 1;
            }
            match fb[fi] {
                b'd' | b'i' | b'u' => {
                    while pos < input.len() && input[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    let neg = if pos < input.len() && input[pos] == b'-' {
                        pos += 1;
                        true
                    } else {
                        if pos < input.len() && input[pos] == b'+' {
                            pos += 1;
                        }
                        false
                    };
                    let start = pos;
                    while pos < input.len() && input[pos].is_ascii_digit() {
                        pos += 1;
                    }
                    if pos > start {
                        let s = std::str::from_utf8(&input[start..pos]).unwrap_or("0");
                        let val_i32 = if neg {
                            s.parse::<i32>().unwrap_or(0).wrapping_neg()
                        } else {
                            s.parse::<i32>().unwrap_or(0)
                        };
                        let out = read_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        if out != 0 {
                            drop(engine.mem_write(out, &val_i32.to_le_bytes()));
                        }
                        items += 1;
                    }
                }
                b'c' => {
                    // Skip optional field-width digits (e.g. %1c → skip '1').
                    // %c always reads exactly 1 char regardless of field width
                    // (the width limits the maximum, but the minimum is 1).
                    if pos < input.len() {
                        let c = input[pos];
                        pos += 1;
                        let out = read_u64(engine, va).unwrap_or(0);
                        va = va.wrapping_add(8);
                        if out != 0 {
                            // Write a full i32 (zero-extended). Many student
                            // programs store the %c result in an int variable
                            // and compare with integer constants. A single-byte
                            // write leaves garbage in the upper 3 bytes.
                            drop(engine.mem_write(out, &i32::from(c).to_le_bytes()));
                        }
                        items += 1;
                    }
                }
                b's' => {
                    while pos < input.len() && input[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    let start = pos;
                    while pos < input.len() && !input[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    let out = read_u64(engine, va).unwrap_or(0);
                    va = va.wrapping_add(8);
                    if out != 0 && pos > start {
                        let mut w = input[start..pos].to_vec();
                        w.push(0);
                        drop(engine.mem_write(out, &w));
                        items += 1;
                    }
                }
                _ => {}
            }
        } else if fb[fi].is_ascii_whitespace() {
            while pos < input.len() && input[pos].is_ascii_whitespace() {
                pos += 1;
            }
        } else if pos < input.len() && input[pos] == fb[fi] {
            pos += 1;
        }
        fi += 1;
    }
    // Advance stdin cursor only by the bytes actually consumed.
    {
        let state = &mut *ctx.state;
        state.file_io.stdin_cursor = base
            .saturating_add(pos)
            .min(state.file_io.stdin_bytes.len());
    }
    finish(engine, items.try_into().unwrap_or(0))
}
/// `fputc(c, stream)`.
pub(crate) fn handle_fputc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let c = ctx.engine.read_rcx()? & 0xff;
    let stream = ctx.engine.read_rdx()?;
    let ch = u8::try_from(c).unwrap_or(0);
    if stream == FILE_STDERR {
        write_host_console(FILE_STDERR, &[ch]);
        let engine = &mut *ctx.engine;
        return finish(engine, u64::from(ch));
    }
    if stream == FILE_STDIN {
        let engine = &mut *ctx.engine;
        return finish(engine, u64::from(u32::MAX)); // EOF
    }
    // stdout or unknown FILE* — route through console buffer.
    crate::kernel32::console::emit_text_from_bytes(ctx, &[ch]);
    let engine = &mut *ctx.engine;
    finish(engine, u64::from(ch))
}
/// `putchar(c)` — write character to stdout, return the character or EOF on error.
pub(crate) fn handle_putchar(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let c = ctx.engine.read_rcx()? & 0xff;
    let ch = u8::try_from(c).unwrap_or(0);
    crate::kernel32::console::emit_text_from_bytes(ctx, &[ch]);
    let engine = &mut *ctx.engine;
    finish(engine, u64::from(ch))
}
/// `getchar()` — read one character from stdin, return it or EOF.
pub(crate) fn handle_getchar(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.state.flush_console();
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    if state.file_io.stdin_cursor < state.file_io.stdin_bytes.len() {
        // Data already in the guest-side buffer.
        let idx = state.file_io.stdin_cursor;
        let ch = state.file_io.stdin_bytes[idx];
        if ch == b'\n' {
            state.file_io.stdin_cursor = idx.wrapping_add(1);
            return finish(engine, u64::from(b'\n'));
        }
        // Not '\n' — skip ahead if there's a newline later in the buffer.
        if let Some(nl_pos) = state.file_io.stdin_bytes[idx..]
            .iter()
            .position(|&b| b == b'\n')
        {
            state.file_io.stdin_cursor = idx.wrapping_add(nl_pos).wrapping_add(1);
            return finish(engine, u64::from(b'\n'));
        }
        state.file_io.stdin_cursor = idx.wrapping_add(1);
        return finish(engine, u64::from(ch));
    }
    if state.file_io.stdin_mode != GuestStdinMode::LiveHost {
        return finish(engine, u64::from(u32::MAX)); // EOF (InjectOnly)
    }
    // LiveHost mode with a TTY: read raw bytes from stdin.
    // We buffer up to 64 bytes so arrow-key escape sequences
    // (3 bytes each) don't need three separate poll/read rounds.
    if crate::console::host_term::is_tty() {
        use crate::console::host_term;
        const BUF_CAP: usize = 64;
        let mut buf = [0_u8; BUF_CAP];
        // Only poll when the buffer is empty (subsequent getchar
        // calls after an arrow key will find buffered bytes).
        if state.file_io.stdin_cursor >= state.file_io.stdin_bytes.len() {
            state.file_io.stdin_bytes.clear();
            if host_term::poll_stdin_ready(-1) {
                let n = host_term::read_stdin(&mut buf);
                if n > 0 {
                    state.file_io.stdin_bytes = buf[..n].to_vec();
                    state.file_io.stdin_cursor = 0;
                }
            }
        }
        let idx = state.file_io.stdin_cursor;
        if idx < state.file_io.stdin_bytes.len() {
            let ch = state.file_io.stdin_bytes[idx];
            state.file_io.stdin_cursor = idx.wrapping_add(1);
            return finish(engine, u64::from(ch));
        }
    } else {
        // Piped input: read a line (like fgets) so shared state works.
        use std::io::Read;
        let mut line = Vec::new();
        let mut byte = [0_u8; 1];
        let mut host_stdin = std::io::stdin().lock();
        loop {
            if line.len() >= 4096 || host_stdin.read(&mut byte).unwrap_or(0) == 0 {
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
            let idx = 0_usize;
            let ch = state.file_io.stdin_bytes[idx];
            state.file_io.stdin_cursor = idx.wrapping_add(1);
            return finish(engine, u64::from(ch));
        }
    }
    finish(engine, u64::from(u32::MAX)) // EOF
}
/// `fputs(s, stream)`.
pub(crate) fn handle_fputs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let s = ctx.engine.read_rcx()?;
    let stream = ctx.engine.read_rdx()?;
    if s == 0 {
        return finish(&mut *ctx.engine, u64::from(u32::MAX)); // EOF
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
    finish(&mut *ctx.engine, 0)
}
/// `puts(s)` — write NUL-terminated string + newline to stdout.
pub(crate) fn handle_puts(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let s = ctx.engine.read_rcx()?;
    if s == 0 {
        return finish(&mut *ctx.engine, u64::from(u32::MAX)); // EOF
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
    bytes.push(b'\n');
    crate::kernel32::console::emit_text_from_bytes(ctx, &bytes);
    let engine = &mut *ctx.engine;
    finish(engine, 0) // non-negative = success
}
/// `fopen(path, mode)` — open a file for stdio access.
pub(crate) fn handle_fopen(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let p = engine.read_rcx()?;
    let m = engine.read_rdx()?;
    drop(read_guest_str(engine, p, 1024).ok());
    drop(read_guest_str(engine, m, 16).ok());
    finish(engine, 0) // NULL = not implemented yet (needs VFS-to-CRT bridge)
}

/// `fclose(stream)` — close a stdio file handle.
pub(crate) fn handle_fclose(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    engine.read_rcx()?;
    finish(engine, u64::from(u32::MAX)) // EOF = not implemented
}
/// `fgets(buf, max, stream)` — read one line from stdin.
pub(crate) fn handle_fgets(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.state.flush_console();
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let buf = engine.read_rcx()?;
    let max = engine.read_rdx()?;
    let _stream = engine.read_r8()?;
    if buf == 0 || max == 0 {
        return finish(engine, 0); // NULL
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
        return finish(engine, 0); // NULL -> EOF / error
    }
    // NUL-terminate.
    let nul_byte = [0_u8];
    engine.mem_write(
        buf.wrapping_add(u64::try_from(written).unwrap_or(0)),
        &nul_byte,
    )?;
    finish(engine, buf) // returns buf on success
}
/// `fgetc(stream)` — EOF for empty stdin inject.
pub(crate) fn handle_fgetc(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _stream = engine.read_rcx()?;
    finish(engine, u64::from(u32::MAX)) // EOF
}
