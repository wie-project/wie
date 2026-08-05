//! VERSION.dll handlers: the Windows Version API.
//!
//! The standard version-reading flow is fully supported:
//! `GetFileVersionInfoSize(W/A)` → `GetFileVersionInfo(W/A)` →
//! `VerQueryValue(W/A)`. The guest file name resolves through the VFS (the
//! bottle mapping, pick-mounts, and virtual files — the same funnel every
//! file op uses), the `RT_VERSION` resource is parsed by `wie-pe`, and
//! `GetFileVersionInfo` copies the **raw** resource bytes into the caller's
//! buffer. `VerQueryValue` then walks that copied block in guest memory and
//! returns a pointer *into it* — the faithful Windows semantics.
//!
//! Honest dispositions (documented, never fake success):
//! * `GetFileVersionInfoSizeEx*`: the three `FILE_VER_GET_*` flags are
//!   accepted and treated as no-ops (WIE always selects the single version
//!   resource; there is no neutral/localised split or prefetch cache to
//!   honor). Unknown flag bits fail with `ERROR_INVALID_PARAMETER`.
//! * The opaque handle output is always 0 (WIE needs no handle bookkeeping;
//!   callers pass it straight back into `GetFileVersionInfo`, which ignores it).
//! * `VerFindFile*` really searches the application directory, current
//!   directory, Windows directory, and System32; `VFF_NOTFOUND` when absent.
//!   The version-comparison flags (`VFF_CURNEDEST`, `VFF_PREVWIN*`) are never
//!   produced — WIE has no previous-version model.
//! * `VerInstallFile*` really copies the source into the destination
//!   directory (a side effect, like the real API) and reports the documented
//!   `VIF_*` failures (`VIF_SRCFILENOTFOUND`, `VIF_CANNOTCREATE`,
//!   `VIF_BUFFTOOSMALL`); the install-detail flags (`VIF_FILEINUSE`, …) never
//!   apply to a host-side copy.

use anyhow::{Context, Result};

use crate::{
    HandlerContext, WinApiHandlerResult, WinApiState,
    kernel32::{self, read_ansi_string_from_cpu, read_stack_u64, read_wide_string_from_cpu},
};

/// Win32 `ERROR_FILE_NOT_FOUND`.
const ERROR_FILE_NOT_FOUND: u32 = 2;
/// Win32 `ERROR_INVALID_PARAMETER`.
const ERROR_INVALID_PARAMETER: u32 = 87;
/// Win32 `ERROR_INSUFFICIENT_BUFFER`.
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
/// Win32 `ERROR_RESOURCE_DATA_NOT_FOUND` — the file exists but carries no
/// version resource (the documented failure of the version APIs).
const ERROR_RESOURCE_DATA_NOT_FOUND: u32 = 1815;

/// `FILE_VER_GET_NEUTRAL` (verrsrc.h).
const FILE_VER_GET_NEUTRAL: u32 = 0x0001;
/// `FILE_VER_GET_PREFETCHED` (verrsrc.h) — a performance hint only.
const FILE_VER_GET_PREFETCHED: u32 = 0x0002;
/// `FILE_VER_GET_LOCALISED` (verrsrc.h).
const FILE_VER_GET_LOCALISED: u32 = 0x0004;
/// Every documented `FILE_VER_GET_*` bit (anything else is invalid).
const SUPPORTED_VER_GET_FLAGS: u32 =
    FILE_VER_GET_NEUTRAL | FILE_VER_GET_PREFETCHED | FILE_VER_GET_LOCALISED;

/// `VFF_NOTFOUND` (VerFindFile) — the file could not be located.
const VFF_NOTFOUND: u32 = 0x8000;
/// `VFF_BUFFTOOSMALL` (VerFindFile) — a directory buffer was too small.
const VFF_BUFFTOOSMALL: u32 = 0x0004;

/// `VIF_SRCFILENOTFOUND` (VerInstallFile) — the source file is missing.
const VIF_SRCFILENOTFOUND: u32 = 0x0004;
/// `VIF_CANNOTCREATE` (VerInstallFile) — the destination could not be created.
const VIF_CANNOTCREATE: u32 = 0x0080;
/// `VIF_BUFFTOOSMALL` (VerInstallFile) — the temp-file buffer was too small.
const VIF_BUFFTOOSMALL: u32 = 0x4000;

/// Sanity cap on a guest version block (real ones are a few KiB).
const MAX_GUEST_VERSION_BLOCK: usize = 1024 * 1024;

/// Outcome of loading the version resource of a guest file.
enum VersionFileOutcome {
    /// The file does not exist on any mapped volume.
    NotFound,
    /// The file exists but is not a PE or carries no `RT_VERSION` resource.
    NoVersionResource,
    /// The raw `VS_VERSIONINFO` block (what the API hands the caller).
    Block(Vec<u8>),
}

/// Read the whole guest file `full_path` into host bytes.
///
/// Resolution order: the main module (its bytes are already in memory), a
/// host-file mount (including pick-mounts), a virtual file, then the volume
/// mapping — the same funnel `GetFileAttributes` uses. `None` = not found.
fn read_target_bytes(state: &WinApiState, full_path: &str) -> Option<Vec<u8>> {
    if kernel32::is_main_module_path(state, full_path) {
        return Some(state.file_io.executable_file_bytes.as_ref().clone());
    }
    for mount in &state.file_io.host_file_mounts {
        if crate::vfs::paths_equal_ci(full_path, &mount.guest_path) {
            return std::fs::read(&mount.host_path).ok();
        }
    }
    for virtual_file in &state.file_io.virtual_files {
        if crate::vfs::paths_equal_ci(full_path, &virtual_file.guest_path) {
            return Some(virtual_file.bytes.clone());
        }
    }
    let map = crate::vfs::guest_path_to_host(&state.file_io.volumes, full_path)?;
    std::fs::read(&map.host).ok()
}

/// Read the bytes of an open `CreateFile` handle (the `ByHandle` path).
fn read_open_handle_bytes(state: &WinApiState, handle: u64) -> Option<Vec<u8>> {
    let open = state.file_io.open_files.get(&handle)?;
    if !open.bytes.is_empty() {
        return Some(open.bytes.clone());
    }
    let host = open.host_path.as_ref()?;
    std::fs::read(host).ok()
}

/// Load the raw version block of the guest file at `full_path`.
fn load_version_block(state: &WinApiState, full_path: &str) -> Result<VersionFileOutcome> {
    let Some(bytes) = read_target_bytes(state, full_path) else {
        return Ok(VersionFileOutcome::NotFound);
    };
    let Ok(plan) = wie_pe::pe_map_plan_from_bytes(&bytes) else {
        // Not a parseable PE64: no version resource can exist.
        return Ok(VersionFileOutcome::NoVersionResource);
    };
    let resource = wie_pe::resources::parse_pe_version_resources(&bytes, &plan.sections)
        .into_iter()
        .next();
    Ok(match resource {
        Some(r) => VersionFileOutcome::Block(r.raw),
        None => VersionFileOutcome::NoVersionResource,
    })
}

/// Shared `GetFileVersionInfoSize*` tail: resolve, size, write the handle.
fn finish_version_size(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    full_path: &str,
    handle_ptr: u64,
) -> Result<WinApiHandlerResult> {
    let (value, last_error) = match load_version_block(state, full_path)? {
        VersionFileOutcome::NotFound => (0, ERROR_FILE_NOT_FOUND),
        VersionFileOutcome::NoVersionResource => (0, ERROR_RESOURCE_DATA_NOT_FOUND),
        VersionFileOutcome::Block(raw) => {
            if handle_ptr != 0 {
                // The opaque handle: always 0 — WIE keeps no handle bookkeeping,
                // and `GetFileVersionInfo` ignores the value anyway.
                kernel32::write_guest_u32(engine, handle_ptr, 0)
                    .context("failed to write version handle")?;
            }
            (u64::try_from(raw.len()).unwrap_or(0), 0)
        }
    };
    state.process.last_error = last_error;
    let return_address = engine
        .return_from_win64_api(value)
        .context("failed to return from GetFileVersionInfoSize")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// Shared `GetFileVersionInfo*` tail: copy the raw block into the guest buffer.
fn finish_version_info(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    full_path: &str,
    data_len: u64,
    data_ptr: u64,
) -> Result<WinApiHandlerResult> {
    let (ok, last_error) = match load_version_block(state, full_path)? {
        VersionFileOutcome::NotFound => (0, ERROR_FILE_NOT_FOUND),
        VersionFileOutcome::NoVersionResource => (0, ERROR_RESOURCE_DATA_NOT_FOUND),
        VersionFileOutcome::Block(raw) => {
            let raw_len = u64::try_from(raw.len()).unwrap_or(0);
            if data_ptr == 0 || raw_len > data_len {
                (0, ERROR_INSUFFICIENT_BUFFER)
            } else {
                crate::guest_memory::write_bytes(engine, data_ptr, &raw)
                    .context("failed to copy version block to guest")?;
                (1, 0)
            }
        }
    };
    state.process.last_error = last_error;
    let return_address = engine
        .return_from_win64_api(ok)
        .context("failed to return from GetFileVersionInfo")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: ok,
    })
}

/// Shared `VerQueryValue*` tail: walk the copied block in guest memory.
fn finish_ver_query_value(
    engine: &mut dyn wie_cpu::CpuEngine,
    block_ptr: u64,
    path: &str,
    buffer_out: u64,
    len_out: u64,
) -> Result<WinApiHandlerResult> {
    let ok = if block_ptr == 0 || buffer_out == 0 || len_out == 0 {
        0
    } else {
        match read_version_block_from_guest(engine, block_ptr)? {
            None => 0,
            Some(block) => match wie_pe::resources::query_version_value(&block, path) {
                None => 0,
                Some(m) => {
                    let offset =
                        u64::try_from(m.offset).context("query offset does not fit u64")?;
                    let value_ptr = block_ptr
                        .checked_add(offset)
                        .context("query value pointer overflow")?;
                    kernel32::write_guest_u64(engine, buffer_out, value_ptr)
                        .context("failed to write query buffer pointer")?;
                    let len = u32::try_from(m.len).context("query length does not fit u32")?;
                    kernel32::write_guest_u32(engine, len_out, len)
                        .context("failed to write query length")?;
                    1
                }
            },
        }
    };
    let return_address = engine
        .return_from_win64_api(ok)
        .context("failed to return from VerQueryValue")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: ok,
    })
}

/// Read the whole guest version block (bounded by its `wLength`).
fn read_version_block_from_guest(
    engine: &mut dyn wie_cpu::CpuEngine,
    block_ptr: u64,
) -> Result<Option<Vec<u8>>> {
    let len = u64::from(kernel32::read_guest_u16(engine, block_ptr)?);
    if len < 6 {
        return Ok(None);
    }
    let len_usize = usize::try_from(len).context("version block length does not fit usize")?;
    let len_usize = len_usize.min(MAX_GUEST_VERSION_BLOCK);
    let mut block = vec![0_u8; len_usize];
    crate::guest_memory::read_bytes(engine, block_ptr, &mut block)?;
    Ok(Some(block))
}

/// The language name `VerLanguageName` reports for a LANGID.
///
/// `English (United States)` for 0x0409, `German (Germany)` for 0x0407, …
/// The table mirrors the LANGID set of the repo's locale work (`user32::lang`);
/// unknown primary languages report `Unknown language (0xNNNN)` — the honest
/// Windows format, never a fabricated name.
fn ver_language_name(langid: u32) -> String {
    // LANGID layout (winnt.h): primary language in the low 10 bits,
    // sublanguage in the high 6 bits.
    let primary = langid & 0x3FF;
    let sublang = langid >> 10;
    let Some(base) = primary_language_name(primary) else {
        return format!("Unknown language (0x{langid:04X})");
    };
    // Chinese is reported by script, not region.
    if primary == 0x04 {
        let script = match sublang {
            0x01 => "Traditional",
            0x02 => "Simplified",
            _ => return base.to_owned(),
        };
        return format!("Chinese ({script})");
    }
    match sublanguage_region(primary, sublang) {
        Some(region) => format!("{base} ({region})"),
        None => base.to_owned(),
    }
}

/// Primary-language names for the common LANGID primary values (ISO 639).
fn primary_language_name(primary: u32) -> Option<&'static str> {
    match primary {
        0x01 => Some("Arabic"),
        0x02 => Some("Bulgarian"),
        0x03 => Some("Catalan"),
        0x04 => Some("Chinese"),
        0x05 => Some("Czech"),
        0x06 => Some("Danish"),
        0x07 => Some("German"),
        0x08 => Some("Greek"),
        0x09 => Some("English"),
        0x0A => Some("Spanish"),
        0x0B => Some("Finnish"),
        0x0C => Some("French"),
        0x0D => Some("Hebrew"),
        0x0E => Some("Hungarian"),
        0x0F => Some("Icelandic"),
        0x10 => Some("Italian"),
        0x11 => Some("Japanese"),
        0x12 => Some("Korean"),
        0x13 => Some("Dutch"),
        0x14 => Some("Norwegian"),
        0x15 => Some("Polish"),
        0x16 => Some("Portuguese"),
        0x18 => Some("Romanian"),
        0x19 => Some("Russian"),
        0x1A => Some("Croatian"),
        0x1B => Some("Slovak"),
        0x1C => Some("Albanian"),
        0x1D => Some("Swedish"),
        0x1E => Some("Thai"),
        0x1F => Some("Turkish"),
        0x20 => Some("Urdu"),
        0x22 => Some("Ukrainian"),
        0x2A => Some("Vietnamese"),
        _ => None,
    }
}

/// Region suffix for the common `<primary><sublang>` LANGID combinations.
fn sublanguage_region(primary: u32, sublang: u32) -> Option<&'static str> {
    match (primary, sublang) {
        (0x09, 0x01) => Some("United States"),
        (0x09, 0x02) => Some("United Kingdom"),
        (0x09, 0x03) => Some("Australia"),
        (0x09, 0x04) => Some("Canada"),
        (0x07, 0x01) => Some("Germany"),
        (0x07, 0x02) => Some("Switzerland"),
        (0x07, 0x03) => Some("Austria"),
        (0x0A, 0x01) => Some("Spain"),
        (0x0A, 0x02) => Some("Mexico"),
        (0x0C, 0x01) => Some("France"),
        (0x0C, 0x02) => Some("Belgium"),
        (0x0C, 0x03) => Some("Canada"),
        (0x0C, 0x04) => Some("Switzerland"),
        (0x10, 0x01) => Some("Italy"),
        (0x11, 0x01) => Some("Japan"),
        (0x12, 0x01) => Some("Korea"),
        (0x13, 0x01) => Some("Netherlands"),
        (0x13, 0x02) => Some("Belgium"),
        (0x14, 0x01) => Some("Bokmål"),
        (0x14, 0x02) => Some("Nynorsk"),
        (0x16, 0x01) => Some("Brazil"),
        (0x16, 0x02) => Some("Portugal"),
        (0x19, 0x01) => Some("Russia"),
        (0x1D, 0x01) => Some("Sweden"),
        (0x1E, 0x01) => Some("Thailand"),
        (0x1F, 0x01) => Some("Turkey"),
        (0x22, 0x01) => Some("Ukraine"),
        (0x2A, 0x01) => Some("Vietnam"),
        _ => None,
    }
}

/// Join a Windows directory and file name into a guest path.
fn join_guest_path(dir: &str, file: &str) -> String {
    if file.is_empty() {
        return dir.to_owned();
    }
    if dir.is_empty() {
        return file.to_owned();
    }
    if file.contains(['\\', '/', ':']) {
        return file.to_owned();
    }
    format!("{dir}\\{file}")
}

/// `VerFindFile` core: search the standard directories for `file`.
///
/// Returns `(flags, located_dir)`. The search order is the documented one
/// minus the PATH fallback: application directory, current directory, Windows
/// directory, System32.
fn find_file_dirs(state: &WinApiState, file: &str) -> (u32, String) {
    let mut dirs = Vec::new();
    if !state.process.main_module_path.is_empty() {
        dirs.push(crate::vfs::guest_parent(&state.process.main_module_path));
    }
    dirs.push(String::from_utf16_lossy(
        &state.file_io.current_directory_wide,
    ));
    dirs.push(crate::vfs::GUEST_WINDOWS_DIR.to_owned());
    dirs.push(crate::vfs::GUEST_SYSTEM_DIR.to_owned());
    for dir in dirs {
        let candidate = join_guest_path(&dir, file);
        if read_target_bytes(state, &candidate).is_some() {
            return (0, dir);
        }
    }
    (VFF_NOTFOUND, String::new())
}

/// Write one optional directory string, reporting `VFF_BUFFTOOSMALL` when the
/// caller's buffer cannot hold it (the buffer length is still updated to the
/// required size, `GetTempPath`-style).
fn write_dir_out(
    engine: &mut dyn wie_cpu::CpuEngine,
    buf_ptr: u64,
    len_ptr: u64,
    dir: &str,
) -> Result<u32> {
    if len_ptr == 0 {
        // Nothing to report back.
        return Ok(0);
    }
    let cap = usize::try_from(u64::from(kernel32::read_guest_u32(engine, len_ptr)?))
        .context("directory capacity does not fit usize")?;
    let needed = dir.encode_utf16().count();
    let written = if buf_ptr == 0 {
        0
    } else {
        crate::guest_string::write_utf16_c_string(engine, buf_ptr, cap, dir)?
    };
    let needed_u32 = u32::try_from(needed).unwrap_or(u32::MAX);
    kernel32::write_guest_u32(engine, len_ptr, needed_u32)?;
    Ok(if written < needed {
        VFF_BUFFTOOSMALL
    } else {
        0
    })
}

/// Validate the `FILE_VER_GET_*` flags of the `Ex` variants.
///
/// The three documented flags are accepted (they select the version resource
/// or prefetch it — WIE has one resource and no prefetch cache, so they are
/// no-ops). Unknown bits are an invalid parameter.
fn validate_ver_get_flags(flags: u32, state: &mut WinApiState) -> bool {
    if flags & !SUPPORTED_VER_GET_FLAGS == 0 {
        return true;
    }
    state.process.last_error = ERROR_INVALID_PARAMETER;
    false
}

// ── Handlers (Win64 register ABI) ────────────────────────────────────────

/// Handles `VERSION.dll!GetFileVersionInfoSizeW`.
pub fn handle_get_file_version_info_size_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let file_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetFileVersionInfoSizeW")?;
    let handle_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetFileVersionInfoSizeW")?;
    let path = read_wide_string_from_cpu(engine, file_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = kernel32::resolve_full_windows_path(&cwd, &path);
    finish_version_size(engine, state, &full_path, handle_ptr)
}

/// Handles `VERSION.dll!GetFileVersionInfoSizeA`.
pub fn handle_get_file_version_info_size_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let file_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetFileVersionInfoSizeA")?;
    let handle_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetFileVersionInfoSizeA")?;
    let path = read_ansi_string_from_cpu(engine, file_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = kernel32::resolve_full_windows_path(&cwd, &path);
    finish_version_size(engine, state, &full_path, handle_ptr)
}

/// Handles `VERSION.dll!GetFileVersionInfoSizeExW`.
pub fn handle_get_file_version_info_size_ex_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let flags = kernel32::low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for GetFileVersionInfoSizeExW")?,
        "GetFileVersionInfoSizeExW dwFlags",
    )?;
    if !validate_ver_get_flags(flags, state) {
        return return_zero(engine, "GetFileVersionInfoSizeExW");
    }
    let file_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetFileVersionInfoSizeExW")?;
    let handle_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetFileVersionInfoSizeExW")?;
    let path = read_wide_string_from_cpu(engine, file_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = kernel32::resolve_full_windows_path(&cwd, &path);
    finish_version_size(engine, state, &full_path, handle_ptr)
}

/// Handles `VERSION.dll!GetFileVersionInfoSizeExA`.
pub fn handle_get_file_version_info_size_ex_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let flags = kernel32::low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for GetFileVersionInfoSizeExA")?,
        "GetFileVersionInfoSizeExA dwFlags",
    )?;
    if !validate_ver_get_flags(flags, state) {
        return return_zero(engine, "GetFileVersionInfoSizeExA");
    }
    let file_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetFileVersionInfoSizeExA")?;
    let handle_ptr = engine
        .read_r8()
        .context("failed to read R8 for GetFileVersionInfoSizeExA")?;
    let path = read_ansi_string_from_cpu(engine, file_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = kernel32::resolve_full_windows_path(&cwd, &path);
    finish_version_size(engine, state, &full_path, handle_ptr)
}

/// Handles `VERSION.dll!GetFileVersionInfoW`.
pub fn handle_get_file_version_info_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let file_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetFileVersionInfoW")?;
    let _handle = engine
        .read_rdx()
        .context("failed to read RDX for GetFileVersionInfoW")?;
    let data_len = engine
        .read_r8()
        .context("failed to read R8 for GetFileVersionInfoW")?;
    let data_ptr = engine
        .read_r9()
        .context("failed to read R9 for GetFileVersionInfoW")?;
    let path = read_wide_string_from_cpu(engine, file_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = kernel32::resolve_full_windows_path(&cwd, &path);
    finish_version_info(engine, state, &full_path, data_len, data_ptr)
}

/// Handles `VERSION.dll!GetFileVersionInfoA`.
pub fn handle_get_file_version_info_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let file_ptr = engine
        .read_rcx()
        .context("failed to read RCX for GetFileVersionInfoA")?;
    let _handle = engine
        .read_rdx()
        .context("failed to read RDX for GetFileVersionInfoA")?;
    let data_len = engine
        .read_r8()
        .context("failed to read R8 for GetFileVersionInfoA")?;
    let data_ptr = engine
        .read_r9()
        .context("failed to read R9 for GetFileVersionInfoA")?;
    let path = read_ansi_string_from_cpu(engine, file_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = kernel32::resolve_full_windows_path(&cwd, &path);
    finish_version_info(engine, state, &full_path, data_len, data_ptr)
}

/// Handles `VERSION.dll!GetFileVersionInfoExW`.
pub fn handle_get_file_version_info_ex_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let flags = kernel32::low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for GetFileVersionInfoExW")?,
        "GetFileVersionInfoExW dwFlags",
    )?;
    if !validate_ver_get_flags(flags, state) {
        return return_zero(engine, "GetFileVersionInfoExW");
    }
    let file_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetFileVersionInfoExW")?;
    let _handle = engine
        .read_r8()
        .context("failed to read R8 for GetFileVersionInfoExW")?;
    let data_len = engine
        .read_r9()
        .context("failed to read R9 for GetFileVersionInfoExW")?;
    let data_ptr = read_stack_u64(engine, 0x28)?; // 5th arg
    let path = read_wide_string_from_cpu(engine, file_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = kernel32::resolve_full_windows_path(&cwd, &path);
    finish_version_info(engine, state, &full_path, data_len, data_ptr)
}

/// Handles `VERSION.dll!GetFileVersionInfoExA`.
pub fn handle_get_file_version_info_ex_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let flags = kernel32::low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for GetFileVersionInfoExA")?,
        "GetFileVersionInfoExA dwFlags",
    )?;
    if !validate_ver_get_flags(flags, state) {
        return return_zero(engine, "GetFileVersionInfoExA");
    }
    let file_ptr = engine
        .read_rdx()
        .context("failed to read RDX for GetFileVersionInfoExA")?;
    let _handle = engine
        .read_r8()
        .context("failed to read R8 for GetFileVersionInfoExA")?;
    let data_len = engine
        .read_r9()
        .context("failed to read R9 for GetFileVersionInfoExA")?;
    let data_ptr = read_stack_u64(engine, 0x28)?; // 5th arg
    let path = read_ansi_string_from_cpu(engine, file_ptr, 1024)?;
    let cwd = String::from_utf16_lossy(&state.file_io.current_directory_wide);
    let full_path = kernel32::resolve_full_windows_path(&cwd, &path);
    finish_version_info(engine, state, &full_path, data_len, data_ptr)
}

/// Handles `VERSION.dll!VerQueryValueW`.
pub fn handle_ver_query_value_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let block_ptr = engine
        .read_rcx()
        .context("failed to read RCX for VerQueryValueW")?;
    let path_ptr = engine
        .read_rdx()
        .context("failed to read RDX for VerQueryValueW")?;
    let buffer_out = engine
        .read_r8()
        .context("failed to read R8 for VerQueryValueW")?;
    let len_out = engine
        .read_r9()
        .context("failed to read R9 for VerQueryValueW")?;
    let path = read_wide_string_from_cpu(engine, path_ptr, 1024)?;
    finish_ver_query_value(engine, block_ptr, &path, buffer_out, len_out)
}

/// Handles `VERSION.dll!VerQueryValueA`.
pub fn handle_ver_query_value_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let block_ptr = engine
        .read_rcx()
        .context("failed to read RCX for VerQueryValueA")?;
    let path_ptr = engine
        .read_rdx()
        .context("failed to read RDX for VerQueryValueA")?;
    let buffer_out = engine
        .read_r8()
        .context("failed to read R8 for VerQueryValueA")?;
    let len_out = engine
        .read_r9()
        .context("failed to read R9 for VerQueryValueA")?;
    // The A-path is decoded via the shared CP1252 path (UTF-8 literals first).
    let path = read_ansi_string_from_cpu(engine, path_ptr, 1024)?;
    finish_ver_query_value(engine, block_ptr, &path, buffer_out, len_out)
}

/// Handles `VERSION.dll!GetFileVersionInfoByHandleW`.
///
/// Reads the version resource of an already-open `CreateFile` handle. The
/// handle-based path is exempt from the bottle policy, like `ReadFile`.
pub fn handle_get_file_version_info_by_handle_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let handle = engine
        .read_rcx()
        .context("failed to read RCX for GetFileVersionInfoByHandleW")?;
    let _handle_arg = engine
        .read_rdx()
        .context("failed to read RDX for GetFileVersionInfoByHandleW")?;
    let data_len = engine
        .read_r8()
        .context("failed to read R8 for GetFileVersionInfoByHandleW")?;
    let data_ptr = engine
        .read_r9()
        .context("failed to read R9 for GetFileVersionInfoByHandleW")?;
    let Some(bytes) = read_open_handle_bytes(state, handle) else {
        state.process.last_error = ERROR_FILE_NOT_FOUND;
        return return_zero(engine, "GetFileVersionInfoByHandleW");
    };
    let Ok(plan) = wie_pe::pe_map_plan_from_bytes(&bytes) else {
        state.process.last_error = ERROR_RESOURCE_DATA_NOT_FOUND;
        return return_zero(engine, "GetFileVersionInfoByHandleW");
    };
    let Some(resource) = wie_pe::resources::parse_pe_version_resources(&bytes, &plan.sections)
        .into_iter()
        .next()
    else {
        state.process.last_error = ERROR_RESOURCE_DATA_NOT_FOUND;
        return return_zero(engine, "GetFileVersionInfoByHandleW");
    };
    let raw = resource.raw;
    let raw_len = u64::try_from(raw.len()).unwrap_or(0);
    let ok = if data_ptr == 0 || raw_len > data_len {
        state.process.last_error = ERROR_INSUFFICIENT_BUFFER;
        0
    } else {
        crate::guest_memory::write_bytes(engine, data_ptr, &raw)
            .context("failed to copy version block to guest")?;
        state.process.last_error = 0;
        1
    };
    let return_address = engine
        .return_from_win64_api(ok)
        .context("failed to return from GetFileVersionInfoByHandleW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: ok,
    })
}

/// Handles `VERSION.dll!GetFileVersionInfoByHandleA` (identical body).
pub fn handle_get_file_version_info_by_handle_a(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    handle_get_file_version_info_by_handle_w(ctx)
}

/// Handles `VERSION.dll!VerLanguageNameW`.
pub fn handle_ver_language_name_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let lang = kernel32::low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for VerLanguageNameW")?,
        "VerLanguageNameW wLang",
    )?;
    let buf = engine
        .read_rdx()
        .context("failed to read RDX for VerLanguageNameW")?;
    let cch = engine
        .read_r8()
        .context("failed to read R8 for VerLanguageNameW")?;
    let name = ver_language_name(lang);
    let cch_usize = usize::try_from(cch).context("VerLanguageNameW cch does not fit usize")?;
    let _written = crate::guest_string::write_utf16_c_string(engine, buf, cch_usize, &name)?;
    // The full length in characters excluding NUL — the "required size"
    // contract even when a small buffer truncated the write.
    let units = u64::try_from(name.encode_utf16().count()).unwrap_or(0);
    let return_address = engine
        .return_from_win64_api(units)
        .context("failed to return from VerLanguageNameW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: units,
    })
}

/// Handles `VERSION.dll!VerLanguageNameA`.
pub fn handle_ver_language_name_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let lang = kernel32::low_u32(
        engine
            .read_rcx()
            .context("failed to read RCX for VerLanguageNameA")?,
        "VerLanguageNameA wLang",
    )?;
    let buf = engine
        .read_rdx()
        .context("failed to read RDX for VerLanguageNameA")?;
    let cch = engine
        .read_r8()
        .context("failed to read R8 for VerLanguageNameA")?;
    let name = ver_language_name(lang);
    let cch_usize = usize::try_from(cch).context("VerLanguageNameA cch does not fit usize")?;
    let _written = crate::guest_string::write_ansi_c_string(engine, buf, cch_usize, &name)?;
    // CP1252 is one byte per char, so the byte count is the char count.
    let bytes = u64::try_from(crate::guest_string::encode_cp1252(&name).len()).unwrap_or(0);
    let return_address = engine
        .return_from_win64_api(bytes)
        .context("failed to return from VerLanguageNameA")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: bytes,
    })
}

/// Handles `VERSION.dll!VerFindFileW`.
pub fn handle_ver_find_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let _flags = engine
        .read_rcx()
        .context("failed to read RCX for VerFindFileW")?;
    let file_ptr = engine
        .read_rdx()
        .context("failed to read RDX for VerFindFileW")?;
    let cur_dir_ptr = read_stack_u64(engine, 0x28)?;
    let cur_dir_len_ptr = read_stack_u64(engine, 0x30)?;
    let dest_dir_ptr = read_stack_u64(engine, 0x38)?;
    let dest_dir_len_ptr = read_stack_u64(engine, 0x40)?;
    let file = read_wide_string_from_cpu(engine, file_ptr, 1024)?;
    let (mut flags, located) = find_file_dirs(state, &file);
    // Report the located directory; the dest dir is a Windows-versioning
    // concept WIE does not model, so it stays empty (documented above).
    flags |= write_dir_out(engine, cur_dir_ptr, cur_dir_len_ptr, &located)?;
    flags |= write_dir_out(engine, dest_dir_ptr, dest_dir_len_ptr, "")?;
    let return_address = engine
        .return_from_win64_api(u64::from(flags))
        .context("failed to return from VerFindFileW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(flags),
    })
}

/// Handles `VERSION.dll!VerFindFileA`.
pub fn handle_ver_find_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let _flags = engine
        .read_rcx()
        .context("failed to read RCX for VerFindFileA")?;
    let file_ptr = engine
        .read_rdx()
        .context("failed to read RDX for VerFindFileA")?;
    let cur_dir_ptr = read_stack_u64(engine, 0x28)?;
    let cur_dir_len_ptr = read_stack_u64(engine, 0x30)?;
    let dest_dir_ptr = read_stack_u64(engine, 0x38)?;
    let dest_dir_len_ptr = read_stack_u64(engine, 0x40)?;
    let file = read_ansi_string_from_cpu(engine, file_ptr, 1024)?;
    let (mut flags, located) = find_file_dirs(state, &file);
    flags |= write_dir_out(engine, cur_dir_ptr, cur_dir_len_ptr, &located)?;
    flags |= write_dir_out(engine, dest_dir_ptr, dest_dir_len_ptr, "")?;
    let return_address = engine
        .return_from_win64_api(u64::from(flags))
        .context("failed to return from VerFindFileA")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(flags),
    })
}

/// `VerInstallFile` core: really copy the source into the destination dir.
fn install_file(
    state: &WinApiState,
    src_dir: &str,
    src_file: &str,
    dest_dir: &str,
    dest_file: &str,
) -> Result<u32> {
    let src_guest = join_guest_path(src_dir, src_file);
    let Some(src_bytes) = read_target_bytes(state, &src_guest) else {
        return Ok(VIF_SRCFILENOTFOUND);
    };
    let dest_guest = join_guest_path(dest_dir, dest_file);
    let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, &dest_guest) else {
        return Ok(VIF_CANNOTCREATE);
    };
    match crate::vfs::create_host_file(&map.host)
        .and_then(|_| std::fs::write(&map.host, &src_bytes))
    {
        Ok(()) => Ok(0),
        Err(_) => Ok(VIF_CANNOTCREATE),
    }
}

/// Handles `VERSION.dll!VerInstallFileW`.
pub fn handle_ver_install_file_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let _flags = engine
        .read_rcx()
        .context("failed to read RCX for VerInstallFileW")?;
    let src_file_ptr = engine
        .read_rdx()
        .context("failed to read RDX for VerInstallFileW")?;
    let dest_file_ptr = engine
        .read_r8()
        .context("failed to read R8 for VerInstallFileW")?;
    let src_dir_ptr = engine
        .read_r9()
        .context("failed to read R9 for VerInstallFileW")?;
    let dest_dir_ptr = read_stack_u64(engine, 0x28)?;
    let tmp_file_ptr = read_stack_u64(engine, 0x38)?;
    let tmp_file_len_ptr = read_stack_u64(engine, 0x40)?;
    let src_file = read_wide_string_from_cpu(engine, src_file_ptr, 1024)?;
    let dest_file = read_wide_string_from_cpu(engine, dest_file_ptr, 1024)?;
    let src_dir = read_wide_string_from_cpu(engine, src_dir_ptr, 1024)?;
    let dest_dir = read_wide_string_from_cpu(engine, dest_dir_ptr, 1024)?;
    let mut flags = install_file(state, &src_dir, &src_file, &dest_dir, &dest_file)?;
    // The temp file is the destination file itself (WIE copies in place —
    // there is no two-phase temp-then-rename dance to report).
    let cap = if tmp_file_len_ptr == 0 {
        0
    } else {
        usize::try_from(u64::from(kernel32::read_guest_u32(
            engine,
            tmp_file_len_ptr,
        )?))
        .context("VerInstallFileW temp length does not fit usize")?
    };
    let needed = dest_file.encode_utf16().count();
    let written = if tmp_file_ptr == 0 {
        0
    } else {
        crate::guest_string::write_utf16_c_string(engine, tmp_file_ptr, cap, &dest_file)?
    };
    if tmp_file_len_ptr != 0 {
        kernel32::write_guest_u32(
            engine,
            tmp_file_len_ptr,
            u32::try_from(needed).unwrap_or(u32::MAX),
        )?;
    }
    if written < needed {
        flags |= VIF_BUFFTOOSMALL;
    }
    let return_address = engine
        .return_from_win64_api(u64::from(flags))
        .context("failed to return from VerInstallFileW")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(flags),
    })
}

/// Handles `VERSION.dll!VerInstallFileA`.
pub fn handle_ver_install_file_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    crate::vfs::enforce_bottle(&state.file_io.volumes)?;
    let _flags = engine
        .read_rcx()
        .context("failed to read RCX for VerInstallFileA")?;
    let src_file_ptr = engine
        .read_rdx()
        .context("failed to read RDX for VerInstallFileA")?;
    let dest_file_ptr = engine
        .read_r8()
        .context("failed to read R8 for VerInstallFileA")?;
    let src_dir_ptr = engine
        .read_r9()
        .context("failed to read R9 for VerInstallFileA")?;
    let dest_dir_ptr = read_stack_u64(engine, 0x28)?;
    let tmp_file_ptr = read_stack_u64(engine, 0x38)?;
    let tmp_file_len_ptr = read_stack_u64(engine, 0x40)?;
    let src_file = read_ansi_string_from_cpu(engine, src_file_ptr, 1024)?;
    let dest_file = read_ansi_string_from_cpu(engine, dest_file_ptr, 1024)?;
    let src_dir = read_ansi_string_from_cpu(engine, src_dir_ptr, 1024)?;
    let dest_dir = read_ansi_string_from_cpu(engine, dest_dir_ptr, 1024)?;
    let mut flags = install_file(state, &src_dir, &src_file, &dest_dir, &dest_file)?;
    let cap = if tmp_file_len_ptr == 0 {
        0
    } else {
        usize::try_from(u64::from(kernel32::read_guest_u32(
            engine,
            tmp_file_len_ptr,
        )?))
        .context("VerInstallFileA temp length does not fit usize")?
    };
    let needed = crate::guest_string::encode_cp1252(&dest_file).len();
    let written = if tmp_file_ptr == 0 {
        0
    } else {
        crate::guest_string::write_ansi_c_string(engine, tmp_file_ptr, cap, &dest_file)?
    };
    if tmp_file_len_ptr != 0 {
        kernel32::write_guest_u32(
            engine,
            tmp_file_len_ptr,
            u32::try_from(needed).unwrap_or(u32::MAX),
        )?;
    }
    if written < needed {
        flags |= VIF_BUFFTOOSMALL;
    }
    let return_address = engine
        .return_from_win64_api(u64::from(flags))
        .context("failed to return from VerInstallFileA")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: u64::from(flags),
    })
}

/// Return 0 with the current last-error already set.
fn return_zero(engine: &mut dyn wie_cpu::CpuEngine, api: &str) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(0)
        .with_context(|| format!("failed to return from {api}"))?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: 0,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::ProcessState;

    #[test]
    fn language_names_cover_the_common_langids() {
        assert_eq!(ver_language_name(0x0409), "English (United States)");
        assert_eq!(ver_language_name(0x0809), "English (United Kingdom)");
        assert_eq!(ver_language_name(0x0407), "German (Germany)");
        assert_eq!(ver_language_name(0x040C), "French (France)");
        assert_eq!(ver_language_name(0x0419), "Russian (Russia)");
        assert_eq!(ver_language_name(0x0411), "Japanese (Japan)");
        // Neutral sublanguage 0 reports the bare primary name.
        assert_eq!(ver_language_name(0x0009), "English");
        assert_eq!(ver_language_name(0x0007), "German");
        // Chinese is script-reported; an unknown primary is honestly flagged.
        assert_eq!(ver_language_name(0x0804), "Chinese (Simplified)");
        assert_eq!(ver_language_name(0x0404), "Chinese (Traditional)");
        assert_eq!(ver_language_name(0x0FFF), "Unknown language (0x0FFF)");
    }

    /// The A-path decode feeds the query walk: CP1252 byte 0xE9 (é) must
    /// reach the walk as the same char the UTF-16 block stores.
    #[test]
    fn a_path_cp1252_decode_feeds_the_query() {
        let path = crate::guest_string::decode_ansi_lossy(b"\\StringFileInfo\\040904b0\\caf\xE9");
        assert_eq!(path, r"\StringFileInfo\040904b0\café");
    }

    #[test]
    fn ver_get_flags_validation() {
        let mut state = winapi_state_for_test();
        assert!(validate_ver_get_flags(0, &mut state));
        assert!(validate_ver_get_flags(FILE_VER_GET_NEUTRAL, &mut state));
        assert!(validate_ver_get_flags(
            FILE_VER_GET_NEUTRAL | FILE_VER_GET_LOCALISED | FILE_VER_GET_PREFETCHED,
            &mut state,
        ));
        // An unknown bit is rejected with ERROR_INVALID_PARAMETER.
        assert!(!validate_ver_get_flags(0x8000_0000, &mut state));
        assert_eq!(state.process.last_error, ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn join_paths() {
        assert_eq!(join_guest_path(r"C:\App", "app.exe"), r"C:\App\app.exe");
        assert_eq!(join_guest_path(r"C:\App", r"D:\x\y.dll"), r"D:\x\y.dll");
        assert_eq!(join_guest_path("", "app.exe"), "app.exe");
        assert_eq!(join_guest_path(r"C:\App", ""), r"C:\App");
    }

    /// Minimal `WinApiState` for the pure-function tests above.
    fn winapi_state_for_test() -> WinApiState {
        WinApiState {
            process: ProcessState {
                last_error: 0,
                ..winapi_state_default().process
            },
            ..winapi_state_default()
        }
    }

    fn winapi_state_default() -> WinApiState {
        use crate::dll_loader;
        use crate::state::{DllStateMap, FileIoState, HeapState, KernelState, ModuleState};
        use crate::sync_obj::SyncState;
        use crate::thread::ThreadState;
        use ahash::HashMap;
        use ahash::HashMapExt;
        use std::sync::{Arc, Mutex};

        WinApiState {
            heap_state: HeapState {
                heap: crate::guest_heap::GuestHeap::new(0x2000, 0x10000),
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: crate::FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: crate::FileHandle::from(0),
                next_resource_handle: crate::ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: crate::vfs::VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: crate::GuestStdinMode::InjectOnly,
                ucrt_files: HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: crate::RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: "app.exe".to_owned(),
                main_module_path: r"C:\app.exe".to_owned(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: Vec::new(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(crate::present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: crate::ModuleHandle::from(dll_loader::REAL_MODULE_HANDLE_BASE),
            },
        }
    }
}
