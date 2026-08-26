//! Minimal `shell32.dll` stubs (folder paths / browse UI / About / launch) for CLI tools.

use crate::guest_memory::{read_u64, write_u32 as write_guest_u32, write_u64 as write_guest_u64};
use crate::guest_string::{
    read_arg_string, read_utf16_lossy, write_ansi_c_string, write_utf16_c_string,
};
use crate::state::{MessageBoxRequest, PendingNativeMessageBox, WinApiControlSignal, WindowFlags};
use crate::user32::{
    FAKE_ICON_HANDLE, IDOK, ModalResult, NativePanelCtx, NativePanelKind, find_window_mut,
};
use crate::{HandlerContext, WinApiHandlerResult, WinApiState};
use anyhow::{Context, Result};

/// `S_OK` / success for SH* path APIs that return HRESULT.
const S_OK: u64 = 0;
/// `E_FAIL` for optional shell UI we do not implement.
const E_FAIL: u64 = 0x8000_4005;
/// `ShellExecuteW` success value: the contract is "> 32 means success" and
/// every `SE_ERR_*` failure code sits at 2..32, so 33 is the conventional
/// success answer.
const SHELL_EXECUTE_SUCCESS: u64 = 33;
/// `SE_ERR_FNF` — the file was not found / could not be launched.
const SE_ERR_FNF: u64 = 2;
/// `SE_ERR_NOASSOC` — no application is associated with the operation.
const SE_ERR_NOASSOC: u64 = 31;

/// `SHAddToRecentDocs` `uFlags` values (shlobj_core.h): the pointer's shape.
const SHARD_PIDL: u64 = 0x1;
const SHARD_PATHA: u64 = 0x2;
const SHARD_PATHW: u64 = 0x3;

fn finish(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("shell32 return")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// `SHAddToRecentDocs(uFlags, lpName)` — records a file in the shell's
/// recent-documents list. WIE has no recent-docs UI, so the honest
/// implementation validates the arguments and no-ops; the call must not stop
/// the session (RNotepad calls it after every open/save). Returns void.
fn handle_sh_add_to_recent_docs(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let flags = engine
        .read_rcx()
        .context("failed to read RCX for SHAddToRecentDocs")?;
    let name_va = engine
        .read_rdx()
        .context("failed to read RDX for SHAddToRecentDocs")?;
    match flags {
        // A PIDL (item id list) — unparsed; nothing to record.
        SHARD_PIDL => {}
        // ANSI path.
        SHARD_PATHA => {
            if let Ok(name) = crate::guest_string::read_arg_string(engine, name_va, false) {
                tracing::debug!(recent_doc = %name, "SHAddToRecentDocs");
            }
        }
        // Wide path (RNotepad's call).
        SHARD_PATHW => {
            if let Ok(name) = crate::guest_string::read_arg_string(engine, name_va, true) {
                tracing::debug!(recent_doc = %name, "SHAddToRecentDocs");
            }
        }
        _ => {}
    }
    finish(engine, 0)
}

/// Soft dispatch for `shell32.dll`.
pub fn dispatch_shell32(
    ctx: &mut HandlerContext<'_>,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "shgetfolderpathw" => Ok(Some(handle_sh_get_folder_path_w(ctx)?)),
        "shgetpathfromidlistw" => Ok(Some(handle_sh_get_path_from_id_list_w(ctx)?)),
        "shbrowseforfolderw" => Ok(Some(handle_sh_browse_for_folder_w(ctx)?)),
        "commandlinetoargvw" => Ok(Some(handle_command_line_to_argv_w(ctx)?)),
        "shaddtorecentdocs" => Ok(Some(handle_sh_add_to_recent_docs(ctx)?)),
        "shgetfileinfoa" => Ok(Some(handle_sh_get_file_info(ctx, false)?)),
        "shgetfileinfow" => Ok(Some(handle_sh_get_file_info(ctx, true)?)),
        "shgetspecialfolderpathw" => Ok(Some(handle_sh_get_special_folder_path_w(ctx)?)),
        "shellexecuteexw" => Ok(Some(handle_sh_execute_ex_w(ctx)?)),
        // Phase-3 stub wave: the ANSI variants share the W logic.
        "shellexecutea" => Ok(Some(handle_shell_execute_a(ctx)?)),
        "shellexecuteexa" => Ok(Some(handle_sh_execute_ex_a(ctx)?)),
        _ => Ok(None),
    }
}

/// Map a `CSIDL` folder id to the synthetic bottle path WIE exposes.
///
/// Every returned path points INTO the seeded default skeleton
/// ([`crate::vfs::BOTTLE_SKELETON_DIRS`]), so the folder exists on the host
/// once the bottle is materialized — except `CSIDL_PROGRAMS`, which the
/// fCreate path of `SHGetSpecialFolderPathW` materializes on demand.
fn csidl_to_guest_path(csidl: u64) -> &'static str {
    match csidl {
        0x00 => r"C:\Users\WIE\Desktop", // CSIDL_DESKTOP
        0x02 => r"C:\Users\WIE\AppData\Roaming\Microsoft\Windows\Start Menu\Programs", // CSIDL_PROGRAMS
        0x05 => r"C:\Users\WIE\Documents", // CSIDL_PERSONAL / My Documents
        0x1a => r"C:\Users\WIE\AppData\Roaming", // CSIDL_APPDATA
        0x1c => r"C:\Users\WIE\AppData\Local", // CSIDL_LOCAL_APPDATA
        0x23 => r"C:\ProgramData",         // CSIDL_COMMON_APPDATA
        0x24 => r"C:\Windows",             // CSIDL_WINDOWS
        0x25 => r"C:\Windows\System32",    // CSIDL_SYSTEM
        0x26 => r"C:\Program Files",       // CSIDL_PROGRAM_FILES
        0x2a => r"C:\Program Files\Common Files", // CSIDL_PROGRAM_FILES_COMMON
        // CSIDL_PROFILE (0x28) and unknown → user home.
        _ => r"C:\Users\WIE",
    }
}

/// `HRESULT SHGetFolderPathW(hwnd, csidl, hToken, dwFlags, pszPath)`
///
/// Fills a fixed bottle-friendly path under `C:\Users\WIE\…` style so tools
/// that only need a writable home directory keep going. Every returned path
/// points INTO the seeded default skeleton ([`crate::vfs::BOTTLE_SKELETON_DIRS`]),
/// so the folder exists on the host once the bottle is materialized.
fn handle_sh_get_folder_path_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwnd = engine.read_rcx()?;
    let csidl = engine.read_rdx()? & 0xffff_ffff;
    let _token = engine.read_r8()?;
    let _flags = engine.read_r9()?;
    let mut path_ptr_bytes = [0_u8; 8];
    let rsp = engine.read_rsp()?;
    engine.mem_read(rsp.wrapping_add(0x28), &mut path_ptr_bytes)?;
    let path_va = u64::from_le_bytes(path_ptr_bytes);

    // Common CSIDL values → synthetic bottle paths (MAX_PATH buffer expected).
    // `csidl` already masked to low 32 bits (u64).
    let path = csidl_to_guest_path(csidl);

    if path_va != 0 {
        write_utf16_c_string(engine, path_va, 260, path)?;
    }
    finish(engine, S_OK)
}

/// `BOOL SHGetSpecialFolderPathW(hwnd, pszPath, csidl, fCreate)`
///
/// Same CSIDL → bottle-path map as [`handle_sh_get_folder_path_w`], with the
/// `fCreate` flag honored: when set, the mapped host directory is materialized
/// under the bottle (a no-op for the seeded skeleton folders — and what makes
/// `CSIDL_PROGRAMS`, which the skeleton lacks, resolvable). Returns TRUE.
fn handle_sh_get_special_folder_path_w(
    ctx: &mut HandlerContext<'_>,
) -> Result<WinApiHandlerResult> {
    let (path_va, csidl, f_create) = {
        let engine = &mut *ctx.engine;
        let _hwnd = engine.read_rcx()?;
        let path_va = engine.read_rdx()?;
        let csidl = engine.read_r8()? & 0xffff_ffff;
        let f_create = engine.read_r9()?;
        (path_va, csidl, f_create)
    };
    let path = csidl_to_guest_path(csidl);
    if f_create != 0
        && let Some(map) = crate::vfs::guest_path_to_host(&ctx.state.file_io.volumes, path)
    {
        let _created = std::fs::create_dir_all(map.host);
    }
    if path_va != 0 {
        write_utf16_c_string(ctx.engine, path_va, 260, path)?;
    }
    finish(ctx.engine, 1)
}

/// `SHFILEINFO` fixed prefix size (hIcon + iIcon + dwAttributes).
const SHFILEINFO_PREFIX_SIZE: u64 = 16;
/// `sizeof(SHFILEINFOW)`: HICON hIcon @0, int iIcon @8, DWORD dwAttributes
/// @12, WCHAR szDisplayName[260] @16, WCHAR szTypeName[80] @536 (offsets
/// verified against the Windows SDK headers).
const SHFILEINFO_W_SIZE: u64 = 696;
/// `sizeof(SHFILEINFOA)`: same prefix; CHAR szDisplayName[260] @16,
/// CHAR szTypeName[80] @276.
const SHFILEINFO_A_SIZE: u64 = 356;

/// `DWORD_PTR SHGetFileInfoA/W(pszPath, dwFileAttributes, psfi, cbFileInfo, uFlags)`
///
/// KISS shell info: fills the SHFILEINFO prefix with a fake icon handle,
/// `iIcon = 0`, and `dwAttributes = FILE_ATTRIBUTE_NORMAL` when `pszPath`
/// names an existing guest file; the display name (file-name portion) is
/// written when `cbFileInfo` covers the full struct. Returns 1 (non-zero) —
/// real Windows returns 0 only when the struct is too small or NULL.
fn handle_sh_get_file_info(
    ctx: &mut HandlerContext<'_>,
    wide: bool,
) -> Result<WinApiHandlerResult> {
    let (path, psfi, cb_file_info) = {
        let engine = &mut *ctx.engine;
        let path_va = engine.read_rcx()?;
        let _dw_file_attributes = engine.read_rdx()?;
        let psfi = engine.read_r8()?;
        let cb_file_info = engine.read_r9()?;
        let rsp = engine.read_rsp()?;
        let _u_flags = read_u64(engine, rsp.wrapping_add(0x28))?;
        let path = read_arg_string(engine, path_va, wide)?;
        (path, psfi, cb_file_info)
    };
    if psfi == 0 || cb_file_info < SHFILEINFO_PREFIX_SIZE {
        return finish(ctx.engine, 0);
    }
    let exists = crate::vfs::guest_path_to_host(&ctx.state.file_io.volumes, &path)
        .is_some_and(|map| map.host.is_file());
    write_guest_u64(ctx.engine, psfi.wrapping_add(0), FAKE_ICON_HANDLE)?;
    write_guest_u32(ctx.engine, psfi.wrapping_add(8), 0)?; // iIcon
    write_guest_u32(
        // dwAttributes
        ctx.engine,
        psfi.wrapping_add(12),
        if exists {
            crate::vfs::FILE_ATTRIBUTE_NORMAL
        } else {
            0
        },
    )?;
    let full_size = if wide {
        SHFILEINFO_W_SIZE
    } else {
        SHFILEINFO_A_SIZE
    };
    if cb_file_info >= full_size {
        let display_name = crate::vfs::guest_basename(&path);
        if wide {
            write_utf16_c_string(ctx.engine, psfi.wrapping_add(16), 260, display_name)?;
        } else {
            write_ansi_c_string(ctx.engine, psfi.wrapping_add(16), 260, display_name)?;
        }
    }
    finish(ctx.engine, 1)
}

/// `LPWSTR* CommandLineToArgvW(LPCWSTR lpCmdLine, int* pNumArgs)`.
///
/// Parses a command-line string into an argv-style array, allocating the result
/// and the argument strings from the process heap.
fn handle_command_line_to_argv_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let cmd_line_va = engine.read_rcx()?;
    let num_args_va = engine.read_rdx()?;
    if cmd_line_va == 0 || num_args_va == 0 {
        return finish(engine, 0); // NULL → failure
    }
    // Read the command line.
    let mut units = Vec::new();
    let mut i = 0_u64;
    loop {
        let mut b = [0_u8; 2];
        engine.mem_read(cmd_line_va.wrapping_add(i.wrapping_mul(2)), &mut b)?;
        let w = u16::from_le_bytes(b);
        if w == 0 {
            break;
        }
        units.push(w);
        i = i.saturating_add(1);
        if i > 8192 {
            break;
        }
    }
    // Parse into arguments (simple whitespace splitting).
    let cmd = String::from_utf16_lossy(&units);
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in cmd.chars() {
        if ch == '"' {
            in_quote = !in_quote;
        } else if ch.is_whitespace() && !in_quote {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    // Allocate argv array: one pointer per arg, plus NULL terminator.
    let argv_bytes = args.len().saturating_add(1).checked_mul(8).unwrap_or(8);
    let argv_va = state
        .heap_state
        .heap
        .alloc_coherent(engine, u64::try_from(argv_bytes).unwrap_or(64));
    if argv_va == 0 {
        return finish(engine, 0);
    }
    let mut offset = 0_u64;
    for arg in &args {
        let units: Vec<u16> = arg.encode_utf16().collect();
        let bstr = alloc_shell_bstr(engine, state, &units)?;
        if bstr == 0 {
            return finish(engine, 0);
        }
        engine.mem_write(argv_va.wrapping_add(offset), &bstr.to_le_bytes())?;
        offset = offset.saturating_add(8);
    }
    // NULL terminator.
    engine.mem_write(argv_va.wrapping_add(offset), &[0_u8; 8])?;
    // Write argc.
    let argc_u32 = u32::try_from(args.len()).unwrap_or(0);
    drop(write_guest_u32(engine, num_args_va, argc_u32));
    finish(engine, argv_va)
}

/// Allocate a shell-style BSTR from the process heap.
fn alloc_shell_bstr(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    units: &[u16],
) -> Result<u64> {
    let byte_len = u32::try_from(units.len().saturating_mul(2)).unwrap_or(0);
    let total = 4_u64
        .saturating_add(u64::from(byte_len))
        .saturating_add(2)
        .saturating_add(8);
    let raw = state.heap_state.heap.alloc_coherent(engine, total);
    if raw == 0 {
        return Ok(0);
    }
    let data = raw.wrapping_add(4);
    engine.mem_write(data.wrapping_sub(4), &byte_len.to_le_bytes())?;
    let mut bytes = Vec::with_capacity(units.len().saturating_mul(2).saturating_add(2));
    for u in units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    engine.mem_write(data, &bytes)?;
    Ok(data)
}

/// `BOOL SHGetPathFromIDListW(pidl, pszPath)` — no real PIDLs; fail cleanly.
fn handle_sh_get_path_from_id_list_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _pidl = engine.read_rcx()?;
    let path_va = engine.read_rdx()?;
    if path_va != 0 {
        write_utf16_c_string(engine, path_va, 260, "")?;
    }
    finish(engine, 0) // FALSE
}

/// `PIDLIST_ABSOLUTE SHBrowseForFolderW(lpbi)` — no UI; return NULL.
fn handle_sh_browse_for_folder_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _lpbi = engine.read_rcx()?;
    let _ = E_FAIL;
    finish(engine, 0)
}

/// `void DragAcceptFiles(HWND hWnd, BOOL fAccept)`.
///
/// Only the registration is handled here: mark the window as a drop target on
/// its window record and return. The drop path itself (`WM_DROPFILES` +
/// `DragQueryFileA/W`) is plan Task 5.2. Returns non-zero like the other void
/// handlers in this module.
pub fn handle_drag_accept_files(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hwnd = engine
        .read_rcx()
        .context("failed to read RCX for DragAcceptFiles")?;
    let f_accept = engine
        .read_rdx()
        .context("failed to read RDX for DragAcceptFiles")?;
    if let Some(window) = find_window_mut(state, hwnd) {
        if f_accept != 0 {
            window.flags.insert(WindowFlags::DROP_ACCEPTED);
        } else {
            window.flags.remove(WindowFlags::DROP_ACCEPTED);
        }
    }
    finish(engine, 1)
}

/// `BOOL ShellAboutW(HWND hwnd, LPCWSTR szAppName, LPCWSTR szOtherStuff, HICON hIcon)`.
///
/// Shows an About box. When the host MessageBox bridge is registered (the GUI
/// presenter's rfd alert), the message routes through it with caption =
/// `szAppName` and text = `szOtherStuff` — the same bridge `MessageBoxW` uses
/// (`user32::misc`), through the same lock-free two-entry flow (see
/// [`PendingNativeMessageBox`]). Headless runs echo to the host console.
/// Always returns TRUE (real Windows shows the box and returns TRUE).
pub fn handle_shell_about_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let (app_name, other_stuff) = {
        let engine = &mut *ctx.engine;
        let _hwnd = engine
            .read_rcx()
            .context("failed to read RCX for ShellAboutW")?;
        let app_name_va = engine
            .read_rdx()
            .context("failed to read RDX for ShellAboutW")?;
        let other_stuff_va = engine
            .read_r8()
            .context("failed to read R8 for ShellAboutW")?;
        let _h_icon = engine
            .read_r9()
            .context("failed to read R9 for ShellAboutW")?;
        // read_utf16_lossy treats NULL as an empty string, so either argument
        // may be omitted (real Windows falls back to module/version strings).
        let app_name = read_utf16_lossy(engine, app_name_va, 256)?;
        let other_stuff = read_utf16_lossy(engine, other_stuff_va, 1024)?;
        (app_name, other_stuff)
    };

    let text = if other_stuff.is_empty() {
        "This application is running under WIE.".to_owned()
    } else {
        other_stuff
    };

    tracing::info!(target: "wiegui", app_name = %app_name, "ShellAboutW");

    // Re-entry: the runtime ran the bridge WITHOUT the shared state lock (the
    // winit event loop needs that lock to service frame events while the
    // alert is up) and recorded the pick — the fix-27 MessageBox two-entry
    // flow. ShellAboutW is an MB_OK alert, so the pick is IDOK — which is
    // also TRUE, the documented return. Close the modal frame the first entry
    // opened (depth down, owner restored) before returning.
    if let Some(pending) = ctx.state.window_state().pending_native_message_box.take() {
        let win32_id = pending
            .pick
            .and_then(|id| u64::try_from(id).ok())
            .unwrap_or(IDOK);
        let result = if win32_id == IDOK {
            ModalResult::Ok(win32_id)
        } else {
            ModalResult::Cancel
        };
        let signal = {
            let mut native =
                NativePanelCtx::new(ctx.state, ctx.engine, NativePanelKind::ShellAbout);
            native.finish(pending.frame, result)?
        };
        if let Some(signal) = signal {
            return Err(signal.into());
        }
        return finish(ctx.engine, win32_id);
    }

    if ctx
        .state
        .try_present()
        .is_some_and(|present| present.message_box_bridge.is_some())
    {
        // First entry: record the write-back slot and hand the request to the
        // runtime — it drops the shared state lock, runs the bridge on this
        // guest thread, and the engine's re-execution of the fake API
        // re-enters this handler. Reuses MessageBoxRequest as-is: an About box
        // is exactly caption + text + MB_OK. The alert is a modal session
        // (same frame protocol as MessageBoxA/W).
        let frame = {
            let mut native =
                NativePanelCtx::new(ctx.state, ctx.engine, NativePanelKind::ShellAbout);
            native.open()?
        };
        ctx.state.window_state().pending_native_message_box = Some(PendingNativeMessageBox {
            pick: None,
            frame: Some(frame),
        });
        return Err(WinApiControlSignal::MessageBoxBridgeRequested {
            request: MessageBoxRequest {
                caption: app_name,
                text,
                message_box_type: 0, // MB_OK — a single OK button, like the real About dialog.
            },
        }
        .into());
    }

    // Headless/trace: echo to the host console and auto-answer TRUE (IDOK) so
    // the guest never hangs on a missing host.
    tracing::error!(app_name = %app_name, text = %text, "[ShellAboutW]");
    finish(ctx.engine, 1)
}

/// Decode one guest string for the `ShellExecute*` bodies.
type ReadShellString = fn(&mut dyn wie_cpu::CpuEngine, u64, usize) -> anyhow::Result<String>;

/// Shared `ShellExecuteW/A` body (`decode` picks UTF-16 vs ANSI, `label`
/// names the variant for logs and error contexts).
///
/// Implements the one operation notepad needs — "open" (or NULL) on the
/// running PE (File → New Window). The guest path resolves to a host path: an
/// exact `host_file_mounts` entry, the bottle/D: volume mapping, or — for the
/// main module's own path — a temp-file spill of the loaded image bytes (so
/// relaunch works for PEs run directly from outside the bottle). A new WIE
/// instance of the host `wie-cli` binary is spawned detached (`spawn`, never
/// `wait`): the guest thread does not block on the child. Returns 33 (> 32)
/// on a successful spawn, `SE_ERR_FNF` (2) when no host file is reachable,
/// and `SE_ERR_NOASSOC` (31) for other verbs.
///
/// Limitation (guest→host process spawn semantics): the child is a full WIE
/// host process, not a Win32 process, so cross-process Windows semantics
/// (waiting on the returned handle, exit codes, argv marshalling) do not
/// carry over — this is a fire-and-forget relaunch.
fn shell_execute(
    ctx: &mut HandlerContext<'_>,
    decode: ReadShellString,
    label: &str,
) -> Result<WinApiHandlerResult> {
    let _hwnd = ctx
        .engine
        .read_rcx()
        .with_context(|| format!("failed to read RCX for {label}"))?;
    let operation_va = ctx
        .engine
        .read_rdx()
        .with_context(|| format!("failed to read RDX for {label}"))?;
    let file_va = ctx
        .engine
        .read_r8()
        .with_context(|| format!("failed to read R8 for {label}"))?;
    let _parameters_va = ctx
        .engine
        .read_r9()
        .with_context(|| format!("failed to read R9 for {label}"))?;
    // The two stack arguments (lpDirectory, nShowCmd) are consumed and
    // ignored — every variant shares this ABI shape.
    let _directory_va = crate::kernel32::read_stack_u64(ctx.engine, 0x28)?;
    let _show_cmd = crate::kernel32::read_stack_u64(ctx.engine, 0x30)?;
    let operation = decode(ctx.engine, operation_va, 64)?;
    let file = decode(ctx.engine, file_va, 1024)?;

    let return_value = if file.is_empty() {
        SE_ERR_FNF
    } else if operation.is_empty() || operation.eq_ignore_ascii_case("open") {
        shell_execute_open(ctx.state, &file)
    } else {
        // Only "open" on a PE has a host analogue; every other verb has no
        // guest→host mapping.
        SE_ERR_NOASSOC
    };

    tracing::info!(
        target: "wiegui",
        operation = %operation,
        file = %file,
        return_value,
        "{label}"
    );
    finish(ctx.engine, return_value)
}

/// `HINSTANCE ShellExecuteW(HWND hwnd, LPCWSTR lpOperation, LPCWSTR lpFile,
/// LPCWSTR lpParameters, LPCWSTR lpDirectory, INT nShowCmd)`.
pub fn handle_shell_execute_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    shell_execute(ctx, read_utf16_lossy, "ShellExecuteW")
}

/// `HINSTANCE ShellExecuteA(HWND, LPCSTR, LPCSTR, LPCSTR, LPCSTR, INT)` — the
/// ANSI variant of [`handle_shell_execute_w`] (same detached-run semantics).
fn handle_shell_execute_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    shell_execute(ctx, crate::guest_string::read_ansi_lossy, "ShellExecuteA")
}

/// `BOOL ShellExecuteExW/A(pExecInfo)` shared body — mirrors
/// [`shell_execute`] through the `SHELLEXECUTEINFO*` indirection (`decode`
/// picks UTF-16 vs ANSI; the A and W struct layouts match).
///
/// Reads the struct pointed to by RCX (Win64 SDK layout: `lpVerb` @0x10,
/// `lpFile` @0x18, `hInstApp` @0x38). Verb NULL/"open" on an existing guest
/// path spawns a fresh WIE instance (fire-and-forget, same as
/// [`shell_execute`]) and returns TRUE; every other verb — or an empty file —
/// writes the `SE_ERR_*` code into `hInstApp` and returns FALSE.
fn sh_execute_ex(
    ctx: &mut HandlerContext<'_>,
    decode: ReadShellString,
) -> Result<WinApiHandlerResult> {
    let (exec_info_va, verb, file) = {
        let engine = &mut *ctx.engine;
        let exec_info_va = engine.read_rcx()?;
        if exec_info_va == 0 {
            return finish(engine, 0);
        }
        let verb_va = read_u64(engine, exec_info_va.wrapping_add(0x10))?;
        let file_va = read_u64(engine, exec_info_va.wrapping_add(0x18))?;
        let verb = decode(engine, verb_va, 64)?;
        let file = decode(engine, file_va, 1024)?;
        (exec_info_va, verb, file)
    };
    let (h_inst_app, ok) = if file.is_empty() {
        (SE_ERR_FNF, false)
    } else if verb.is_empty() || verb.eq_ignore_ascii_case("open") {
        let launched = shell_execute_open(ctx.state, &file);
        (launched, launched > 32)
    } else {
        (SE_ERR_NOASSOC, false)
    };
    write_guest_u64(ctx.engine, exec_info_va.wrapping_add(0x38), h_inst_app)?;
    finish(ctx.engine, if ok { 1 } else { 0 })
}

/// `BOOL ShellExecuteExW(pExecInfo)`.
fn handle_sh_execute_ex_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    sh_execute_ex(ctx, read_utf16_lossy)
}

/// Spawn a new WIE instance of the guest exe at `guest_path` (`wie-cli run`).
///
/// Detached: the child is spawned and never waited on, so the guest thread
/// returns immediately (ShellExecute semantics).
fn shell_execute_open(state: &WinApiState, guest_path: &str) -> u64 {
    let Some(host_path) = resolve_launch_host_path(state, guest_path) else {
        return SE_ERR_FNF;
    };
    let Some(host_exe) = std::env::current_exe().ok() else {
        return SE_ERR_FNF;
    };
    if std::process::Command::new(host_exe)
        .arg("run")
        .arg("--gui")
        .arg(&host_path)
        .spawn()
        .is_ok()
    {
        SHELL_EXECUTE_SUCCESS
    } else {
        SE_ERR_FNF
    }
}

/// Resolve a guest path to a host path a fresh `wie-cli run` can load.
///
/// Precedence: an exact `host_file_mounts` entry, the bottle/D: volume
/// mapping, then — for the main module's own path — a temp-file spill of the
/// loaded image bytes.
fn resolve_launch_host_path(state: &WinApiState, guest_path: &str) -> Option<std::path::PathBuf> {
    if let Some(mount) = state
        .file_io
        .host_file_mounts
        .iter()
        .find(|mount| crate::vfs::paths_equal_ci(guest_path, &mount.guest_path))
    {
        return Some(mount.host_path.clone());
    }
    if let Some(map) = crate::vfs::guest_path_to_host(&state.file_io.volumes, guest_path)
        && map.host.is_file()
    {
        return Some(map.host);
    }
    if crate::kernel32::is_main_module_path(state, guest_path) {
        return spill_main_module(state);
    }
    None
}

/// Write the loaded main-module bytes to a temp file and return its path.
///
/// The main module lives in guest memory (the VFS serves its bytes from
/// `executable_file_bytes`), so relaunching it needs a real host file; the
/// temp spill is how "New Window" works for PEs run from outside the bottle.
fn spill_main_module(state: &WinApiState) -> Option<std::path::PathBuf> {
    let file_name = if state.process.main_module_file_name.is_empty() {
        "app.exe".to_owned()
    } else {
        state.process.main_module_file_name.clone()
    };
    let path =
        std::env::temp_dir().join(format!("wie-relaunch-{}-{file_name}", std::process::id()));
    std::fs::write(&path, state.file_io.executable_file_bytes.as_ref()).ok()?;
    Some(path)
}

/// `BOOL ShellExecuteExA(pExecInfo)` — the ANSI variant of
/// [`handle_sh_execute_ex_w`] (the `SHELLEXECUTEINFOA` layout matches the W
/// struct; only the pointed-to strings are ANSI).
fn handle_sh_execute_ex_a(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    sh_execute_ex(ctx, crate::guest_string::read_ansi_lossy)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::sync_obj::SyncState;
    use crate::vfs::VolumeConfig;
    use crate::{
        FileHandle, FindFileHandle, GuestHeap, GuestStdinMode, HeapState, KernelState,
        ModuleHandle, ModuleState, ProcessState, RegistryKeyHandle, ResourceHandle, ThreadState,
        WinApiEnvironment,
    };
    use ahash::HashMap;
    use ahash::HashMapExt;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use wie_cpu::{CpuEngine, IcedCpu, RwxPerms};

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    const STACK_TOP: u64 = 0x100_FF00;

    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, STACK_SIZE, RwxPerms::ALL)
            .expect("map test stack");
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn test_state() -> WinApiState {
        let mut heap = GuestHeap::new(0x2000, 0x10000);
        heap.attach_guest_control(0x2000);
        WinApiState {
            display: crate::DisplayMetrics::default(),
            heap_state: HeapState {
                heap,
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: crate::FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: FileHandle::from(0),
                next_resource_handle: ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: GuestStdinMode::InjectOnly,
                ucrt_files: HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: HashMap::new(),
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: crate::DEFAULT_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
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
            dll_states: crate::DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(crate::present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: ModuleHandle::from(crate::dll_loader::REAL_MODULE_HANDLE_BASE),
            },
        }
    }

    fn test_environment() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 1,
        }
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        cpu.write_rsp(STACK_TOP).ok();
    }

    fn write_wide(engine: &mut IcedCpu, va: u64, s: &str) {
        let mut bytes = Vec::new();
        for unit in s.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        engine.mem_write(va, &bytes).expect("write wide string");
    }

    /// Drive ShellAboutW through the two-entry bridge flow the runtime
    /// performs between entries (the runtime itself is not involved in unit
    /// tests): first entry → [`WinApiControlSignal::MessageBoxBridgeRequested`],
    /// run the bridge without the shared lock, restore it, record the pick,
    /// re-enter the handler for the return value.
    fn dispatch_shell_about_with_bridge(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
    ) -> anyhow::Result<WinApiHandlerResult> {
        let first =
            handle_shell_about_w(&mut HandlerContext::new(engine, test_environment(), state))
                .expect_err("the first entry parks the guest for the host alert");
        let signal = first
            .downcast_ref::<WinApiControlSignal>()
            .expect("a control signal");
        let WinApiControlSignal::MessageBoxBridgeRequested { request } = signal else {
            panic!("expected a message-box bridge request");
        };
        // What the runtime does between the two entries: take the bridge out,
        // run it (no shared lock), restore it, record the chosen id.
        let bridge = state
            .present()
            .message_box_bridge
            .take()
            .expect("bridge registered");
        let picked = bridge(&request.caption, &request.text, request.message_box_type);
        state.present().message_box_bridge = Some(bridge);
        state
            .window_state()
            .pending_native_message_box
            .as_mut()
            .expect("pending session recorded")
            .pick = Some(picked);
        handle_shell_about_w(&mut HandlerContext::new(engine, test_environment(), state))
    }

    #[test]
    fn shell_about_w_routes_through_message_box_bridge() {
        let mut engine = test_engine();
        let mut state = test_state();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        state.present().message_box_bridge = Some(Box::new(move |caption, text, mb_type| {
            captured_clone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((caption.to_owned(), text.to_owned(), mb_type));
            1 // IDOK
        }));

        write_wide(&mut engine, 0x3000, "Notepad");
        write_wide(&mut engine, 0x4000, "Notepad Authors");
        write_regs(&mut engine, 0, 0x3000, 0x4000, 0);
        let result = dispatch_shell_about_with_bridge(&mut engine, &mut state)
            .expect("ShellAboutW should dispatch");
        assert_eq!(result.return_value, 1, "ShellAboutW returns TRUE");

        let calls = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(calls.len(), 1, "bridge called once");
        let (caption, text, mb_type) = calls.first().expect("one call");
        assert_eq!(caption, "Notepad");
        assert_eq!(text, "Notepad Authors");
        assert_eq!(*mb_type, 0, "MB_OK");
    }

    #[test]
    fn shell_about_w_falls_back_for_empty_other_stuff() {
        let mut engine = test_engine();
        let mut state = test_state();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        state.present().message_box_bridge = Some(Box::new(move |_caption, text, _mb_type| {
            captured_clone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(text.to_owned());
            1
        }));

        // NULL otherStuff → the fallback text keeps the box non-empty.
        write_wide(&mut engine, 0x3000, "Notepad");
        write_regs(&mut engine, 0, 0x3000, 0, 0);
        let result = dispatch_shell_about_with_bridge(&mut engine, &mut state)
            .expect("ShellAboutW should dispatch");
        assert_eq!(result.return_value, 1);
        let calls = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            calls.first().is_some_and(|text| !text.is_empty()),
            "fallback text must be non-empty"
        );
    }

    #[test]
    fn shell_execute_w_other_verb_returns_noassoc() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_wide(&mut engine, 0x3000, "edit");
        write_wide(&mut engine, 0x4000, r"C:\App\notepad.exe");
        write_regs(&mut engine, 0, 0x3000, 0x4000, 0);
        let result = handle_shell_execute_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("ShellExecuteW should succeed");
        assert_eq!(result.return_value, SE_ERR_NOASSOC);
    }

    #[test]
    fn shell_execute_w_empty_file_returns_fnf() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_wide(&mut engine, 0x3000, "open");
        write_wide(&mut engine, 0x4000, "");
        write_regs(&mut engine, 0, 0x3000, 0x4000, 0);
        let result = handle_shell_execute_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("ShellExecuteW should succeed");
        assert_eq!(result.return_value, SE_ERR_FNF);
    }

    #[test]
    fn resolve_launch_host_path_prefers_exact_mount() {
        let mut state = test_state();
        let host = std::env::temp_dir().join("wie-shell-execute-mounted.exe");
        std::fs::write(&host, b"MZ").expect("write temp exe");
        state.file_io.host_file_mounts.push(crate::HostFileMount {
            guest_path: r"C:\App\notepad.exe".to_owned(),
            host_path: host.clone(),
        });

        let resolved = resolve_launch_host_path(&state, r"C:\App\notepad.exe")
            .expect("mount resolves to host path");
        assert_eq!(resolved, host);
        let _cleanup = std::fs::remove_file(&host);
    }

    #[test]
    fn resolve_launch_host_path_uses_bottle_mapping() {
        let mut state = test_state();
        let bottle = std::env::temp_dir().join("wie-shell-execute-bottle");
        let host = bottle.join("drive_c").join("App").join("tool.exe");
        std::fs::create_dir_all(host.parent().expect("parent dir")).expect("create dirs");
        std::fs::write(&host, b"MZ").expect("write temp exe");
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(bottle.clone()),
            drive_d_root: None,
        };

        let resolved =
            resolve_launch_host_path(&state, r"C:\App\tool.exe").expect("bottle mapping resolves");
        assert_eq!(resolved, host);
        let _cleanup = std::fs::remove_dir_all(&bottle);
    }

    /// The FS-policy bottle copy (install-style `C:\Program Files\{name}\…` →
    /// `{root}/drive_c/Program Files/{name}/…`): when the main module lives in
    /// the bottle, its relaunch resolves through the volume mapping — never
    /// through the temp-file spill.
    #[test]
    fn resolve_launch_host_path_prefers_bottle_copy_over_spill() {
        let mut state = test_state();
        let bottle = std::env::temp_dir().join("wie-shell-execute-copy");
        let copy = bottle.join("drive_c").join("notepad.exe");
        std::fs::create_dir_all(copy.parent().expect("parent dir")).expect("create dirs");
        std::fs::write(&copy, b"MZ").expect("write bottle copy");
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(bottle.clone()),
            drive_d_root: None,
        };
        // The guest relaunches its own module path (the identity label).
        state.process.main_module_file_name = "notepad.exe".to_owned();
        state.process.main_module_path = r"C:\notepad.exe".to_owned();
        state.file_io.executable_file_bytes = Arc::new(b"MZ".to_vec());

        let resolved = resolve_launch_host_path(&state, r"C:\notepad.exe")
            .expect("main module resolves via the bottle mapping");
        assert_eq!(resolved, copy, "the in-bottle copy wins, no temp spill");
        let _cleanup = std::fs::remove_dir_all(&bottle);
    }

    #[test]
    fn resolve_launch_host_path_spills_main_module() {
        let mut state = test_state();
        state.process.main_module_file_name = "notepad.exe".to_owned();
        state.process.main_module_path = r"C:\App\notepad.exe".to_owned();
        state.file_io.executable_file_size = 2;
        state.file_io.executable_file_bytes = Arc::new(b"MZ".to_vec());

        let resolved = resolve_launch_host_path(&state, r"C:\App\notepad.exe")
            .expect("main module spills to a temp file");
        assert!(resolved.is_file(), "spill file must exist on disk");
        assert_eq!(
            std::fs::read(&resolved).expect("read spill"),
            b"MZ",
            "spill carries the loaded image bytes"
        );
        let _cleanup = std::fs::remove_file(&resolved);
    }

    #[test]
    fn resolve_launch_host_path_unknown_path_is_none() {
        let state = test_state();
        assert!(resolve_launch_host_path(&state, r"C:\does\not\exist.exe").is_none());
    }

    /// A `WinApiState` with a temp override bottle (unique per test thread —
    /// `test_state`'s default volumes would hit the global app-data bottle).
    fn test_bottle_state(tag: &str) -> (WinApiState, PathBuf) {
        let mut state = test_state();
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let bottle =
            std::env::temp_dir().join(format!("wie-shell32-{tag}-{}-{thread}", std::process::id()));
        std::fs::create_dir_all(bottle.join("drive_c/App")).expect("mkdir bottle App");
        state.file_io.volumes = VolumeConfig {
            bottle_root: Some(bottle.clone()),
            drive_d_root: None,
        };
        (state, bottle)
    }

    fn write_ansi(engine: &mut IcedCpu, va: u64, s: &str) {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        engine.mem_write(va, &bytes).expect("write ansi string");
    }

    fn read_guest_u64(engine: &mut IcedCpu, addr: u64) -> u64 {
        let mut bytes = [0_u8; 8];
        engine.mem_read(addr, &mut bytes).expect("read guest u64");
        u64::from_le_bytes(bytes)
    }

    fn read_guest_u32(engine: &mut IcedCpu, addr: u64) -> u32 {
        let mut bytes = [0_u8; 4];
        engine.mem_read(addr, &mut bytes).expect("read guest u32");
        u32::from_le_bytes(bytes)
    }

    #[test]
    fn sh_get_special_folder_path_w_maps_csidl_personal() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_regs(&mut engine, 0, 0x5000, 0x05, 0); // CSIDL_PERSONAL, fCreate=FALSE
        let result = handle_sh_get_special_folder_path_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SHGetSpecialFolderPathW should succeed");
        assert_eq!(result.return_value, 1, "returns TRUE");
        let text = read_utf16_lossy(&mut engine, 0x5000, 64).expect("read path");
        assert_eq!(text, r"C:\Users\WIE\Documents");
    }

    #[test]
    fn sh_get_special_folder_path_w_programs_with_fcreate() {
        let (mut state, bottle) = test_bottle_state("special-programs");
        let mut engine = test_engine();
        write_regs(&mut engine, 0, 0x5000, 0x02, 1); // CSIDL_PROGRAMS, fCreate=TRUE
        let result = handle_sh_get_special_folder_path_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SHGetSpecialFolderPathW should succeed");
        assert_eq!(result.return_value, 1, "returns TRUE");
        let text = read_utf16_lossy(&mut engine, 0x5000, 128).expect("read path");
        assert_eq!(
            text,
            r"C:\Users\WIE\AppData\Roaming\Microsoft\Windows\Start Menu\Programs"
        );
        // fCreate materialized the host dir — this path is NOT in the seeded
        // skeleton, so only the fCreate branch could have created it.
        assert!(
            bottle
                .join("drive_c/Users/WIE/AppData/Roaming/Microsoft/Windows/Start Menu/Programs")
                .is_dir(),
            "fCreate must materialize CSIDL_PROGRAMS under the bottle"
        );
        let _cleanup = std::fs::remove_dir_all(&bottle);
    }

    #[test]
    fn sh_get_file_info_w_fills_struct_for_existing_file() {
        let (mut state, bottle) = test_bottle_state("fileinfo-w");
        std::fs::write(bottle.join("drive_c/App/shell_test.txt"), b"hello")
            .expect("write test file");
        let mut engine = test_engine();
        write_wide(&mut engine, 0x3000, r"C:\App\shell_test.txt");
        write_regs(&mut engine, 0x3000, 0, 0x5000, SHFILEINFO_W_SIZE);
        let result = handle_sh_get_file_info(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            true,
        )
        .expect("SHGetFileInfoW should succeed");
        assert_eq!(result.return_value, 1, "returns non-zero");
        assert_eq!(
            read_guest_u64(&mut engine, 0x5000),
            FAKE_ICON_HANDLE,
            "hIcon"
        );
        assert_eq!(read_guest_u32(&mut engine, 0x5008), 0, "iIcon");
        assert_eq!(
            read_guest_u32(&mut engine, 0x500c),
            crate::vfs::FILE_ATTRIBUTE_NORMAL,
            "dwAttributes on an existing file"
        );
        let name = read_utf16_lossy(&mut engine, 0x5010, 64).expect("read display name");
        assert_eq!(name, "shell_test.txt");
        let _cleanup = std::fs::remove_dir_all(&bottle);
    }

    #[test]
    fn sh_get_file_info_a_fills_struct_for_existing_file() {
        let (mut state, bottle) = test_bottle_state("fileinfo-a");
        std::fs::write(bottle.join("drive_c/App/shell_test.txt"), b"hello")
            .expect("write test file");
        let mut engine = test_engine();
        write_ansi(&mut engine, 0x3000, r"C:\App\shell_test.txt");
        write_regs(&mut engine, 0x3000, 0, 0x5000, SHFILEINFO_A_SIZE);
        let result = handle_sh_get_file_info(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            false,
        )
        .expect("SHGetFileInfoA should succeed");
        assert_eq!(result.return_value, 1, "returns non-zero");
        let mut name_bytes = [0_u8; 15];
        engine
            .mem_read(0x5010, &mut name_bytes)
            .expect("read ANSI display name");
        assert_eq!(
            &name_bytes, b"shell_test.txt\0",
            "ANSI name, NUL-terminated"
        );
        let _cleanup = std::fs::remove_dir_all(&bottle);
    }

    #[test]
    fn sh_get_file_info_w_missing_file_has_zero_attributes() {
        let (mut state, _bottle) = test_bottle_state("fileinfo-missing");
        let mut engine = test_engine();
        write_wide(&mut engine, 0x3000, r"C:\App\missing.txt");
        write_regs(&mut engine, 0x3000, 0, 0x5000, SHFILEINFO_W_SIZE);
        let result = handle_sh_get_file_info(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            true,
        )
        .expect("SHGetFileInfoW should succeed");
        assert_eq!(result.return_value, 1, "KISS: still returns non-zero");
        assert_eq!(read_guest_u32(&mut engine, 0x500c), 0, "no attributes");
    }

    #[test]
    fn sh_get_file_info_w_null_struct_returns_zero() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_wide(&mut engine, 0x3000, r"C:\App\shell_test.txt");
        write_regs(&mut engine, 0x3000, 0, 0, SHFILEINFO_W_SIZE); // psfi = NULL
        let result = handle_sh_get_file_info(
            &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
            true,
        )
        .expect("SHGetFileInfoW should succeed");
        assert_eq!(result.return_value, 0, "NULL struct → failure");
    }

    /// Build a SHELLEXECUTEINFOW at `0x5000` with `lpVerb` (wide, at `0x3000`)
    /// and `lpFile` (wide, at `0x4000`), pointers at the SDK offsets.
    fn write_exec_info(engine: &mut IcedCpu) {
        engine
            .mem_write(0x5010, &0x3000_u64.to_le_bytes())
            .expect("write lpVerb pointer");
        engine
            .mem_write(0x5018, &0x4000_u64.to_le_bytes())
            .expect("write lpFile pointer");
    }

    #[test]
    fn sh_execute_ex_w_other_verb_sets_noassoc() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_wide(&mut engine, 0x3000, "edit");
        write_wide(&mut engine, 0x4000, r"C:\App\notepad.exe");
        write_exec_info(&mut engine);
        write_regs(&mut engine, 0x5000, 0, 0, 0);
        let result = handle_sh_execute_ex_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("ShellExecuteExW should succeed");
        assert_eq!(result.return_value, 0, "non-open verb returns FALSE");
        assert_eq!(
            read_guest_u64(&mut engine, 0x5038),
            SE_ERR_NOASSOC,
            "hInstApp = SE_ERR_NOASSOC"
        );
    }

    #[test]
    fn sh_execute_ex_w_empty_file_sets_fnf() {
        let mut engine = test_engine();
        let mut state = test_state();
        write_wide(&mut engine, 0x3000, "open");
        write_wide(&mut engine, 0x4000, "");
        write_exec_info(&mut engine);
        write_regs(&mut engine, 0x5000, 0, 0, 0);
        let result = handle_sh_execute_ex_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("ShellExecuteExW should succeed");
        assert_eq!(result.return_value, 0, "empty file returns FALSE");
        assert_eq!(
            read_guest_u64(&mut engine, 0x5038),
            SE_ERR_FNF,
            "hInstApp = SE_ERR_FNF"
        );
    }
}
