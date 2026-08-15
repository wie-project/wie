//! KERNEL32 handlers: process/thread, console, heap, file I/O, string, and
//! sync APIs plus the fake process/time constants. Submodules split handlers
//! by concern; this file re-exports the shared guest-memory/string helpers.

pub(crate) use crate::guest_memory::{
    checked_address, read_u16, read_u32, read_u64, write_u16 as write_guest_u16,
    write_u32 as write_guest_u32, write_u64 as write_guest_u64,
};
pub(crate) use crate::guest_string::{
    read_ansi_lossy as read_guest_ansi_lossy, read_utf16_lossy as read_guest_utf16_lossy,
    write_utf16_units as write_guest_utf16_units,
};
pub(crate) use crate::{
    FindHandle, FlsSlot, GlobalAtomRecord, OpenGuestFile, ResourceRecord, WinApiState,
};
use crate::{HandlerContext, dll_loader};
pub(crate) use anyhow::{Context, Result};
pub(crate) use std::path::Path;
pub(crate) use std::sync::OnceLock;

const FIXED_SYSTEM_FILETIME: u64 = 133_485_408_000_000_000;
pub(crate) const FAKE_CURRENT_PROCESS_ID: u64 = 0x1234;
const FIXED_TICK_COUNT: u64 = 12_345;
const FIXED_PERFORMANCE_COUNTER: u64 = 1_000_000;
const FLS_OUT_OF_INDEXES: u64 = 0xffff_ffff;
const STD_INPUT_HANDLE_ID: u32 = 0xffff_fff6;
const STD_OUTPUT_HANDLE_ID: u32 = 0xffff_fff5;
const STD_ERROR_HANDLE_ID: u32 = 0xffff_fff4;

/// Fake console handles returned by `GetStdHandle` (Microsoft Learn std ids).
pub(crate) const FAKE_STDIN_HANDLE: u64 = 0x0000_0000_6000_0001;
pub(crate) const FAKE_STDOUT_HANDLE: u64 = 0x0000_0000_6000_0002;
pub(crate) const FAKE_STDERR_HANDLE: u64 = 0x0000_0000_6000_0003;

/// Cap for a single host console line fill (safety against huge pastes).
const MAX_HOST_STDIN_LINE: usize = 64 * 1024;

// Read one line from host stdin (through `\n` or EOF), capped at
// [`MAX_HOST_STDIN_LINE`].
//
// Models Microsoft Learn default console line input (`ENABLE_LINE_INPUT`):
// `ReadFile` on a console handle does not complete until a carriage return
// is entered. On Unix hosts we treat `\n` as the line terminator.
//
// Returns:
// - `Ok(Some(bytes))` — non-empty fill (may omit `\n` if cap hit first)
// - `Ok(None)` — host EOF with no bytes
// - `Err(_)` — host I/O error
//
// When the inject/live buffer is empty and live mode is on, block on host
// stdin for one line and store it in `state.file_io.stdin_bytes`.
//
// Returns `Ok(true)` if bytes were stored, `Ok(false)` on host EOF,
// `Err(())` on host I/O failure (caller sets `ERROR_READ_FAULT`).

const FILE_TYPE_UNKNOWN: u64 = 0x0000;
const FILE_TYPE_DISK: u64 = 0x0001;
const FILE_TYPE_CHAR: u64 = 0x0002;

const ANSI_CODE_PAGE: u64 = 1252;
const OEM_CODE_PAGE: u64 = 437;

const CT_CTYPE1: u64 = 1;

const C1_UPPER: u16 = 0x0001;
const C1_LOWER: u16 = 0x0002;
const C1_DIGIT: u16 = 0x0004;
const C1_SPACE: u16 = 0x0008;
const C1_PUNCT: u16 = 0x0010;
const C1_CNTRL: u16 = 0x0020;
const C1_BLANK: u16 = 0x0040;
const C1_XDIGIT: u16 = 0x0080;
const C1_ALPHA: u16 = 0x0100;

const HEAP_SIZE_FAILURE: u64 = u64::MAX;
/// `HEAP_ZERO_MEMORY` (heapapi.h / Microsoft Learn).
const HEAP_ZERO_MEMORY: u64 = 0x0000_0008;

/// `IsProcessorFeaturePresent` feature ids (winnt.h `PF_*`).
const PF_FLOATING_POINT_PRECISION_ERRATA: u32 = 0;
const PF_COMPARE_EXCHANGE_DOUBLE: u32 = 6;
const PF_MMX_INSTRUCTIONS_AVAILABLE: u32 = 7;
const PF_XMMI_INSTRUCTIONS_AVAILABLE: u32 = 8;
const PF_3DNOW_INSTRUCTIONS_AVAILABLE: u32 = 10;
const PF_XMMI64_INSTRUCTIONS_AVAILABLE: u32 = 13;
const PF_SSE3_INSTRUCTIONS_AVAILABLE: u32 = 14;
const PF_NX_ENABLED: u32 = 21;
const PF_RDTSC_INSTRUCTION_AVAILABLE: u32 = 23;
const PF_COMPARE_EXCHANGE128: u32 = 25;

const FAKE_KERNEL32_MODULE: u64 = 0x0000_0000_6100_0000;
const FAKE_USER32_MODULE: u64 = 0x0000_0000_6100_1000;
const FAKE_GDI32_MODULE: u64 = 0x0000_0000_6100_2000;
const FAKE_COMCTL32_MODULE: u64 = 0x0000_0000_6100_3000;
const FAKE_ADVAPI32_MODULE: u64 = 0x0000_0000_6100_4000;
const FAKE_SHELL32_MODULE: u64 = 0x0000_0000_6100_5000;
const FAKE_COMDLG32_MODULE: u64 = 0x0000_0000_6100_6000;
const FAKE_WINMM_MODULE: u64 = 0x0000_0000_6100_7000;

const INVALID_FILE_ATTRIBUTES: u64 = 0xffff_ffff;
/// `FILE_ATTRIBUTE_DIRECTORY` (winnt.h) — the entry is a directory.
const FILE_ATTRIBUTE_DIRECTORY: u64 = 0x0000_0010;
/// `FILE_ATTRIBUTE_ARCHIVE` (winnt.h) — the entry is marked for archiving.
const FILE_ATTRIBUTE_ARCHIVE: u64 = 0x0000_0020;

pub(crate) const INVALID_HANDLE_VALUE: u64 = u64::MAX;
/// Win32 `ERROR_NO_MORE_FILES` — the enumeration is exhausted.
const ERROR_NO_MORE_FILES: u32 = 18;

/// Win32 `ERROR_INVALID_FUNCTION` — the operation is not supported on this handle.
pub(crate) const ERROR_INVALID_FUNCTION: u32 = 1;
/// Win32 `ERROR_HANDLE_EOF` — the handle reached end-of-file (FindFirstStreamW).
pub(crate) const ERROR_HANDLE_EOF: u32 = 38;
/// `DUPLICATE_CLOSE_SOURCE` (process.h) — DuplicateHandle flag: close the source.
pub(crate) const DUPLICATE_CLOSE_SOURCE: u32 = 0x1;

const FAKE_RESOURCE_DATA_BASE: u64 = 0x0000_0000_6400_0000;
const FAKE_RESOURCE_SIZE: u32 = 16;
const FAKE_RESOURCE_BYTES: [u8; 16] = [
    0x4c, 0x4d, 0x52, 0x53, // "WIERS"
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

const LANG_EN_US: u64 = 0x0409;

/// Win32 `ERROR_INVALID_HANDLE` — the handle is not open (fileapi.h).
const ERROR_INVALID_HANDLE: u32 = 6;
/// Win32 `ERROR_INVALID_PARAMETER` — an argument is malformed.
const ERROR_INVALID_PARAMETER: u32 = 87;
/// Win32 `ERROR_INSUFFICIENT_BUFFER` — the caller's buffer is too small.
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
/// Win32 `ERROR_READ_FAULT` — host console stdin I/O failure.
const ERROR_READ_FAULT: u32 = 30;

const TIME_ZONE_ID_UNKNOWN: u64 = 0;
const TIME_ZONE_ID_INVALID: u64 = 0xffff_ffff;

/// `FILE_BEGIN` — `SetFilePointer` move method: relative to the file start.
const FILE_BEGIN: u64 = 0;
/// `FILE_CURRENT` — `SetFilePointer` move method: relative to the current position.
const FILE_CURRENT: u64 = 1;
/// `FILE_END` — `SetFilePointer` move method: relative to the file end.
const FILE_END: u64 = 2;
/// `INVALID_SET_FILE_POINTER` — the `SetFilePointer` failure sentinel.
const INVALID_SET_FILE_POINTER: u64 = 0xffff_ffff;

/// Win32 `ERROR_FILE_NOT_FOUND` — the named file does not exist.
const ERROR_FILE_NOT_FOUND: u32 = 2;
/// Win32 `ERROR_PATH_NOT_FOUND` — a path component does not exist.
const ERROR_PATH_NOT_FOUND: u32 = 3;
/// Win32 `ERROR_ACCESS_DENIED` — the operation is not permitted.
const ERROR_ACCESS_DENIED: u32 = 5;
/// Win32 `ERROR_INVALID_DRIVE` — SetCurrentDirectory on an unmapped drive.
const ERROR_INVALID_DRIVE: u32 = 15;
/// CreateFile CREATE_NEW when the file already exists (Microsoft Learn).
const ERROR_FILE_EXISTS: u32 = 80;
/// Win32 `ERROR_MOD_NOT_FOUND` — the requested module is not loaded.
const ERROR_MOD_NOT_FOUND: u32 = 126;
/// Win32 `ERROR_PROC_NOT_FOUND` — the requested export is not present.
const ERROR_PROC_NOT_FOUND: u32 = 127;
/// Win32 `ERROR_ALREADY_EXISTS` — the object already exists.
const ERROR_ALREADY_EXISTS: u32 = 183;
/// Win32 `ERROR_DIR_NOT_EMPTY` — the directory still holds entries.
const ERROR_DIR_NOT_EMPTY: u32 = 145;

// CreateFile disposition values (fileapi.h `CREATE_*` / `OPEN_*`).
/// CreateFile `CREATE_NEW` — fail if the file already exists.
const CREATE_NEW: u64 = 1;
/// CreateFile `CREATE_ALWAYS` — overwrite an existing file.
const CREATE_ALWAYS: u64 = 2;
/// CreateFile `OPEN_EXISTING` — fail if the file does not exist.
const OPEN_EXISTING: u64 = 3;
/// CreateFile `OPEN_ALWAYS` — open, creating when absent.
const OPEN_ALWAYS: u64 = 4;
/// CreateFile `TRUNCATE_EXISTING` — open and truncate to zero length.
const TRUNCATE_EXISTING: u64 = 5;

/// Result returned by a WinAPI handler.
#[derive(Debug, Clone, Copy)]
pub struct WinApiHandlerResult {
    /// Address where emulation should resume.
    pub return_address: u64,

    /// Value written into `RAX`.
    pub return_value: u64,
}

pub(crate) fn low_u32(value: u64, context_name: &str) -> Result<u32> {
    u32::try_from(value & 0xffff_ffff)
        .with_context(|| format!("{context_name} low u32 conversion failed"))
}

// Guest OS identity shared by `GetVersion` / `GetVersionEx*`.
const GUEST_OS_MAJOR: u32 = 10;
const GUEST_OS_MINOR: u32 = 0;
const GUEST_OS_BUILD: u32 = 19045;
const GUEST_OS_PLATFORM_NT: u32 = 2;

// Packed `GetVersion` DWORD for the emulated OS (NT bit set in high word).

///
/// `lpModuleName == NULL` returns the main module image base from the PE
/// (`WinApiEnvironment::image_base`), not a hardcoded Lunar Magic address.
///
/// Microsoft Learn: `lpModuleName == NULL` → handle of the calling process's
/// `.exe`. Named module must already be loaded; otherwise returns `NULL`.
pub(crate) fn read_ansi_string_from_cpu(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_len: usize,
) -> Result<String> {
    // Byte-at-a-time: bulk reads of MAX_PATH-sized buffers fail when the string
    // sits near the end of a mapped PE page (common for freestanding micros).
    read_guest_ansi_lossy(engine, address, max_len)
}

/// Read a NUL-terminated wide string from guest memory, strict-UTF-16.
///
/// Shares the page-safe 4 KiB bulk read loop with `read_utf16_lossy`; the
/// strict decode (`from_utf16`) is the KERNEL32 W-string contract — a lone
/// surrogate fails the read instead of becoming U+FFFD.
pub(crate) fn read_wide_string_from_cpu(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_units: usize,
) -> Result<String> {
    crate::guest_string::read_utf16(
        engine,
        address,
        max_units,
        crate::guest_string::Utf16Decode::Strict,
    )
}

pub(crate) fn create_fake_resource_record(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<ResourceRecord> {
    let handle = state.file_io.next_resource_handle.as_u64();
    state.file_io.next_resource_handle = crate::ResourceHandle::from(
        state
            .file_io
            .next_resource_handle
            .as_u64()
            .checked_add(1)
            .context("resource handle overflow")?,
    );

    let index =
        u64::try_from(state.file_io.resources.len()).context("resource index does not fit u64")?;
    let data_offset = index
        .checked_mul(0x100)
        .context("resource data offset overflow")?;

    let data_va = FAKE_RESOURCE_DATA_BASE
        .checked_add(data_offset)
        .context("resource data pointer overflow")?;

    engine
        .mem_write(data_va, &FAKE_RESOURCE_BYTES)
        .context("failed to write fake resource bytes")?;

    // For this compatibility harness, make the loaded resource handle pointer-like.
    // Some old Win32-style code uses the result of LoadResource directly as data.
    let loaded_handle = data_va;

    let record = ResourceRecord {
        handle,
        loaded_handle,
        data_ptr: data_va,
        size: FAKE_RESOURCE_SIZE,
    };

    state.file_io.resources.push(record.clone());

    Ok(record)
}

pub(crate) fn find_resource_by_handle(state: &WinApiState, handle: u64) -> Option<&ResourceRecord> {
    state
        .file_io
        .resources
        .iter()
        .find(|resource| resource.handle == handle || resource.loaded_handle == handle)
}

// Returns the active guest TID from [`crate::ThreadState`] (primary `0x5678`
// until MT.2 spawns workers).

// Microsoft Learn (`heapapi.h`):
// - success → pointer to allocated block (at least `dwBytes`)
// - failure → `NULL` (does not call `SetLastError`)
// - `HEAP_ZERO_MEMORY` zeros the block
// - `dwBytes == 0` allocates a zero-length item and still returns a valid pointer
//   (same practical behaviour as the Windows process heap / CRT `malloc(0)`)

// Microsoft Learn: `lpMem` may be `NULL` (no-op, success). Double-free /
// unknown pointer fails with a non-zero last-error in this emulator
// (`ERROR_INVALID_HANDLE`) so freestanding tests can detect the failure.

// Microsoft Learn: preserves contents; failure leaves the original block valid
// and returns `NULL`. `dwBytes == 0` is treated as free + `NULL` (common Windows
// process-heap behaviour used by the micro-suite).

//
// Guest `RTL_CRITICAL_SECTION` layout (Win64) written by Initialize*:
// `LockCount` (-1 unlocked), `RecursionCount`, `OwningThread` (guest TID).
//
// Contended path: returns [`crate::WinApiControlSignal::HostPark`] so the
// session drops the shared CPU lock and waits on the CS condvar (MT.3).

// Zeros the CS fields. Calling Delete while owned is undefined on Windows;
// we still clear so a subsequent Initialize can reuse the memory.

/// Result of a non-blocking CS enter attempt.
pub(crate) enum EnterCsResult {
    Acquired,
    NeedPark,
}

// Try enter (or re-enter) a guest critical section for `owner_tid`.

// Leave a guest critical section owned by `owner_tid`.
//
// Returns `true` if the CS became fully unlocked (wake one waiter).

// Publish one FLS slot into the guest table used by in-guest `FlsGetValue`.

//
// Lean host path (also fallback for guest SBCS helper). Single-byte code pages
// use zero-extend (matches guest accelerator); others use UTF-8 lossy.

// Zero-extend each byte to UTF-16 (SBCS / Latin-1 identity).

//
// Microsoft Learn: returns character count excluding NUL. If the buffer is too
// small, the path is truncated (NUL-terminated), the return value is `nSize`,
// and last-error is `ERROR_INSUFFICIENT_BUFFER`.

//
// Microsoft Learn: returns the export address, or `NULL` if not found
// (`GetLastError` → `ERROR_PROC_NOT_FOUND`). Does **not** abort the process.

// ─── Soft console / process helpers for real CLI tools (7za) ────────────────

const FIXED_PERFORMANCE_FREQUENCY: u64 = 10_000_000;
// Console mode bits and their defaults now live in `crate::console`, which owns
// the mode words themselves rather than reporting a fixed constant.

const FAKE_DISK_GIB: u64 = 1024 * 1024 * 1024;
/// ~100 GiB of 4 KiB clusters (8 sectors × 512).
const FAKE_DISK_CLUSTERS: u32 = 26_214_400;
/// Drive string payload: `C:\` + NUL + final NUL (TCHARs).
const LOGICAL_DRIVE_TCHARS: u32 = 4;

pub mod clock;
pub mod console;
pub mod console_cells;
pub mod console_input;
pub mod environment;
pub mod file_io;
pub mod heap;
pub mod memory;
pub mod misc;
pub mod module;
pub mod process_thread;
pub mod string;
pub mod sync;

pub use clock::*;
pub use console::*;
pub use console_cells::*;
pub use console_input::*;
pub use environment::*;
pub use file_io::*;
pub use heap::*;
pub use memory::*;
pub use misc::*;
pub use module::*;
pub use process_thread::*;
pub use string::*;
pub use sync::*;

pub fn dispatch_kernel32_extra(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "virtualalloc" => Ok(Some(handle_virtual_alloc(ctx)?)),
        "virtualfree" => Ok(Some(handle_virtual_free(ctx)?)),
        "virtualprotect" => Ok(Some(handle_virtual_protect(ctx)?)),
        "virtualquery" => Ok(Some(handle_virtual_query(ctx)?)),
        "flushinstructioncache" => Ok(Some(handle_flush_instruction_cache(ctx)?)),
        "tlsgetvalue" => Ok(Some(handle_tls_get_value(ctx)?)),
        "tlssetvalue" => Ok(Some(handle_tls_set_value(ctx)?)),
        "tlsalloc" => Ok(Some(handle_tls_alloc(ctx)?)),
        "tlsfree" => Ok(Some(handle_tls_free(ctx)?)),
        // MT.2 / MT.3
        "createthread" => Ok(Some(handle_create_thread(ctx)?)),
        "exitthread" => Ok(Some(handle_exit_thread(ctx)?)),
        "getexitcodethread" => Ok(Some(handle_get_exit_code_thread(ctx)?)),
        "waitforsingleobject" => Ok(Some(handle_wait_for_single_object(ctx)?)),
        "createeventa" | "createeventw" => Ok(Some(handle_create_event(ctx)?)),
        "setevent" => Ok(Some(handle_set_event(ctx)?)),
        "resetevent" => Ok(Some(handle_reset_event(ctx)?)),
        "getcurrentthread" => Ok(Some(handle_get_current_thread(ctx)?)),
        // MT.4 Interlocked* (host atomics on soft-translated guest memory)
        "interlockedincrement" => Ok(Some(handle_interlocked_increment(ctx)?)),
        "interlockeddecrement" => Ok(Some(handle_interlocked_decrement(ctx)?)),
        "interlockedexchange" => Ok(Some(handle_interlocked_exchange(ctx)?)),
        "interlockedcompareexchange" => Ok(Some(handle_interlocked_compare_exchange(ctx)?)),
        "interlockedexchangeadd" => Ok(Some(handle_interlocked_exchange_add(ctx)?)),
        "interlockedincrement64" => Ok(Some(handle_interlocked_increment64(ctx)?)),
        "interlockeddecrement64" => Ok(Some(handle_interlocked_decrement64(ctx)?)),
        "interlockedexchange64" => Ok(Some(handle_interlocked_exchange64(ctx)?)),
        "interlockedcompareexchange64" => Ok(Some(handle_interlocked_compare_exchange64(ctx)?)),
        "interlockedexchangeadd64" => Ok(Some(handle_interlocked_exchange_add64(ctx)?)),
        // Real-tool surface (7z / CRT-linked PE)
        "getversion" => Ok(Some(handle_get_version(ctx)?)),
        "getmodulehandlew" => Ok(Some(handle_get_module_handle_w(ctx)?)),
        "lstrlenw" => Ok(Some(handle_lstrlen_w(ctx)?)),
        "lstrcpyw" => Ok(Some(handle_lstrcpy_w(ctx)?)),
        "lstrcatw" => Ok(Some(handle_lstrcat_w(ctx)?)),
        // Console / process identity
        "allocconsole" => Ok(Some(handle_alloc_console(ctx)?)),
        "attachconsole" => Ok(Some(handle_attach_console(ctx)?)),
        "createconsolescreenbuffer" => Ok(Some(handle_create_console_screen_buffer(ctx)?)),
        "fillconsoleoutputattribute" => Ok(Some(handle_fill_console_output_attribute(ctx)?)),
        "fillconsoleoutputcharactera" => Ok(Some(handle_fill_console_output_character_a(ctx)?)),
        "fillconsoleoutputcharacterw" => Ok(Some(handle_fill_console_output_character_w(ctx)?)),
        "flushconsoleinputbuffer" => Ok(Some(handle_flush_console_input_buffer(ctx)?)),
        "freeconsole" => Ok(Some(handle_free_console(ctx)?)),
        "getconsolecp" => Ok(Some(handle_get_console_cp(ctx)?)),
        "getconsolecursorinfo" => Ok(Some(handle_get_console_cursor_info(ctx)?)),
        "getconsoleoutputcp" => Ok(Some(handle_get_console_output_cp(ctx)?)),
        "getconsolescreenbufferinfo" => Ok(Some(handle_get_console_screen_buffer_info(ctx)?)),
        "getconsoletitlea" => Ok(Some(handle_get_console_title_a(ctx)?)),
        "getconsoletitlew" => Ok(Some(handle_get_console_title_w(ctx)?)),
        "getconsolewindow" => Ok(Some(handle_get_console_window(ctx)?)),
        "getlargestconsolewindowsize" => Ok(Some(handle_get_largest_console_window_size(ctx)?)),
        "getnumberofconsoleinputevents" => {
            Ok(Some(handle_get_number_of_console_input_events(ctx)?))
        }
        "getnumberofconsolemousebuttons" => {
            Ok(Some(handle_get_number_of_console_mouse_buttons(ctx)?))
        }
        "peekconsoleinputw" => Ok(Some(handle_peek_console_input_w(ctx)?)),
        "readconsoleinputw" => Ok(Some(handle_read_console_input_w(ctx)?)),
        "readconsolew" => Ok(Some(handle_read_console_w(ctx)?)),
        "readconsolea" => Ok(Some(handle_read_console_a(ctx)?)),
        "scrollconsolescreenbufferw" => Ok(Some(handle_scroll_console_screen_buffer_w(ctx)?)),
        "setconsoleactivescreenbuffer" => Ok(Some(handle_set_console_active_screen_buffer(ctx)?)),
        "setconsolecp" => Ok(Some(handle_set_console_cp(ctx)?)),
        "setconsolecursorinfo" => Ok(Some(handle_set_console_cursor_info(ctx)?)),
        "setconsolemode" => Ok(Some(handle_set_console_mode(ctx)?)),
        "getconsolemode" => Ok(Some(handle_get_console_mode(ctx)?)),
        "setconsoleoutputcp" => Ok(Some(handle_set_console_output_cp(ctx)?)),
        "setconsolescreenbuffersize" => Ok(Some(handle_set_console_screen_buffer_size(ctx)?)),
        "setconsolecursorposition" => Ok(Some(handle_set_console_cursor_position(ctx)?)),
        "setconsolectrlhandler" => Ok(Some(handle_set_console_ctrl_handler(ctx)?)),
        "setconsoletitlea" => Ok(Some(handle_set_console_title_a(ctx)?)),
        "setconsoletitlew" => Ok(Some(handle_set_console_title_w(ctx)?)),
        "setconsolewindowinfo" => Ok(Some(handle_set_console_window_info(ctx)?)),
        "writeconsolew" => Ok(Some(handle_write_console_w(ctx)?)),
        "writeconsolea" => Ok(Some(handle_write_console_a(ctx)?)),
        "writeconsoleoutputcharactera" => Ok(Some(handle_write_console_output_character_a(ctx)?)),
        "writeconsoleoutputcharacterw" => Ok(Some(handle_write_console_output_character_w(ctx)?)),
        "writeconsoleoutputattribute" => Ok(Some(handle_write_console_output_attribute(ctx)?)),
        "gettickcount64" => Ok(Some(handle_get_tick_count_64(ctx)?)),
        "getenvironmentvariablew" => Ok(Some(handle_get_environment_variable_w(ctx)?)),
        "getenvironmentvariablea" => Ok(Some(handle_get_environment_variable_a(ctx)?)),
        "setenvironmentvariablew" => Ok(Some(handle_set_environment_variable_w(ctx)?)),
        "setenvironmentvariablea" => Ok(Some(handle_set_environment_variable_a(ctx)?)),
        "expandenvironmentstringsw" => Ok(Some(handle_expand_environment_strings_w(ctx)?)),
        "expandenvironmentstringsa" => Ok(Some(handle_expand_environment_strings_a(ctx)?)),
        "setfileapistooem" => Ok(Some(handle_set_file_apis_to_oem(ctx)?)),
        "queryperformancefrequency" => Ok(Some(handle_query_performance_frequency(ctx)?)),
        "getsysteminfo" => Ok(Some(handle_get_system_info(ctx)?)),
        "isprocessorfeaturepresent" => Ok(Some(handle_is_processor_feature_present(ctx)?)),
        "globalmemorystatusex" => Ok(Some(handle_global_memory_status_ex(ctx)?)),
        "getprocesstimes" => Ok(Some(handle_get_process_times(ctx)?)),
        "getlargepageminimum" => Ok(Some(handle_get_large_page_minimum(ctx)?)),
        "getprocessaffinitymask" => Ok(Some(handle_get_process_affinity_mask(ctx)?)),
        "setprocessaffinitymask" => Ok(Some(handle_set_process_affinity_mask(ctx)?)),
        "setthreadaffinitymask" => Ok(Some(handle_set_thread_affinity_mask(ctx)?)),
        "comparefiletime" => Ok(Some(handle_compare_file_time(ctx)?)),
        "localfiletimetofiletime" => Ok(Some(handle_local_file_time_to_file_time(ctx)?)),
        "filetimetodosdatetime" => Ok(Some(handle_file_time_to_dos_date_time(ctx)?)),
        "dosdatetimetofiletime" => Ok(Some(handle_dos_date_time_to_file_time(ctx)?)),
        "getdiskfreespaceexw" => Ok(Some(handle_get_disk_free_space_ex_w(ctx)?)),
        "getdiskfreespacew" => Ok(Some(handle_get_disk_free_space_w(ctx)?)),
        "getlogicaldrivestringsw" => Ok(Some(handle_get_logical_drive_strings_w(ctx)?)),
        "setfileattributesw" => Ok(Some(handle_set_file_attributes_w(ctx)?)),
        "setfiletime" => Ok(Some(handle_set_file_time(ctx)?)),
        "formatmessagew" => Ok(Some(handle_format_message_w(ctx)?)),
        "resumethread" => Ok(Some(handle_resume_thread(ctx)?)),
        "createsemaphorew" | "createsemaphorea" => Ok(Some(handle_create_semaphore(ctx)?)),
        "releasesemaphore" => Ok(Some(handle_release_semaphore(ctx)?)),
        "openeventw" | "openeventa" => Ok(Some(handle_open_event(ctx)?)),
        "waitformultipleobjects" => Ok(Some(handle_wait_for_multiple_objects(ctx)?)),
        "movefilewithprogressw" => Ok(Some(handle_move_file_with_progress_w(ctx)?)),
        "createhardlinkw" => Ok(Some(handle_create_hard_link_w(ctx)?)),
        "duplicatehandle" => Ok(Some(handle_duplicate_handle(ctx)?)),
        "getthreadpriority" => Ok(Some(handle_get_thread_priority(ctx)?)),
        "raiseexception" => Ok(Some(handle_raise_exception(ctx)?)),
        "rtlcapturecontext" => Ok(Some(handle_rtl_capture_context(ctx)?)),
        "rtlunwindex" => Ok(Some(handle_rtl_unwind_ex(ctx)?)),
        "findfirststreamw" => Ok(Some(handle_find_first_stream_w(ctx)?)),
        "findnextstreamw" => Ok(Some(handle_find_next_stream_w(ctx)?)),
        "deviceiocontrol" => Ok(Some(handle_device_io_control(ctx)?)),
        "mapviewoffile" => Ok(Some(handle_map_view_of_file(ctx)?)),
        "unmapviewoffile" => Ok(Some(handle_unmap_view_of_file(ctx)?)),
        "createfilemappingw" => Ok(Some(handle_create_file_mapping_w(ctx)?)),
        "openfilemappingw" | "openfilemappinga" => Ok(Some(handle_open_file_mapping(ctx)?)),
        // Mock-data stubs
        "isdebuggerpresent" => Ok(Some(handle_is_debugger_present(ctx)?)),
        "debugbreak" => Ok(Some(handle_debug_break(ctx)?)),
        "outputdebugstringa" => Ok(Some(handle_output_debug_string_a(ctx)?)),
        "outputdebugstringw" => Ok(Some(handle_output_debug_string_w(ctx)?)),
        "seterrormode" => Ok(Some(handle_set_error_mode(ctx)?)),
        "setthreaderrormode" => Ok(Some(handle_set_thread_error_mode(ctx)?)),
        "getcompressedfilesizea" => Ok(Some(handle_get_compressed_file_size_a(ctx)?)),
        "getcompressedfilesizew" => Ok(Some(handle_get_compressed_file_size_w(ctx)?)),
        "getvolumeinformationw" => Ok(Some(handle_get_volume_information_w(ctx)?)),
        "getvolumeinformationa" => Ok(Some(handle_get_volume_information_a(ctx)?)),
        "lockfile" => Ok(Some(handle_lock_file(ctx)?)),
        "unlockfile" => Ok(Some(handle_unlock_file(ctx)?)),
        "setfilevaliddata" => Ok(Some(handle_set_file_valid_data(ctx)?)),
        "getlongpathnamew" => Ok(Some(handle_get_long_path_name_w(ctx)?)),
        "getlongpathnamea" => Ok(Some(handle_get_long_path_name_a(ctx)?)),
        "getshortpathnamew" => Ok(Some(handle_get_short_path_name_w(ctx)?)),
        "getshortpathnamea" => Ok(Some(handle_get_short_path_name_a(ctx)?)),
        "getcomputernamew" => Ok(Some(handle_get_computer_name_w(ctx)?)),
        "getcomputernamea" => Ok(Some(handle_get_computer_name_a(ctx)?)),
        "getcomputernameexw" => Ok(Some(handle_get_computer_name_ex_w(ctx)?)),
        "getusernamew" => Ok(Some(handle_get_user_name_w(ctx)?)),
        "getusernamea" => Ok(Some(handle_get_user_name_a(ctx)?)),
        "getuserprofiledirectoryw" => Ok(Some(handle_get_user_profile_directory_w(ctx)?)),
        "getuserprofiledirectorya" => Ok(Some(handle_get_user_profile_directory_a(ctx)?)),
        "openthread" => Ok(Some(handle_open_thread(ctx)?)),
        "queryfullprocessimagenamew" => Ok(Some(handle_query_full_process_image_name_w(ctx)?)),
        "queryfullprocessimagenamea" => Ok(Some(handle_query_full_process_image_name_a(ctx)?)),
        "createjobobjectw" => Ok(Some(handle_create_job_object_w(ctx)?)),
        "createjobobjecta" => Ok(Some(handle_create_job_object_a(ctx)?)),
        "assignprocesstojobobject" => Ok(Some(handle_assign_process_to_job_object(ctx)?)),
        "terminateprocess" => Ok(Some(handle_terminate_process(ctx)?)),
        "terminatethread" => Ok(Some(handle_terminate_thread(ctx)?)),
        "suspendthread" => Ok(Some(handle_suspend_thread(ctx)?)),
        "getfileattributesexw" => Ok(Some(handle_get_file_attributes_ex_w(ctx)?)),
        "getfileattributesexa" => Ok(Some(handle_get_file_attributes_ex_a(ctx)?)),
        "signalobjectandwait" => Ok(Some(handle_signal_object_and_wait(ctx)?)),
        "backupread" => Ok(Some(handle_backup_read(ctx)?)),
        "backupseek" => Ok(Some(handle_backup_seek(ctx)?)),
        "backupwrite" => Ok(Some(handle_backup_write(ctx)?)),
        // Directory change notifications (FindFirst/Next/CloseChangeNotification,
        // ReadDirectoryChangesW — see kernel32/file_io/watch.rs).
        "findfirstchangenotificationw" => Ok(Some(handle_find_first_change_notification_w(ctx)?)),
        "findfirstchangenotificationa" => Ok(Some(handle_find_first_change_notification_a(ctx)?)),
        "findnextchangenotification" => Ok(Some(handle_find_next_change_notification(ctx)?)),
        "findclosechangenotification" => Ok(Some(handle_find_close_change_notification(ctx)?)),
        "readdirectorychangesw" => Ok(Some(handle_read_directory_changes_w(ctx)?)),
        // CreateProcess + process-wait surface (child guest sessions).
        "createprocessw" => Ok(Some(handle_create_process_w(ctx)?)),
        "createprocessa" => Ok(Some(handle_create_process_a(ctx)?)),
        "getexitcodeprocess" => Ok(Some(handle_get_exit_code_process(ctx)?)),
        "openprocess" => Ok(Some(handle_open_process(ctx)?)),
        // Phase-2 stub wave: boot-surface names for DOOM Retro / SDL2.
        "initializecriticalsectionex" => Ok(Some(handle_initialize_critical_section_ex(ctx)?)),
        "tryentercriticalsection" => Ok(Some(handle_try_enter_critical_section(ctx)?)),
        "createmutexa" => Ok(Some(handle_create_mutex_a(ctx)?)),
        "releasemutex" => Ok(Some(handle_release_mutex(ctx)?)),
        "initializeslisthead" => Ok(Some(handle_initialize_slist_head(ctx)?)),
        "interlockedflushslist" => Ok(Some(handle_interlocked_flush_slist(ctx)?)),
        "waitforsingleobjectex" => Ok(Some(handle_wait_for_single_object_ex(ctx)?)),
        "getmodulehandleexw" => Ok(Some(handle_get_module_handle_ex_w(ctx)?)),
        "getprocessid" => Ok(Some(handle_get_process_id(ctx)?)),
        "rtlpctofileheader" => Ok(Some(handle_rtl_pc_to_file_header(ctx)?)),
        "rtllookupfunctionentry" => Ok(Some(handle_rtl_lookup_function_entry(ctx)?)),
        "rtlunwind" => Ok(Some(handle_rtl_unwind(ctx)?)),
        "rtlvirtualunwind" => Ok(Some(handle_rtl_virtual_unwind(ctx)?)),
        "comparestringa" => Ok(Some(handle_compare_string_a(ctx)?)),
        "comparestringw" => Ok(Some(handle_compare_string_w(ctx)?)),
        "findfirstfileexw" => Ok(Some(handle_find_first_file_ex_w(ctx)?)),
        "movefileexw" => Ok(Some(handle_move_file_ex_w(ctx)?)),
        "peeknamedpipe" => Ok(Some(handle_peek_named_pipe(ctx)?)),
        "setnamedpipehandlestate" => Ok(Some(handle_set_named_pipe_handle_state(ctx)?)),
        "getoverlappedresult" => Ok(Some(handle_get_overlapped_result(ctx)?)),
        "cancelio" => Ok(Some(handle_cancel_io(ctx)?)),
        "getsystempowerstatus" => Ok(Some(handle_get_system_power_status(ctx)?)),
        "setthreadexecutionstate" => Ok(Some(handle_set_thread_execution_state(ctx)?)),
        "setthreadpriority" => Ok(Some(handle_set_thread_priority(ctx)?)),
        "setstdhandle" => Ok(Some(handle_set_std_handle(ctx)?)),
        "unhandledexceptionfilter" => Ok(Some(handle_unhandled_exception_filter(ctx)?)),
        "systemtimetotzspecificlocaltime" => {
            Ok(Some(handle_system_time_to_tz_specific_local_time(ctx)?))
        }
        "getlocaleinfoa" => Ok(Some(handle_get_locale_info_a(ctx)?)),
        "versetconditionmask" => Ok(Some(handle_ver_set_condition_mask(ctx)?)),
        "verifyversioninfow" => Ok(Some(handle_verify_version_info_w(ctx)?)),
        "enumresourcenamesw" => Ok(Some(handle_enum_resource_names_w(ctx)?)),
        // Console surface: the hot calls have their own arms above; the rest
        // live in `console` so this match does not grow a fourth screenful.
        _ => console::dispatch_console_extra(ctx, n.as_str()),
    }
}

const DEFAULT_WORKER_STACK: usize = 0x10_0000;
// Guest VA region for worker stacks (distinct from primary stack at 0x2000_0000).
const WORKER_STACK_REGION_BASE: u64 = 0x0000_0000_2200_0000;
const WORKER_STACK_STRIDE: u64 = 0x0000_0000_0020_0000;
// Windows x64 CALL/thread entry frame: 0x20 home space + 0x8 return address.
const THREAD_ENTRY_HOME_AND_RET: usize = 0x28;

const CREATE_SUSPENDED: u32 = 0x4;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const PAGE_READWRITE: u32 = 0x04;

// Default max guest worker threads (`CreateThread`), overridable by env.
const DEFAULT_MT_MAX_THREADS: u32 = 64;

/// Win32 `ERROR_NOT_ENOUGH_MEMORY` — the allocation failed.
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
/// `ERROR_NOT_SUPPORTED` — used when `WIE_MT=0` refuses `CreateThread`.
const ERROR_NOT_SUPPORTED_MT: u32 = 50;
/// `ERROR_TOO_MANY_POSTS` — semaphore release would exceed maximum.
const ERROR_TOO_MANY_POSTS: u32 = 298;
