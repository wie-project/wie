//! Host-side UCRT / `api-ms-win-crt-*` API set for PE64 CRT-linked programs.
//!
//! The UCRT emulation handles printf/scanf-style format parsing, locale helpers,
//! and ctype operations with intentional integer arithmetic, indexing, and casts.
//!
//! Clean room: implement enough of the Universal CRT surface that a normal
//! mingw/MSVC CRT startup + `main` can run. Not a port of Wine/ReactOS UCRT.
//!
//! API set DLLs (`api-ms-win-crt-stdio-l1-1-0.dll`, …) are Windows forwarders to
//! `ucrtbase.dll`; we treat them as one dispatch namespace by export name.
//!
//! Split into `stdio`, `crt`, `string`, `format`, and `misc` submodules. This
//! file keeps the dense dispatch table and the shared helpers (guest-VA
//! constants, `finish`, the string reader, status bitcasts) plus the small
//! heap-handler group.

use crate::{HandlerContext, WinApiHandlerResult};
use anyhow::Result;

mod crt;
mod format;
mod misc;
mod stdio;
mod string;

#[cfg(test)]
mod tests;

// Keep `crate::ucrt::pad_or_trim` resolving for the vsprintf handler in `stdio`.
use stdio::pad_or_trim;

#[cfg(unix)]
pub(crate) use stdio::write_all_fd;

use crt::{
    handle_abort, handle_cexit, handle_configure_narrow_argv, handle_configure_wide_argv,
    handle_crt_atexit, handle_exit_like, handle_fpreset, handle_getenv, handle_getmainargs,
    handle_initialize_narrow_environment, handle_initialize_wide_environment, handle_initterm,
    handle_initterm_e, handle_onexit, handle_p_acmdln, handle_p_argc, handle_p_argv,
    handle_p_commode, handle_p_environ, handle_p_fmode, handle_p_wargv, handle_p_wenviron,
    handle_set_app_type, handle_set_new_mode,
};
use format::{
    handle_snprintf_s, handle_sprintf_s, handle_stdio_common_vsprintf_s,
    handle_stdio_common_vswprintf_s, handle_vsnprintf, handle_vsnprintf_s, handle_vsnwprintf,
    handle_vsnwprintf_s,
};
use misc::{
    handle_atoi, handle_atol, handle_begin_thread_ex, handle_c_specific_handler,
    handle_config_thread_locale, handle_cxx_throw_exception, handle_end_thread_ex, handle_errno,
    handle_get_osfhandle, handle_getch, handle_isatty, handle_kbhit, handle_localtime64,
    handle_perror, handle_purecall, handle_rand, handle_set_invalid_parameter_handler,
    handle_set_user_matherr, handle_setlocale, handle_signal, handle_srand, handle_strerror,
    handle_system, handle_terminate_cxx, handle_time64, handle_type_info_dtor, handle_xcpt_filter,
};
use stdio::{
    handle_acrt_iob_func, handle_fclose, handle_fflush, handle_fgetc, handle_fgets, handle_fgetwc,
    handle_fopen, handle_fopen_s, handle_fputc, handle_fputs, handle_freopen_s, handle_fwrite,
    handle_getchar, handle_putchar, handle_puts, handle_setvbuf, handle_stdio_common_vfprintf,
    handle_stdio_common_vfscanf, handle_stdio_common_vsprintf, handle_stdio_common_vsscanf,
    handle_vfprintf,
};
use string::{
    handle_isalnum, handle_isalpha, handle_isdigit, handle_islower, handle_isspace, handle_isupper,
    handle_iswctype, handle_memcmp, handle_memcpy, handle_memcpy_s, handle_memmove_s,
    handle_memset, handle_memset_s, handle_qsort_s, handle_strcat_s, handle_strchr, handle_strcmp,
    handle_strcpy_s, handle_strlen, handle_strncat_s, handle_strncmp, handle_strncpy,
    handle_strncpy_s, handle_strstr, handle_strtod, handle_strtok, handle_strtok_s, handle_strtol,
    handle_strtoul, handle_tolower, handle_toupper, handle_towupper, handle_wcscat, handle_wcscmp,
    handle_wcscpy, handle_wcslen, handle_wcsncmp, handle_wcsncpy, handle_wcsnicmp, handle_wcsrchr,
    handle_wcsstr,
};
/// Guest VA base for synthetic CRT objects (FILE cookies, env pointers, etc.).
const ACMDLN_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x328;
const CRT_GUEST_BASE: u64 = 0x0000_0000_6800_0000;
/// CRT `errno` values the secure `_s` handlers report (Win32 CRT numbering).
pub(crate) const EINVAL: i32 = 22;
pub(crate) const ERANGE: i32 = 34;
pub(crate) const ENOENT: i32 = 2;
const FILE_STDIN: u64 = CRT_GUEST_BASE;
const FILE_STDOUT: u64 = CRT_GUEST_BASE + 0x100;
const FILE_STDERR: u64 = CRT_GUEST_BASE + 0x200;
const ENVIRON_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x300;
const ARGV_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x308;
const ARGC_SLOT: u64 = CRT_GUEST_BASE + 0x310;
const COMMODE_SLOT: u64 = CRT_GUEST_BASE + 0x318;
const FMODE_SLOT: u64 = CRT_GUEST_BASE + 0x320;
/// Slot holding `wchar_t**` (wide argv table) for `__p___wargv`.
const WARGV_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x330;
/// Slot holding `wchar_t**` (wide environment table) for `__p__wenviron`.
const WENVIRON_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x338;
/// Narrow `char* argv[]` pointer table materialized by the runtime session
/// (see `crates/wie-runtime/src/session/mod.rs` `CRT_ARGV_TABLE`).
const NARROW_ARGV_TABLE: u64 = CRT_GUEST_BASE + 0x400;
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
#[must_use]
pub fn is_ucrt_library(library: &str) -> bool {
    let mut scratch = [0_u8; ASCII_LOWER_SCRATCH];
    let mut owned: Option<String> = None;
    let l = ascii_lower(library, &mut scratch, &mut owned);
    // Legacy VC7+ runtimes (msvcr71 … msvcp140) forward to the same name
    // dispatch as msvcrt: their exports (printf, memcpy, sprintf_s,
    // `??2@YAPEAX_K@Z`, …) are all CRT functions already handled below. Data
    // imports (_iob, _fmode, …) resolve via `crt_data_import_va`.
    l.starts_with("api-ms-win-crt-")
        || l == "ucrtbase.dll"
        || l == "msvcrt.dll"
        || l == "msvcr71.dll"
        || l == "msvcp71.dll"
        || l == "msvcr100.dll"
        || l == "msvcp100.dll"
        || l == "msvcr110.dll"
        || l == "msvcp110.dll"
        || l == "msvcr120.dll"
        || l == "msvcp120.dll"
        || l == "msvcr140.dll"
        || l == "msvcp140.dll"
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
        // Legacy msvcrt: `wchar_t *_wcmdln` (wide command line). The session
        // materializes no dedicated wide slot and `crt_data_import_va` is pure
        // (no engine to write one), so `_wcmdln` aliases the narrow
        // `ACMDLN_PTR_SLOT` cell: the session fills it with the ANSI command
        // line pointer (`env_data.base + 0x100`), a valid mapped address. Read
        // as `wchar_t*` it is a NUL-terminated buffer of interleaved ANSI
        // bytes (no spaces — notepad's `*__p__wcmdln()` + wchar scan sees no
        // args), never a dangling pointer. WIE's host-side `__wgetmainargs` /
        // `GetCommandLineW` paths use the wide env-page buffer
        // (`env_data.base + 0x200`) directly, so this value is only a
        // load-safety contract.
        "_wcmdln" => Some(ACMDLN_PTR_SLOT),
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
        "_vsnwprintf" => handle_vsnwprintf(ctx),
        "_vsnprintf" => handle_vsnprintf(ctx),
        // Secure-CRT `_s` format variants (MSVCR100+).
        "sprintf_s" => handle_sprintf_s(ctx),
        "snprintf_s" | "_snprintf_s" => handle_snprintf_s(ctx),
        "_vsnprintf_s" => handle_vsnprintf_s(ctx),
        "_vsnwprintf_s" => handle_vsnwprintf_s(ctx),
        "__stdio_common_vsprintf_s" => handle_stdio_common_vsprintf_s(ctx),
        "__stdio_common_vswprintf_s" => handle_stdio_common_vswprintf_s(ctx),
        "__stdio_common_vfprintf" => handle_stdio_common_vfprintf(ctx),
        "__stdio_common_vsprintf" => handle_stdio_common_vsprintf(ctx),
        "__stdio_common_vsscanf" => handle_stdio_common_vsscanf(ctx),
        "__stdio_common_vfscanf" => handle_stdio_common_vfscanf(ctx),
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
        "__c_specific_handler" => handle_c_specific_handler(ctx),
        "__cxxframehandler" => handle_cxx_frame_handler(ctx),
        "memcpy" | "memmove" => handle_memcpy(ctx),
        "memcmp" => handle_memcmp(ctx),
        "memset" => handle_memset(ctx),
        // Secure-CRT bounds-checked memory variants.
        "memcpy_s" => handle_memcpy_s(ctx),
        "memmove_s" => handle_memmove_s(ctx),
        "memset_s" => handle_memset_s(ctx),
        "strlen" => handle_strlen(ctx),
        "strncmp" => handle_strncmp(ctx),
        "_initterm" => handle_initterm(ctx),
        "_initterm_e" => handle_initterm_e(ctx),
        "_configure_narrow_argv" => handle_configure_narrow_argv(ctx),
        "_initialize_narrow_environment" => handle_initialize_narrow_environment(ctx),
        "_configure_wide_argv" => handle_configure_wide_argv(ctx),
        "_initialize_wide_environment" => handle_initialize_wide_environment(ctx),
        "__p___wargv" => handle_p_wargv(ctx),
        "__p__wenviron" => handle_p_wenviron(ctx),
        "_fpreset" => handle_fpreset(ctx),
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
        "perror" => handle_perror(ctx),
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
        "putchar" => handle_putchar(ctx),
        "getchar" => handle_getchar(ctx),
        "fputs" => handle_fputs(ctx),
        "atoi" => handle_atoi(ctx),
        "atol" => handle_atol(ctx),
        "strtol" => handle_strtol(ctx),
        "strtoul" => handle_strtoul(ctx),
        "strtod" | "strtof" => handle_strtod(ctx),
        "fopen" => handle_fopen(ctx),
        "fclose" => handle_fclose(ctx),
        // Secure-CRT FILE** variants (fopen always fails: no VFS-to-CRT bridge).
        "fopen_s" => handle_fopen_s(ctx),
        "freopen_s" => handle_freopen_s(ctx),
        "fgets" => handle_fgets(ctx),
        "fgetc" => handle_fgetc(ctx),
        // getc is a macro for fgetc in the real headers; msvcrt exports both.
        "getc" => handle_fgetc(ctx),
        "fgetwc" => handle_fgetwc(ctx),
        "vfprintf" => handle_vfprintf(ctx),
        "strtok" => handle_strtok(ctx),
        "strcmp" => handle_strcmp(ctx),
        "strchr" => handle_strchr(ctx),
        "strstr" => handle_strstr(ctx),
        "strncpy" => handle_strncpy(ctx),
        // Secure-CRT size-checked string variants.
        "strcpy_s" => handle_strcpy_s(ctx),
        "strncpy_s" => handle_strncpy_s(ctx),
        "strcat_s" => handle_strcat_s(ctx),
        "strncat_s" => handle_strncat_s(ctx),
        "strtok_s" | "_strtok_s" => handle_strtok_s(ctx),
        "qsort_s" => handle_qsort_s(ctx),
        "isalpha" => handle_isalpha(ctx),
        "isdigit" => handle_isdigit(ctx),
        "isalnum" => handle_isalnum(ctx),
        "islower" => handle_islower(ctx),
        "isupper" => handle_isupper(ctx),
        "isspace" => handle_isspace(ctx),
        "iswctype" => handle_iswctype(ctx),
        "toupper" => handle_toupper(ctx),
        "tolower" => handle_tolower(ctx),
        "wcscmp" => handle_wcscmp(ctx),
        "wcsstr" => handle_wcsstr(ctx),
        "wcslen" => handle_wcslen(ctx),
        "wcscat" => handle_wcscat(ctx),
        "wcscpy" => handle_wcscpy(ctx),
        "wcsncmp" => handle_wcsncmp(ctx),
        "wcsncpy" => handle_wcsncpy(ctx),
        "_wcsnicmp" => handle_wcsnicmp(ctx),
        "towupper" => handle_towupper(ctx),
        "wcsrchr" => handle_wcsrchr(ctx),
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
fn finish(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
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
/// `__CxxFrameHandler(pExceptionObject, pContextRecord, pDispatcherContext,
/// pFuncInfo)` — the MSVC x64 C++ EH frame entry.
///
/// WIE drives MSVC EH host-side (`msvc_eh::find_msvc_catch` from
/// `_CxxThrowException`; see `crates/wie-winapi/src/seh.rs`), so the guest
/// never calls this export during a dispatch — MSVC-compiled PEs still import
/// it, so it must resolve. When guest code calls it directly there is no
/// host-dispatched handler in progress, so the honest disposition is
/// `EXCEPTION_CONTINUE_SEARCH` (0).
pub(crate) fn handle_cxx_frame_handler(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let pfunc_info = engine.read_r9()?;
    tracing::debug!(
        pfunc_info = format_args!("{pfunc_info:#x}"),
        "msvcrt!__CxxFrameHandler → EXCEPTION_CONTINUE_SEARCH (host-side MSVC EH dispatch not active)"
    );
    finish(engine, 0)
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
    finish(engine, ptr)
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
    finish(engine, ptr)
}

fn handle_free(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let ptr = engine.read_rcx()?;
    if ptr != 0 {
        let _ = state.heap_state.heap.free_coherent(engine, ptr);
    }
    finish(engine, 0)
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
        return finish(engine, p);
    }
    if new_size == 0 {
        let _ = state.heap_state.heap.free_coherent(engine, ptr);
        return finish(engine, 0);
    }
    if let Some(same) = state.heap_state.heap.try_realloc_in_place(ptr, new_size) {
        return finish(engine, same);
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
        return finish(engine, 0);
    }
    let copy_len = usize::try_from(old_size.min(new_size)).unwrap_or(0);
    if copy_len > 0 {
        let mut bytes = vec![0_u8; copy_len];
        engine.mem_read(ptr, &mut bytes)?;
        engine.mem_write(new_addr, &bytes)?;
    }
    let _ = state.heap_state.heap.free_coherent(engine, ptr);
    finish(engine, new_addr)
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
