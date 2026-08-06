//! Minimal `shell32.dll` stubs (folder paths / browse UI / About / launch) for CLI tools.

use crate::guest_memory::{read_int, write_u32 as write_guest_u32};
use crate::guest_string::{read_utf16_lossy, write_utf16_c_string};
use crate::state::{MessageBoxRequest, PendingNativeMessageBox, WinApiControlSignal, WindowFlags};
use crate::user32::{IDOK, find_window_mut};
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

fn ret(engine: &mut dyn wie_cpu::CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("shell32 return")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
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
        _ => Ok(None),
    }
}

/// `HRESULT SHGetFolderPathW(hwnd, csidl, hToken, dwFlags, pszPath)`
///
/// Fills a fixed bottle-friendly path under `C:\Users\WIE\…` style so tools
/// that only need a writable home directory keep going.
fn handle_sh_get_folder_path_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hwnd = engine.read_rcx()?;
    let csidl = engine.read_rdx()? & 0xffff_ffff;
    let _token = engine.read_r8()?;
    let _flags = engine.read_r9()?;
    let mut path_ptr_bytes = [0_u8; 8];
    let rsp = engine.read_rsp()?;
    engine.mem_read(rsp.wrapping_add(0x28), &mut path_ptr_bytes)?;
    let path_ptr = u64::from_le_bytes(path_ptr_bytes);

    // Common CSIDL values → synthetic bottle paths (MAX_PATH buffer expected).
    // `csidl` already masked to low 32 bits (u64).
    let path = match csidl {
        0x00 => r"C:\Users\WIE\Desktop",          // CSIDL_DESKTOP
        0x05 => r"C:\Users\WIE\Documents",        // CSIDL_PERSONAL / My Documents
        0x1a => r"C:\Users\WIE\AppData\Roaming",  // CSIDL_APPDATA
        0x1c => r"C:\Users\WIE\AppData\Local",    // CSIDL_LOCAL_APPDATA
        0x23 => r"C:\ProgramData",                // CSIDL_COMMON_APPDATA
        0x24 => r"C:\Windows",                    // CSIDL_WINDOWS
        0x25 => r"C:\Windows\System32",           // CSIDL_SYSTEM
        0x26 => r"C:\Program Files",              // CSIDL_PROGRAM_FILES
        0x2a => r"C:\Program Files\Common Files", // CSIDL_PROGRAM_FILES_COMMON
        // CSIDL_PROFILE (0x28) and unknown → user home.
        _ => r"C:\Users\WIE",
    };

    if path_ptr != 0 {
        write_utf16_c_string(engine, path_ptr, 260, path)?;
    }
    ret(engine, S_OK)
}

/// `LPWSTR* CommandLineToArgvW(LPCWSTR lpCmdLine, int* pNumArgs)`.
///
/// Parses a command-line string into an argv-style array, allocating the result
/// and the argument strings from the process heap.
fn handle_command_line_to_argv_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let cmd_line_ptr = engine.read_rcx()?;
    let num_args_ptr = engine.read_rdx()?;
    if cmd_line_ptr == 0 || num_args_ptr == 0 {
        return ret(engine, 0); // NULL → failure
    }
    // Read the command line.
    let mut units = Vec::new();
    let mut i = 0_u64;
    loop {
        let mut b = [0_u8; 2];
        engine.mem_read(cmd_line_ptr.wrapping_add(i.wrapping_mul(2)), &mut b)?;
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
        return ret(engine, 0);
    }
    let mut offset = 0_u64;
    for arg in &args {
        let units: Vec<u16> = arg.encode_utf16().collect();
        let bstr = alloc_shell_bstr(engine, state, &units)?;
        if bstr == 0 {
            return ret(engine, 0);
        }
        engine.mem_write(argv_va.wrapping_add(offset), &bstr.to_le_bytes())?;
        offset = offset.saturating_add(8);
    }
    // NULL terminator.
    engine.mem_write(argv_va.wrapping_add(offset), &[0_u8; 8])?;
    // Write argc.
    let argc_u32 = u32::try_from(args.len()).unwrap_or(0);
    drop(write_guest_u32(engine, num_args_ptr, argc_u32));
    ret(engine, argv_va)
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
    let path_ptr = engine.read_rdx()?;
    if path_ptr != 0 {
        write_utf16_c_string(engine, path_ptr, 260, "")?;
    }
    ret(engine, 0) // FALSE
}

/// `PIDLIST_ABSOLUTE SHBrowseForFolderW(lpbi)` — no UI; return NULL.
fn handle_sh_browse_for_folder_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _lpbi = engine.read_rcx()?;
    let _ = E_FAIL;
    ret(engine, 0)
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
    ret(engine, 1)
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
        let app_name_ptr = engine
            .read_rdx()
            .context("failed to read RDX for ShellAboutW")?;
        let other_stuff_ptr = engine
            .read_r8()
            .context("failed to read R8 for ShellAboutW")?;
        let _h_icon = engine
            .read_r9()
            .context("failed to read R9 for ShellAboutW")?;
        // read_utf16_lossy treats NULL as an empty string, so either argument
        // may be omitted (real Windows falls back to module/version strings).
        let app_name = read_utf16_lossy(engine, app_name_ptr, 256)?;
        let other_stuff = read_utf16_lossy(engine, other_stuff_ptr, 1024)?;
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
    // also TRUE, the documented return.
    if let Some(pending) = ctx.state.window_state().pending_native_message_box.take() {
        let win32_id = pending
            .pick
            .and_then(|id| u64::try_from(id).ok())
            .unwrap_or(IDOK);
        return ret(ctx.engine, win32_id);
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
        // is exactly caption + text + MB_OK.
        ctx.state.window_state().pending_native_message_box =
            Some(PendingNativeMessageBox { pick: None });
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
    eprintln!("[ShellAboutW] {app_name}: {text}");
    ret(ctx.engine, 1)
}

/// `HINSTANCE ShellExecuteW(HWND hwnd, LPCWSTR lpOperation, LPCWSTR lpFile,
/// LPCWSTR lpParameters, LPCWSTR lpDirectory, INT nShowCmd)`.
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
pub fn handle_shell_execute_w(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let (operation, file) = {
        let engine = &mut *ctx.engine;
        let _hwnd = engine
            .read_rcx()
            .context("failed to read RCX for ShellExecuteW")?;
        let operation_ptr = engine
            .read_rdx()
            .context("failed to read RDX for ShellExecuteW")?;
        let file_ptr = engine
            .read_r8()
            .context("failed to read R8 for ShellExecuteW")?;
        let _parameters_ptr = engine
            .read_r9()
            .context("failed to read R9 for ShellExecuteW")?;
        let rsp = engine
            .read_rsp()
            .context("failed to read RSP for ShellExecuteW")?;
        let _directory_ptr = read_int::<u64>(engine, rsp.wrapping_add(0x28))?;
        let _show_cmd = read_int::<u64>(engine, rsp.wrapping_add(0x30))?;
        let operation = read_utf16_lossy(engine, operation_ptr, 64)?;
        let file = read_utf16_lossy(engine, file_ptr, 1024)?;
        (operation, file)
    };

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
        "ShellExecuteW"
    );
    ret(ctx.engine, return_value)
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

    /// The FS-policy bottle copy (`C:\{name}` → `{root}/drive_c/{name}`): when
    /// the main module lives in the bottle, its relaunch resolves through the
    /// volume mapping — never through the temp-file spill.
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
}
