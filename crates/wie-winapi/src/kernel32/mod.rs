#![allow(clippy::empty_line_after_doc_comments)]

pub(crate) use crate::guest_memory::{
    checked_address, checked_field_address, read_u16 as read_guest_u16, read_u64 as read_guest_u64,
    write_u16 as write_guest_u16, write_u32 as write_guest_u32, write_u64 as write_guest_u64,
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
const FAKE_CURRENT_PROCESS_ID: u64 = 0x1234;
const FIXED_TICK_COUNT: u64 = 12_345;
const FIXED_PERFORMANCE_COUNTER: u64 = 1_000_000;
const FLS_OUT_OF_INDEXES: u64 = 0xffff_ffff;
const STD_INPUT_HANDLE_ID: u32 = 0xffff_fff6;
const STD_OUTPUT_HANDLE_ID: u32 = 0xffff_fff5;
const STD_ERROR_HANDLE_ID: u32 = 0xffff_fff4;

/// Fake console handles returned by `GetStdHandle` (Microsoft Learn std ids).
const FAKE_STDIN_HANDLE: u64 = 0x0000_0000_6000_0001;
const FAKE_STDOUT_HANDLE: u64 = 0x0000_0000_6000_0002;
const FAKE_STDERR_HANDLE: u64 = 0x0000_0000_6000_0003;

/// Host console write for `WriteFile` on stdout/stderr (Microsoft Learn: valid on console handles).
#[cfg(unix)]
#[cfg(not(unix))]
pub(crate) fn write_host_console_handle(handle: u64, bytes: &[u8]) {
    use std::io::Write;
    if handle == FAKE_STDOUT_HANDLE {
        drop(std::io::stdout().write_all(bytes));
    } else if handle == FAKE_STDERR_HANDLE {
        drop(std::io::stderr().write_all(bytes));
    }
}

/// Cap for a single host console line fill (safety against huge pastes).
const MAX_HOST_STDIN_LINE: usize = 64 * 1024;

/// Read one line from host stdin (through `\n` or EOF), capped at
/// [`MAX_HOST_STDIN_LINE`].
///
/// Models Microsoft Learn default console line input (`ENABLE_LINE_INPUT`):
/// `ReadFile` on a console handle does not complete until a carriage return
/// is entered. On Unix hosts we treat `\n` as the line terminator.
//
// Returns:
// - `Ok(Some(bytes))` — non-empty fill (may omit `\n` if cap hit first)
// - `Ok(None)` — host EOF with no bytes
// - `Err(_)` — host I/O error

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

const FAKE_KERNEL32_MODULE: u64 = 0x0000_0000_6100_0000;
const FAKE_USER32_MODULE: u64 = 0x0000_0000_6100_1000;
const FAKE_GDI32_MODULE: u64 = 0x0000_0000_6100_2000;
const FAKE_COMCTL32_MODULE: u64 = 0x0000_0000_6100_3000;
const FAKE_ADVAPI32_MODULE: u64 = 0x0000_0000_6100_4000;
const FAKE_SHELL32_MODULE: u64 = 0x0000_0000_6100_5000;
const FAKE_COMDLG32_MODULE: u64 = 0x0000_0000_6100_6000;
const FAKE_WINMM_MODULE: u64 = 0x0000_0000_6100_7000;

const INVALID_FILE_ATTRIBUTES: u64 = 0xffff_ffff;
const FILE_ATTRIBUTE_DIRECTORY: u64 = 0x0000_0010;
const FILE_ATTRIBUTE_ARCHIVE: u64 = 0x0000_0020;

const INVALID_HANDLE_VALUE: u64 = u64::MAX;
const ERROR_NO_MORE_FILES: u32 = 18;

const FAKE_RESOURCE_DATA_BASE: u64 = 0x0000_0000_6400_0000;
const FAKE_RESOURCE_SIZE: u32 = 16;
const FAKE_RESOURCE_BYTES: [u8; 16] = [
    0x4c, 0x4d, 0x52, 0x53, // "WIERS"
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

const LANG_EN_US: u64 = 0x0409;

const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
/// Win32 `ERROR_READ_FAULT` — host console stdin I/O failure.
const ERROR_READ_FAULT: u32 = 30;

const TIME_ZONE_ID_UNKNOWN: u64 = 0;
const TIME_ZONE_ID_INVALID: u64 = 0xffff_ffff;

const FILE_BEGIN: u64 = 0;
const FILE_CURRENT: u64 = 1;
const FILE_END: u64 = 2;
const INVALID_SET_FILE_POINTER: u64 = 0xffff_ffff;

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
/// CreateFile CREATE_NEW when the file already exists (Microsoft Learn).
const ERROR_FILE_EXISTS: u32 = 80;
const ERROR_MOD_NOT_FOUND: u32 = 126;
const ERROR_PROC_NOT_FOUND: u32 = 127;
const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_DIR_NOT_EMPTY: u32 = 145;

// CreateFile disposition values.
const CREATE_NEW: u64 = 1;
const CREATE_ALWAYS: u64 = 2;
const OPEN_EXISTING: u64 = 3;
const OPEN_ALWAYS: u64 = 4;
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

pub(crate) fn low_u32_to_i32(value: u64, context_name: &str) -> Result<i32> {
    let low = u32::try_from(value & 0xffff_ffff)
        .with_context(|| format!("{context_name} low u32 conversion failed"))?;

    Ok(i32::from_ne_bytes(low.to_ne_bytes()))
}

// Copy a NUL-terminated ANSI path into a guest buffer.
///
/// Returns `(chars_written_or_nSize, truncated)` per Microsoft Learn
/// `GetModuleFileNameA` semantics.
pub(crate) fn copy_path_a_to_guest_buffer(
    engine: &mut dyn wie_cpu::CpuEngine,
    source_ptr: u64,
    dest_ptr: u64,
    dest_len: u64,
) -> Result<(u64, bool)> {
    if dest_ptr == 0 || dest_len == 0 {
        return Ok((0, false));
    }

    let dest_len_usize =
        usize::try_from(dest_len).context("guest buffer length does not fit usize")?;

    // Read full source path (bounded) including room to detect truncation.
    let mut source_bytes = Vec::new();
    let max_scan = dest_len_usize.saturating_add(1).max(1);
    for index in 0..max_scan {
        let index_u64 = u64::try_from(index).context("guest string index does not fit u64")?;
        let source_address = checked_address(source_ptr, index_u64, "guest source string")?;
        let mut byte = [0_u8; 1];
        engine
            .mem_read(source_address, &mut byte)
            .context("failed to read guest source string byte")?;
        if byte[0] == 0 {
            break;
        }
        source_bytes.push(byte[0]);
    }

    let path_len = source_bytes.len();
    // Need room for path + NUL. If dest_len is too small, truncate and NUL-terminate.
    let truncated = path_len >= dest_len_usize;
    if truncated {
        let keep = dest_len_usize.saturating_sub(1);
        let mut out = source_bytes.get(..keep).unwrap_or(&[]).to_vec();
        out.push(0);
        engine
            .mem_write(dest_ptr, &out)
            .context("failed to write truncated guest path")?;
        Ok((dest_len, true))
    } else {
        let mut out = source_bytes;
        out.push(0);
        engine
            .mem_write(dest_ptr, &out)
            .context("failed to write guest path")?;
        let written = u64::try_from(path_len).context("path length does not fit u64")?;
        Ok((written, false))
    }
}

// Copy a NUL-terminated UTF-16 path into a guest buffer (WCHAR units).
pub(crate) fn copy_path_w_to_guest_buffer(
    engine: &mut dyn wie_cpu::CpuEngine,
    source_ptr: u64,
    dest_ptr: u64,
    dest_len: u64,
) -> Result<(u64, bool)> {
    if dest_ptr == 0 || dest_len == 0 {
        return Ok((0, false));
    }

    let dest_len_usize =
        usize::try_from(dest_len).context("wide guest buffer length does not fit usize")?;

    let mut units = Vec::new();
    let max_scan = dest_len_usize.saturating_add(1).max(1);
    for index in 0..max_scan {
        let index_u64 = u64::try_from(index).context("wide guest string index does not fit u64")?;
        let source_offset = index_u64
            .checked_mul(2)
            .context("wide guest string source offset overflow")?;
        let source_address =
            checked_address(source_ptr, source_offset, "wide guest source string")?;
        let unit = read_guest_u16(engine, source_address)?;
        if unit == 0 {
            break;
        }
        units.push(unit);
    }

    let path_len = units.len();
    let truncated = path_len >= dest_len_usize;
    if truncated {
        let keep = dest_len_usize.saturating_sub(1);
        let mut out_units = units.get(..keep).unwrap_or(&[]).to_vec();
        out_units.push(0);
        let byte_cap = out_units.len().saturating_mul(2);
        let mut bytes = Vec::with_capacity(byte_cap);
        for unit in out_units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        engine
            .mem_write(dest_ptr, &bytes)
            .context("failed to write truncated wide guest path")?;
        Ok((dest_len, true))
    } else {
        let mut out_units = units;
        out_units.push(0);
        let byte_cap = out_units.len().saturating_mul(2);
        let mut bytes = Vec::with_capacity(byte_cap);
        for unit in out_units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        engine
            .mem_write(dest_ptr, &bytes)
            .context("failed to write wide guest path")?;
        let written = u64::try_from(path_len).context("wide path length does not fit u64")?;
        Ok((written, false))
    }
}

// Whether `path` refers to the loaded main PE (any basename/path form).

// Resolve a module that is considered already loaded (`GetModuleHandle*`).
///
/// Microsoft Learn: returns `NULL` when the named module is not in the process.

// Resolve a DLL by name: first check loaded and fake modules, then try to load from disk.

/// Build a guest Windows-style path for a DLL name relative to the main module directory.

/// Write an unlocked `RTL_CRITICAL_SECTION` (Win64 layout) at `critical_section_ptr`.

pub(crate) fn read_ansi_string_from_cpu(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_len: usize,
) -> Result<String> {
    // Byte-at-a-time: bulk reads of MAX_PATH-sized buffers fail when the string
    // sits near the end of a mapped PE page (common for freestanding micros).
    read_guest_ansi_lossy(engine, address, max_len)
}

pub(crate) fn read_wide_string_from_cpu(
    engine: &mut dyn wie_cpu::CpuEngine,
    address: u64,
    max_units: usize,
) -> Result<String> {
    if address == 0 {
        return Ok(String::new());
    }

    let mut units = Vec::new();

    for index in 0..max_units {
        let index_u64 = u64::try_from(index).context("wide string index does not fit u64")?;
        let offset = index_u64
            .checked_mul(2)
            .context("wide string offset overflow")?;
        let unit_address = checked_address(address, offset, "wide string read")?;
        let unit = read_guest_u16(engine, unit_address)?;

        if unit == 0 {
            break;
        }

        units.push(unit);
    }

    String::from_utf16(&units).context("wide string is not valid UTF-16")
}

pub(crate) fn file_attributes_for_path(state: &WinApiState, path: &str) -> u64 {
    let normalized = path.trim();
    if normalized.is_empty() {
        return INVALID_FILE_ATTRIBUTES;
    }

    let mounts_ref: Vec<(String, std::path::PathBuf)> = state
        .file_io
        .host_file_mounts
        .iter()
        .map(|m| (m.guest_path.clone(), m.host_path.clone()))
        .collect();
    let virtuals_ref: Vec<(String, usize)> = state
        .file_io
        .virtual_files
        .iter()
        .map(|v| (v.guest_path.clone(), v.bytes.len()))
        .collect();
    let ctx = crate::vfs::ResolveCtx {
        volumes: &state.file_io.volumes,
        main_module_path: &state.process.main_module_path,
        main_module_file_name: &state.process.main_module_file_name,
        host_file_mounts: &mounts_ref,
        virtual_files: &virtuals_ref,
        synthetic_dirs: crate::vfs::DEFAULT_SYNTHETIC_DIRS,
    };

    let st = crate::vfs::stat_path(&ctx, normalized);
    match st.kind {
        crate::vfs::PathKind::NotFound => INVALID_FILE_ATTRIBUTES,
        crate::vfs::PathKind::Directory => u64::from(st.attributes),
        crate::vfs::PathKind::File => {
            if ctx.path_is_main_module(normalized) {
                FILE_ATTRIBUTE_ARCHIVE
            } else {
                u64::from(st.attributes).max(FILE_ATTRIBUTE_ARCHIVE)
            }
        }
    }
}

/// Collect dir entries for a Find pattern (dir + mask).
pub(crate) fn collect_find_entries(
    state: &WinApiState,
    full_pattern: &str,
) -> Vec<crate::vfs::DirEntry> {
    let mounts_ref: Vec<(String, std::path::PathBuf)> = state
        .file_io
        .host_file_mounts
        .iter()
        .map(|m| (m.guest_path.clone(), m.host_path.clone()))
        .collect();
    let virtuals_ref: Vec<(String, usize)> = state
        .file_io
        .virtual_files
        .iter()
        .map(|v| (v.guest_path.clone(), v.bytes.len()))
        .collect();
    let ctx = crate::vfs::ResolveCtx {
        volumes: &state.file_io.volumes,
        main_module_path: &state.process.main_module_path,
        main_module_file_name: &state.process.main_module_file_name,
        host_file_mounts: &mounts_ref,
        virtual_files: &virtuals_ref,
        synthetic_dirs: crate::vfs::DEFAULT_SYNTHETIC_DIRS,
    };
    let (dir, mask) = crate::vfs::split_find_pattern(full_pattern);
    crate::vfs::list_dir_filtered(&ctx, &dir, &mask)
}

/// Write shared `WIN32_FIND_DATA{A,W}` header fields (not the name).
///
/// Layout (minwinbase.h) — **not** `BY_HANDLE_FILE_INFORMATION`:
/// ```text
/// 0  dwFileAttributes
/// 4  ftCreationTime / 12 ftLastAccessTime / 20 ftLastWriteTime
/// 28 nFileSizeHigh / 32 nFileSizeLow
/// 36 dwReserved0 / 40 dwReserved1
/// 44 cFileName[MAX_PATH]
/// ```
pub(crate) fn write_find_data_common(
    engine: &mut dyn wie_cpu::CpuEngine,
    find_data_ptr: u64,
    attributes: u32,
    file_size: u64,
) -> Result<()> {
    if find_data_ptr == 0 {
        return Ok(());
    }

    // Build the 44-byte WIN32_FIND_DATA common header on the host stack and
    // push it in a single mem_write. Previously nine scalar write_guest_u32/u64
    // calls, each taking a fresh guest memory RwLock and page-walk.
    //
    // Layout (from wine/mingw headers, matches Microsoft SDK):
    //   +0  dwFileAttributes  u32
    //   +4  ftCreationTime    u64
    //   +12 ftLastAccessTime  u64
    //   +20 ftLastWriteTime   u64
    //   +28 nFileSizeHigh     u32
    //   +32 nFileSizeLow      u32
    //   +36 dwReserved0       u32
    //   +40 dwReserved1       u32
    let mut header = [0_u8; 44];
    header[0..4].copy_from_slice(&attributes.to_le_bytes());
    header[4..12].copy_from_slice(&FIXED_SYSTEM_FILETIME.to_le_bytes());
    header[12..20].copy_from_slice(&FIXED_SYSTEM_FILETIME.to_le_bytes());
    header[20..28].copy_from_slice(&FIXED_SYSTEM_FILETIME.to_le_bytes());
    let hi = u32::try_from(file_size >> 32).unwrap_or(0);
    let lo = u32::try_from(file_size & 0xffff_ffff).unwrap_or(0);
    header[28..32].copy_from_slice(&hi.to_le_bytes());
    header[32..36].copy_from_slice(&lo.to_le_bytes());
    // header[36..44] already zero (dwReserved0 / dwReserved1)
    engine
        .mem_write(find_data_ptr, &header)
        .context("failed to write WIN32_FIND_DATA header")
}

pub(crate) fn write_find_data_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    find_data_ptr: u64,
    file_name: &str,
    attributes: u32,
    file_size: u64,
) -> Result<()> {
    write_find_data_common(engine, find_data_ptr, attributes, file_size)?;

    if find_data_ptr == 0 {
        return Ok(());
    }

    // cFileName is at offset 44 (after dwReserved1), not 48.
    let file_name_address = checked_field_address(find_data_ptr, 44, "WIN32_FIND_DATAW.cFileName")?;
    // cAlternateFileName[14] starts at 44 + MAX_PATH*2 = 564.
    let alt_name_address =
        checked_field_address(find_data_ptr, 564, "WIN32_FIND_DATAW.cAlternateFileName")?;

    let mut bytes = Vec::new();
    for unit in file_name.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());

    engine
        .mem_write(file_name_address, &bytes)
        .context("failed to write WIN32_FIND_DATAW.cFileName")?;
    write_guest_u16(engine, alt_name_address, 0)?;

    Ok(())
}

pub(crate) fn write_find_data_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    find_data_ptr: u64,
    file_name: &str,
    attributes: u32,
    file_size: u64,
) -> Result<()> {
    write_find_data_common(engine, find_data_ptr, attributes, file_size)?;

    if find_data_ptr == 0 {
        return Ok(());
    }

    // Same header as W; cFileName is CHAR[MAX_PATH] at offset 44.
    let file_name_address = checked_field_address(find_data_ptr, 44, "WIN32_FIND_DATAA.cFileName")?;
    let alt_name_address =
        checked_field_address(find_data_ptr, 304, "WIN32_FIND_DATAA.cAlternateFileName")?;

    let mut bytes = crate::vfs::encode_acp(file_name);
    bytes.push(0);

    engine
        .mem_write(file_name_address, &bytes)
        .context("failed to write WIN32_FIND_DATAA.cFileName")?;
    engine
        .mem_write(alt_name_address, &[0_u8])
        .context("failed to write WIN32_FIND_DATAA.cAlternateFileName")?;

    Ok(())
}

pub(crate) fn create_fake_resource_record(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
) -> Result<ResourceRecord> {
    let handle = state.file_io.next_resource_handle;
    state.file_io.next_resource_handle = state
        .file_io
        .next_resource_handle
        .checked_add(1)
        .context("resource handle overflow")?;

    let index =
        u64::try_from(state.file_io.resources.len()).context("resource index does not fit u64")?;
    let data_offset = index
        .checked_mul(0x100)
        .context("resource data offset overflow")?;

    let data_ptr = FAKE_RESOURCE_DATA_BASE
        .checked_add(data_offset)
        .context("resource data pointer overflow")?;

    engine
        .mem_write(data_ptr, &FAKE_RESOURCE_BYTES)
        .context("failed to write fake resource bytes")?;

    // For this compatibility harness, make the loaded resource handle pointer-like.
    // Some old Win32-style code uses the result of LoadResource directly as data.
    let loaded_handle = data_ptr;

    let record = ResourceRecord {
        handle,
        loaded_handle,
        data_ptr,
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

///
/// Returns the active guest TID from [`crate::ThreadState`] (primary `0x5678`
/// until MT.2 spawns workers).

///
/// Microsoft Learn (`heapapi.h`):
/// - success → pointer to allocated block (at least `dwBytes`)
/// - failure → `NULL` (does not call `SetLastError`)
// - `HEAP_ZERO_MEMORY` zeros the block
// - `dwBytes == 0` allocates a zero-length item and still returns a valid pointer
///   (same practical behaviour as the Windows process heap / CRT `malloc(0)`)

///
/// Microsoft Learn: `lpMem` may be `NULL` (no-op, success). Double-free /
/// unknown pointer fails with a non-zero last-error in this emulator
/// (`ERROR_INVALID_HANDLE`) so freestanding tests can detect the failure.

///
/// Microsoft Learn: preserves contents; failure leaves the original block valid
/// and returns `NULL`. `dwBytes == 0` is treated as free + `NULL` (common Windows
/// process-heap behaviour used by the micro-suite).

///
// Guest `RTL_CRITICAL_SECTION` layout (Win64) written by Initialize*:
/// `LockCount` (-1 unlocked), `RecursionCount`, `OwningThread` (guest TID).
///
/// Contended path: returns [`crate::WinApiControlSignal::HostPark`] so the
/// session drops the shared CPU lock and waits on the CS condvar (MT.3).

///
/// Zeros the CS fields. Calling Delete while owned is undefined on Windows;
/// we still clear so a subsequent Initialize can reuse the memory.

/// Result of a non-blocking CS enter attempt.
pub(crate) enum EnterCsResult {
    Acquired,
    NeedPark,
}

/// Try enter (or re-enter) a guest critical section for `owner_tid`.

/// Leave a guest critical section owned by `owner_tid`.
///
/// Returns `true` if the CS became fully unlocked (wake one waiter).

/// Publish one FLS slot into the guest table used by in-guest `FlsGetValue`.

///
/// Lean host path (also fallback for guest SBCS helper). Single-byte code pages
/// use zero-extend (matches guest accelerator); others use UTF-8 lossy.

/// Zero-extend each byte to UTF-16 (SBCS / Latin-1 identity).

///
/// Microsoft Learn: returns character count excluding NUL. If the buffer is too
/// small, the path is truncated (NUL-terminated), the return value is `nSize`,
/// and last-error is `ERROR_INSUFFICIENT_BUFFER`.

///
/// Microsoft Learn: returns the export address, or `NULL` if not found
/// (`GetLastError` → `ERROR_PROC_NOT_FOUND`). Does **not** abort the process.

pub(crate) fn finish_find_first(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    pattern: &str,
    find_data_ptr: u64,
    unicode: bool,
) -> Result<u64> {
    if pattern.trim().is_empty() {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        return Ok(INVALID_HANDLE_VALUE);
    }

    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_pattern = resolve_full_windows_path(&cwd, pattern);
    let entries_vec = collect_find_entries(state, &full_pattern);
    if entries_vec.is_empty() {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        return Ok(INVALID_HANDLE_VALUE);
    }

    let mut entries: std::collections::VecDeque<_> = entries_vec.into();
    let first = entries
        .pop_front()
        .context("find entries went empty after non-empty check")?;
    if unicode {
        write_find_data_w(
            engine,
            find_data_ptr,
            &first.name,
            first.attributes,
            first.size,
        )?;
    } else {
        write_find_data_a(
            engine,
            find_data_ptr,
            &first.name,
            first.attributes,
            first.size,
        )?;
    }

    let handle = state.file_io.next_find_handle;
    state.file_io.next_find_handle = state
        .file_io
        .next_find_handle
        .checked_add(1)
        .context("find handle overflow")?;

    state.file_io.find_handles.push(FindHandle {
        handle,
        pattern: full_pattern,
        remaining: entries,
    });
    state.process.last_error = 0;
    Ok(handle)
}

pub(crate) fn finish_find_next(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    find_handle: u64,
    find_data_ptr: u64,
    unicode: bool,
) -> Result<u64> {
    let Some(slot) = state
        .file_io
        .find_handles
        .iter_mut()
        .find(|h| h.handle == find_handle)
    else {
        state.process.last_error = ERROR_INVALID_HANDLE;
        return Ok(0);
    };

    let Some(next) = slot.remaining.pop_front() else {
        state.process.last_error = ERROR_NO_MORE_FILES;
        return Ok(0);
    };
    if unicode {
        write_find_data_w(
            engine,
            find_data_ptr,
            &next.name,
            next.attributes,
            next.size,
        )?;
    } else {
        write_find_data_a(
            engine,
            find_data_ptr,
            &next.name,
            next.attributes,
            next.size,
        )?;
    }
    state.process.last_error = 0;
    Ok(1)
}

pub(crate) fn finish_create_file(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    file_name: &str,
    desired_access: u64,
    creation_disposition: u64,
    api_name: &str,
) -> u64 {
    let return_value =
        match open_or_create_guest_path(state, file_name, desired_access, creation_disposition) {
            Ok(OpenFileOutcome::Handle(handle)) => {
                state.process.last_error = 0;
                handle
            }
            Ok(OpenFileOutcome::HandleCreated(handle)) => {
                // OPEN_ALWAYS / CREATE_ALWAYS created a new file (docs: GetLastError may be 0).
                state.process.last_error = 0;
                handle
            }
            Ok(OpenFileOutcome::HandleExists(handle)) => {
                // Microsoft Learn: CREATE_ALWAYS / OPEN_ALWAYS set ERROR_ALREADY_EXISTS
                // when the named file already existed.
                if creation_disposition == CREATE_ALWAYS || creation_disposition == OPEN_ALWAYS {
                    state.process.last_error = ERROR_ALREADY_EXISTS;
                } else {
                    state.process.last_error = 0;
                }
                handle
            }
            Err(win_error) => {
                tracing::debug!(
                    path = %file_name,
                    desired_access,
                    creation_disposition,
                    win_error,
                    "{api_name} open failed"
                );
                state.process.last_error = win_error;
                INVALID_HANDLE_VALUE
            }
        };

    if return_value != INVALID_HANDLE_VALUE {
        tracing::debug!(
            path = %file_name,
            desired_access,
            creation_disposition,
            handle = return_value,
            "{api_name}"
        );
        let _ = crate::guest_io_host::register_open_file(engine, state, return_value).ok();
    }

    return_value
}

///
/// Microsoft Learn: success → nonzero; failure → zero + last-error.
/// `NULL` / `INVALID_HANDLE_VALUE` fail with `ERROR_INVALID_HANDLE`.
/// Open guest files are flushed to the virtual store / bottle host path.

/// Full path equality (case-insensitive). Used for FS identity.

/// Opens a guest path using the same resolution rules as `CreateFile*`.
///
/// Returns the new handle on success.

/// Result of a successful `CreateFile` open (handle + existence semantics for last-error).
pub(crate) enum OpenFileOutcome {
    /// Opened or created; last-error should be 0.
    Handle(u64),
    /// Opened a file that already existed (OPEN_ALWAYS / CREATE_ALWAYS overwrite).
    HandleExists(u64),
    /// Created a new file (OPEN_ALWAYS / CREATE_NEW / CREATE_ALWAYS on new path).
    HandleCreated(u64),
}

/// Open/create using Microsoft Learn disposition rules (clean room).
///
/// Relative paths (`.\\file`, `subdir\\file`, `\\rooted`) are resolved against
/// the process current directory before open (same idea as `GetFullPathName`).
///
/// Returns `Err(Win32 error code)` on failure (not an anyhow chain).

/// Flush open-file buffer to bottle/mount host path when present (buffered only).

/// If a buffered host-backed file has grown past [`crate::vfs::BUFFER_SIZE_THRESHOLD`],
/// spill once to disk and switch to streaming so further writes do not retain a full
/// in-memory copy (and do not clone it into `virtual_files` on every close).

/// Mounts a host file into the guest path namespace.

// Whether a **file** exists at the guest path (for CreateFile open dispositions).

///
/// Microsoft Learn: returns nonzero on success; stores the spin count in the CS.

///
/// Microsoft Learn: valid on disk and console handles. Console stdin is served
/// from `WinApiState::stdin_bytes` (host inject and/or live host line-fill when
/// `stdin_mode` is `LiveHost`). Default console line input: a live fill blocks
/// until `\n` or EOF. Success with 0 bytes means EOF.

///
/// Microsoft Learn: valid on disk and console handles. Stdout/stderr write to the
/// host console; stdin is not writable.

/// Copies the open handle's buffer into `virtual_files` for the same path.
///
/// **Host-backed / streaming files are never mirrored.** Bottle volume paths
/// (WIE_ROOT / drive-D) used to land here on every CloseHandle because they are
/// not `host_file_mounts` entries — so opening+closing every source file during
/// a 7za scan permanently retained full contents in `virtual_files` (session-long
/// RAM growth proportional to scanned data).
pub(crate) fn sync_open_bytes_to_virtual(state: &mut WinApiState, path: &str, handle: u64) {
    let Some(open_file) = find_open_file(state, handle) else {
        return;
    };
    // Host path or streaming ⇒ content lives on disk; never retain a second copy.
    if open_file.host_path.is_some() || open_file.streaming {
        return;
    }
    if is_main_module_path(state, path) {
        return;
    }
    // Volume-mapped paths without an open host_path still must not accumulate.
    if crate::vfs::guest_path_to_host(&state.file_io.volumes, path).is_some() {
        return;
    }
    if state
        .file_io
        .host_file_mounts
        .iter()
        .any(|mount| paths_match_guest(path, &mount.guest_path))
    {
        return;
    }

    let bytes = open_file.bytes.clone();

    if let Some(virtual_file) = state
        .file_io
        .virtual_files
        .iter_mut()
        .find(|entry| paths_match_guest(path, &entry.guest_path))
    {
        virtual_file.bytes = bytes;
        return;
    }

    // Pure in-session virtual files only (no bottle/mount/volume backing).
    state.file_io.virtual_files.push(crate::VirtualGuestFile {
        guest_path: path.to_owned(),
        bytes,
    });
}

///
/// Microsoft Learn return value:
/// - success: number of characters written **excluding** the terminating NUL
/// - buffer too small: required size **including** the terminating NUL
/// - size query: `lpBuffer == NULL` and `nBufferLength == 0` → required size with NUL
/// - failure: zero (not used for the insufficient-buffer case)

// ─── Soft console / process helpers for real CLI tools (7za) ────────────────

const FIXED_PERFORMANCE_FREQUENCY: u64 = 10_000_000;
const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
const ENABLE_LINE_INPUT: u32 = 0x0002;
const ENABLE_ECHO_INPUT: u32 = 0x0004;
const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
const DEFAULT_CONSOLE_MODE_IN: u32 = ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT;
const DEFAULT_CONSOLE_MODE_OUT: u32 = ENABLE_PROCESSED_OUTPUT | ENABLE_WRAP_AT_EOL_OUTPUT;

/// `BOOL SetConsoleCtrlHandler(PHANDLER_ROUTINE, BOOL)` — accept, ignore handler.

/// `BOOL GetConsoleMode(HANDLE, LPDWORD)`.

/// `BOOL SetConsoleMode(HANDLE, DWORD)`.

/// `BOOL GetConsoleScreenBufferInfo(HANDLE, PCONSOLE_SCREEN_BUFFER_INFO)`.

/// `VOID SetFileApisToOEM(void)`.

/// `BOOL QueryPerformanceFrequency(LARGE_INTEGER*)`.

/// `VOID GetSystemInfo(LPSYSTEM_INFO)`.

/// `BOOL IsProcessorFeaturePresent(DWORD)`.

/// `BOOL GlobalMemoryStatusEx(LPMEMORYSTATUSEX)`.

/// `BOOL GetProcessTimes(HANDLE, LPFILETIME×4)`.

/// `SIZE_T GetLargePageMinimum(void)` — 0 = large pages unavailable.

/// `BOOL GetProcessAffinityMask(HANDLE, PDWORD_PTR, PDWORD_PTR)`.

/// `BOOL SetProcessAffinityMask(HANDLE, DWORD_PTR)`.

/// `DWORD_PTR SetThreadAffinityMask(HANDLE, DWORD_PTR)` — return previous mask.

/// `LONG CompareFileTime(const FILETIME*, const FILETIME*)`.

/// `BOOL LocalFileTimeToFileTime(const FILETIME*, LPFILETIME)`.

/// `BOOL FileTimeToDosDateTime(const FILETIME*, LPWORD, LPWORD)`.

/// `BOOL DosDateTimeToFileTime(WORD, WORD, LPFILETIME)`.

/// Fake free/total disk sizes for `GetDiskFreeSpace*`.
const FAKE_DISK_GIB: u64 = 1024 * 1024 * 1024;
/// ~100 GiB of 4 KiB clusters (8 sectors × 512).
const FAKE_DISK_CLUSTERS: u32 = 26_214_400;
/// Drive string payload: `C:\` + NUL + final NUL (TCHARs).
const LOGICAL_DRIVE_TCHARS: u32 = 4;

/// `BOOL GetDiskFreeSpaceExW(LPCWSTR, PULARGE_INTEGER×3)`.

/// `BOOL GetDiskFreeSpaceW(LPCWSTR, LPDWORD×4)`.

/// `DWORD GetLogicalDriveStringsW(DWORD, LPWSTR)` — report `C:\`.

/// `BOOL SetFileAttributesW(LPCWSTR, DWORD)`.

/// `BOOL SetFileTime(HANDLE, const FILETIME*, const FILETIME*, const FILETIME*)`.

/// Minimal `FormatMessageW` — empty string / return 0 for now.

/// `DWORD ResumeThread(HANDLE)` — start a `CREATE_SUSPENDED` worker.

/// `HANDLE CreateSemaphoreA/W(...)` — counting semaphore waitable.

/// `BOOL ReleaseSemaphore(HANDLE, LONG, LPLONG)`.

/// `HANDLE OpenEventW(DWORD, BOOL, LPCWSTR)`.

/// `DWORD WaitForMultipleObjects(...)` — wait-all / any on kernel waitables.

/// `BOOL MoveFileWithProgressW` — alias MoveFileW semantics.

/// `BOOL CreateHardLinkW` — not supported; return FALSE.

/// `HANDLE FindFirstStreamW` — no alternate streams.

/// `BOOL FindNextStreamW`.

/// `BOOL DeviceIoControl` — unsupported; return FALSE.

/// `LPVOID MapViewOfFile` — not supported yet.

/// `BOOL UnmapViewOfFile`.

/// `HANDLE OpenFileMappingW` — not found.

/// Extra KERNEL32 exports used by CRT / modern PE (not yet in dense WinApiId table).
pub mod console;
pub mod file_io;
pub mod heap;
pub mod memory;
pub mod misc;
pub mod module;
pub mod process_thread;
pub mod string;
pub mod sync;

pub use console::*;
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
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let environment = ctx.environment;
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "virtualalloc" => Ok(Some(handle_virtual_alloc(engine, state)?)),
        "virtualfree" => Ok(Some(handle_virtual_free(engine, state)?)),
        "virtualprotect" => Ok(Some(handle_virtual_protect(engine, state)?)),
        "virtualquery" => Ok(Some(handle_virtual_query(engine, state)?)),
        "flushinstructioncache" => Ok(Some(handle_flush_instruction_cache(engine, state)?)),
        "tlsgetvalue" => Ok(Some(handle_tls_get_value(engine, state)?)),
        "tlssetvalue" => Ok(Some(handle_tls_set_value(engine, state)?)),
        "tlsalloc" => Ok(Some(handle_tls_alloc(engine, state)?)),
        "tlsfree" => Ok(Some(handle_tls_free(engine, state)?)),
        // MT.2 / MT.3
        "createthread" => Ok(Some(handle_create_thread(engine, state)?)),
        "exitthread" => Ok(Some(handle_exit_thread(engine, state)?)),
        "getexitcodethread" => Ok(Some(handle_get_exit_code_thread(engine, state)?)),
        "waitforsingleobject" => Ok(Some(handle_wait_for_single_object(engine, state)?)),
        "createeventa" | "createeventw" => Ok(Some(handle_create_event(engine, state)?)),
        "setevent" => Ok(Some(handle_set_event(engine, state)?)),
        "resetevent" => Ok(Some(handle_reset_event(engine, state)?)),
        "getcurrentthread" => Ok(Some(handle_get_current_thread(engine)?)),
        // MT.4 Interlocked* (host atomics on soft-translated guest memory)
        "interlockedincrement" => Ok(Some(handle_interlocked_increment(engine)?)),
        "interlockeddecrement" => Ok(Some(handle_interlocked_decrement(engine)?)),
        "interlockedexchange" => Ok(Some(handle_interlocked_exchange(engine)?)),
        "interlockedcompareexchange" => Ok(Some(handle_interlocked_compare_exchange(engine)?)),
        "interlockedexchangeadd" => Ok(Some(handle_interlocked_exchange_add(engine)?)),
        "interlockedincrement64" => Ok(Some(handle_interlocked_increment64(engine)?)),
        "interlockeddecrement64" => Ok(Some(handle_interlocked_decrement64(engine)?)),
        "interlockedexchange64" => Ok(Some(handle_interlocked_exchange64(engine)?)),
        "interlockedcompareexchange64" => Ok(Some(handle_interlocked_compare_exchange64(engine)?)),
        "interlockedexchangeadd64" => Ok(Some(handle_interlocked_exchange_add64(engine)?)),
        // Real-tool surface (7z / CRT-linked PE)
        "getversion" => Ok(Some(handle_get_version(engine)?)),
        "getmodulehandlew" => Ok(Some(handle_get_module_handle_w(
            engine,
            environment,
            state,
        )?)),
        "lstrlenw" => Ok(Some(handle_lstrlen_w(engine)?)),
        "lstrcpyw" => Ok(Some(handle_lstrcpy_w(engine)?)),
        "lstrcatw" => Ok(Some(handle_lstrcat_w(engine)?)),
        // Console / process identity (7za CLI startup)
        "setconsolectrlhandler" => Ok(Some(handle_set_console_ctrl_handler(engine)?)),
        "getconsolemode" => Ok(Some(handle_get_console_mode(engine)?)),
        "setconsolemode" => Ok(Some(handle_set_console_mode(engine)?)),
        "getconsolescreenbufferinfo" => Ok(Some(handle_get_console_screen_buffer_info(engine)?)),
        "setfileapistooem" => Ok(Some(handle_set_file_apis_to_oem(engine)?)),
        "queryperformancefrequency" => Ok(Some(handle_query_performance_frequency(engine)?)),
        "getsysteminfo" => Ok(Some(handle_get_system_info(engine)?)),
        "isprocessorfeaturepresent" => Ok(Some(handle_is_processor_feature_present(engine)?)),
        "globalmemorystatusex" => Ok(Some(handle_global_memory_status_ex(engine)?)),
        "getprocesstimes" => Ok(Some(handle_get_process_times(engine)?)),
        "getlargepageminimum" => Ok(Some(handle_get_large_page_minimum(engine)?)),
        "getprocessaffinitymask" => Ok(Some(handle_get_process_affinity_mask(engine)?)),
        "setprocessaffinitymask" => Ok(Some(handle_set_process_affinity_mask(engine)?)),
        "setthreadaffinitymask" => Ok(Some(handle_set_thread_affinity_mask(engine)?)),
        "comparefiletime" => Ok(Some(handle_compare_file_time(engine)?)),
        "localfiletimetofiletime" => Ok(Some(handle_local_file_time_to_file_time(engine)?)),
        "filetimetodosdatetime" => Ok(Some(handle_file_time_to_dos_date_time(engine)?)),
        "dosdatetimetofiletime" => Ok(Some(handle_dos_date_time_to_file_time(engine)?)),
        "getdiskfreespaceexw" => Ok(Some(handle_get_disk_free_space_ex_w(engine, state)?)),
        "getdiskfreespacew" => Ok(Some(handle_get_disk_free_space_w(engine, state)?)),
        "getlogicaldrivestringsw" => Ok(Some(handle_get_logical_drive_strings_w(engine)?)),
        "setfileattributesw" => Ok(Some(handle_set_file_attributes_w(engine, state)?)),
        "setfiletime" => Ok(Some(handle_set_file_time(engine, state)?)),
        "formatmessagew" => Ok(Some(handle_format_message_w(engine)?)),
        "resumethread" => Ok(Some(handle_resume_thread(engine, state)?)),
        "createsemaphorew" | "createsemaphorea" => {
            Ok(Some(handle_create_semaphore(engine, state)?))
        }
        "releasesemaphore" => Ok(Some(handle_release_semaphore(engine, state)?)),
        "openeventw" | "openeventa" => Ok(Some(handle_open_event(engine, state)?)),
        "waitformultipleobjects" => Ok(Some(handle_wait_for_multiple_objects(engine, state)?)),
        "movefilewithprogressw" => Ok(Some(handle_move_file_with_progress_w(engine, state)?)),
        "createhardlinkw" => Ok(Some(handle_create_hard_link_w(engine, state)?)),
        "duplicatehandle" => Ok(Some(handle_duplicate_handle(engine, state)?)),
        "getthreadpriority" => Ok(Some(handle_get_thread_priority(engine, state)?)),
        "raiseexception" => Ok(Some(handle_raise_exception(engine, state)?)),
        "rtlcapturecontext" => Ok(Some(handle_rtl_capture_context(engine, state)?)),
        "rtlunwindex" => Ok(Some(handle_rtl_unwind_ex(engine, state)?)),
        "findfirststreamw" => Ok(Some(handle_find_first_stream_w(engine, state)?)),
        "findnextstreamw" => Ok(Some(handle_find_next_stream_w(engine, state)?)),
        "deviceiocontrol" => Ok(Some(handle_device_io_control(engine, state)?)),
        "mapviewoffile" => Ok(Some(handle_map_view_of_file(engine, state)?)),
        "unmapviewoffile" => Ok(Some(handle_unmap_view_of_file(engine, state)?)),
        "openfilemappingw" | "openfilemappinga" => {
            Ok(Some(handle_open_file_mapping(engine, state)?))
        }
        // Mock-data stubs
        "isdebuggerpresent" => Ok(Some(handle_is_debugger_present(engine)?)),
        "debugbreak" => Ok(Some(handle_debug_break(engine)?)),
        "outputdebugstringa" => Ok(Some(handle_output_debug_string_a(engine)?)),
        "outputdebugstringw" => Ok(Some(handle_output_debug_string_w(engine)?)),
        "seterrormode" => Ok(Some(handle_set_error_mode(engine, state)?)),
        "setthreaderrormode" => Ok(Some(handle_set_thread_error_mode(engine, state)?)),
        "getcompressedfilesizea" => Ok(Some(handle_get_compressed_file_size_a(engine, state)?)),
        "getcompressedfilesizew" => Ok(Some(handle_get_compressed_file_size_w(engine, state)?)),
        "getvolumeinformationw" => Ok(Some(handle_get_volume_information_w(engine, state)?)),
        "getvolumeinformationa" => Ok(Some(handle_get_volume_information_a(engine, state)?)),
        "lockfile" => Ok(Some(handle_lock_file(engine, state)?)),
        "unlockfile" => Ok(Some(handle_unlock_file(engine, state)?)),
        "setfilevaliddata" => Ok(Some(handle_set_file_valid_data(engine, state)?)),
        "getlongpathnamew" => Ok(Some(handle_get_long_path_name_w(engine, state)?)),
        "getlongpathnamea" => Ok(Some(handle_get_long_path_name_a(engine, state)?)),
        "getshortpathnamew" => Ok(Some(handle_get_short_path_name_w(engine, state)?)),
        "getshortpathnamea" => Ok(Some(handle_get_short_path_name_a(engine, state)?)),
        "getcomputernamew" => Ok(Some(handle_get_computer_name_w(engine, state)?)),
        "getcomputernamea" => Ok(Some(handle_get_computer_name_a(engine, state)?)),
        "getcomputernameexw" => Ok(Some(handle_get_computer_name_ex_w(engine, state)?)),
        "getusernamew" => Ok(Some(handle_get_user_name_w(engine, state)?)),
        "getusernamea" => Ok(Some(handle_get_user_name_a(engine, state)?)),
        "getuserprofiledirectoryw" => Ok(Some(handle_get_user_profile_directory_w(engine, state)?)),
        "getuserprofiledirectorya" => Ok(Some(handle_get_user_profile_directory_a(engine, state)?)),
        "openthread" => Ok(Some(handle_open_thread(engine, state)?)),
        "queryfullprocessimagenamew" => {
            Ok(Some(handle_query_full_process_image_name_w(engine, state)?))
        }
        "queryfullprocessimagenamea" => {
            Ok(Some(handle_query_full_process_image_name_a(engine, state)?))
        }
        "createjobobjectw" => Ok(Some(handle_create_job_object_w(engine, state)?)),
        "createjobobjecta" => Ok(Some(handle_create_job_object_a(engine, state)?)),
        "assignprocesstojobobject" => Ok(Some(handle_assign_process_to_job_object(engine, state)?)),
        "terminateprocess" => Ok(Some(handle_terminate_process(engine, state)?)),
        "terminatethread" => Ok(Some(handle_terminate_thread(engine, state)?)),
        "suspendthread" => Ok(Some(handle_suspend_thread(engine, state)?)),
        "getfileattributesexw" => Ok(Some(handle_get_file_attributes_ex_w(engine, state)?)),
        "getfileattributesexa" => Ok(Some(handle_get_file_attributes_ex_a(engine, state)?)),
        "signalobjectandwait" => Ok(Some(handle_signal_object_and_wait(engine, state)?)),
        "backupread" => Ok(Some(handle_backup_read(engine, state)?)),
        "backupseek" => Ok(Some(handle_backup_seek(engine, state)?)),
        "backupwrite" => Ok(Some(handle_backup_write(engine, state)?)),
        _ => Ok(None),
    }
}

/// Stat a guest path using the VFS, building the resolve context from state.
pub(crate) fn stat_guest_path(state: &WinApiState, full_path: &str) -> crate::vfs::PathStat {
    let mounts_ref: Vec<(String, std::path::PathBuf)> = state
        .file_io
        .host_file_mounts
        .iter()
        .map(|m| (m.guest_path.clone(), m.host_path.clone()))
        .collect();
    let virtuals_ref: Vec<(String, usize)> = state
        .file_io
        .virtual_files
        .iter()
        .map(|v| (v.guest_path.clone(), v.bytes.len()))
        .collect();
    let ctx = crate::vfs::ResolveCtx {
        volumes: &state.file_io.volumes,
        main_module_path: &state.process.main_module_path,
        main_module_file_name: &state.process.main_module_file_name,
        host_file_mounts: &mounts_ref,
        virtual_files: &virtuals_ref,
        synthetic_dirs: crate::vfs::DEFAULT_SYNTHETIC_DIRS,
    };
    crate::vfs::stat_path(&ctx, full_path)
}

/// Write a NUL-terminated ANSI string into a guest buffer at `buf` with room
/// for `buf_len` bytes.  Returns the number of characters written (excluding
/// NUL), or 0 with `ERROR_INSUFFICIENT_BUFFER` on truncation.
pub(crate) fn write_mock_string_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    s: &str,
    buf: u64,
    buf_len: u64,
) -> Result<u64> {
    if buf == 0 || buf_len == 0 {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        return Ok(0);
    }
    let encoded = crate::vfs::encode_acp(s);
    let needed = encoded.len(); // bytes (excluding NUL)
    let cap = usize::try_from(buf_len).unwrap_or(0);
    if cap < needed.saturating_add(1) {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        return Ok(0);
    }
    let mut payload = encoded;
    payload.push(0);
    engine.mem_write(buf, &payload)?;
    state.process.last_error = 0;
    Ok(u64::try_from(needed).unwrap_or(0))
}

/// Write a NUL-terminated UTF-16 string into a guest buffer at `buf` with room
/// for `buf_len` WCHARs.  Returns the number of characters written (excluding
/// NUL), or 0 with `ERROR_INSUFFICIENT_BUFFER` on truncation.
pub(crate) fn write_mock_string_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    s: &str,
    buf: u64,
    buf_len: u64,
) -> Result<u64> {
    if buf == 0 || buf_len == 0 {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        return Ok(0);
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let needed = units.len();
    let cap = usize::try_from(buf_len).unwrap_or(0);
    if cap < needed.saturating_add(1) {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        return Ok(0);
    }
    let mut bytes = Vec::with_capacity(needed.saturating_add(1).saturating_mul(2));
    for u in &units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(buf, &bytes)?;
    state.process.last_error = 0;
    Ok(u64::try_from(needed).unwrap_or(0))
}

/// Friendly computer name (NetBIOS equivalent) — `scutil --get ComputerName` on macOS.

/// DNS hostname (DnsHostname equivalent) — `hostname` on Unix.

// Resolve user name from environment.

/// Write a string into a guest buffer with size_ptr update.  Returns
/// the handler result (TRUE on success, FALSE with last_error on failure).

/// NetBIOS / friendly name — `GetComputerName` / `GetComputerNameEx(NetBIOS)`.

/// DNS hostname — `GetComputerNameEx(DnsHostname)`.

/// Common implementation for GetUserName(A/W).

/// Derive the profile directory from the bottle root or a default.

/// Common implementation for GetUserProfileDirectory(A/W).

///
/// `NameType` parameter (RCX):
/// - 0 (ComputerNameNetBIOS) → friendly name
/// - 1 (ComputerNameDnsHostname) → DNS hostname
/// - 2 (ComputerNameDnsDomain) → empty (no domain)
/// - 3 (ComputerNamePhysicalDnsHostname) → DNS hostname
/// - 5 (ComputerNamePhysicalNetBIOS) → friendly name

// Handles `KERNEL32.dll!ReadFileScatter` — stub (synchronous, returns TRUE).
// Default worker stack size when `dwStackSize == 0`.
///
// Matches the common Windows default commit size (1 MiB) rather than a tiny
/// micro-test stack — real PE tools (compressors, CRT workers) need room.
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

/// `WIE_MT=0` kills multi-thread spawn (ST-only). Unset / other = enabled.

/// Cap on live + pending worker TIDs (`WIE_MT_MAX_THREADS`, default 64).

// ─── MT.4 Interlocked* (host atomics on soft-translated memory) ─────────────

/// Truncate a Win64 register operand to signed LONG (low 32 bits).

/// Sign-extend LONG result into RAX (Windows x64 calling convention for LONG).

/// Bitcast i64 result into RAX.

/// Perform an aligned `i32` RMW via host `AtomicI32` when soft-translate works;
/// otherwise fall back to non-atomic mem_read/mem_write (correct under process
/// engine lock; still used for unaligned / non-span cases).

/// Like [`interlocked_i32`] but the return value may be the *previous* value
/// (Exchange / ExchangeAdd / CompareExchange).

/// `InterlockedIncrement` — returns **new** value (Microsoft Learn).

/// `InterlockedDecrement` — returns **new** value.

/// `InterlockedExchange` — returns **previous** value.

/// `InterlockedCompareExchange(dest, exchange, comparand)` — returns previous.
///
/// Win64: RCX=dest, RDX=exchange, R8=comparand.

/// `InterlockedExchangeAdd` — returns **previous** value.

/// `InterlockedCompareExchange64(dest, exchange, comparand)`.
///
/// Win64: RCX=dest, RDX=exchange, R8=comparand (all 64-bit).

/// `CreateThread` — allocate stack/TID/handle and queue a host spawn (MT.2).

/// Shared guest-thread spawn for `CreateThread` and CRT `_beginthreadex`.
///
/// Returns the kernel handle, or `0` with `state.process.last_error` set on failure.
/// Does **not** pop the Win64 API frame (caller completes the return).

const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
/// `ERROR_NOT_SUPPORTED` — used when `WIE_MT=0` refuses `CreateThread`.
const ERROR_NOT_SUPPORTED_MT: u32 = 50;
/// `ERROR_TOO_MANY_POSTS` — semaphore release would exceed maximum.
const ERROR_TOO_MANY_POSTS: u32 = 298;

/// `ExitThread` — mark thread finished and signal worker loop to stop.

/// `GetExitCodeThread`.

/// `WaitForSingleObject` — thread, event, or semaphore (MT.2/3).

// Resolve a waitable handle to a detachable target (wait **outside** process locks).

/// Clone the CS wait queue for parking **outside** process locks.

/// `CreateEventA/W`.

/// `DuplicateHandle` — duplicate a kernel handle.
///
/// Pseudohandles (`GetCurrentProcess`, `GetCurrentThread`) are resolved to
/// real kernel object handles.  Unknown handles are rejected with
/// `ERROR_INVALID_HANDLE`.  `DUPLICATE_CLOSE_SOURCE` closes the source after
/// duplication.

/// `GetThreadPriority` → THREAD_PRIORITY_NORMAL (0).
///
/// No priority model — all guest threads run at the same host priority.
/// Validates the handle: returns `THREAD_PRIORITY_ERROR_RETURN` with
/// `ERROR_INVALID_HANDLE` for garbage values.

/// `RaiseException` — start exception processing (two-pass SEH dispatch).

/// `RtlUnwindEx` — forced stack unwinding.
///
/// RCX = TargetFrame (establisher RSP to stop at; NULL = no frame target)
/// RDX = TargetIp    (landing pad after unwind; NULL = keep frame RIP)
/// R8  = ExceptionRecord (ignored for now)
/// R9  = ReturnValue → RAX after unwind

/// `SetEvent`.

/// `ResetEvent`.

/// `GetCurrentThread` — pseudo-handle `-2` (Microsoft Learn).

/// `FlushInstructionCache(hProcess, lpBaseAddress, dwSize)`.
///
/// Microsoft Learn: after patching code, flush so subsequent fetches see new
/// bytes. WIE maps this to selective JIT Ready invalidation (Phase 7).
/// `dwSize == 0` flushes the whole process instruction cache.

/// `VirtualAlloc(lpAddress, dwSize, flAllocationType, flProtect)`.

/// `VirtualFree(lpAddress, dwSize, dwFreeType)`.

/// `VirtualProtect` — Microsoft Learn: `lpflOldProtect` must be non-NULL or the
/// call fails. Real page protection via guest PageMap (Phase 3).

/// `VirtualQuery` — fill real `MEMORY_BASIC_INFORMATION` from PageMap / VAD.

///
/// Idle policy (Phase 6 — see [`crate::idle`]):
// - `Sleep(0)` always yields the host thread (`yield_now`).
// - `Sleep(n>0)`: **no-op** under `WIE_IDLE=yield|busy` (micros); parks under
///   `WIE_IDLE=park` or legacy `WIE_HOST_SLEEP=1` (capped by `WIE_IDLE_CAP_MS`).
///
/// Not planted as an in-guest stub — side effects depend on host idle policy.

// Resolve a Windows path against the process current directory.
pub(crate) fn resolve_full_windows_path(current_directory: &str, input_path: &str) -> String {
    crate::vfs::resolve_full_windows_path(current_directory, input_path)
}

#[cfg(test)]
pub(crate) fn normalize_windows_path_components(path: &str) -> String {
    crate::vfs::normalize_windows_path_components(path)
}

pub(crate) fn finish_create_directory(state: &mut WinApiState, path: &str) -> u64 {
    if path.is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    }
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, path);
    if guest_dir_exists(state, &full) {
        state.process.last_error = ERROR_ALREADY_EXISTS;
        return 0;
    }
    let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full) else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    };
    if crate::vfs::mkdir_host(&map.host).is_ok() {
        state.process.last_error = 0;
        1
    } else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        0
    }
}

pub(crate) fn finish_delete_file(state: &mut WinApiState, path: &str) -> u64 {
    if path.is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    }
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, path);
    state
        .file_io
        .virtual_files
        .retain(|v| !paths_match_guest(&full, &v.guest_path));
    if let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full) {
        if crate::vfs::remove_file_host(&map.host).is_ok() {
            state.process.last_error = 0;
            return 1;
        }
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        return 0;
    }
    state.process.last_error = ERROR_FILE_NOT_FOUND;
    0
}

pub(crate) fn finish_remove_directory(state: &mut WinApiState, path: &str) -> u64 {
    if path.is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    }
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, path);
    let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full) else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    };
    match crate::vfs::remove_dir_host(&map.host) {
        Ok(()) => {
            state.process.last_error = 0;
            1
        }
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            state.process.last_error = ERROR_DIR_NOT_EMPTY;
            0
        }
        Err(_) => {
            state.process.last_error = ERROR_PATH_NOT_FOUND;
            0
        }
    }
}

pub(crate) fn finish_move_file(state: &mut WinApiState, from: &str, to: &str) -> u64 {
    if from.is_empty() || to.is_empty() {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    }
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_from = resolve_full_windows_path(&cwd, from);
    let full_to = resolve_full_windows_path(&cwd, to);
    let Some(src) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full_from) else {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        return 0;
    };
    let Some(dst) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full_to) else {
        state.process.last_error = ERROR_PATH_NOT_FOUND;
        return 0;
    };
    if crate::vfs::rename_host(&src.host, &dst.host).is_ok() {
        state.process.last_error = 0;
        1
    } else {
        state.process.last_error = ERROR_ACCESS_DENIED;
        0
    }
}

pub(crate) fn temp_name_id_u32(id: u64) -> u32 {
    u32::try_from(id & 0xffff_ffff).unwrap_or(0)
}

pub(crate) fn finish_create_file_create_only(state: &mut WinApiState, guest_path: &str) {
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full = resolve_full_windows_path(&cwd, guest_path);
    if state.file_io.volumes.bottle_root != state.file_io.bottle_root {
        state.file_io.volumes.bottle_root = state.file_io.bottle_root.clone();
    }
    if let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &full) {
        drop(crate::vfs::create_host_file(&map.host));
    } else {
        ensure_virtual_file(state, &full);
    }
}

pub(crate) fn write_fixed_dir_w(
    engine: &mut dyn wie_cpu::CpuEngine,
    dir: &str,
) -> Result<WinApiHandlerResult> {
    let buffer_len = engine.read_rcx()?;
    let buffer_ptr = engine.read_rdx()?;
    let units: Vec<u16> = dir.encode_utf16().collect();
    let required = u64::try_from(units.len().saturating_add(1)).unwrap_or(0);
    let return_value = if buffer_ptr == 0 || buffer_len < required {
        required
    } else {
        let mut t = units;
        t.push(0);
        write_guest_utf16_units(engine, buffer_ptr, &t)?;
        u64::try_from(t.len().saturating_sub(1)).unwrap_or(0)
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

pub(crate) fn write_fixed_dir_a(
    engine: &mut dyn wie_cpu::CpuEngine,
    dir: &str,
) -> Result<WinApiHandlerResult> {
    let buffer_len = engine.read_rcx()?;
    let buffer_ptr = engine.read_rdx()?;
    let bytes = crate::vfs::encode_acp(dir);
    let required = u64::try_from(bytes.len().saturating_add(1)).unwrap_or(0);
    let return_value = if buffer_ptr == 0 || buffer_len < required {
        required
    } else {
        let mut out = bytes;
        out.push(0);
        engine.mem_write(buffer_ptr, &out)?;
        u64::try_from(out.len().saturating_sub(1)).unwrap_or(0)
    };
    let return_address = engine.return_from_win64_api(return_value)?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value,
    })
}

#[cfg(test)]
mod path_resolve_tests {
    use super::{normalize_windows_path_components, resolve_full_windows_path};

    #[test]
    fn relative_dot_slash_against_cwd() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r".\config.ini"),
            r"C:\App\config.ini"
        );
    }

    #[test]
    fn relative_bare_name() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r"config.ini"),
            r"C:\App\config.ini"
        );
    }

    #[test]
    fn relative_dotdot() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App\data", r"..\config.ini"),
            r"C:\App\config.ini"
        );
    }

    #[test]
    fn rooted_on_current_drive() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r"\Windows\win.ini"),
            r"C:\Windows\win.ini"
        );
    }

    #[test]
    fn absolute_unchanged_after_normalize() {
        assert_eq!(
            resolve_full_windows_path(r"C:\App", r"D:\other\file.txt"),
            r"D:\other\file.txt"
        );
    }

    #[test]
    fn collapses_dot_components() {
        assert_eq!(
            normalize_windows_path_components(r"C:\App\.\sub\..\x.txt"),
            r"C:\App\x.txt"
        );
    }
}
