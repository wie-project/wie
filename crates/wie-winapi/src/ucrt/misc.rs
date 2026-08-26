//! UCRT miscellany: time, errno/IO, rand, locale, thread creation, and the
//! MSVC C++ EH dispatch entry points.

use crate::kernel32::create_guest_thread;
use crate::seh::{self, ThrowPayload};
use crate::sync_obj::KernelObject;
use crate::{HandlerContext, WinApiControlSignal, WinApiHandlerResult};
use anyhow::{Context, Result};

use super::{MAX_GUEST_STR, finish, read_guest_str};

/// Guest address of the CRT `errno` slot: TEB page (0x7EFD_0000) + 0x70,
/// just past `TEB.LastErrorValue` at 0x68.
const ERRNO_SLOT_VA: u64 = 0x7EFD_0070;
/// Guest address of the static `"Unknown error"` string for `strerror`.
const STRERROR_SLOT_VA: u64 = 0x7EFD_0080;
/// Guest address of the static `"C"` locale string for `setlocale`.
const LOCALE_SLOT_VA: u64 = 0x7EFD_0090;
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
pub(crate) fn handle_localtime64(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let t_va = engine.read_rcx()?;
    if t_va == 0 {
        return finish(engine, 0);
    }
    let mut buf = [0_u8; 8];
    engine.mem_read(t_va, &mut buf)?;
    let ts = i64::from_le_bytes(buf);
    let tm = unix_ts_to_tm(ts);
    // x64 struct tm layout: tm_sec(4), tm_min(4), tm_hour(4), tm_mday(4),
    // tm_mon(4), tm_year(4), tm_wday(4), tm_yday(4), tm_isdst(4) = 36 bytes.
    // Allocate and write from the heap.
    let va = state
        .heap_state
        .heap
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .alloc_coherent(engine, 36);
    if va == 0 {
        return finish(engine, 0);
    }
    for (i, &v) in tm.iter().enumerate() {
        let off = u64::try_from(i * 4).unwrap_or(0);
        drop(engine.mem_write(va.wrapping_add(off), &(v as u32).to_le_bytes()));
    }
    finish(engine, va)
}

/// `_time64(t)` — get current time in seconds since epoch.
pub(crate) fn handle_time64(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let t_va = engine.read_rcx()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if t_va != 0 {
        drop(engine.mem_write(t_va, &now.to_le_bytes()));
    }
    finish(engine, now)
}

/// `srand(seed)` — seed the CRT random number generator.
///
/// Uses the Windows UCRT algorithm (MSVC CRT compatible):
/// `state = state * 214013 + 2531011`, return `(state >> 16) & 0x7FFF`.
/// The constants differ from BSD/glibc (`1103515245, 12345`), so a mingw
/// program linked against `api-ms-win-crt-utility-l1-1-0.dll` gets the
/// same sequence as MSVC.
pub(crate) fn handle_srand(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let seed = engine.read_rcx()?;
    CRT_RNG.store(seed as u32, std::sync::atomic::Ordering::Relaxed);
    finish(engine, 0)
}

/// `rand()` → pseudo-random integer between 0 and RAND_MAX (0x7FFF).
pub(crate) fn handle_rand(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let prev = CRT_RNG.load(std::sync::atomic::Ordering::Relaxed);
    let next = prev.wrapping_mul(214_013).wrapping_add(2_531_011);
    CRT_RNG.store(next, std::sync::atomic::Ordering::Relaxed);
    let val = (next >> 16) & 0x7FFF;
    finish(engine, u64::from(val))
}
/// `_kbhit()` — non-blocking key-press check (peek, does NOT consume).
pub(crate) fn handle_kbhit(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.state.flush_console();
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::console::pump::ensure_input_ready(state);
    let ready = crate::console::pump::peek_key_press(state);
    finish(engine, u64::from(ready))
}

// Map a Windows VK code to the scan code MSVC `_getch` returns for
// extended keys (arrows, F-keys, etc.) — the two-call protocol.
const fn vk_to_scan(vk: u16) -> Option<u8> {
    Some(match vk {
        0x25 => 75,                            // VK_LEFT
        0x26 => 72,                            // VK_UP
        0x27 => 77,                            // VK_RIGHT
        0x28 => 80,                            // VK_DOWN
        0x24 => 71,                            // VK_HOME
        0x23 => 79,                            // VK_END
        0x2D => 82,                            // VK_INSERT
        0x2E => 83,                            // VK_DELETE
        0x21 => 73,                            // VK_PRIOR (PgUp)
        0x22 => 81,                            // VK_NEXT (PgDn)
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
pub(crate) fn handle_getch(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    ctx.state.flush_console();
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;

    // If a scan code is pending from a previous extended-key prefix, return it.
    if let Some(scan) = PENDING_SCAN.get() {
        PENDING_SCAN.set(None);
        return finish(engine, u64::from(scan));
    }

    crate::console::pump::ensure_input_ready(state);
    let Some(key) = crate::console::pump::next_key_press(state, true) else {
        return finish(engine, 0);
    };

    if key.unit != 0 {
        // Regular key: return the character directly.
        return finish(engine, u64::from(key.unit));
    }

    // Extended key (no character): return 0 now, save scan code for next call.
    if let Some(scan) = vk_to_scan(key.virtual_key_code) {
        PENDING_SCAN.set(Some(scan));
    }
    finish(engine, 0)
}
/// `system(command)` — run a shell command on the host.
///
/// Reads the command string from guest memory, executes it via the host
/// shell, and returns the exit code. When `command` is NULL, returns
/// non-zero to indicate a command processor is available (per spec).
pub(crate) fn handle_system(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let cmd_va = ctx.engine.read_rcx()?;
    if cmd_va == 0 {
        // MSDN: passing NULL queries whether a command processor exists.
        let eng = &mut *ctx.engine;
        #[cfg(not(target_os = "windows"))]
        return finish(eng, 1);
        #[cfg(target_os = "windows")]
        return finish(eng, 0);
    }
    // Read the command string from guest memory (null-terminated).
    let mut cmd_bytes = Vec::new();
    let mut addr = cmd_va;
    loop {
        let mut byte = [0_u8];
        ctx.engine.mem_read(addr, &mut byte)?;
        if byte[0] == 0 {
            break;
        }
        cmd_bytes.push(byte[0]);
        addr = addr.wrapping_add(1);
        if cmd_bytes.len() > MAX_GUEST_STR {
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
        crate::kernel32::console::emit_text_from_bytes(ctx, b"\x1b[2J\x1b[H");
        let eng = &mut *ctx.engine;
        return finish(eng, 0);
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
        finish(eng, code as u64)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = cmd;
        finish(eng, 0)
    }
}
pub(crate) fn handle_config_thread_locale(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _ = engine.read_rcx()?;
    finish(engine, 0)
}

pub(crate) fn handle_set_user_matherr(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _ = engine.read_rcx()?;
    finish(engine, 0)
}

pub(crate) fn handle_c_specific_handler(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Exception filter: continue search.
    finish(engine, 1)
}
/// `_XcptFilter` — SEH filter; continue search (no host exception model).
pub(crate) fn handle_xcpt_filter(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _xcptnum = engine.read_rcx()?;
    let _info = engine.read_rdx()?;
    // EXCEPTION_CONTINUE_SEARCH
    finish(engine, 0)
}
pub(crate) fn handle_set_invalid_parameter_handler(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _h = engine.read_rcx()?;
    finish(engine, 0)
}
pub(crate) fn handle_signal(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _sig = engine.read_rcx()?;
    let _handler = engine.read_rdx()?;
    finish(engine, 0)
}
/// `_isatty(fd)` — treat 0/1/2 as console TTYs.
pub(crate) fn handle_isatty(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let fd = engine.read_rcx()? & 0xffff_ffff;
    let is_tty = (0..=2).contains(&fd);
    finish(engine, u64::from(is_tty))
}

/// `_get_osfhandle(fd)` → fake console HANDLE for std streams.
pub(crate) fn handle_get_osfhandle(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let fd = engine.read_rcx()? & 0xffff_ffff;
    // Align with kernel32 fake std handles.
    let handle = match fd {
        0 => crate::kernel32::FAKE_STDIN_HANDLE,
        1 => crate::kernel32::FAKE_STDOUT_HANDLE,
        2 => crate::kernel32::FAKE_STDERR_HANDLE,
        _ => crate::kernel32::INVALID_HANDLE_VALUE,
    };
    finish(engine, handle)
}
/// `atoi(s)` — parse ASCII string to int.
pub(crate) fn handle_atoi(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ptr = engine.read_rcx()?;
    let s = read_guest_str(engine, ptr, 32)?;
    let val: i32 = s.trim().parse().unwrap_or(0);
    finish(engine, val as u64)
}

/// `atol(s)` — parse ASCII string to long.
pub(crate) fn handle_atol(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ptr = engine.read_rcx()?;
    let s = read_guest_str(engine, ptr, 32)?;
    let val: i64 = s.trim().parse().unwrap_or(0);
    finish(engine, val as u64)
}
/// `strerror(errnum)` — returns a string describing the error code.
pub(crate) fn handle_strerror(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _code = engine.read_rcx()?;
    // Return a pointer to a static "Unknown error" string in guest memory.
    static STRERROR_VA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let va = STRERROR_VA.load(std::sync::atomic::Ordering::Relaxed);
    if va == 0 {
        let msg = b"Unknown error\0";
        // Write to a known address after errno slot.
        let addr = STRERROR_SLOT_VA;
        drop(engine.mem_write(addr, msg));
        STRERROR_VA.store(addr, std::sync::atomic::Ordering::Relaxed);
        finish(engine, addr)
    } else {
        finish(engine, va)
    }
}
/// `setlocale(category, locale)` — set/get program locale.
pub(crate) fn handle_setlocale(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _cat = engine.read_rcx()?;
    let locale_va = engine.read_rdx()?;
    if locale_va == 0 {
        // Query: return "C" from a static location.
        static LOCALE_VA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let va = LOCALE_VA.load(std::sync::atomic::Ordering::Relaxed);
        if va == 0 {
            let addr = LOCALE_SLOT_VA;
            drop(engine.mem_write(addr, b"C\0"));
            LOCALE_VA.store(addr, std::sync::atomic::Ordering::Relaxed);
            finish(engine, addr)
        } else {
            finish(engine, va)
        }
    } else {
        // Set: ignore, return the old locale.
        // For now, return "C" as the old locale.
        let old = LOCALE_SLOT_VA;
        drop(engine.mem_write(old, b"C\0"));
        finish(engine, old)
    }
}
/// `_errno()` — returns a pointer to the thread-local errno variable.
pub(crate) fn handle_errno(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    static ERRNO_VA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let va = ERRNO_VA.load(std::sync::atomic::Ordering::Relaxed);
    if va == 0 {
        // Use a fixed address in the guest data area for errno.
        // The TEB page is at 0x7EFD_0000; place errno at 0x7EFD_0070
        // which is just after TEB.LastErrorValue at 0x68.
        let addr = ERRNO_SLOT_VA; // TEB page + offset after LastErrorValue
        engine.mem_write(addr, &[0u8; 4]).ok();
        ERRNO_VA.store(addr, std::sync::atomic::Ordering::Relaxed);
        finish(engine, addr)
    } else {
        finish(engine, va)
    }
}
/// `perror(str)` — print `str: errno_message\n` to stderr.
pub(crate) fn handle_perror(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let s_va = engine.read_rcx()?;
    let prefix = if s_va != 0 {
        read_guest_str(engine, s_va, 256)?
    } else {
        String::new()
    };
    // Read errno from the fixed guest slot (set by _errno() / pthread).
    let mut errno_bytes = [0_u8; 4];
    if engine.mem_read(ERRNO_SLOT_VA, &mut errno_bytes).is_ok() {
        let errno_val = i32::from_le_bytes(errno_bytes);
        let desc = std::io::Error::from_raw_os_error(errno_val).to_string();
        let msg = if prefix.is_empty() {
            format!("{desc}\n")
        } else {
            format!("{prefix}: {desc}\n")
        };
        crate::kernel32::console::emit_text_from_bytes(ctx, msg.as_bytes());
    } else {
        // Can't read errno — still print the prefix.
        if !prefix.is_empty() {
            let msg = format!("{prefix}: Unknown error\n");
            crate::kernel32::console::emit_text_from_bytes(ctx, msg.as_bytes());
        }
    }
    let engine = &mut *ctx.engine;
    finish(engine, 0)
}
/// `_beginthreadex` — same worker spawn path as `CreateThread` (MSVC CRT).
///
/// ABI (x64): security, stack_size, start, arg, initflag, thrdaddr — identical
/// layout to `CreateThread` for the args we care about.
pub(crate) fn handle_begin_thread_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
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
    finish(engine, handle)
}

/// `_endthreadex` — terminate the current guest worker (like `ExitThread`).
pub(crate) fn handle_end_thread_ex(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
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
pub(crate) fn handle_purecall(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    // Pure virtual call — abort-like.
    finish(engine, 0)
}

pub(crate) fn handle_terminate_cxx(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    finish(engine, 0)
}

pub(crate) fn handle_type_info_dtor(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let this = engine.read_rcx()?;
    finish(engine, this)
}
/// `_CxxThrowException(pExceptionObject, pThrowInfo)` — MSVC C++ throw.
///
/// Builds the usual MSVC EH `EXCEPTION_RECORD` payload and enters the shared
/// two-pass SEH dispatcher (host FuncInfo / LSDA search + register restore).
pub(crate) fn handle_cxx_throw_exception(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
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
    engine.mem_write(rec, &crate::seh::MSVC_EXCEPTION_CODE.to_le_bytes())?;
    engine.mem_write(rec.saturating_add(4), &1_u32.to_le_bytes())?; // noncontinuable
    engine.mem_write(rec.saturating_add(8), &[0u8; 8])?;
    engine.mem_write(rec.saturating_add(16), &rip.to_le_bytes())?;
    engine.mem_write(rec.saturating_add(24), &4_u32.to_le_bytes())?; // NumberParameters
    // Parameters[0] = EH magic, [1] = object, [2] = ThrowInfo, [3] = image base (0)
    engine.mem_write(
        rec.saturating_add(32),
        &u64::from(crate::msvc_eh::FUNCINFO_MAGIC_V1).to_le_bytes(),
    )?;
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
