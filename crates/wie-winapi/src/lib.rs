//! WinAPI dispatcher model for WIE (generic PE64 userspace).

#![allow(clippy::type_complexity)]

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

pub mod advapi32;
pub mod bottle;
pub mod comctl32;
pub mod comdlg32;
pub mod console;
pub mod d3d9;
pub mod dll_loader;
pub mod dynamic_apis;
pub mod pthread;
pub use dynamic_apis::{DYNAMIC_FAKE_APIS, PREPLANTED_SOFT_APIS, resolve_get_proc_address};
pub mod exception;
pub mod fake_va;
pub mod gdi32;
pub mod guest_heap;
pub mod guest_io_host;
mod guest_memory;
mod guest_string;
pub mod idle;
pub mod kernel32;
pub mod mingw_dispatch;
pub mod msvc_eh;
pub mod ole32;
pub mod oleaut32;
pub mod seh;
pub mod shell32;
pub mod sync_obj;
pub mod thread;
pub mod ucrt;
pub mod user32;
pub mod uxtheme;
pub mod vfs;
pub mod winmm;
pub use bottle::{bottle_root_from_env, drive_d_from_env, guest_path_to_host};
pub use exception::{RuntimeFunction, lookup_function_entry};
pub use sync_obj::{
    CsWaitQueue, INFINITE, KernelObject, MAXIMUM_WAIT_OBJECTS, MultiWaitRequest, PendingSpawn,
    STILL_ACTIVE, SemaphoreObject, SyncState, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WaitTarget,
    wait_multiple,
};
pub use vfs::{VolumeConfig, ensure_bottle_skeleton};
#[cfg(test)]
mod exception_helpers;
#[cfg(test)]
mod exception_tests;
// HostParkReason is defined with WinApiControlSignal below.
pub use fake_va::{
    COM_IFACE_IDIRECT3D9, COM_IFACE_IDIRECT3DDEVICE9, FAKE_API_BASE, FAKE_API_SIZE, FakeVa,
    SPECIAL_CALLBACK_RETURN, SPECIAL_SEH_CONTINUE, callback_return_trampoline_va,
    decode as decode_fake_va, encode_alias, encode_com, encode_export, encode_unresolved,
    seh_continue_trampoline_va,
};
pub use guest_heap::GuestHeap;
pub use idle::{IdleContext, IdlePolicy};
pub use kernel32::WinApiHandlerResult;
pub use thread::{FIRST_WORKER_TID, GuestThread, PRIMARY_THREAD_ID, ThreadState};

/// Runtime environment values visible to WinAPI handlers.
#[derive(Debug, Clone, Copy)]
pub struct WinApiEnvironment {
    /// Main module image base.
    pub image_base: u64,

    /// Pointer to ANSI command line string in emulated memory.
    pub command_line_a_ptr: u64,

    /// Pointer to UTF-16 command line string in emulated memory.
    pub command_line_w_ptr: u64,

    /// Pointer to UTF-16 environment strings block in emulated memory.
    pub environment_strings_w_ptr: u64,

    /// Pointer to ANSI module file name string in emulated memory.
    pub module_file_name_a_ptr: u64,

    /// Pointer to UTF-16 module file name string in emulated memory.
    pub module_file_name_w_ptr: u64,

    /// Fake process heap handle.
    pub process_heap_handle: u64,
}

/// Heap and FLS (Fiber-Local Storage) state.
#[derive(Debug, Clone)]
pub struct HeapState {
    /// Process heap: segregated freelist + bump (see [`GuestHeap`]).
    pub heap: GuestHeap,
    /// Next fake `FLS` index.
    pub next_fls_index: u32,
    /// Fake `FLS` slots.
    pub fls_slots: Vec<FlsSlot>,
    /// Guest VA of the FLS value table (u64 slots), 0 if not installed.
    pub guest_fls_table_va: u64,
}

/// File I/O, VFS, and console stdin state.
#[derive(Debug, Clone)]
pub struct FileIoState {
    pub executable_file_size: u64,
    pub executable_file_bytes: Vec<u8>,
    pub executable_file_cursor: u64,
    pub next_find_handle: u64,
    pub find_handles: Vec<FindHandle>,
    pub host_file_mounts: Vec<HostFileMount>,
    pub virtual_files: Vec<VirtualGuestFile>,
    pub open_files: HashMap<u64, OpenGuestFile>,
    pub next_file_handle: u64,
    pub next_resource_handle: u64,
    pub resources: Vec<ResourceRecord>,
    pub current_directory_wide: Vec<u16>,
    pub bottle_root: Option<std::path::PathBuf>,
    pub volumes: VolumeConfig,
    pub guest_file_data_next: u64,
    pub guest_io: Option<GuestIoRuntimeConfig>,
    pub stdin_bytes: Vec<u8>,
    pub stdin_cursor: usize,
    pub stdin_mode: GuestStdinMode,
    pub ucrt_files: HashMap<u64, u64>,
    pub ucrt_next_file_va: u64,
    /// Cached open `File` handles for streaming host-backed guest files.
    ///
    /// Keyed by the guest-visible file handle. Populated lazily on the first
    /// streamed `ReadFile`/`WriteFile` for that handle and dropped on
    /// `CloseHandle`. Amortises the per-syscall `File::open` + seek + drop cost
    /// that dominated 7za-style workloads (a 500 MiB archive read in 64 KiB
    /// chunks previously did ~8k open/close/fstat triples per side).
    pub cached_streams: HashMap<u64, std::sync::Arc<std::sync::Mutex<std::fs::File>>>,
}

/// Thread-safe resolver for dynamic DLL imports.
///
/// Wraps a closure behind `Arc<Mutex<…>>` so [`ModuleState`] can derive
/// `Clone` and `Debug` without losing the closure's captured state.
type ImportResolverInner =
    std::sync::Arc<std::sync::Mutex<Box<dyn FnMut(&str, &str, u64) -> anyhow::Result<u64> + Send>>>;

#[derive(Clone)]
pub struct ImportResolver {
    inner: ImportResolverInner,
}

impl std::fmt::Debug for ImportResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportResolver").finish()
    }
}

impl ImportResolver {
    pub fn new(f: Box<dyn FnMut(&str, &str, u64) -> anyhow::Result<u64> + Send>) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(f)),
        }
    }

    pub fn resolve(&mut self, lib: &str, name: &str, slot: u64) -> anyhow::Result<u64> {
        // unwrap: the Mutex is not poisoned in practice (single-threaded use).
        self.inner.lock().unwrap_or_else(|e| e.into_inner())(lib, name, slot)
    }
}

/// DLL loading and export resolution cache.
#[derive(Debug, Clone)]
pub struct ModuleState {
    pub loaded_modules: HashMap<String, dll_loader::LoadedModule>,
    pub import_resolver: Option<ImportResolver>,
    pub get_proc_address_cache: std::collections::HashMap<String, GetProcAddressCacheEntry>,
    pub next_module_handle: u64,
}

/// Direct3D 9 rendering state.
#[derive(Debug, Clone, Default)]
pub struct D3D9State {
    pub d3d9_current_vertex_shader: u64,
    pub d3d9_current_fvf: u32,
    pub d3d9_render_states: Vec<(u32, u32)>,
    pub d3d9_texture_stage_states: Vec<(u32, u32, u32)>,
    pub d3d9_sampler_states: Vec<(u32, u32, u32)>,
    pub d3d9_device_object_address: u64,
    pub d3d9_device_ref_count: u32,
    pub d3d9_object_address: u64,
    pub d3d9_ref_count: u32,
}

/// Wrapper for the 256-byte keyboard state array. Exists so [`WindowState`]
/// can use `#[derive(Default)]` — bare `[u8; 256]` does not implement `Default`.
#[derive(Debug, Clone)]
pub struct KeyboardState(pub [u8; 256]);

impl Default for KeyboardState {
    fn default() -> Self {
        Self([0; 256])
    }
}

impl std::ops::Deref for KeyboardState {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::DerefMut for KeyboardState {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

/// Window, UI, and input state.
#[derive(Debug, Clone, Default)]
pub struct WindowState {
    pub window_long_ptr_values: Vec<(u64, i64, u64)>,
    pub image_list_counts: Vec<(u64, u64)>,
    pub image_list_background_colors: Vec<(u64, u32)>,
    pub window_visible: bool,
    pub window_enabled: bool,
    pub active_window_handle: u64,
    pub foreground_window_handle: u64,
    pub focus_window_handle: u64,
    pub capture_window_handle: u64,
    pub cursor_handle: u64,
    pub window_title: String,
    pub window_x: i32,
    pub window_y: i32,
    pub window_width: i32,
    pub window_height: i32,
    pub window_invalidated: bool,
    pub tick_count: u64,
    pub keyboard_state: KeyboardState,
    pub next_timer_id: u64,
    pub timers: Vec<TimerRecord>,
    pub next_global_atom: u16,
    pub global_atoms: Vec<GlobalAtomRecord>,
    pub next_windows_hook_handle: u64,
    pub windows_hooks: Vec<WindowsHookRecord>,
    pub menu_item_states: Vec<(u64, u32, u32)>,
    pub menu_item_check_states: Vec<(u64, u32, u32)>,
    pub message_queue: Vec<QueuedWindowMessage>,
    pub next_message_time: u32,
    pub message_queue_idle_policy: MessageQueueIdlePolicy,
    pub next_window_class_atom: u16,
    pub window_classes: Vec<WindowClassRecord>,
    pub next_window_handle: u64,
    pub windows: Vec<WindowRecord>,
    pub file_dialog_policy: FileDialogPolicy,
    pub last_file_dialog_path: Option<String>,
    pub comm_dlg_extended_error: u32,
    pub next_menu_handle: u64,
}

/// Process-level state (identity, error handling, registry, misc).
#[derive(Debug, Clone)]
pub struct ProcessState {
    pub last_error: u32,
    pub next_registry_key_handle: u64,
    pub registry_keys: Vec<RegistryKey>,
    pub main_module_file_name: String,
    pub main_module_path: String,
    pub main_module_host_dir: Option<std::path::PathBuf>,
    pub error_mode: u32,
    pub suspended_threads: HashMap<u32, u32>,
    /// Win32 process environment, as `(name, value)` in insertion order.
    ///
    /// Kept beside the guest-memory block that `GetEnvironmentStringsW`
    /// returns rather than inside it. Windows draws the same distinction: the
    /// block is a snapshot copy, so a later `SetEnvironmentVariable` is visible
    /// to `GetEnvironmentVariable` without rewriting memory the guest may still
    /// hold a pointer into.
    pub environment: Vec<(String, String)>,
}

/// Environment every guest process starts with.
///
/// Single source of truth: `wie-runtime` builds the in-guest UTF-16 block from
/// this same list, so the block and [`ProcessState::environment`] cannot drift.
pub const DEFAULT_ENVIRONMENT: &[(&str, &str)] = &[
    ("PATH", "C:\\Windows\\System32"),
    ("TEMP", "C:\\Users\\WIE\\AppData\\Local\\Temp"),
    ("TMP", "C:\\Users\\WIE\\AppData\\Local\\Temp"),
    ("SystemRoot", "C:\\Windows"),
    ("windir", "C:\\Windows"),
    ("COMPUTERNAME", "WIE"),
    ("USERNAME", "WIE"),
    ("OS", "Windows_NT"),
    ("PROCESSOR_ARCHITECTURE", "AMD64"),
    ("NUMBER_OF_PROCESSORS", "4"),
];

/// Fake VA for the pthread return trampoline.
pub const PTHREAD_RETURN_TRAMPOLINE_VA: u64 = 0x7000_0000_0000_FF00;

/// Return the fake VA for the pthread return trampoline.
#[must_use]
pub fn pthread_return_trampoline_va() -> u64 {
    PTHREAD_RETURN_TRAMPOLINE_VA
}

/// Kernel execution state (threading, synchronisation, SEH).
#[derive(Debug, Clone)]
pub struct KernelState {
    pub threads: ThreadState,
    pub sync: SyncState,
    /// Per-thread pending SEH / MSVC-EH sequences, keyed by guest TID.
    ///
    /// Previously a single `Option<SehPending>` at process scope — that raced
    /// when two guest threads threw concurrently: whichever throw grabbed the
    /// shared WinAPI mutex second overwrote the first thread's payload, and
    /// the second thread later found nothing in the slot and unwound past its
    /// own catch (see `cpp_threads` micro). Per-TID storage isolates them.
    pub seh_pending: std::collections::HashMap<u32, seh::SehPending>,
}

/// Identifies a slot in [`DllStateMap`]. One variant per emulated DLL
/// that carries host-side state.
///
/// # Adding a new DLL
///
/// 1. Add a variant here.
/// 2. Add a `None` to the array in [`DllStateMap::new`].
/// 3. Add a match arm to [`DllStateMap::slot_of`].
/// 4. Add an accessor on [`WinApiState`].
///
/// That is the entire change. One file, four lines.
/// [`DllId::COUNT`] is derived from the number of variants and must match
/// the `new()` array — the compile-time assertion at [`DLL_ID_SLOT_COUNT`]
/// catches mismatches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DllId {
    Console,
    Window,
    D3D9,
    Pthread,
}

impl DllId {
    /// Number of variants. The assertion at [`DLL_ID_SLOT_COUNT`] ensures
    /// it stays in sync with the slot array in [`DllStateMap::new`].
    pub const COUNT: usize = 4;
}

/// Lazy DLL state storage. Fixed-size array, zero per-call overhead.
///
/// Each slot is `Option<Box<dyn Any + Send>>` — a nullable fat pointer
/// (16 bytes) when unloaded. Access is a direct array index + one `TypeId`
/// compare. No hash, no indirect dispatch.
///
/// `WinApiState` is shared behind `Arc<Mutex<>>` and is **never cloned**.
/// Do not add `Clone` to this type or to [`WinApiState`].
///
/// # Adding a new DLL (alongside [`DllId`])
///
/// Add a `None` to the array in `new()` and a match arm to `slot_of`.
/// Both must stay in sync — the const assertion catches drift.
pub struct DllStateMap {
    slots: [Option<Box<dyn Any + Send>>; DllId::COUNT],
}

// The struct field `slots: [Option<Box<dyn Any + Send>>; DllId::COUNT]`
// and the `new()` array literal are kept in sync by the type system:
// a mismatch in element count is a compile error.

impl Default for DllStateMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a [`DllId`] to its index in the slot array. Matches explicitly
/// so the compiler warns if a variant is added without a corresponding arm.
const fn dll_index(id: DllId) -> usize {
    match id {
        DllId::Console => 0,
        DllId::Window => 1,
        DllId::D3D9 => 2,
        DllId::Pthread => 3,
    }
}

impl std::fmt::Debug for DllStateMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loaded: Vec<&str> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|_| slot_of(i)))
            .collect();
        f.debug_struct("DllStateMap")
            .field("loaded", &loaded)
            .finish()
    }
}

/// Slot label for debug output. Add a match arm per new [`DllId`] variant.
const fn slot_of(i: usize) -> &'static str {
    match i {
        0 => "console",
        1 => "window",
        2 => "d3d9",
        3 => "pthread",
        _ => "?",
    }
}

impl DllStateMap {
    /// All slots start unloaded. Add a `None` per new [`DllId`] variant.
    pub fn new() -> Self {
        Self {
            slots: [None, None, None, None],
        }
    }

    /// Access the state for `id`, heap-allocating a default on first call.
    ///
    /// # Panics
    /// If the slot type does not match `T` — a programming error when a
    /// `DllId` variant is reused for a different type.
    pub fn get_or_init<T: Default + Send + 'static>(&mut self, id: DllId) -> &mut T {
        let idx = dll_index(id);
        let Some(slot) = self.slots.get_mut(idx) else {
            std::process::abort();
        };
        slot.get_or_insert_with(|| Box::new(T::default()));
        let Some(boxed) = slot.as_mut() else {
            std::process::abort();
        };
        let Some(t) = boxed.as_mut().downcast_mut::<T>() else {
            std::process::abort();
        };
        t
    }

    /// Read-only access — returns `None` if the slot was never initialised.
    pub fn get<T: 'static>(&self, id: DllId) -> Option<&T> {
        let boxed = self.slots.get(dll_index(id))?.as_ref()?;
        boxed.as_ref().downcast_ref::<T>()
    }
}

pub struct WinApiState {
    /// Heap + FLS state.
    pub heap_state: HeapState,
    /// File I/O, VFS, and console stdin state.
    pub file_io: FileIoState,
    /// DLL loading and export resolution cache.
    pub module_state: ModuleState,
    /// Process-level state (error, registry, identity, misc).
    pub process: ProcessState,
    /// Kernel execution state (threading, sync, SEH).
    pub kernel: KernelState,
    /// On-demand state for optional WIE-hosted DLLs.
    pub dll_states: DllStateMap,
}

// NOTE: WinApiState is deliberately not Clone. It lives behind
// Arc<Mutex<>> in the MT runtime and is never copied per thread.

// Manual Debug impl: Box<dyn FnMut + Send> does not implement Debug.
impl std::fmt::Debug for WinApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WinApiState")
            .field("heap_state", &self.heap_state)
            .field("file_io", &self.file_io)
            .field("dll_states", &self.dll_states)
            .field("module_state", &self.module_state)
            .field("process", &self.process)
            .field("kernel", &self.kernel)
            .finish()
    }
}

// Manual Clone impl: Box<dyn FnMut + Send> does not implement Clone.
// ── DLL state accessors ─────────────────────────────────────────────────
//
// When adding a new DLL, add a pair of methods here (mut + try_)
// and a variant to [`DllId`]. That is the only change needed.
impl WinApiState {
    /// Flush buffered Stream-mode console output to the host terminal.
    /// Called at frame boundaries (Sleep, _getch, etc.) so multiple
    /// WriteConsole calls within one frame render atomically.
    pub fn flush_console(&mut self) {
        // get_or_init is fine — if the console state hasn't been allocated
        // yet there's nothing to flush, and allocating an empty state is cheap.
        self.console().flush_stream_output();
    }

    /// Mutable access — lazy-initialises on first call.
    pub fn console(&mut self) -> &mut console::ConsoleState {
        self.dll_states
            .get_or_init::<console::ConsoleState>(DllId::Console)
    }
    pub fn window_state(&mut self) -> &mut WindowState {
        self.dll_states.get_or_init::<WindowState>(DllId::Window)
    }
    pub fn d3d9(&mut self) -> &mut D3D9State {
        self.dll_states.get_or_init::<D3D9State>(DllId::D3D9)
    }
    pub fn pthread(&mut self) -> &mut pthread::PthreadState {
        self.dll_states
            .get_or_init::<pthread::PthreadState>(DllId::Pthread)
    }

    /// Read-only access — returns `None` if the state was never initialised.
    /// Use when the caller only holds `&Self`.
    pub fn try_console(&self) -> Option<&console::ConsoleState> {
        self.dll_states.get::<console::ConsoleState>(DllId::Console)
    }
    pub fn try_window_state(&self) -> Option<&WindowState> {
        self.dll_states.get::<WindowState>(DllId::Window)
    }
    pub fn try_d3d9(&self) -> Option<&D3D9State> {
        self.dll_states.get::<D3D9State>(DllId::D3D9)
    }
    pub fn try_pthread(&self) -> Option<&pthread::PthreadState> {
        self.dll_states.get::<pthread::PthreadState>(DllId::Pthread)
    }
}

/// Bundle of everything a WinAPI handler may need.
///
/// Passed as `HandlerContext` to every handler so adding new context
/// fields doesn't touch handler signatures and the dispatch table is uniform.
pub struct HandlerContext<'a> {
    /// CPU engine (mem_read / mem_write / register access).
    pub engine: &'a mut dyn CpuEngine,
    /// Session environment (image base, command line, heap handle, …).
    pub environment: WinApiEnvironment,
    /// Full emulator state.
    pub state: &'a mut WinApiState,
}

impl<'a> HandlerContext<'a> {
    pub fn new(
        engine: &'a mut dyn CpuEngine,
        environment: WinApiEnvironment,
        state: &'a mut WinApiState,
    ) -> Self {
        Self {
            engine,
            environment,
            state,
        }
    }
}

/// Source policy for console `ReadFile(STD_INPUT_HANDLE)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GuestStdinMode {
    /// Serve only [`WinApiState::stdin_bytes`]; exhausted buffer → EOF (0 bytes).
    ///
    /// Used for `--stdin FILE` and deterministic micro-tests (no TTY hang).
    #[default]
    InjectOnly,
    /// When the buffer is empty, block-fill one host line (Microsoft Learn
    /// default console line input / `ENABLE_LINE_INPUT` approximation).
    LiveHost,
}

/// Runtime-published guest I/O layout (filled by `wie-runtime` at session start).
#[derive(Debug, Clone)]
pub struct GuestIoRuntimeConfig {
    /// Guest VA of the open-file handle table.
    pub table_va: u64,
    /// Guest VA of the file-content mirror arena base.
    pub file_data_base: u64,
    /// Size in bytes of the file-content mirror arena.
    pub file_data_size: usize,
}

/// Host-side decision for `GetOpenFileName` / `GetSaveFileName`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FileDialogPolicy {
    #[default]
    /// Simulate the user cancelling the dialog (`return FALSE`).
    Cancel,

    /// Simulate the user accepting `path` (`return TRUE` and fill `lpstrFile`).
    Accept {
        /// Absolute or relative Windows-style path written into the dialog buffer.
        path: String,
    },
}

/// One host file exposed to the guest under one or more Windows paths.
#[derive(Debug, Clone)]
pub struct HostFileMount {
    /// Preferred guest path (Windows-style), e.g. `C:\LunarMagic\game.sfc`.
    pub guest_path: String,

    /// Absolute path on the host filesystem.
    pub host_path: std::path::PathBuf,
}

/// One open guest file handle returned by `CreateFile*`.
#[derive(Debug, Clone)]
pub struct OpenGuestFile {
    /// Fake handle value.
    pub handle: u64,

    /// Guest path used to open the file.
    ///
    /// Stored as `Arc<str>` so the many `open_file.path.clone()` in Read/Write
    /// / SetFilePointer / SetEndOfFile hot loops are refcount bumps instead of
    /// full String copies. 100k-Read streams stop allocating 100k Strings.
    pub path: Arc<str>,

    /// File contents (working buffer). Empty when [`Self::streaming`] is true.
    pub bytes: Vec<u8>,

    /// Current read/write cursor.
    pub cursor: u64,

    /// When set, file is bottle/mount/D-backed and may be flushed/streamed here.
    ///
    /// `Arc<Path>` for the same reason as [`Self::path`] — streaming Read/Write
    /// clones per syscall.
    pub host_path: Option<Arc<std::path::Path>>,

    /// Large host file: I/O via `host_path` seek/read/write without full buffer.
    pub streaming: bool,

    /// Guest VA of mirrored file bytes for in-guest ReadFile (if registered).
    pub guest_data_va: Option<u64>,

    /// Index into the guest I/O handle table (if registered).
    pub guest_slot_index: Option<u32>,
}

impl OpenGuestFile {
    /// Logical file size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        if self.streaming {
            // Route through the VFS stat cache rather than a bare `metadata`
            // syscall — streaming WriteFile queries this per call.
            self.host_path
                .as_ref()
                .map_or(0, |p| vfs::host_file_len(p).unwrap_or(0))
        } else {
            u64::try_from(self.bytes.len()).unwrap_or(0)
        }
    }
}

/// A pure-guest virtual file (not backed by a host path).
#[derive(Debug, Clone)]
pub struct VirtualGuestFile {
    /// Guest Windows path.
    pub guest_path: String,

    /// Mutable contents.
    pub bytes: Vec<u8>,
}

/// One cached `GetProcAddress` resolution.
#[derive(Debug, Clone)]
pub struct GetProcAddressCacheEntry {
    /// Normalized (lowercase) export name.
    pub name: Arc<str>,

    /// Module handle that first requested this export.
    pub module_handle: u64,

    /// Resolved fake target VA (may be zero for probed-but-absent exports).
    pub address: u64,

    /// How many times this export was resolved.
    pub hit_count: u64,
}

/// Registered fake USER32 window class.
#[derive(Debug, Clone)]
pub struct WindowClassRecord {
    /// Atom returned by `RegisterClassExA/W`.
    pub atom: u16,

    /// Registered class name.
    pub class_name: String,

    /// Guest address of the class window procedure.
    pub window_proc: u64,

    /// Class style flags.
    pub style: u32,

    /// Module instance associated with the class.
    pub instance_handle: u64,

    /// Default icon handle.
    pub icon_handle: u64,

    /// Default cursor handle.
    pub cursor_handle: u64,

    /// Background brush handle.
    pub background_brush: u64,

    /// Small icon handle.
    pub small_icon_handle: u64,

    /// Whether the class was registered through the Unicode API.
    pub unicode: bool,
}

/// USER32 window created inside the compatibility runtime.
#[derive(Debug, Clone, Default)]
pub struct WindowRecord {
    /// Runtime-owned fake HWND.
    pub handle: u64,

    /// Registered class atom.
    pub class_atom: u16,

    /// Registered class name.
    pub class_name: String,

    /// Guest address of the window procedure.
    pub window_proc: u64,

    /// Whether the class uses the Unicode window procedure contract.
    pub unicode: bool,

    /// Window title.
    pub title: String,

    /// Standard window style flags.
    pub style: u32,

    /// Extended window style flags.
    pub extended_style: u32,

    /// Parent or owner window.
    pub parent_handle: u64,

    /// Menu handle or child-window identifier.
    pub menu_handle: u64,

    /// Module instance passed to `CreateWindowExA/W`.
    pub instance_handle: u64,

    /// Initial horizontal position.
    pub x: i32,

    /// Initial vertical position.
    pub y: i32,

    /// Initial width.
    pub width: i32,

    /// Initial height.
    pub height: i32,

    /// Current visibility state.
    pub visible: bool,

    /// Current enabled state.
    pub enabled: bool,
}

/// Request to invoke a function located inside guest executable code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestCallbackRequest {
    /// Guest address of the callback function.
    pub callback_address: u64,

    /// Target runtime-owned window handle.
    pub window_handle: u64,

    /// Numeric Windows message identifier.
    pub message: u32,

    /// Message word parameter.
    pub word_parameter: u64,

    /// Message long parameter.
    pub long_parameter: u64,

    /// Whether the target window class uses the Unicode contract.
    pub unicode: bool,
}

/// A queued fake USER32 message.
#[derive(Debug, Clone)]
pub struct QueuedWindowMessage {
    /// Target window handle.
    pub window_handle: u64,

    /// Numeric Windows message identifier.
    pub message: u32,

    /// Message word parameter.
    pub word_parameter: u64,

    /// Message long parameter.
    pub long_parameter: u64,

    /// Deterministic fake message timestamp.
    pub time: u32,

    /// Fake cursor X coordinate.
    pub point_x: i32,

    /// Fake cursor Y coordinate.
    pub point_y: i32,
}

/// Registered fake USER32 hook.
#[derive(Debug, Clone)]
pub struct WindowsHookRecord {
    /// Fake hook handle returned to the guest.
    pub handle: u64,

    /// Hook type such as `WH_CBT` or `WH_CALLWNDPROC`.
    pub hook_type: i32,

    /// Guest hook procedure address.
    pub callback_address: u64,

    /// Optional module handle supplied by the guest.
    pub module_handle: u64,

    /// Target thread identifier, or zero for a global hook.
    pub thread_id: u32,
}

/// Fake global atom table entry.
#[derive(Debug, Clone)]
pub struct GlobalAtomRecord {
    /// Atom identifier.
    pub atom: u16,

    /// Stored ANSI atom name.
    pub name: String,
}

/// Fake USER32 timer record.
#[derive(Debug, Clone)]
pub struct TimerRecord {
    /// Window associated with the timer, or zero for a thread timer.
    pub window_handle: u64,

    /// Timer identifier.
    pub timer_id: u64,

    /// Requested timer interval in milliseconds.
    pub interval_ms: u32,

    /// Optional guest timer callback address.
    pub callback_address: u64,
}

/// Fake resource record.
#[derive(Debug, Clone)]
pub struct ResourceRecord {
    /// Fake resource handle.
    pub handle: u64,

    /// Fake loaded resource handle.
    pub loaded_handle: u64,

    /// Pointer to fake resource bytes.
    pub data_ptr: u64,

    /// Resource size.
    pub size: u32,
}

/// Fake find-file handle (materialized directory enumeration).
///
/// `remaining` is a `VecDeque` so `FindNextFile` pops in O(1) via `pop_front`.
/// Was `Vec<DirEntry>` + `remove(0)`, i.e. O(n) shift per FindNext — scanning a
/// directory with N files became O(N²).
#[derive(Debug, Clone)]
pub struct FindHandle {
    /// Fake find handle.
    pub handle: u64,

    /// Search pattern/path as provided by the guest.
    pub pattern: String,

    /// Remaining entries after the one returned by FindFirst (FindNext consumes).
    pub remaining: std::collections::VecDeque<vfs::DirEntry>,
}

/// Fake registry key.
#[derive(Debug, Clone)]
pub struct RegistryKey {
    /// Fake registry key handle.
    pub handle: u64,

    /// Parent key handle.
    pub parent: u64,

    /// Subkey path.
    pub subkey: String,
}

/// Fake heap allocation record.
#[derive(Debug, Clone)]
pub struct HeapAllocation {
    /// Allocation base address.
    pub address: u64,

    /// Allocation size in bytes.
    pub size: u64,
}

/// Fake `FLS` slot.
#[derive(Debug, Clone)]
pub struct FlsSlot {
    /// Slot index.
    pub index: u32,

    /// Slot value.
    pub value: u64,
}

/// Behavior of `GetMessageA` when no matching message is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageQueueIdlePolicy {
    #[default]
    /// Produce a synthetic `WM_QUIT`.
    ///
    /// This preserves the deterministic bootstrap regression path.
    ExitOnIdle,

    /// Yield execution back to the runtime without modifying the guest `MSG`.
    ///
    /// This will be used by the persistent interactive runtime.
    YieldOnIdle,
}

/// Non-error control signal emitted by a WinAPI handler.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum WinApiControlSignal {
    /// `GetMessageA` cannot continue until a message becomes available.
    #[error("waiting for a window message")]
    WaitingForMessage,

    /// `DispatchMessageA/W` requires execution of a guest window procedure.
    #[error("guest window callback requested: {request:?}")]
    GuestCallbackRequested {
        /// Description of the pending guest callback.
        request: GuestCallbackRequest,
    },

    /// Host thread must park (drop CPU lock) then retry / continue (MT.2/3).
    #[error("host park: {reason:?}")]
    HostPark {
        /// Why the host thread is parking.
        reason: HostParkReason,
    },

    /// Guest `ExitThread` — worker run loop should terminate this host thread.
    #[error("exit thread code={code}")]
    ExitThread {
        /// Thread exit code.
        code: u32,
    },
}

/// Reason for [`WinApiControlSignal::HostPark`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostParkReason {
    /// Waiting to enter critical section at guest VA.
    CriticalSection {
        /// Guest `RTL_CRITICAL_SECTION*`.
        cs: u64,
    },
    /// Waiting on a pthread object.
    PthreadWait,
    /// `WaitForSingleObject` (or similar) on a kernel handle.
    WaitObject {
        /// Kernel handle.
        handle: u64,
        /// Timeout in ms (`INFINITE` = forever).
        timeout_ms: u32,
    },
    /// `WaitForMultipleObjects` — handles live in [`SyncState::multi_wait`].
    ///
    /// Kept small/`Copy` so [`WinApiControlSignal`] stays compact; the handle
    /// list is stored on process sync state for the duration of the park.
    WaitMultiple,
}

mod dispatch_table;
pub use dispatch_table::{
    WINAPI_ID_COUNT, WinApiId, WinApiTraits, dispatch_winapi, dispatch_winapi_id,
    is_winapi_implemented, resolve_winapi_id, winapi_id_export,
};
use wie_cpu::CpuEngine;

#[cfg(test)]
#[expect(clippy::expect_used)]
mod tests {
    use super::*;
    use wie_cpu::{CpuEngine, IcedCpu};

    const STACK_VA: u64 = 0x100_0000;
    const STACK_SIZE: usize = 0x1_0000;
    // STACK_VA + STACK_SIZE - 0x100 (leave room for a dummy return address).
    const STACK_TOP: u64 = 0x100_FF00;

    /// Minimal engine for handler unit tests: maps guest pages with a valid return address on the stack.
    fn test_engine() -> IcedCpu {
        let mut cpu = IcedCpu::open_x86_64();
        cpu.mem_map(0x1000, 0x10_0000, wie_cpu::RwxPerms::ALL)
            .expect("map test memory");
        cpu.mem_map(STACK_VA, STACK_SIZE, wie_cpu::RwxPerms::ALL)
            .expect("map test stack");
        // Write a dummy return address — every handler calls return_from_win64_api which reads it.
        cpu.mem_write(STACK_TOP, &0_u64.to_le_bytes())
            .expect("write return address");
        cpu.write_rsp(STACK_TOP).ok();
        cpu
    }

    fn write_regs(cpu: &mut IcedCpu, rcx: u64, rdx: u64, r8: u64, r9: u64, rsp: u64) {
        cpu.write_rcx(rcx).ok();
        cpu.write_rdx(rdx).ok();
        cpu.write_r8(r8).ok();
        cpu.write_r9(r9).ok();
        cpu.write_rsp(if rsp == 0 { STACK_TOP } else { rsp }).ok();
    }

    fn default_env() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0x0000_0000_1400_0000,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 1,
        }
    }

    /// Low 32 bits of RAX as signed LONG (Win64 return convention for Interlocked*).
    fn rax_low_i32(rax: u64) -> i32 {
        i32::from_le_bytes(u32::try_from(rax & 0xffff_ffff).unwrap_or(0).to_le_bytes())
    }

    fn default_winapi_state() -> WinApiState {
        // Simplified default with a bump heap covering [0x2000, 0x10000).
        let mut heap = GuestHeap::new(0x2000, 0x10000);
        heap.attach_guest_control(0x2000);
        WinApiState {
            heap_state: HeapState {
                heap,
                ..winapi_state_default().heap_state
            },
            ..winapi_state_default()
        }
    }

    fn winapi_state_default() -> WinApiState {
        // This must stay in sync with the fields of WinApiState.
        // Only the heap is customised; everything else is default.
        WinApiState {
            heap_state: HeapState {
                heap: GuestHeap::new(0x2000, 0x10000),
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Vec::new(),
                executable_file_cursor: 0,
                next_find_handle: 0,
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: HashMap::new(),
                next_file_handle: 0,
                next_resource_handle: 0,
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
                next_registry_key_handle: 0,
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: HashMap::new(),
                environment: DEFAULT_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            module_state: ModuleState {
                loaded_modules: HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: HashMap::new(),
                next_module_handle: dll_loader::REAL_MODULE_HANDLE_BASE,
            },
        }
    }

    /// All-zero environment for handlers that don't read it.
    fn test_environment() -> WinApiEnvironment {
        WinApiEnvironment {
            image_base: 0,
            command_line_a_ptr: 0,
            command_line_w_ptr: 0,
            environment_strings_w_ptr: 0,
            module_file_name_a_ptr: 0,
            module_file_name_w_ptr: 0,
            process_heap_handle: 0,
        }
    }

    macro_rules! assert_return_value {
        ($result:expr, $expected:expr) => {
            let r = $result.expect("handler should succeed");
            assert_eq!(r.return_value, $expected, "return value mismatch");
        };
    }

    // --- Kernel32 ---

    #[test]
    fn test_critical_section_reenter_single_thread() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // `test_engine` maps [0x1000, 0x101000); place CS there.
        let cs = 0x3000_u64;
        write_regs(&mut engine, cs, 0, 0, 0, 0);
        kernel32::handle_initialize_critical_section(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("init");
        write_regs(&mut engine, cs, 0, 0, 0, 0);
        kernel32::handle_enter_critical_section(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("enter1");
        write_regs(&mut engine, cs, 0, 0, 0, 0);
        kernel32::handle_enter_critical_section(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("enter2");
        let mut rec = [0_u8; 4];
        engine.mem_read(cs + 12, &mut rec).expect("read recursion");
        assert_eq!(u32::from_le_bytes(rec), 2);
        let mut owner = [0_u8; 8];
        engine.mem_read(cs + 16, &mut owner).expect("read owner");
        assert_eq!(u64::from_le_bytes(owner), u64::from(PRIMARY_THREAD_ID));
        write_regs(&mut engine, cs, 0, 0, 0, 0);
        kernel32::handle_leave_critical_section(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("leave1");
        write_regs(&mut engine, cs, 0, 0, 0, 0);
        kernel32::handle_leave_critical_section(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("leave2");
        engine
            .mem_read(cs + 16, &mut owner)
            .expect("read owner unlocked");
        assert_eq!(u64::from_le_bytes(owner), 0);
        assert_eq!(state.kernel.threads.current_tid(), PRIMARY_THREAD_ID);
    }

    #[test]
    fn test_interlocked_ops_host_atomics() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let cell = 0x4000_u64;
        // Zero cell.
        engine.mem_write(cell, &0_i32.to_le_bytes()).expect("zero");

        // Increment → 1
        write_regs(&mut engine, cell, 0, 0, 0, 0);
        let r = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedIncrement")
        }
        .expect("dispatch")
        .expect("handled");
        assert_eq!(rax_low_i32(r.return_value), 1);

        // ExchangeAdd(+5) returns previous 1, cell becomes 6
        write_regs(&mut engine, cell, 5, 0, 0, 0);
        let r = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedExchangeAdd")
        }
        .expect("dispatch")
        .expect("handled");
        assert_eq!(rax_low_i32(r.return_value), 1);

        // CompareExchange success 6→99
        write_regs(&mut engine, cell, 99, 6, 0, 0);
        let r = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedCompareExchange")
        }
        .expect("dispatch")
        .expect("handled");
        assert_eq!(rax_low_i32(r.return_value), 6);

        // CompareExchange fail (expect 6, still 99)
        write_regs(&mut engine, cell, 1, 6, 0, 0);
        let r = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedCompareExchange")
        }
        .expect("dispatch")
        .expect("handled");
        assert_eq!(rax_low_i32(r.return_value), 99);

        let mut bytes = [0_u8; 4];
        engine.mem_read(cell, &mut bytes).expect("read");
        assert_eq!(i32::from_le_bytes(bytes), 99);

        // 64-bit Increment64
        let cell64 = 0x4010_u64;
        engine
            .mem_write(cell64, &10_i64.to_le_bytes())
            .expect("zero64");
        write_regs(&mut engine, cell64, 0, 0, 0, 0);
        let r = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            kernel32::dispatch_kernel32_extra(&mut ctx, "InterlockedIncrement64")
        }
        .expect("dispatch")
        .expect("handled");
        assert_eq!(i64::from_le_bytes(r.return_value.to_le_bytes()), 11);
    }

    #[test]
    fn test_free_library_valid_handle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x6100_0001, 0, 0, 0, 0);
        assert_return_value!(
            kernel32::handle_free_library(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
        assert_eq!(state.process.last_error, 0);
    }

    #[test]
    fn test_free_library_null_handle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0, 0, 0, 0, 0);
        assert_return_value!(
            kernel32::handle_free_library(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0
        );
        assert_eq!(state.process.last_error, 6); // ERROR_INVALID_HANDLE
    }

    #[test]
    fn test_get_last_error() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        state.process.last_error = 123;
        let r = kernel32::handle_get_last_error(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetLastError");
        assert_eq!(r.return_value, 123);
    }

    #[test]
    fn test_set_last_error() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        state.process.last_error = 0;
        write_regs(&mut engine, 456, 0, 0, 0, 0);
        let _ = kernel32::handle_set_last_error(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetLastError");
        assert_eq!(state.process.last_error, 456);
    }

    #[test]
    fn test_heap_free_double_free_returns_false() {
        let mut engine = test_engine();
        let mut state = winapi_state_default();
        let p = state.heap_state.heap.alloc(64);
        assert_ne!(p, 0);

        write_regs(&mut engine, 0x1, 0, p, 0, 0);
        let r = kernel32::handle_heap_free(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("HeapFree");
        assert_eq!(r.return_value, 1, "first free must succeed");

        state.process.last_error = 0;
        write_regs(&mut engine, 0x1, 0, p, 0, 0);
        let r = kernel32::handle_heap_free(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("HeapFree double");
        assert_eq!(r.return_value, 0, "double free must return FALSE");
        assert_eq!(state.process.last_error, 6, "ERROR_INVALID_HANDLE");
    }

    // --- User32 ---

    #[test]
    fn test_get_async_key_state_default() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // VK_RETURN = 0x0D, keyboard_state starts all zero.
        write_regs(&mut engine, 0x0D, 0, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_async_key_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0
        );
    }

    #[test]
    fn test_get_async_key_state_down() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // VK_RETURN high bit set — index is a compile-time constant in bounds.
        state.window_state().keyboard_state.0[0x0D] = 0x80;
        write_regs(&mut engine, 0x0D, 0, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_async_key_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0x81
        );
    }

    #[test]
    fn test_peek_message_a_empty_queue() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // Write a valid MSG struct address (doesn't matter since queue is empty).
        write_regs(&mut engine, 0x1000, 0, 0, 0, 0x2000);
        assert_return_value!(
            user32::handle_peek_message_a(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0
        );
    }

    #[test]
    fn test_peek_message_a_with_message() {
        use crate::QueuedWindowMessage;
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let msg_va = 0x4000;
        // Map memory for the MSG struct.
        engine
            .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
            .expect("map msg struct");
        // Push a WM_PAINT message for any window.
        state
            .window_state()
            .message_queue
            .push(QueuedWindowMessage {
                window_handle: 0x100,
                message: 15, // WM_PAINT
                word_parameter: 0,
                long_parameter: 0,
                time: 1,
                point_x: 0,
                point_y: 0,
            });
        // PeekMessageA(msg_ptr=msg_va, hwnd=0, min=0, max=0, wRemoveMsg=1)
        // wRemoveMsg is on the stack at RSP+0x28.
        write_regs(&mut engine, msg_va, 0, 0, 0, 0x3000);
        // Write wRemoveMsg=1 (PM_REMOVE) at RSP+0x28.
        engine.mem_write(0x3028, &1_u32.to_le_bytes()).ok();
        assert_return_value!(
            user32::handle_peek_message_a(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
        // WM_PAINT should have been removed from the queue.
        assert_eq!(state.window_state().message_queue.len(), 0);
    }

    #[test]
    fn test_peek_message_a_noremove() {
        use crate::QueuedWindowMessage;
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let msg_va = 0x4000;
        engine
            .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
            .expect("map msg struct");
        state
            .window_state()
            .message_queue
            .push(QueuedWindowMessage {
                window_handle: 0x100,
                message: 15,
                word_parameter: 0,
                long_parameter: 0,
                time: 1,
                point_x: 0,
                point_y: 0,
            });
        write_regs(&mut engine, msg_va, 0, 0, 0, 0x3000);
        // wRemoveMsg=0 (PM_NOREMOVE) at RSP+0x28.
        engine.mem_write(0x3028, &0_u32.to_le_bytes()).ok();
        assert_return_value!(
            user32::handle_peek_message_a(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
        // Message should still be in the queue.
        assert_eq!(state.window_state().message_queue.len(), 1);
    }

    // --- Comctl32 ---

    #[test]
    fn test_init_common_controls() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        assert_return_value!(
            comctl32::handle_init_common_controls(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
    }

    // --- Comdlg32 ---

    #[test]
    fn test_choose_color_a_writes_color() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let cc_ptr = 0x5000;
        engine
            .mem_map(cc_ptr, 0x1000, wie_cpu::RwxPerms::ALL)
            .expect("map CHOOSECOLOR");
        write_regs(&mut engine, cc_ptr, 0, 0, 0, 0);
        assert_return_value!(
            comdlg32::handle_choose_color_a(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
        // rgbResult is at offset 0x10 in CHOOSECOLOR — should be RGB black (0).
        let mut rgb = [0_u8; 4];
        engine.mem_read(cc_ptr + 0x10, &mut rgb).ok();
        assert_eq!(u32::from_le_bytes(rgb), 0x00_00_00);
    }

    // --- Gdi32 ---

    #[test]
    fn test_text_out_a_returns_cch() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x100, 10, 20, 0x2000, 0x3000);
        // cchString at RSP+0x28 = 5.
        engine.mem_write(0x3028, &5_u32.to_le_bytes()).ok();
        assert_return_value!(
            gdi32::handle_text_out_a(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            5
        );
    }

    #[test]
    fn test_bit_blt_success() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x100, 0, 0, 100, 0);
        assert_return_value!(
            gdi32::handle_bit_blt(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
    }

    #[test]
    fn test_stretch_blt_success() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x100, 0, 0, 100, 0);
        assert_return_value!(
            gdi32::handle_stretch_blt(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
    }

    #[test]
    fn test_pat_blt_success() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x100, 0, 0, 100, 0);
        assert_return_value!(
            gdi32::handle_pat_blt(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
    }

    // --- Advapi32 ---

    #[test]
    fn test_set_security_descriptor_dacl_null_fails() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0, 0, 0, 0, 0);
        assert_return_value!(
            advapi32::handle_set_security_descriptor_dacl(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0
        );
    }

    #[test]
    fn test_set_security_descriptor_dacl_valid_succeeds() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x1000, 1, 0x2000, 1, 0);
        assert_return_value!(
            advapi32::handle_set_security_descriptor_dacl(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
    }

    // ── Kernel32: new mock-data-free handlers ─────────────────────────

    #[test]
    fn test_is_debugger_present() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        assert_return_value!(
            kernel32::handle_is_debugger_present(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0
        );
    }

    #[test]
    fn test_debug_break() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        assert_return_value!(
            kernel32::handle_debug_break(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0
        );
    }

    #[test]
    fn test_output_debug_string_a() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x3000, 0, 0, 0, STACK_TOP);
        engine.mem_write(0x3000, b"hello\0").ok();
        assert_return_value!(
            kernel32::handle_output_debug_string_a(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
    }

    #[test]
    fn test_set_error_mode() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        state.process.error_mode = 0;
        write_regs(&mut engine, 0x02, 0, 0, 0, STACK_TOP);
        let r = kernel32::handle_set_error_mode(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetErrorMode");
        // Previous mode was 0.
        assert_eq!(r.return_value, 0);
        assert_eq!(state.process.error_mode, 2);
    }

    #[test]
    fn test_set_thread_error_mode() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        state.process.error_mode = 1;
        let prev_ptr = 0x4000;
        write_regs(&mut engine, 0x03, prev_ptr, 0, 0, STACK_TOP);
        let r = kernel32::handle_set_thread_error_mode(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetThreadErrorMode");
        assert_eq!(r.return_value, 1); // TRUE
        assert_eq!(state.process.error_mode, 3);
        let mut buf = [0_u8; 4];
        engine.mem_read(prev_ptr, &mut buf).ok();
        assert_eq!(u32::from_le_bytes(buf), 1); // previous mode written back
    }

    #[test]
    fn test_get_long_path_name_w_returns_input() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let src = 0x3000;
        let dst = 0x4000;
        let units: Vec<u16> = "C:\\test".encode_utf16().collect();
        let mut bytes = Vec::new();
        for u in &units {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        bytes.push(0);
        bytes.push(0); // NUL terminator
        engine.mem_write(src, &bytes).ok();
        write_regs(&mut engine, src, dst, 260, 0, STACK_TOP);
        let r = kernel32::handle_get_long_path_name_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetLongPathNameW");
        assert_eq!(r.return_value, 7); // "C:\test" = 7 chars
    }

    #[test]
    fn test_create_job_object_w() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
        let r = kernel32::handle_create_job_object_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("CreateJobObjectW");
        assert!(r.return_value != 0);
    }

    #[test]
    fn test_assign_process_to_job_object() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x8000_0001, 0, 0, 0, STACK_TOP);
        assert_return_value!(
            kernel32::handle_assign_process_to_job_object(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
    }

    #[test]
    fn test_terminate_process() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        state.kernel.sync.process_dying = false;
        write_regs(&mut engine, 0x8000_0001, 0, 0, 0, STACK_TOP);
        assert_return_value!(
            kernel32::handle_terminate_process(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            1
        );
        assert!(state.kernel.sync.process_dying);
    }

    #[test]
    fn test_open_thread_creates_handle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x1000, 0, 0x5678, 0, STACK_TOP);
        let r = kernel32::handle_open_thread(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("OpenThread");
        assert!(r.return_value != 0);
    }

    #[test]
    fn test_get_file_attributes_ex_w_not_found() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let path_ptr = 0x3000;
        engine
            .mem_write(
                path_ptr,
                &"C:\\nonexistent"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .ok();
        engine.mem_write(path_ptr.wrapping_add(26), &[0, 0]).ok();
        write_regs(
            &mut engine,
            path_ptr,
            1, /* GetFileExInfoStandard */
            0x4000,
            0,
            STACK_TOP,
        );
        let r = kernel32::handle_get_file_attributes_ex_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetFileAttributesExW");
        assert_eq!(r.return_value, 0); // FALSE
        assert_eq!(state.process.last_error, 2); // ERROR_FILE_NOT_FOUND
    }

    #[test]
    fn test_backup_read_invalid_handle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0xDEAD, 0x4000, 64, 0x5000, STACK_TOP);
        let r = kernel32::handle_backup_read(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("BackupRead");
        assert_eq!(r.return_value, 0); // FALSE
        assert_eq!(state.process.last_error, 6); // ERROR_INVALID_HANDLE
    }

    #[test]
    fn test_suspend_thread_invalid_handle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0xDEAD, 0, 0, 0, STACK_TOP);
        let _r = kernel32::handle_suspend_thread(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SuspendThread");
        assert_eq!(state.process.last_error, 6); // ERROR_INVALID_HANDLE
    }

    #[test]
    fn test_lock_file_validates_handle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0xDEAD, 0, 0, 0, STACK_TOP);
        let r = kernel32::handle_lock_file(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("LockFile");
        assert_eq!(r.return_value, 0); // FALSE — invalid handle
        assert_eq!(state.process.last_error, 6); // ERROR_INVALID_HANDLE
    }

    #[test]
    fn test_set_file_valid_data_validates_handle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0xDEAD, 0, 0, 0, STACK_TOP);
        let r = kernel32::handle_set_file_valid_data(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("SetFileValidData");
        assert_eq!(r.return_value, 0); // FALSE — invalid handle
        assert_eq!(state.process.last_error, 6);
    }

    // ── Shell32 ───────────────────────────────────────────────────────

    #[test]
    fn test_command_line_to_argv_w() {
        use crate::guest_string::write_utf16_c_string;
        let mut engine = test_engine();
        let mut state = winapi_state_default();
        let cmd_ptr = 0x3000;
        let num_args_ptr = 0x4000;
        // Write "hello" as the command line.
        write_utf16_c_string(&mut engine, cmd_ptr, 10, "hello").ok();
        engine.mem_write(num_args_ptr, &[0_u8; 4]).ok();
        // Call handler directly.
        write_regs(&mut engine, cmd_ptr, num_args_ptr, 0, 0, STACK_TOP);
        let result = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            shell32::dispatch_shell32(&mut ctx, "CommandLineToArgvW")
        }
        .expect("dispatch failed")
        .expect("handler not found");
        assert!(result.return_value != 0, "return_value is 0");
        let mut argc_buf = [0_u8; 4];
        engine.mem_read(num_args_ptr, &mut argc_buf).ok();
        assert_eq!(u32::from_le_bytes(argc_buf), 1);
    }

    // ── OLEAUT32 ──────────────────────────────────────────────────────

    #[test]
    fn test_var_add() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let presult = 0x3000;
        let plhs = 0x4000;
        let prhs = 0x5000;
        // lhs = VT_I4, value = 10
        engine.mem_write(plhs, &(3_u16).to_le_bytes()).ok(); // VT_I4
        engine
            .mem_write(plhs.wrapping_add(8), &10_u64.to_le_bytes())
            .ok();
        // rhs = VT_I4, value = 20
        engine.mem_write(prhs, &(3_u16).to_le_bytes()).ok();
        engine
            .mem_write(prhs.wrapping_add(8), &20_u64.to_le_bytes())
            .ok();
        write_regs(&mut engine, presult, plhs, prhs, 0, STACK_TOP);
        let r = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            oleaut32::dispatch_oleaut32(&mut ctx, "VarAdd")
        }
        .expect("dispatch")
        .expect("handled");
        assert_eq!(r.return_value, 0); // S_OK
        let mut result_vt = [0_u8; 2];
        engine.mem_read(presult, &mut result_vt).ok();
        assert_eq!(u16::from_le_bytes(result_vt), 3); // VT_I4
        let mut result_val = [0_u8; 8];
        engine
            .mem_read(presult.wrapping_add(8), &mut result_val)
            .ok();
        assert_eq!(i64::from_le_bytes(result_val), 30);
    }

    #[test]
    fn test_var_bstr_from_i4() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let presult = 0x3000;
        // VarBstrFromI4(42, 0, 0, &result)
        write_regs(&mut engine, presult, 42, 0, 0, STACK_TOP);
        let r = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            oleaut32::dispatch_oleaut32(&mut ctx, "VarBstrFromI4")
        }
        .expect("dispatch")
        .expect("handled");
        assert_eq!(r.return_value, 0); // S_OK
        let mut vt = [0_u8; 2];
        engine.mem_read(presult, &mut vt).ok();
        assert_eq!(u16::from_le_bytes(vt), 8); // VT_BSTR
    }

    // ── ADVAPI32 ──────────────────────────────────────────────────────

    #[test]
    fn test_reg_enum_key_ex() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // Create a registry key with parent 0x7000_0001 (HKEY_CURRENT_USER)
        let parent = 0x7000_0001;
        state.process.registry_keys.push(crate::RegistryKey {
            handle: 0x100,
            parent,
            subkey: "Software\\test".into(),
        });
        state.process.registry_keys.push(crate::RegistryKey {
            handle: 0x101,
            parent: 0x100,
            subkey: "Nested".into(),
        });
        let name_buf = 0x4000;
        let name_len_ptr = 0x5000;
        let name_len: u32 = 32;
        engine.mem_write(name_len_ptr, &name_len.to_le_bytes()).ok();
        // RegEnumKeyExW(hKey=0x100, dwIndex=0, lpName=name_buf, lpcchName=name_len_ptr, ...)
        write_regs(&mut engine, 0x100, 0, name_buf, name_len_ptr, STACK_TOP);
        let r = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            advapi32::dispatch_advapi32_extra(&mut ctx, "RegEnumKeyExW")
        }
        .expect("dispatch")
        .expect("handled");
        assert_eq!(r.return_value, 0); // ERROR_SUCCESS
        let mut len_out = [0_u8; 4];
        engine.mem_read(name_len_ptr, &mut len_out).ok();
        assert_eq!(u32::from_le_bytes(len_out), 6); // "Nested" length
    }

    #[test]
    fn test_reg_enum_value_returns_no_more() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0x100, 0, 0x4000, 0x5000, STACK_TOP);
        let r = {
            let mut ctx = HandlerContext::new(&mut engine, default_env(), &mut state);
            advapi32::dispatch_advapi32_extra(&mut ctx, "RegEnumValueW")
        }
        .expect("dispatch")
        .expect("handled");
        assert_eq!(r.return_value, 259); // ERROR_NO_MORE_ITEMS
    }

    // ── USER32 ────────────────────────────────────────────────────────

    #[test]
    fn test_get_menu_returns_zero_for_unknown_window() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0xDEAD, 0, 0, 0, STACK_TOP);
        assert_return_value!(
            user32::handle_get_menu(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0
        );
    }

    #[test]
    fn test_get_menu_returns_menu_handle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let hwnd = 0x100;
        let hmenu = 0x200;
        state.window_state().windows.push(crate::WindowRecord {
            handle: hwnd,
            menu_handle: hmenu,
            ..Default::default()
        });
        write_regs(&mut engine, hwnd, 0, 0, 0, STACK_TOP);
        let r = user32::handle_get_menu(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetMenu");
        assert_eq!(r.return_value, hmenu);
    }

    // ── GDI32 ─────────────────────────────────────────────────────────

    #[test]
    fn test_get_stock_object_white_brush() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
        let r = gdi32::handle_get_stock_object(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetStockObject(WHITE_BRUSH)");
        assert!(r.return_value != 0);
    }

    #[test]
    fn test_get_stock_object_unknown_returns_zero() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 0xFF, 0, 0, 0, STACK_TOP);
        assert_return_value!(
            gdi32::handle_get_stock_object(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0
        );
    }

    // ── SEH hardware fault dispatch ──────────────────────────────────

    #[test]
    fn test_dispatch_hardware_fault_unhandled_returns_error() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // No function tables registered → no handler found → should error.
        let result = crate::seh::dispatch_hardware_fault(
            &mut engine,
            &mut state,
            wie_cpu::exception_code::ACCESS_VIOLATION,
            0x0, // fault at address 0
        );
        assert!(
            result.is_err(),
            "unhandled hardware fault should return error"
        );
    }
}
