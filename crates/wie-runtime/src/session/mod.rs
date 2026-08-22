//! Persistent `RuntimeSession`: guest setup, yield/resume, and API hook loop.

mod callback;
mod init;
mod menu;
mod profile;
mod pump;
mod types;
mod window;
mod window_msg;

pub use self::menu::MenuNode;
pub use self::profile::RuntimeProfile;
pub use self::window::GuestHandle;

pub(crate) use self::types::{GuestHwnd, GuestStackPtr, GuestTid, GuestVa};

use crate::memory::RuntimeMemoryLayout;
use crate::mt_runtime::ProcessResources;
use crate::trace::EntryTraceTermination;
use ahash::HashMap;
use anyhow::{Context, Result};
use std::sync::{Arc, RwLock};

use self::callback::PendingGuestCallback;

/// Bootstrap options for a new guest session (argv / stdin injection).
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    /// Extra command-line arguments after argv[0] (module basename).
    pub guest_args: Vec<String>,
    /// Bytes for console `ReadFile(STD_INPUT_HANDLE)`.
    ///
    /// Non-empty: inject-only (no host block). Empty: live host stdin when the
    /// guest reads STD_INPUT (line-oriented).
    pub stdin_bytes: Vec<u8>,
    /// Bottle root for guest `C:\…` mapping (`None` = `WIE_ROOT`, else the
    /// global app-data bottle). Applied before the process identity is
    /// derived, so an in-bottle exe's guest module path reflects its real
    /// location (e.g. `C:\Program Files\{name}\{name}.exe`).
    pub bottle_root: Option<std::path::PathBuf>,
    /// Optional host root for guest `D:\…` (`None` = `WIE_DRIVE_D`, else no
    /// D: drive).
    pub drive_d_root: Option<std::path::PathBuf>,
    /// Guest current directory the process starts in (e.g.
    /// `C:\Program Files\{name}`). `None` keeps the loader default (`C:\`,
    /// the drive root). Set by the CLI when it stages an external app folder,
    /// so relative resource paths resolve from the staged exe's directory
    /// like a normal Windows launch.
    pub current_directory: Option<String>,
}

/// Guest CRT page layout constants (must match `wie_winapi::ucrt` and guest
/// stubs). Re-exported from the shared `wie_cpu::guest_layout` home so the
/// session init and the stub builders cannot drift apart.
pub(crate) use wie_cpu::guest_layout::CRT_GUEST_BASE;
use wie_cpu::guest_layout::{
    CRT_ACMDLN_PTR_SLOT, CRT_ARGC_SLOT, CRT_ARGV_PTR_SLOT, CRT_ARGV_STRINGS, CRT_ARGV_TABLE,
    CRT_COMMODE_SLOT, CRT_ENVIRON_PTR_SLOT, CRT_FMODE_SLOT, CRT_PAGE_END,
};

/// Default per-quantum API-stop budget for [`RuntimeSession::run_until_stop`]
/// callers that do not need a custom cap (the GUI loop uses this).
pub(crate) const MAX_API_QUANTUM: usize = 1_000_000;

/// Materialize UCRT `__p___argc` / `__p___argv` / `__p__acmdln` guest slots.
fn materialize_crt_argv(
    engine: &mut dyn wie_cpu::CpuEngine,
    argv0: &str,
    extra_args: &[String],
    command_line_a_ptr: u64,
) -> Result<()> {
    let mut argv: Vec<String> = Vec::with_capacity(1 + extra_args.len());
    argv.push(argv0.to_owned());
    argv.extend(extra_args.iter().cloned());

    let argc = u32::try_from(argv.len()).context("argc does not fit u32")?;
    engine
        .mem_write(CRT_ARGC_SLOT, &argc.to_le_bytes())
        .context("failed to write CRT argc")?;

    // acmdln → GetCommandLineA buffer (char*).
    engine
        .mem_write(CRT_ACMDLN_PTR_SLOT, &command_line_a_ptr.to_le_bytes())
        .context("failed to write CRT acmdln")?;

    // argv pointer table at CRT_ARGV_TABLE; strings packed from CRT_ARGV_STRINGS.
    let mut string_cursor = CRT_ARGV_STRINGS;
    for (i, arg) in argv.iter().enumerate() {
        let slot = CRT_ARGV_TABLE
            .checked_add(u64::try_from(i).context("argv index")? * 8)
            .context("argv slot overflow")?;
        if slot.saturating_add(8) > CRT_ARGV_STRINGS {
            anyhow::bail!("too many argv entries for CRT page");
        }
        let mut bytes = arg.as_bytes().to_vec();
        bytes.push(0);
        let end = string_cursor
            .checked_add(u64::try_from(bytes.len()).context("arg len")?)
            .context("argv string end overflow")?;
        if end > CRT_PAGE_END {
            anyhow::bail!("argv strings exceed CRT guest page");
        }
        engine
            .mem_write(string_cursor, &bytes)
            .context("failed to write argv string")?;
        engine
            .mem_write(slot, &string_cursor.to_le_bytes())
            .context("failed to write argv pointer")?;
        string_cursor = end;
    }

    // Trailing NULL pointer (char** is null-terminated).
    let null_slot = CRT_ARGV_TABLE
        .checked_add(u64::from(argc) * 8)
        .context("argv null slot overflow")?;
    engine
        .mem_write(null_slot, &0_u64.to_le_bytes())
        .context("failed to write argv NULL terminator")?;

    // ARGV_PTR_SLOT holds char** (address of the pointer table).
    engine
        .mem_write(CRT_ARGV_PTR_SLOT, &CRT_ARGV_TABLE.to_le_bytes())
        .context("failed to write CRT argv ptr slot")?;

    Ok(())
}

/// Long-lived executable runtime session.
///
/// The session owns the CPU engine, guest memory, WinAPI state and
/// dispatcher metadata so execution can yield and later resume.
pub struct RuntimeSession {
    /// CPU + WinAPI (single struct for both JIT and Iced).
    process: ProcessResources,
    entry_point_va: GuestVa,
    initial_rsp: GuestStackPtr,
    next_api_index: usize,
    no_hook_slices: usize,
    pending_callbacks: Vec<PendingGuestCallback>,
    /// Cache of `Arc<str>` copies of callback-outer API names, so every
    /// bridged window message clones a refcounted string instead of
    /// allocating a fresh `Arc` box + copy per message.
    outer_api_names: HashMap<String, Arc<str>>,
    /// When true, accumulate [`RuntimeProfile`] across `run_until_stop` calls.
    profile_enabled: bool,
    profile: RuntimeProfile,
    /// Whether the guest entry point has been reached (set on the first run).
    entry_reached: bool,
    /// Whether the previous `run_until_stop` returned `WaitingForMessage`;
    /// gates the idle-transition log so it fires on transitions only.
    was_waiting_for_message: bool,
    /// Last observed present generation (frame-boundary sampling).
    frame_last_gen: u64,
    /// `host_stops` baseline at the last frame boundary.
    frame_last_stops: u64,
    /// Iced-instruction baseline at the last frame boundary.
    frame_last_iced: u64,
    /// JIT-instruction baseline at the last frame boundary.
    frame_last_jit: u64,
}

/// Rows of the guest's top-level windows, in guest creation order.
///
/// A top-level window has no parent or owner (`parent_handle == 0`); every
/// other record — dialog boxes, controls, child windows — is excluded. The
/// order is the `windows` record order, so the main window (created first) is
/// the first row.
fn top_level_window_rows(windows: &[wie_winapi::WindowRecord]) -> Vec<(u64, String, i32, i32)> {
    windows
        .iter()
        .filter(|window| window.parent_handle == wie_winapi::handles::Hwnd::NULL)
        .map(|window| {
            (
                window.handle.as_u64(),
                window.title.clone(),
                window.width,
                window.height,
            )
        })
        .collect()
}

impl RuntimeSession {
    /// Returns the PE entry-point address associated with this session.
    #[must_use]
    pub fn entry_point_va(&self) -> u64 {
        self.entry_point_va.0
    }

    /// Sets the bottle root for guest `C:\…` → host `{root}/drive_c/…` mapping.
    ///
    /// Overrides any `WIE_ROOT` applied at session construction.
    pub fn set_bottle_root(&mut self, root: Option<std::path::PathBuf>) {
        self.process.with_mut(|_, s| {
            if let Some(ref r) = root {
                let _ = wie_winapi::seed_default_skeleton(r);
            }
            s.file_io.bottle_root = root.clone();
            s.file_io.volumes.bottle_root = root;
        });
    }

    /// Sets optional host-bridge root for guest `D:\…` (`None` unmounts D:).
    pub fn set_drive_d(&mut self, root: Option<std::path::PathBuf>) {
        self.process
            .with_mut(|_, s| s.file_io.volumes.drive_d_root = root);
    }

    /// Replaces guest stdin buffer for console `ReadFile` on STD_INPUT_HANDLE.
    ///
    /// Non-empty bytes are inject-only. Empty enables live host stdin on the
    /// next guest read.
    pub fn set_stdin_bytes(&mut self, bytes: Vec<u8>) {
        self.process.with_mut(|_, s| {
            s.file_io.stdin_mode = if bytes.is_empty() {
                wie_winapi::GuestStdinMode::LiveHost
            } else {
                wie_winapi::GuestStdinMode::InjectOnly
            };
            s.file_io.stdin_bytes = bytes;
            s.file_io.stdin_cursor = 0;
        });
    }

    /// Upserts a guest-visible environment variable for `GetEnvironmentVariable`.
    ///
    /// Names match case-insensitively, exactly as Windows does; inserting a
    /// new name appends in insertion order so guest-visible ordering stays
    /// stable. Rejects empty names and names containing `=`, mirroring
    /// `SetEnvironmentVariableA`'s validation. The `GetEnvironmentStringsW`
    /// snapshot block is intentionally untouched — the Vec is authoritative
    /// for the per-variable APIs.
    pub fn set_guest_env(&self, name: &str, value: &str) -> Result<()> {
        if name.is_empty() || name.contains('=') {
            anyhow::bail!("invalid guest environment variable name: {name:?}");
        }
        let shared_winapi = self.process.winapi_arc();
        let mut state = crate::mt_runtime::lock_wait(&shared_winapi, &self.process.lock_wait_stats);
        let environment = &mut state.process.environment;
        match environment
            .iter()
            .position(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            Some(index) => {
                if let Some(slot) = environment.get_mut(index) {
                    slot.1 = String::from(value);
                }
            }
            None => environment.push((name.to_owned(), value.to_owned())),
        }
        Ok(())
    }

    /// Returns the original stack pointer used to start the guest.
    #[must_use]
    pub fn initial_rsp(&self) -> u64 {
        self.initial_rsp.0
    }

    /// Returns the guest memory layout used by this session.
    #[must_use]
    pub fn layout(&self) -> RuntimeMemoryLayout {
        *self.process.layout()
    }

    /// Changes the behavior of `GetMessageA` when the queue is empty.
    pub fn set_message_queue_idle_policy(&mut self, policy: wie_winapi::MessageQueueIdlePolicy) {
        self.process
            .with_mut(|_, s| s.window_state().message_queue_idle_policy = policy);
    }

    /// Adds one message to the persistent guest message queue.
    pub fn post_message(&mut self, message: wie_winapi::QueuedWindowMessage) {
        self.process
            .message_queue_arc()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .messages
            .push(message);
    }

    /// Queues one deterministic USER32 message for the guest.
    ///
    /// Internal seam for tests and future host automation; the interactive
    /// presenter posts through [`GuestHandle::post_message`] instead.
    /// No caller exists yet, so the newtyped signature is kept as the
    /// reserved API (dead code under `-Dwarnings` until a caller lands).
    #[allow(dead_code)]
    pub(crate) fn post_window_message(
        &mut self,
        window_handle: GuestHwnd,
        message: u32,
        word_parameter: u64,
        long_parameter: u64,
    ) -> Result<()> {
        let queue_arc = self.process.message_queue_arc();
        let mut queue = queue_arc.lock().unwrap_or_else(|e| e.into_inner());
        queue.push(
            wie_winapi::handles::Hwnd::from(window_handle.0),
            message,
            word_parameter,
            long_parameter,
        )
    }

    /// Returns the first window backed by a guest WndProc.
    #[must_use]
    pub fn first_guest_window_handle(&self) -> Option<u64> {
        self.process.with_winapi_ref(|st| {
            st.try_window_state().and_then(|ws| {
                ws.windows
                    .iter()
                    .find(|window| window.window_proc != 0)
                    .map(|window| window.handle.as_u64())
            })
        })
    }

    /// Return a cloneable handle for cross-thread WinAPI access.
    #[must_use]
    pub fn guest_handle(&self) -> GuestHandle {
        GuestHandle {
            state: self.process.winapi_arc(),
            queue: self.process.message_queue_arc(),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::clone(&self.process.lock_wait_stats),
        }
    }

    /// Take the latest published frame for `hwnd`, if any.
    #[must_use]
    pub fn take_frame(&self, hwnd: u64) -> Option<wie_winapi::present::SurfaceFrame> {
        self.process.with_winapi_ref(|state| {
            state
                .try_present()?
                .published
                .get(&wie_winapi::handles::Hwnd::from(hwnd))
                .cloned()
        })
    }

    /// Snapshot of runtime-owned windows (handle, class, title, has WndProc).
    #[must_use]
    pub fn guest_windows_snapshot(&self) -> Vec<(u64, String, String, bool)> {
        self.process.with_winapi_ref(|st| {
            st.try_window_state()
                .map(|ws| {
                    ws.windows
                        .iter()
                        .map(|window| {
                            (
                                window.handle.as_u64(),
                                window.class_name.clone(),
                                window.title.clone(),
                                window.window_proc != 0,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// Number of guest callbacks currently nested on the bridge stack.
    #[must_use]
    pub fn pending_callback_depth(&self) -> usize {
        self.pending_callbacks.len()
    }

    /// Configures the next common file dialog outcome (`GetOpenFileName` / `GetSaveFileName`).
    pub fn set_file_dialog_policy(&mut self, policy: wie_winapi::FileDialogPolicy) {
        self.process
            .with_mut(|_, s| s.window_state().file_dialog_policy = policy);
    }

    /// Configures the next common font dialog outcome (`ChooseFontW`).
    pub fn set_font_dialog_policy(&mut self, policy: wie_winapi::FontDialogPolicy) {
        self.process
            .with_mut(|_, s| s.window_state().font_dialog_policy = policy);
    }

    /// Configures the next common print dialog outcome (`PrintDlgW`).
    pub fn set_print_dialog_policy(&mut self, policy: wie_winapi::PrintDialogPolicy) {
        self.process
            .with_mut(|_, s| s.window_state().print_dialog_policy = policy);
    }

    /// Configures the next common page-setup dialog outcome (`PageSetupDlgW`).
    pub fn set_page_setup_dialog_policy(&mut self, policy: wie_winapi::PageSetupDialogPolicy) {
        self.process
            .with_mut(|_, s| s.window_state().page_setup_dialog_policy = policy);
    }

    /// Returns the last path accepted by a simulated file dialog.
    #[must_use]
    pub fn last_file_dialog_path(&self) -> Option<String> {
        self.process.with_winapi_ref(|st| {
            st.try_window_state()
                .and_then(|ws| ws.last_file_dialog_path.clone())
        })
    }

    /// Mounts a host file so the guest can open it via `CreateFile*` under `guest_path`.
    pub fn mount_host_file(
        &mut self,
        guest_path: &str,
        host_path: impl AsRef<std::path::Path>,
    ) -> Result<()> {
        self.process
            .with_mut(|_, st| wie_winapi::kernel32::mount_host_file(st, guest_path, host_path))
    }

    /// Opens a guest path with the same rules as `CreateFile*` (for smoke tests).
    pub fn open_guest_path(&mut self, guest_path: &str) -> Result<u64> {
        self.process
            .with_mut(|_, st| wie_winapi::kernel32::open_guest_path(st, guest_path))
    }

    /// Returns the size of an open guest file handle.
    pub fn guest_file_size(&self, handle: u64) -> Result<u64> {
        self.process.with_winapi_ref(|st| {
            let file = st
                .file_io
                .open_files
                .get(&handle)
                .with_context(|| format!("unknown guest file handle {handle:#018x}"))?;
            u64::try_from(file.bytes.len()).context("guest file size does not fit u64")
        })
    }

    /// Reads a slice from an open guest file without advancing the cursor.
    pub fn peek_guest_file(&self, handle: u64, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.process.with_winapi_ref(|st| {
            let file = st
                .file_io.open_files
                .get(&handle)
                .with_context(|| format!("unknown guest file handle {handle:#018x}"))?;
            let end = offset
                .checked_add(len)
                .context("guest file peek range overflow")?;
            file.bytes
                .get(offset..end)
                .map(<[u8]>::to_vec)
                .with_context(|| {
                    format!(
                        "guest file peek out of range handle={handle:#018x} offset={offset} len={len}"
                    )
                })
        })
    }

    /// Snapshot of currently open guest files (path + handle + size).
    #[must_use]
    pub fn open_guest_files_snapshot(&self) -> Vec<(u64, String, u64)> {
        self.process.with_winapi_ref(|st| {
            st.file_io
                .open_files
                .iter()
                .filter_map(|(&handle, file)| {
                    let size = u64::try_from(file.bytes.len()).ok()?;
                    Some((handle, file.path.to_string(), size))
                })
                .collect()
        })
    }
}

impl GuestHandle {
    /// Snapshot of the guest's top-level windows: (hwnd, title, width, height).
    ///
    /// The size-carrying variant of the session's `guest_windows_snapshot`
    /// that the host presenter reconciles its winit window registry against
    /// on every published frame — one host window per top-level. The rows
    /// follow guest creation order (the `windows` record order), so the first
    /// row is the main window.
    ///
    /// Kept here (the session module) instead of `session/window.rs` so this
    /// lane owns the host-presentation seam; the implementation shares the
    /// [`top_level_window_rows`] predicate with the session API.
    #[must_use]
    pub fn guest_top_level_windows(&self) -> Vec<(u64, String, i32, i32)> {
        let Some(state) = self.lock_state() else {
            return Vec::new();
        };
        state
            .try_window_state()
            .map(|ws| top_level_window_rows(&ws.windows))
            .unwrap_or_default()
    }
}

impl Drop for RuntimeSession {
    fn drop(&mut self) {
        // Reap guest worker threads when the explicit ExitProcess path
        // (session/pump.rs) never ran: early CLI abort, headless idle exit,
        // budget exhaustion, or test teardown. Without this the JoinHandles
        // go out of scope unjoined and workers leak as detached daemons.
        //
        // Idempotency: `join_workers_impl` drains `worker_joins` to empty, so
        // after ExitProcess (or with zero spawned workers) this is a no-op.
        // Guarding on emptiness also skips re-running the wake/finish pass.
        if !self.process.worker_joins.is_empty() {
            self.process.join_workers();
        }
    }
}

/// [`Cold`] diagnostic for invalid memory access — 32 stack slot reads + object
/// dump + vtable dump.  Kept out of line so the normal `run_until_stop` hot path
/// does not pay the I-cache cost of this heavyweight crash instrumentation.
///
/// Shared by the primary pump (via the quantum executor) and the entry-trace
/// helpers; the module lives here so the dump can reuse the session's layout
/// constants without a crate cycle.
#[cold]
pub(crate) fn invalid_memory_diagnostic(
    engine: &mut dyn wie_cpu::CpuEngine,
    access: &wie_cpu::InvalidMemoryAccess,
) -> Result<EntryTraceTermination> {
    let rip = engine.read_rip()?;
    let rsp = engine.read_rsp()?;
    let rax = engine.read_rax()?;
    let rcx = engine.read_rcx()?;
    let rdx = engine.read_rdx()?;
    let r8 = engine.read_r8()?;
    let r9 = engine.read_r9()?;

    // Stack slots (return addr + shadow) and *this / vtable for null-call diagnosis.
    let mut stack_slots = String::new();
    for i in 0_u64..32 {
        let mut b = [0_u8; 8];
        let off = i.wrapping_mul(8);
        let va = rsp.wrapping_add(off);
        match engine.mem_read(va, &mut b) {
            Ok(()) => {
                let v = u64::from_le_bytes(b);
                stack_slots.push_str(&format!(" [rsp+{off:#x}]={v:#x}"));
            }
            Err(_) => stack_slots.push_str(&format!(" [rsp+{off:#x}]=?")),
        }
    }

    let mut this_info = String::new();
    if rcx != 0 {
        let mut b = [0_u8; 8];
        if engine.mem_read(rcx, &mut b).is_ok() {
            let vtbl = u64::from_le_bytes(b);
            this_info.push_str(&format!(" [rcx]={vtbl:#x}"));
            // Dump object body (stack COM objects often ~0x40–0x80 bytes).
            for i in 0_u64..12 {
                let mut e = [0_u8; 8];
                let ova = rcx.wrapping_add(i.wrapping_mul(8));
                if engine.mem_read(ova, &mut e).is_ok() {
                    this_info.push_str(&format!(" obj[{i}]={:#x}", u64::from_le_bytes(e)));
                }
            }
            if vtbl > 0x10000 {
                for i in 0_u64..8 {
                    let mut e = [0_u8; 8];
                    let eva = vtbl.wrapping_add(i.wrapping_mul(8));
                    if engine.mem_read(eva, &mut e).is_ok() {
                        this_info.push_str(&format!(" vtbl[{i}]={:#x}", u64::from_le_bytes(e)));
                    }
                }
            }
        } else {
            this_info.push_str(" [rcx]=unmapped");
        }
    }

    Ok(EntryTraceTermination::RuntimeStop(format!(
        "invalid memory access before fake API hook: \
         type={} address={:#018x} size={} value={} \
         rip={rip:#018x}; rsp={rsp:#018x}; rax={rax:#018x}; \
         rcx={rcx:#018x}; rdx={rdx:#018x}; \
         r8={r8:#018x}; r9={r9:#018x};{stack_slots};{this_info}",
        access.access_type, access.address, access.size, access.value,
    )))
}

/// Transient `[FAULT]` capture: one tagged line with faulting guest RIP
/// (resolved to module+offset), faulting address, access kind, and the raw
/// instruction bytes at RIP for offline disassembly.
#[cold]
pub(crate) fn fault_capture(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &wie_winapi::WinApiState,
    tid: u32,
    access: &wie_cpu::InvalidMemoryAccess,
) {
    let rip = engine.read_rip().unwrap_or(0);
    let rsp = engine.read_rsp().unwrap_or(0);
    let regs = [
        engine.read_rax().unwrap_or(0),
        engine.read_rcx().unwrap_or(0),
        engine.read_rdx().unwrap_or(0),
        engine.read_r8().unwrap_or(0),
        engine.read_r9().unwrap_or(0),
    ];
    let access_kind = match access.access_type {
        0 => "read",
        1 => "write",
        16 => "exec",
        _ => "?",
    };
    let mut module = String::from("?");
    for m in state.module_state.loaded_modules.values() {
        if rip >= m.image_base && rip < m.image_base.saturating_add(m.image_size as u64) {
            module = format!("{} +0x{:x}", m.name, rip - m.image_base);
            break;
        }
    }
    // Main exe fallback: derive its base from the PE header found by walking
    // the standard high half (preferred base for PE32+ images).
    if module == "?" && rip >= 0x10000 {
        module = format!("<main_exe> +0x{:x}", rip - 0x140000000);
    }
    //
    let mut bytes = [0_u8; 15];
    let n = engine
        .mem_read(rip, &mut bytes)
        .map(|_| bytes.len())
        .unwrap_or(0);
    let hex: Vec<String> = bytes[..n].iter().map(|b| format!("{b:02x}")).collect();
    let resolve = |va: u64| -> Option<String> {
        for m in state.module_state.loaded_modules.values() {
            if va >= m.image_base && va < m.image_base.saturating_add(m.image_size as u64) {
                return Some(format!("{} +0x{:x}", m.name, va - m.image_base));
            }
        }
        None
    };
    let mut stack = String::new();
    for i in 0_u64..40 {
        let mut b = [0_u8; 8];
        let va = rsp.wrapping_add(i.wrapping_mul(8));
        match engine.mem_read(va, &mut b) {
            Ok(()) => {
                let v = u64::from_le_bytes(b);
                let ann = match resolve(v) {
                    Some(sym) => format!(" <= {sym}"),
                    None => String::new(),
                };
                stack.push_str(&format!(" [{rsp:#x}+{:#x}]={v:#x}{ann}", i * 8))
            }
            Err(_) => stack.push_str(&format!(" [{rsp:#x}+{:#x}]=?", i * 8)),
        }
    }
    let msg = format!(
        "[FAULT] tid={tid:#x} exc={:#x} rip={rip:#x} ({module}) fault_addr={:#x} access={access_kind} size={} insn=[{}] rax={:#x} rcx={:#x} rdx={:#x} r8={:#x} r9={:#x} rbx={:#x} r12={:#x} rsp={rsp:#x}{stack}",
        access.exception_code,
        access.address,
        access.size,
        hex.join(" "),
        regs[0],
        regs[1],
        regs[2],
        regs[3],
        regs[4],
        engine.read_rbx().unwrap_or(0),
        engine.read_r12().unwrap_or(0),
    );
    tracing::error!("{msg}");
}

fn journal_api_return(
    index: usize,
    library: &str,
    name: &str,
    engine: &mut dyn wie_cpu::CpuEngine,
    return_value: u64,
    return_address: u64,
) {
    // Cached once: the runtime does not observe env var changes at runtime, so
    // any subsequent call is a monomorphic branch on an atomic-loaded pointer
    // instead of a full getenv() + 7 wasted register reads (previously the
    // env lookup was per-call, followed by 8 register reads before the
    // OpenOptions::open would bail on IO error).
    use std::sync::OnceLock;
    static JOURNAL_PATH: OnceLock<Option<String>> = OnceLock::new();
    let Some(path) = JOURNAL_PATH
        .get_or_init(|| std::env::var("WIE_API_JOURNAL").ok())
        .as_deref()
    else {
        return;
    };
    let rip = engine.read_rip().unwrap_or(0);
    let rsp = engine.read_rsp().unwrap_or(0);
    let rax = engine.read_rax().unwrap_or(return_value);
    let rcx = engine.read_rcx().unwrap_or(0);
    let rdx = engine.read_rdx().unwrap_or(0);
    let r8 = engine.read_r8().unwrap_or(0);
    let r9 = engine.read_r9().unwrap_or(0);
    // Sample stack slots that the crash site uses as call tables.
    let mut slot160 = [0_u8; 8];
    let s160 = engine
        .mem_read(rsp.wrapping_add(0x160), &mut slot160)
        .ok()
        .map_or(0_u64, |()| u64::from_le_bytes(slot160));
    let line = format!(
        "{index}|{library}|{name}|ret={return_value:#x}|retaddr={return_address:#x}|\
         rip={rip:#x}|rsp={rsp:#x}|rax={rax:#x}|rcx={rcx:#x}|rdx={rdx:#x}|r8={r8:#x}|r9={r9:#x}|\
         [rsp+160]={s160:#x}\n"
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::GuestHandle;
    use super::top_level_window_rows;
    use crate::memory::DEFAULT_LAYOUT;
    use std::sync::{Arc, Mutex, RwLock};
    use wie_cpu::{CpuEngine, IcedCpu};
    use wie_pe::ProcessIdentity;
    use wie_winapi::WindowRecord;
    use wie_winapi::handles::Hwnd;
    use wie_winapi::{HandlerContext, WinApiEnvironment, gdi32, user32};

    fn record(handle: u64, parent: u64, title: &str, width: i32, height: i32) -> WindowRecord {
        WindowRecord {
            handle: Hwnd::from(handle),
            parent_handle: Hwnd::from(parent),
            title: title.to_owned(),
            width,
            height,
            ..Default::default()
        }
    }

    /// Build a `GuestHandle` whose window table holds the given records,
    /// mirroring the `session/window.rs` test setup.
    fn handle_with_windows(windows: Vec<WindowRecord>) -> GuestHandle {
        let process = ProcessIdentity {
            module_file_name: "top-levels.exe".to_owned(),
            module_path: r"C:\App\top-levels.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "top-levels.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        winapi_state.window_state().windows = windows;
        GuestHandle {
            state: Arc::new(Mutex::new(winapi_state)),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        }
    }

    /// The top-level snapshot is size-carrying and per-top-level: N top-level
    /// windows yield N rows, each carrying its own title and dimensions, and
    /// children (parented controls, dialog-owned buttons) are excluded. The
    /// rows are in guest creation order, so the main window is first.
    #[test]
    fn guest_top_level_windows_include_only_parentless_windows() {
        let handle = handle_with_windows(vec![
            record(0x100, 0, "main", 640, 420),
            record(0x101, 0x100, "child edit", 100, 40),
            record(0x102, 0, "dialog", 360, 140),
        ]);
        let rows = handle.guest_top_level_windows();
        assert_eq!(rows.len(), 2, "only the parentless windows are top-level");
        assert_eq!(
            rows.first(),
            Some(&(0x100, "main".to_owned(), 640, 420)),
            "the main window is the first row, with its own size"
        );
        assert_eq!(
            rows.get(1),
            Some(&(0x102, "dialog".to_owned(), 360, 140)),
            "an owned dialog is a top-level window too"
        );
    }

    /// An empty window table yields an empty snapshot (no rows, no panic).
    #[test]
    fn guest_top_level_windows_are_empty_without_windows() {
        let handle = handle_with_windows(Vec::new());
        assert!(handle.guest_top_level_windows().is_empty());
        assert!(top_level_window_rows(&[]).is_empty());
    }

    // ── Host-window-on-first-frame contract ──────────────────────────────

    /// A fixed scratch address for the guest BITMAPINFO / `*ppvBits` slots,
    /// far from the runtime layout's process heap (0x1_6000_0000) and stack.
    const SCRATCH_VA: u64 = 0x0000_0001_7000_0000;
    /// The test stack top: handlers read stack args at `rsp + 0x28…` and the
    /// dummy return address at `rsp`.
    const STACK_TOP: u64 = 0x0000_0001_8000_0000;

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        cpu.write_rsp(STACK_TOP).ok();
    }

    /// The full "guest rendered its first frame" contract at the seam the
    /// host presenter reads: a software-renderer blit into a PARENTLESS
    /// window's DC must publish a frame that `GuestHandle::take_frame` can
    /// drain, while the window stays visible to the presenter's reconcile
    /// (`guest_top_level_windows`) and the window-set revision advances so
    /// the host `Frame` handler's reconcile latch fires.
    ///
    /// This mirrors exactly how Doom Retro presents under WIE: its SDL2.dll
    /// holds the main window's HDC, renders into a DIB section selected into
    /// a memory DC (`CreateDIBSection` + `CreateCompatibleDC` +
    /// `SelectObject`), and `SDL_UpdateWindowSurface`'s Windows framebuffer
    /// updater runs `BitBlt(window_dc ← mem_dc)` — the signature SDL2.dll
    /// imports from GDI32. Until such a blit publishes a frame there is no
    /// host window, only a dock icon, so this is the "visible window" proof.
    #[test]
    fn top_level_software_blit_publishes_frame_the_host_presenter_drains() {
        let mut engine = IcedCpu::open_x86_64();
        // Map a slice of the runtime process heap (the DIB backing buffer
        // lands there), plus scratch and stack regions for the guest-side
        // arguments.
        let heap_base = crate::memory::PROCESS_HEAP_BASE;
        engine
            .mem_map(heap_base, 0x10_0000, wie_cpu::RwxPerms::ALL)
            .expect("map process heap slice");
        engine
            .mem_map(SCRATCH_VA, 0x1_0000, wie_cpu::RwxPerms::ALL)
            .expect("map scratch");
        engine
            .mem_map(STACK_TOP, 0x1_000, wie_cpu::RwxPerms::ALL)
            .expect("map stack");
        engine
            .mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("dummy return address");
        engine.write_rsp(STACK_TOP).ok();

        let process = ProcessIdentity {
            module_file_name: "doomretro.exe".to_owned(),
            module_path: r"C:\Program Files\DoomRetro\doomretro.exe".to_owned(),
            current_directory: r"C:\Program Files\DoomRetro".to_owned(),
            command_line: "doomretro.exe".to_owned(),
        };
        let mut state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");

        // SDL_CreateWindow's main window: parentless and visible, 320×200.
        let hwnd: u64 = 0x6610_0101;
        state.window_state().windows.push(WindowRecord {
            handle: Hwnd::from(hwnd),
            title: "D00M".to_owned(),
            visible: true,
            width: 320,
            height: 200,
            ..Default::default()
        });
        // Exactly what the CreateWindowExA/W handlers do for parentless
        // windows: enter the host-visible window set (bumps `windows_rev`, the
        // Frame handler's reconcile latch).
        state.present().register_top_level(Hwnd::from(hwnd));
        let handle = GuestHandle {
            state: Arc::new(Mutex::new(state)),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };
        let env = || WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 0,
        };

        // GetDC(main) — the window DC SDL's framebuffer updater blits into.
        let window_dc = {
            let mut st = handle.state.lock().expect("lock");
            write_regs(&mut engine, hwnd, 0, 0, 0);
            let r = user32::handle_get_dc(&mut HandlerContext::new(&mut engine, env(), &mut st))
                .expect("GetDC");
            assert_ne!(r.return_value, 0, "GetDC returns a window DC");
            r.return_value
        };

        // SDL's framebuffer: a memory DC holding a top-down 320×200 32-bpp
        // DIB section (CreateCompatibleDC + CreateDIBSection + SelectObject).
        let mem_dc = {
            let mut st = handle.state.lock().expect("lock");
            write_regs(&mut engine, 0, 0, 0, 0);
            let r = gdi32::handle_create_compatible_dc(&mut HandlerContext::new(
                &mut engine,
                env(),
                &mut st,
            ))
            .expect("CreateCompatibleDC");
            assert_ne!(r.return_value, 0, "memory DC");
            r.return_value
        };
        let bmi_va = SCRATCH_VA + 0x100;
        let bits_out_va = SCRATCH_VA + 0x200;
        // One contiguous BITMAPINFOHEADER staging buffer instead of five
        // scattered slot writes; field offsets follow wingdi.h.
        let mut bmi = [0_u8; 40];
        bmi[0..4].copy_from_slice(&40_u32.to_le_bytes()); // biSize
        bmi[4..8].copy_from_slice(&320_i32.to_le_bytes()); // biWidth
        bmi[8..12].copy_from_slice(&(-200_i32).to_le_bytes()); // biHeight (top-down)
        bmi[12..14].copy_from_slice(&1_u16.to_le_bytes()); // biPlanes
        bmi[14..16].copy_from_slice(&32_u16.to_le_bytes()); // biBitCount
        engine.mem_write(bmi_va, &bmi).expect("BITMAPINFOHEADER");
        let (hbitmap, dib_bits_va) = {
            let mut st = handle.state.lock().expect("lock");
            write_regs(&mut engine, mem_dc, bmi_va, 0, bits_out_va);
            let r = gdi32::handle_create_dib_section(&mut HandlerContext::new(
                &mut engine,
                env(),
                &mut st,
            ))
            .expect("CreateDIBSection");
            assert_ne!(r.return_value, 0, "DIB handle");
            let mut bits = [0_u8; 8];
            engine
                .mem_read(bits_out_va, &mut bits)
                .expect("read *ppvBits");
            (r.return_value, u64::from_le_bytes(bits))
        };
        assert_ne!(dib_bits_va, 0, "DIB backing buffer");
        {
            let mut st = handle.state.lock().expect("lock");
            write_regs(&mut engine, mem_dc, hbitmap, 0, 0);
            let _ =
                gdi32::handle_select_object(&mut HandlerContext::new(&mut engine, env(), &mut st))
                    .expect("SelectObject");
        }

        // The renderer painted the whole DIB: BGRA (A=FF, B=0, G=0, R=FF)
        // → 0RGB red (0x00FF0000) in the present surface.
        let red_row = vec![0x00_u8, 0x00, 0xFF, 0xFF]; // B, G, R, A bytes → BGRA red
        let mut dib_bytes = Vec::with_capacity(320 * 200 * 4);
        for _ in 0..(320 * 200) {
            dib_bytes.extend_from_slice(&red_row);
        }
        engine
            .mem_write(dib_bits_va, &dib_bytes)
            .expect("fill DIB pixels");

        // BitBlt(window_dc, 0, 0, 320, 200, mem_dc, 0, 0, SRCCOPY) —
        // SDL_UpdateWindowSurface's Windows framebuffer blit, 1:1.
        {
            let mut st = handle.state.lock().expect("lock");
            write_regs(&mut engine, window_dc, 0, 0, 320);
            engine
                .mem_write(STACK_TOP + 0x28, &200_i32.to_le_bytes())
                .expect("cy");
            engine
                .mem_write(STACK_TOP + 0x30, &mem_dc.to_le_bytes())
                .expect("hdcSrc");
            engine
                .mem_write(STACK_TOP + 0x38, &0_i32.to_le_bytes())
                .expect("x1");
            engine
                .mem_write(STACK_TOP + 0x40, &0_i32.to_le_bytes())
                .expect("y1");
            engine
                .mem_write(STACK_TOP + 0x48, &0x00CC_0020_u32.to_le_bytes())
                .expect("SRCCOPY rop");
            let r = gdi32::handle_bit_blt(&mut HandlerContext::new(&mut engine, env(), &mut st))
                .expect("BitBlt");
            assert_eq!(r.return_value, 1, "BitBlt succeeds");
        }

        // The blit deferred a publish (one frame per repaint cycle): nothing
        // is in the published slot until the runtime drains at the idle
        // boundary — `take_frame` reads that same slot, so it sees nothing
        // before the drain.
        assert!(
            handle.take_frame(hwnd).is_none(),
            "the blit defers: nothing published pre-drain"
        );
        {
            let mut st = handle.state.lock().expect("lock");
            assert!(
                st.present().drain_pending_publishes() >= 1,
                "the runtime drain publishes the blit frame"
            );
        }

        // The host contract: take_frame drains the frame the presenter's
        // RedrawRequested reads; the window is a parentless top-level the
        // reconcile would build a winit window for; the window-set revision
        // advanced so the host Frame handler's reconcile latch fires.
        let frame = handle
            .take_frame(hwnd)
            .expect("take_frame drains the published frame");
        assert_eq!((frame.width, frame.height), (320, 200));
        assert_eq!(frame.pixels.len(), 320 * 200);
        assert_eq!(
            frame.pixels[0], 0x00FF_0000,
            "the blit's red pixels reached the frame"
        );
        assert_eq!(
            handle.guest_top_level_windows(),
            vec![(hwnd, "D00M".to_owned(), 320, 200)],
            "the parentless window is visible to the host reconcile"
        );
        assert_eq!(
            handle.windows_rev(),
            1,
            "the top-level create bumped the reconcile latch"
        );
    }
}
