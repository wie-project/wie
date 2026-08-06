//! Persistent `RuntimeSession`: guest setup, yield/resume, and API hook loop.

mod callback;
mod init;
mod menu;
mod profile;
mod pump;
mod types;
mod window;

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
}

/// Guest CRT page layout (must match `wie_winapi::ucrt` and guest stubs).
pub(crate) const CRT_GUEST_BASE: u64 = 0x0000_0000_6800_0000;
const CRT_ENVIRON_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x300;
const CRT_ARGV_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x308;
const CRT_ARGC_SLOT: u64 = CRT_GUEST_BASE + 0x310;
const CRT_COMMODE_SLOT: u64 = CRT_GUEST_BASE + 0x318;
const CRT_FMODE_SLOT: u64 = CRT_GUEST_BASE + 0x320;
const CRT_ACMDLN_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x328;
/// Pointer table for `char *argv[]` (null-terminated).
const CRT_ARGV_TABLE: u64 = CRT_GUEST_BASE + 0x400;
/// Storage for argv string bodies.
const CRT_ARGV_STRINGS: u64 = CRT_GUEST_BASE + 0x500;
const CRT_PAGE_END: u64 = CRT_GUEST_BASE + 0x1000;

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
    /// Last value written to guest TEB.LastErrorValue (skip redundant mem_write).
    last_published_last_error: Option<u32>,
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
                let _ = wie_winapi::ensure_bottle_skeleton(r);
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
        let mut state = crate::mt_runtime::lock(&shared_winapi);
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
        let Ok(state) = self.state.lock() else {
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
#[cold]
fn invalid_memory_diagnostic(
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
    use wie_pe::ProcessIdentity;
    use wie_winapi::WindowRecord;
    use wie_winapi::handles::Hwnd;

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
}
