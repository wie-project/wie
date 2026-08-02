//! Persistent `RuntimeSession`: guest setup, yield/resume, and API hook loop.

mod callback;
mod init;
mod menu;
mod profile;
mod pump;
mod window;

pub use self::menu::MenuNode;
pub use self::profile::RuntimeProfile;
pub use self::window::GuestHandle;

use crate::memory::RuntimeMemoryLayout;
use crate::mt_runtime::ProcessResources;
use crate::trace::EntryTraceTermination;
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};

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
const CRT_GUEST_BASE: u64 = 0x0000_0000_6800_0000;
const CRT_ARGV_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x308;
const CRT_ARGC_SLOT: u64 = CRT_GUEST_BASE + 0x310;
const CRT_ACMDLN_PTR_SLOT: u64 = CRT_GUEST_BASE + 0x328;
/// Pointer table for `char *argv[]` (null-terminated).
const CRT_ARGV_TABLE: u64 = CRT_GUEST_BASE + 0x400;
/// Storage for argv string bodies.
const CRT_ARGV_STRINGS: u64 = CRT_GUEST_BASE + 0x500;
const CRT_PAGE_END: u64 = CRT_GUEST_BASE + 0x1000;

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
    entry_point_va: u64,
    initial_rsp: u64,
    next_api_index: usize,
    no_hook_slices: usize,
    pending_callbacks: Vec<PendingGuestCallback>,
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
    /// B9: last observed present generation (frame-boundary sampling).
    frame_last_gen: u64,
    /// B9: `host_stops` baseline at the last frame boundary.
    frame_last_stops: u64,
    /// B9: iced-instruction baseline at the last frame boundary.
    frame_last_iced: u64,
    /// B9: jit-instruction baseline at the last frame boundary.
    frame_last_jit: u64,
}

impl RuntimeSession {
    /// Returns the PE entry-point address associated with this session.
    #[must_use]
    pub fn entry_point_va(&self) -> u64 {
        self.entry_point_va
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
        self.initial_rsp
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
    pub fn post_window_message(
        &mut self,
        window_handle: u64,
        message: u32,
        word_parameter: u64,
        long_parameter: u64,
    ) -> Result<()> {
        let queue_arc = self.process.message_queue_arc();
        let mut queue = queue_arc.lock().unwrap_or_else(|e| e.into_inner());
        let time = queue.next_message_time;
        queue.next_message_time = queue
            .next_message_time
            .checked_add(1)
            .context("runtime message timestamp overflow")?;
        queue.messages.push(wie_winapi::QueuedWindowMessage {
            window_handle: wie_winapi::handles::Hwnd::from(window_handle),
            message,
            word_parameter,
            long_parameter,
            time,
            point_x: 0,
            point_y: 0,
        });
        Ok(())
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
            menu_tree_cache: Arc::new(Mutex::new(None)),
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
