//! WinAPI dispatcher model for WIE (generic PE64 userspace).

#![allow(clippy::type_complexity)]

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub mod advapi32;
pub mod bottle;
pub mod comctl32;
pub mod comdlg32;
pub mod console;
pub mod d3d9;
pub mod d3d9_render;
pub mod d3d9_shader;
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
pub mod handles;
pub mod idle;
pub mod kernel32;
pub mod mingw_dispatch;
pub mod msvc_eh;
pub mod ole32;
pub mod oleaut32;
pub mod present;
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
    ComMethod, D3d9Iface, Device9Method, Direct3D9Method, FAKE_API_BASE, FAKE_API_SIZE, FakeVa,
    PixelShader9Method, SPECIAL_CALLBACK_RETURN, SPECIAL_SEH_CONTINUE, Surface9Method,
    Texture9Method, VertexShader9Method, callback_return_trampoline_va, decode as decode_fake_va,
    encode_alias, encode_com, encode_export, encode_unresolved, seh_continue_trampoline_va,
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
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)(lib, name, slot)
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
#[derive(Debug, Clone)]
pub struct D3D9State {
    pub d3d9_current_vertex_shader: u64,
    pub d3d9_current_fvf: u32,
    /// Typed device render state (`D3DRS_*`), decoded at the Set/GetRenderState
    /// register boundary.
    pub d3d9_render_state: crate::d3d9_render::RenderState,
    /// Per-stage texture state (TSS + sampler), indexed by stage (0..8).
    pub d3d9_stage_states: [crate::d3d9_render::TextureStageState; 8],
    pub d3d9_device_object_address: u64,
    pub d3d9_device_ref_count: u32,
    pub d3d9_object_address: u64,
    pub d3d9_ref_count: u32,
    // ── P3 software-render state (slice 1) ──────────────────────────────
    /// Backbuffer size (from the CreateDevice presentation parameters).
    pub d3d9_backbuffer_width: u32,
    /// Backbuffer size (from the CreateDevice presentation parameters).
    pub d3d9_backbuffer_height: u32,
    /// Host-owned 0RGB backbuffer (top-down), the frame the guest renders into.
    ///
    /// Chosen over a guest-visible block: every write in slice 1 comes from a
    /// host-side handler (Clear/Draw*), so the guest never touches this
    /// memory directly and the rasterizer owns the buffer outright.
    pub d3d9_backbuffer: Vec<u32>,
    /// Window handle that receives Present frames (hDeviceWindow).
    pub d3d9_present_hwnd: crate::handles::Hwnd,
    /// Whether BeginScene has been called (and EndScene has not).
    pub d3d9_scene_active: bool,
    /// Fixed-function world matrix (`D3DTS_WORLD`).
    pub d3d9_world_matrix: [f32; 16],
    /// Fixed-function view matrix (`D3DTS_VIEW`).
    pub d3d9_view_matrix: [f32; 16],
    /// Fixed-function projection matrix (`D3DTS_PROJECTION`).
    pub d3d9_projection_matrix: [f32; 16],
    /// Viewport (`D3DVIEWPORT9` fields: X, Y, Width, Height, MinZ, MaxZ).
    pub d3d9_viewport: (u32, u32, u32, u32, f32, f32),
    /// Accumulated dirty region since the last Present (backbuffer coords);
    /// `None` = the whole backbuffer changed (Clear always sets this).
    pub d3d9_dirty: Option<crate::gdi32::IRect>,
    /// `SetStreamSource` stream-0 vertex pointer (buffer form; slice 1 stores
    /// but never creates real buffers — see `CreateVertexBuffer`).
    pub d3d9_stream_source_va: u64,
    /// `SetStreamSource` stream-0 stride.
    pub d3d9_stream_stride: u32,
    /// `SetIndices` index pointer (buffer form; unused in slice 1).
    pub d3d9_index_buffer_va: u64,
    // ── P4b texture state ───────────────────────────────────────────────
    /// Texture records keyed by the texture object's guest VA.
    pub d3d9_textures: HashMap<u64, crate::d3d9::TextureRecord>,
    /// Surface object VA → texture object VA (`GetSurfaceLevel` views).
    pub d3d9_surface_textures: HashMap<u64, u64>,
    /// Per-stage (0..8) texture binding (0 = none).
    pub d3d9_texture_bindings: [u64; 8],
    // ── P4c depth-stencil state ─────────────────────────────────────────
    /// Depth-stencil surfaces keyed by the surface object's guest VA.
    pub d3d9_depth_surfaces: HashMap<u64, crate::d3d9::DepthStencilRecord>,
    /// Bound depth-stencil surface VA (0 = none).
    pub d3d9_depth_stencil: u64,
    // ── P5a shader state ────────────────────────────────────────────────
    /// Pixel/vertex shader records keyed by the shader object's guest VA.
    pub d3d9_shaders: HashMap<u64, crate::d3d9_shader::ShaderRecord>,
    /// Bound pixel shader VA (0 = none — the FFP path runs).
    pub d3d9_pixel_shader: u64,
    /// Pixel-shader constant registers `c0..c31` (float4, set via
    /// `SetPixelShaderConstantF`; `def` at Create* time writes these too).
    pub d3d9_ps_constants: [[f32; 4]; crate::d3d9_shader::PS_CONST_COUNT],
    /// Vertex-shader constant registers `c0..c255` (stored; the vertex stage
    /// stays FFP until P5a-2 executes vertex shaders).
    pub d3d9_vs_constants: [[f32; 4]; crate::d3d9_shader::VS_CONST_COUNT],
}

impl Default for D3D9State {
    fn default() -> Self {
        Self {
            d3d9_current_vertex_shader: 0,
            d3d9_current_fvf: 0,
            d3d9_render_state: crate::d3d9_render::RenderState::default(),
            d3d9_stage_states: std::array::from_fn(|_| {
                crate::d3d9_render::TextureStageState::default()
            }),
            d3d9_device_object_address: 0,
            d3d9_device_ref_count: 0,
            d3d9_object_address: 0,
            d3d9_ref_count: 0,
            d3d9_backbuffer_width: 0,
            d3d9_backbuffer_height: 0,
            d3d9_backbuffer: Vec::new(),
            d3d9_present_hwnd: crate::handles::Hwnd::NULL,
            d3d9_scene_active: false,
            // D3D9's default transform state is the identity matrix (a zero
            // matrix would map every vertex to w=0 and reject all draws).
            d3d9_world_matrix: crate::d3d9_render::IDENTITY,
            d3d9_view_matrix: crate::d3d9_render::IDENTITY,
            d3d9_projection_matrix: crate::d3d9_render::IDENTITY,
            d3d9_viewport: (0, 0, 0, 0, 0.0, 1.0),
            d3d9_dirty: None,
            d3d9_stream_source_va: 0,
            d3d9_stream_stride: 0,
            d3d9_index_buffer_va: 0,
            d3d9_textures: HashMap::new(),
            d3d9_surface_textures: HashMap::new(),
            d3d9_texture_bindings: [0; 8],
            d3d9_depth_surfaces: HashMap::new(),
            d3d9_depth_stencil: 0,
            d3d9_shaders: HashMap::new(),
            d3d9_pixel_shader: 0,
            d3d9_ps_constants: [[0.0; 4]; crate::d3d9_shader::PS_CONST_COUNT],
            d3d9_vs_constants: [[0.0; 4]; crate::d3d9_shader::VS_CONST_COUNT],
        }
    }
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
#[derive(Debug, Clone)]
pub struct WindowState {
    pub window_long_ptr_values: Vec<(u64, i64, u64)>,
    pub image_list_counts: Vec<(u64, u64)>,
    pub image_list_background_colors: Vec<(u64, u32)>,
    pub window_visible: bool,
    pub window_enabled: bool,
    pub active_window_handle: crate::handles::Hwnd,
    pub foreground_window_handle: crate::handles::Hwnd,
    pub focus_window_handle: crate::handles::Hwnd,
    pub capture_window_handle: crate::handles::Hwnd,
    pub cursor_handle: u64,
    pub window_title: String,
    pub window_x: i32,
    pub window_y: i32,
    pub window_width: i32,
    pub window_height: i32,
    pub tick_count: u64,
    pub keyboard_state: KeyboardState,
    pub next_timer_id: u64,
    pub timers: Vec<TimerRecord>,
    pub next_global_atom: u16,
    pub global_atoms: Vec<GlobalAtomRecord>,
    pub next_windows_hook_handle: u64,
    pub windows_hooks: Vec<WindowsHookRecord>,
    /// All fake USER32 menus; each owns its items as a tree via `Popup`
    /// submenu links (`menu.rs`).
    pub menus: Vec<crate::user32::menu::MenuRecord>,
    /// Set by any menu mutation so the host menu-bar sync rebuilds its cached
    /// tree instead of reconstructing it every frame.
    pub menu_dirty: bool,
    /// Class-level `SetClassLongPtr` values keyed by (class atom, signed index).
    pub class_long_ptr_values: Vec<(u16, i64, u64)>,
    pub message_queue_idle_policy: MessageQueueIdlePolicy,
    pub next_window_class_atom: u16,
    pub window_classes: Vec<WindowClassRecord>,
    pub next_window_handle: u64,
    pub windows: Vec<WindowRecord>,
    /// Per-window UI state for built-in controls (pressed/focus/items).
    pub control_states:
        std::collections::HashMap<crate::handles::Hwnd, crate::user32::controls::ControlState>,
    pub file_dialog_policy: FileDialogPolicy,
    pub last_file_dialog_path: Option<String>,
    pub comm_dlg_extended_error: u32,
    pub next_menu_handle: u64,
    /// Guest VA of the modal-dialog result slot (`u32`), set by session init.
    ///
    /// `EndDialog` writes the result here; the in-guest `DialogBoxParam` stub
    /// reads it after its `WM_QUIT`. Zero when no dialog machinery is wired.
    pub dialog_result_va: u64,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            dialog_result_va: 0,
            next_window_class_atom: 0xC000,
            next_window_handle: 0x0000_0000_6610_0000,
            next_menu_handle: 0x0000_0000_6620_0000,
            next_windows_hook_handle: 0x0000_0000_6630_0000,
            next_global_atom: 0xC000,
            next_timer_id: 1,
            window_long_ptr_values: Vec::new(),
            image_list_counts: Vec::new(),
            image_list_background_colors: Vec::new(),
            window_visible: false,
            window_enabled: false,
            active_window_handle: crate::handles::Hwnd::NULL,
            foreground_window_handle: crate::handles::Hwnd::NULL,
            focus_window_handle: crate::handles::Hwnd::NULL,
            capture_window_handle: crate::handles::Hwnd::NULL,
            cursor_handle: 0,
            window_title: String::new(),
            window_x: 0,
            window_y: 0,
            window_width: 0,
            window_height: 0,
            tick_count: 0,
            keyboard_state: KeyboardState::default(),
            timers: Vec::new(),
            global_atoms: Vec::new(),
            windows_hooks: Vec::new(),
            class_long_ptr_values: Vec::new(),
            message_queue_idle_policy: MessageQueueIdlePolicy::default(),
            window_classes: Vec::new(),
            windows: Vec::new(),
            control_states: std::collections::HashMap::new(),
            file_dialog_policy: FileDialogPolicy::default(),
            last_file_dialog_path: None,
            comm_dlg_extended_error: 0,
            menus: Vec::new(),
            menu_dirty: false,
        }
    }
}

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
    /// Parsed `RT_DIALOG` templates of the main EXE module.
    ///
    /// The main module does not go through `dll_loader::load_dll`, so its
    /// templates are parsed at session init (mirroring the `LoadedModule`
    /// construction site). `DialogBoxParam` resolves `hInstance == image base`
    /// against this list.
    pub main_module_dialogs: Vec<wie_pe::resources::DialogTemplate>,
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
    Gdi,
    Present,
}

impl DllId {
    /// Number of variants. The assertion at [`DLL_ID_SLOT_COUNT`] ensures
    /// it stays in sync with the slot array in [`DllStateMap::new`].
    pub const COUNT: usize = 6;
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
        DllId::Gdi => 4,
        DllId::Present => 5,
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
        4 => "gdi",
        5 => "present",
        _ => "?",
    }
}

impl DllStateMap {
    /// All slots start unloaded. Add a `None` per new [`DllId`] variant.
    pub fn new() -> Self {
        Self {
            slots: [None, None, None, None, None, None],
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
    /// Guest message queue behind its OWN mutex.
    ///
    /// The host (winit thread) posts input through this queue without ever
    /// locking the big `WinApiState` mutex that the guest thread holds during
    /// API-handler execution, so input events never block on guest work.
    pub message_queue: Arc<Mutex<present::MessageQueue>>,
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
            .field("message_queue", &self.message_queue)
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

    /// Lock the guest message queue, recovering from a poisoned mutex.
    pub fn lock_message_queue(&self) -> std::sync::MutexGuard<'_, present::MessageQueue> {
        self.message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn d3d9(&mut self) -> &mut D3D9State {
        self.dll_states.get_or_init::<D3D9State>(DllId::D3D9)
    }
    pub fn pthread(&mut self) -> &mut pthread::PthreadState {
        self.dll_states
            .get_or_init::<pthread::PthreadState>(DllId::Pthread)
    }

    /// Mutable access to GDI state (DC records, DIB sections).
    pub fn gdi_state(&mut self) -> &mut gdi32::GdiState {
        self.dll_states.get_or_init::<gdi32::GdiState>(DllId::Gdi)
    }

    /// Mutable access to present state (compositing surfaces, frame publishing).
    pub fn present(&mut self) -> &mut present::PresentState {
        self.dll_states
            .get_or_init::<present::PresentState>(DllId::Present)
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
    pub fn try_gdi_state(&self) -> Option<&gdi32::GdiState> {
        self.dll_states.get::<gdi32::GdiState>(DllId::Gdi)
    }
    pub fn try_present(&self) -> Option<&present::PresentState> {
        self.dll_states.get::<present::PresentState>(DllId::Present)
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "each window flag is an independently documented, directly-read USER32 state bit"
)]
pub struct WindowRecord {
    /// Runtime-owned fake HWND.
    pub handle: crate::handles::Hwnd,

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
    pub parent_handle: crate::handles::Hwnd,

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

    /// Whether the window has been invalidated and needs a repaint.
    pub invalidated: bool,

    /// Whether the pending repaint cycle must erase the background first.
    ///
    /// Set by `InvalidateRect(hwnd, rect, TRUE)` (OR'd — once a region asks
    /// for an erase, a later `FALSE` cannot un-ask it, matching Windows). The
    /// WM_PAINT synthesis queues a `WM_ERASEBKGND` ahead of the paint when a
    /// class background brush exists; the erase dispatch clears the flag.
    /// `BeginPaint`'s `fErase` reports the still-pending flag.
    pub erase_background: bool,

    /// Whether `TrackMouseEvent` armed hover/leave tracking for this window.
    ///
    /// With no tracking request Windows sends no `WM_MOUSEHOVER` /
    /// `WM_MOUSELEAVE`; the host forwards those only for tracked windows.
    pub mouse_tracking: bool,

    /// Mouse press tracking shared by every control kind.
    ///
    /// The `WM_LBUTTONDOWN` / `WM_LBUTTONUP` dispatch arms press and release
    /// every control (a release delivers `BN_CLICKED`); only BUTTONs render or
    /// report the bit (`BM_GETSTATE` / `BM_SETSTATE`), so it lives on the
    /// window record rather than in a per-kind variant.
    pub pressed: bool,

    /// Keyboard focus tracking shared by every control kind.
    ///
    /// Set by `WM_SETFOCUS` / `WM_KILLFOCUS` (and a click on a non-STATIC
    /// control) for any kind; only BUTTON (`BM_GETSTATE`'s `BST_FOCUS`) and
    /// EDIT (caret visibility) read it back.
    pub focused: bool,

    /// Client rectangle (left, top, right, bottom).
    pub client_rect: (i32, i32, i32, i32),

    /// Built-in control class this window belongs to (None for normal
    /// application windows). Controls have no guest WndProc; the runtime
    /// dispatches their messages through `dispatch_control_proc`.
    pub control_kind: Option<crate::user32::controls::ControlClassKind>,

    /// Text buffer for built-in controls (WM_GETTEXT / WM_SETTEXT / painting).
    pub control_text: String,

    /// Guest dialog procedure (`DialogBoxParam` `lpDialogFunc`); 0 for normal
    /// windows. Dialog windows have no guest WndProc — the runtime bridges
    /// `WM_INITDIALOG` / `WM_COMMAND` to this address instead.
    pub dialog_proc: u64,

    /// Whether the dialog procedure uses the Unicode contract.
    pub dialog_unicode: bool,
}

/// Controls what value the outer API returns after a guest WndProc completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OuterReturn {
    /// Return whatever the WndProc returned (default for DispatchMessage etc.).
    Passthrough,
    /// Return the given HWND (used by CreateWindowExA/W — unless WM_CREATE returned -1).
    CreateWindow(u64),
    /// Always return a fixed value (used by DestroyWindow after WM_DESTROY).
    Fixed(u64),
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

    /// Controls what the outer API returns after the WndProc completes.
    pub outer_return: OuterReturn,
}

/// A queued fake USER32 message.
#[derive(Debug, Clone)]
pub struct QueuedWindowMessage {
    /// Target window handle.
    pub window_handle: crate::handles::Hwnd,

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
    pub window_handle: crate::handles::Hwnd,

    /// Timer identifier.
    pub timer_id: u64,

    /// Requested timer interval in milliseconds.
    pub interval_ms: u32,

    /// Optional guest timer callback address.
    pub callback_address: u64,

    /// Host-clock deadline for the next `WM_TIMER` synthesis.
    ///
    /// Timers are the one message source driven by the host clock rather than
    /// the deterministic fake `next_message_time` scheme — real Windows timers
    /// are clock-driven too.
    pub next_fire: std::time::Instant,
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
                main_module_dialogs: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(present::MessageQueue::default())),
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
    fn test_keyboard_state_shift_update_path() {
        // Mirrors the host input seam (app.rs set_key_state): pressing the
        // Shift key sets bit 0x80 on VK_SHIFT (0x10) and releasing clears it,
        // which is exactly what IsDialogMessage's Shift+Tab and
        // GetAsyncKeyState/GetKeyState read.
        let mut engine = test_engine();
        let mut state = default_winapi_state();

        // Press: app.rs set_key_state(0x10, true).
        state.window_state().keyboard_state.0[0x10] |= 0x80;
        write_regs(&mut engine, 0x10, 0, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_async_key_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0x81
        );

        // Release: app.rs set_key_state(0x10, false).
        state.window_state().keyboard_state.0[0x10] &= !0x80;
        write_regs(&mut engine, 0x10, 0, 0, 0, 0);
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
    fn test_get_sys_color_highlight_is_not_bgr_swapped() {
        // COLOR_HIGHLIGHT (13) and COLOR_ACTIVECAPTION (2) are #0078D7 stored
        // as 0RGB — the old 0xD77830 was the B/R-swapped value (rendered
        // orange).
        for index in [13_u64, 2] {
            let mut engine = test_engine();
            let mut state = default_winapi_state();
            write_regs(&mut engine, index, 0, 0, 0, 0);
            assert_return_value!(
                user32::handle_get_sys_color(&mut HandlerContext::new(
                    &mut engine,
                    test_environment(),
                    &mut state
                )),
                0x0000_78D7
            );
        }
    }

    #[test]
    fn test_get_sys_color_3d_edge_colors() {
        // COLOR_BTNHIGHLIGHT (20) = white; COLOR_3DDKSHADOW (21) = dark gray
        // (both previously fell through to the BTNFACE fallback).
        let cases = [(20_u64, 0x00FF_FFFF_u64), (21, 0x0069_6969)];
        for (index, expected) in cases {
            let mut engine = test_engine();
            let mut state = default_winapi_state();
            write_regs(&mut engine, index, 0, 0, 0, 0);
            assert_return_value!(
                user32::handle_get_sys_color(&mut HandlerContext::new(
                    &mut engine,
                    test_environment(),
                    &mut state
                )),
                expected
            );
        }
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
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .push(QueuedWindowMessage {
                window_handle: crate::handles::Hwnd::from(0x100),
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
        assert_eq!(
            state
                .message_queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .messages
                .len(),
            0
        );
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
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .push(QueuedWindowMessage {
                window_handle: crate::handles::Hwnd::from(0x100),
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
        assert_eq!(
            state
                .message_queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .messages
                .len(),
            1
        );
    }

    #[test]
    fn test_get_message_wm_quit_bypasses_window_filter() {
        use crate::QueuedWindowMessage;
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let msg_va = 0x4000;
        engine
            .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
            .expect("map msg struct");
        // WM_QUIT addressed to a window that does NOT match the filter.
        state
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .push(QueuedWindowMessage {
                window_handle: crate::handles::Hwnd::from(0x1234),
                message: 0x12, // WM_QUIT
                word_parameter: 7,
                long_parameter: 0,
                time: 1,
                point_x: 0,
                point_y: 0,
            });
        // GetMessageA(msg_ptr, hWnd=0x5678 filter, min=0, max=0).
        write_regs(&mut engine, msg_va, 0x5678, 0, 0, 0x3000);
        let r = user32::handle_get_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetMessageA");
        // GetMessage returns 0 (FALSE) for WM_QUIT regardless of the filter.
        assert_eq!(r.return_value, 0);
        // The returned MSG carries the WM_QUIT and its wParam.
        let mut bytes = [0_u8; 4];
        engine
            .mem_read(msg_va + 8, &mut bytes)
            .expect("read MSG.message");
        assert_eq!(u32::from_le_bytes(bytes), 0x12);
        engine
            .mem_read(msg_va + 16, &mut bytes)
            .expect("read MSG.wParam");
        assert_eq!(u32::from_le_bytes(bytes), 7);
    }

    #[test]
    fn test_get_message_dialog_filter_matches_descendants() {
        use crate::QueuedWindowMessage;
        use crate::WindowRecord;
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let msg_va = 0x4000;
        engine
            .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
            .expect("map msg struct");
        // Owner (parentless) → dialog (parent = owner) → button (parent = dialog).
        let owner = 0x6610_0001_u64;
        let dialog = 0x6610_0002_u64;
        let child = 0x6610_0003_u64;
        let ws = state.window_state();
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(owner),
            title: "Owner".to_owned(),
            width: 100,
            height: 100,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(dialog),
            parent_handle: crate::handles::Hwnd::from(owner),
            title: "Dialog".to_owned(),
            dialog_proc: 0x7000_0001,
            width: 80,
            height: 60,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(child),
            parent_handle: crate::handles::Hwnd::from(dialog),
            control_kind: Some(crate::user32::controls::ControlClassKind::Button),
            control_text: "OK".to_owned(),
            menu_handle: 1,
            visible: true,
            width: 40,
            height: 20,
            ..Default::default()
        });
        // A message addressed to the button inside the dialog.
        state
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .push(QueuedWindowMessage {
                window_handle: crate::handles::Hwnd::from(child),
                message: 0x0100, // WM_KEYDOWN
                word_parameter: 0x09,
                long_parameter: 0,
                time: 1,
                point_x: 0,
                point_y: 0,
            });
        // GetMessageA(msg_ptr, hWnd=dialog, min=0, max=0): the child's message
        // must match through the dialog's descendant rule.
        write_regs(&mut engine, msg_va, dialog, 0, 0, 0x3000);
        let r = user32::handle_get_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetMessageA");
        assert_eq!(r.return_value, 1);
        let mut bytes = [0_u8; 8];
        engine.mem_read(msg_va, &mut bytes).expect("read MSG.hwnd");
        assert_eq!(u64::from_le_bytes(bytes), child);
    }

    #[test]
    fn test_empty_queue_yields_while_dialog_open_under_exit_on_idle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        state.window_state().message_queue_idle_policy = MessageQueueIdlePolicy::ExitOnIdle;
        // An open modal dialog must never see the synthetic regression WM_QUIT.
        state.lock_message_queue().dialog_depth = 1;
        write_regs(&mut engine, 0x1000, 0, 0, 0, 0x2000);
        let result = user32::handle_get_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ));
        assert!(
            result.is_err(),
            "empty queue under an open dialog must yield"
        );
        let error = result.expect_err("expected the WaitingForMessage signal");
        assert!(
            error
                .downcast_ref::<WinApiControlSignal>()
                .is_some_and(|signal| matches!(signal, WinApiControlSignal::WaitingForMessage)),
            "expected WaitingForMessage, got {error:?}"
        );
        // No WM_QUIT was synthesized into the queue.
        assert!(
            state
                .lock_message_queue()
                .messages
                .iter()
                .all(|m| m.message != 0x12)
        );
        // With no dialog open, ExitOnIdle still synthesizes WM_QUIT (regression
        // path) and GetMessage returns 0.
        state.lock_message_queue().dialog_depth = 0;
        let r = user32::handle_get_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("GetMessageA");
        assert_eq!(r.return_value, 0);
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
            handle: crate::handles::Hwnd::from(hwnd),
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

    // ── USER32 menu tree (P1): MenuRecord / MenuEntry / menu_dirty ─────

    /// Invoke `CreateMenu` and return the allocated handle.
    fn create_menu(state: &mut WinApiState) -> u64 {
        let mut engine = test_engine();
        write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
        let r = user32::handle_create_menu(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            state,
        ))
        .expect("CreateMenu");
        r.return_value
    }

    /// Invoke `AppendMenuA(menu, flags, item_id, text_ptr)`, writing `text`
    /// into guest memory first (empty text uses a null pointer).
    fn append_menu_a(
        engine: &mut IcedCpu,
        state: &mut WinApiState,
        menu: u64,
        flags: u32,
        item_id: u32,
        text: &str,
    ) {
        let text_ptr = if text.is_empty() {
            0
        } else {
            engine
                .mem_write(0x2000, text.as_bytes())
                .expect("write menu text");
            0x2000
        };
        write_regs(
            engine,
            menu,
            u64::from(flags),
            u64::from(item_id),
            text_ptr,
            0,
        );
        assert_return_value!(
            user32::handle_append_menu_a(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            )),
            1
        );
    }

    /// A menu with `Item(100, "Exit")`, a `Separator`, and `Item(200, "About")`.
    fn push_two_item_menu(state: &mut WinApiState) -> u64 {
        let menu = create_menu(state);
        let mut engine = test_engine();
        append_menu_a(&mut engine, state, menu, 0x0000, 100, "Exit");
        append_menu_a(&mut engine, state, menu, 0x0800, 0, ""); // MF_SEPARATOR
        append_menu_a(&mut engine, state, menu, 0x0000, 200, "About");
        menu
    }

    #[test]
    fn test_append_menu_builds_native_tree() {
        let mut state = default_winapi_state();
        let menu = create_menu(&mut state);
        assert!(
            !state.window_state().menu_dirty,
            "CreateMenu alone must not dirty the tree (empty menu changes nothing)"
        );
        let popup = create_menu(&mut state);
        let mut engine = test_engine();
        append_menu_a(&mut engine, &mut state, menu, 0x0000, 100, "Exit");
        append_menu_a(&mut engine, &mut state, menu, 0x0800, 0, "");
        append_menu_a(
            &mut engine,
            &mut state,
            menu,
            0x0010,
            u32::try_from(popup).expect("popup handle"),
            "File",
        ); // MF_POPUP

        assert!(state.window_state().menu_dirty, "AppendMenu must dirty");
        let record = state
            .window_state()
            .menus
            .iter()
            .find(|m| m.handle == crate::handles::Hmenu::from(menu))
            .expect("menu record exists");
        assert_eq!(record.items.len(), 3);
        assert!(
            matches!(
                record.items.first(),
                Some(crate::user32::menu::MenuEntry::Item { id, text, enabled: true, checked: false })
                    if *id == 100 && text == "Exit"
            ),
            "first entry must be the Exit item"
        );
        assert!(
            matches!(
                record.items.get(1),
                Some(crate::user32::menu::MenuEntry::Separator)
            ),
            "second entry must be a separator"
        );
        assert!(
            matches!(
                record.items.get(2),
                Some(crate::user32::menu::MenuEntry::Popup { text, submenu })
                    if text == "File" && submenu.as_u64() == popup
            ),
            "third entry must be the File popup linking the submenu handle"
        );
    }

    #[test]
    fn test_get_menu_state_by_command_and_position() {
        let mut state = default_winapi_state();
        let menu = push_two_item_menu(&mut state);
        let mut engine = test_engine();

        // By command: an enabled, unchecked item reports flag 0.
        write_regs(&mut engine, menu, 100, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_menu_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );

        // By position: index 1 is the separator.
        write_regs(&mut engine, menu, 1, 0x0400, 0, 0); // MF_BYPOSITION
        assert_return_value!(
            user32::handle_get_menu_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0x0800
        );

        // Unknown command id returns -1.
        write_regs(&mut engine, menu, 999, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_menu_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            u64::from(u32::MAX)
        );
    }

    #[test]
    fn test_enable_menu_item_mutates_item_state() {
        let mut state = default_winapi_state();
        let menu = push_two_item_menu(&mut state);
        let mut engine = test_engine();

        // First enable returns the previous state (0 = enabled) and grays it.
        write_regs(&mut engine, menu, 100, 0x0001, 0, 0); // MF_GRAYED
        assert_return_value!(
            user32::handle_enable_menu_item(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        assert!(
            matches!(
                state
                    .window_state()
                    .menus
                    .iter()
                    .find(|m| m.handle == crate::handles::Hmenu::from(menu))
                    .expect("record")
                    .items
                    .first(),
                Some(crate::user32::menu::MenuEntry::Item { enabled: false, .. })
            ),
            "MF_GRAYED must disable the item in the tree"
        );

        // GetMenuState now reports MF_GRAYED.
        write_regs(&mut engine, menu, 100, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_menu_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0x0001
        );

        // Re-enabling returns the previous MF_GRAYED state.
        write_regs(&mut engine, menu, 100, 0x0000, 0, 0); // MF_ENABLED
        assert_return_value!(
            user32::handle_enable_menu_item(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0x0001
        );

        // A missing item returns -1 and does not mutate.
        let dirty_before = state.window_state().menu_dirty;
        write_regs(&mut engine, menu, 999, 0x0001, 0, 0);
        assert_return_value!(
            user32::handle_enable_menu_item(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            u64::from(u32::MAX)
        );
        assert_eq!(state.window_state().menu_dirty, dirty_before);
    }

    #[test]
    fn test_check_menu_item_mutates_item_state() {
        let mut state = default_winapi_state();
        let menu = push_two_item_menu(&mut state);
        let mut engine = test_engine();

        write_regs(&mut engine, menu, 200, 0x0008, 0, 0); // MF_CHECKED
        assert_return_value!(
            user32::handle_check_menu_item(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        write_regs(&mut engine, menu, 200, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_menu_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0x0008
        );

        // Unchecking returns the previous MF_CHECKED state.
        write_regs(&mut engine, menu, 200, 0x0000, 0, 0);
        assert_return_value!(
            user32::handle_check_menu_item(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0x0008
        );
        write_regs(&mut engine, menu, 200, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_menu_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
    }

    #[test]
    fn test_menu_dirty_flag_semantics() {
        let mut state = default_winapi_state();
        assert!(!state.window_state().menu_dirty);

        // SetMenu to a new menu handle dirties (the window's menu changed).
        let menu = create_menu(&mut state);
        state.window_state().windows.push(crate::WindowRecord {
            handle: crate::handles::Hwnd::from(0x100),
            ..Default::default()
        });
        state.window_state().menu_dirty = false;
        let mut engine = test_engine();
        write_regs(&mut engine, 0x100, menu, 0, 0, 0);
        assert_return_value!(
            user32::handle_set_menu(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            1
        );
        assert!(state.window_state().menu_dirty, "SetMenu must dirty");

        // DestroyMenu removes the record and dirties.
        state.window_state().menu_dirty = false;
        write_regs(&mut engine, menu, 0, 0, 0, 0);
        assert_return_value!(
            user32::handle_destroy_menu(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            1
        );
        assert!(state.window_state().menu_dirty, "DestroyMenu must dirty");
        assert!(
            state
                .window_state()
                .menus
                .iter()
                .all(|m| m.handle != crate::handles::Hmenu::from(menu)),
            "DestroyMenu must drop the record"
        );

        // GetMenuState on the destroyed menu now returns -1.
        write_regs(&mut engine, menu, 100, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_menu_state(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            u64::from(u32::MAX)
        );
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

    // ── Phase 0: handle disjointness ─────────────────────────────────
    //
    // Ensures dynamically-allocated handle bases seeded in WindowState do not
    // overlap the FAKE handle ranges used by USER32/GDI32 stubs.
    //
    // FAKE handles live in ranges:
    //   USER32: 0x6600_0000..0x6601_xxxx
    //   GDI32:  0x6800_0000..0x6800_xxxx
    //
    // Seeded bases live in:
    //   window_handle:  0x6610_0000
    //   menu_handle:    0x6620_0000
    //   hook_handle:    0x6630_0000
    //   atoms:          0xC000
    //   timer_id:       1
    //   (future GDI DC: 0x6810_0000, bitmap: 0x6820_0000)

    #[test]
    fn test_fake_handle_disjointness() {
        let ws = WindowState::default();

        // USER32 window handle range (0x6610_0000+) must not overlap FAKE range (0x6600_xxxx)
        assert!(
            ws.next_window_handle >= 0x0000_0000_6610_0000,
            "window handle base collides with FAKE range"
        );

        // Menu/hook bases
        assert!(
            ws.next_menu_handle >= 0x0000_0000_6620_0000,
            "menu handle base collides with FAKE range"
        );
        assert!(
            ws.next_windows_hook_handle >= 0x0000_0000_6630_0000,
            "hook handle base collides with FAKE range"
        );

        // Atoms in user-atom range (0xC000-0xFFFF)
        assert!(
            ws.next_window_class_atom >= 0xC000,
            "class atom base collides with FAKE range"
        );
        assert!(
            ws.next_global_atom >= 0xC000,
            "global atom base collides with FAKE range"
        );

        // Timer ID is trivially disjoint
        assert_eq!(ws.next_timer_id, 1, "timer ID must be 1");
    }

    // ── Windows-fidelity batch (fix-28): BM_* / focus / erase / tracking ──

    /// A button child of a parent with a guest WndProc, for control tests.
    fn push_button_pair(state: &mut WinApiState) -> (u64, u64) {
        let parent = 0x6610_0001_u64;
        let button = 0x6610_0002_u64;
        let ws = state.window_state();
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(parent),
            window_proc: 0x7000_0000,
            title: "Parent".to_owned(),
            width: 200,
            height: 100,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(button),
            parent_handle: crate::handles::Hwnd::from(parent),
            control_kind: Some(crate::user32::controls::ControlClassKind::Button),
            control_text: "OK".to_owned(),
            menu_handle: 7,
            visible: true,
            width: 40,
            height: 20,
            ..Default::default()
        });
        (parent, button)
    }

    #[test]
    fn test_bm_click_delivers_command_to_parent() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (parent, button) = push_button_pair(&mut state);

        // SendMessage(button, BM_CLICK): the host control WndProc must bridge
        // WM_COMMAND(MAKEWPARAM(7, BN_CLICKED)) to the parent's WndProc.
        let result = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            button,
            crate::user32::BM_CLICK,
            0,
            0,
        );
        let error = result.expect_err("BM_CLICK must bridge a WM_COMMAND callback");
        let signal = error
            .downcast_ref::<WinApiControlSignal>()
            .expect("control signal");
        assert!(
            matches!(
                signal,
                WinApiControlSignal::GuestCallbackRequested { request }
                    if request.window_handle == parent
                        && request.message == 0x0111
                        && request.word_parameter == 7
            ),
            "BM_CLICK must bridge WM_COMMAND(MAKEWPARAM(7, BN_CLICKED)) to the \
             parent WndProc, got {signal:?}"
        );
    }

    #[test]
    fn test_space_keyboard_activates_focused_button() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (parent, button) = push_button_pair(&mut state);
        state.window_state().focus_window_handle = crate::handles::Hwnd::from(button);

        // WM_KEYDOWN VK_SPACE on the focused button presses it.
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            button,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_SPACE,
            0,
        )
        .expect("keydown handled")
        .expect("some result");
        assert_eq!(r, 0);
        assert!(
            state
                .window_state()
                .windows
                .iter()
                .find(|w| w.handle == crate::handles::Hwnd::from(button))
                .is_some_and(|w| w.pressed),
            "space keydown must press the focused button"
        );

        // WM_KEYUP VK_SPACE releases and delivers BN_CLICKED to the parent.
        let result = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            button,
            crate::user32::WM_KEYUP,
            crate::user32::VK_SPACE,
            0,
        );
        let error = result.expect_err("space keyup must click the button");
        let signal = error
            .downcast_ref::<WinApiControlSignal>()
            .expect("control signal");
        assert!(
            matches!(
                signal,
                WinApiControlSignal::GuestCallbackRequested { request }
                    if request.window_handle == parent && request.word_parameter == 7
            ),
            "space keyup must deliver WM_COMMAND(7) to the parent, got {signal:?}"
        );

        // Space on a NON-focused button is ignored (no press, no click).
        state.window_state().focus_window_handle = crate::handles::Hwnd::NULL;
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            button,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_SPACE,
            0,
        )
        .expect("non-focused keydown unhandled");
        assert!(r.is_none(), "non-focused button must not react to space");
    }

    #[test]
    fn test_bm_get_set_state_roundtrip() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_, button) = push_button_pair(&mut state);

        // Fresh button: not pressed, not focused.
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            button,
            crate::user32::BM_GETSTATE,
            0,
            0,
        )
        .expect("getstate ok")
        .expect("some result");
        assert_eq!(r, 0);

        // BM_SETSTATE(TRUE) returns the previous (0) and presses.
        let prev = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            button,
            crate::user32::BM_SETSTATE,
            1,
            0,
        )
        .expect("setstate ok")
        .expect("some result");
        assert_eq!(prev, 0);
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            button,
            crate::user32::BM_GETSTATE,
            0,
            0,
        )
        .expect("getstate ok")
        .expect("some result");
        assert_eq!(r, crate::user32::BST_PUSHED);

        // BM_SETSTATE(FALSE) returns 1 (was pressed) and releases.
        let prev = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            button,
            crate::user32::BM_SETSTATE,
            0,
            0,
        )
        .expect("setstate ok")
        .expect("some result");
        assert_eq!(prev, 1);
    }

    #[test]
    fn edit_message_on_a_button_does_not_touch_edit_state() {
        use crate::user32::controls::ControlState;
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_, button) = push_button_pair(&mut state);

        // EM_SETSEL is an EDIT-only message: a BUTTON must not handle it...
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            button,
            crate::user32::wm::WinMsg::EM_SETSEL.as_u32(),
            2,
            4,
        )
        .expect("dispatch");
        assert!(r.is_none(), "a Button must not handle EM_SETSEL");

        // ... and no EDIT-typed state may exist for the button (reading
        // `caret` on the entry would be a compile error anyway).
        let entry = state
            .window_state()
            .control_states
            .get(&crate::handles::Hwnd::from(button));
        assert!(
            matches!(entry, None | Some(ControlState::Button { .. })),
            "EM_SETSEL on a Button must not create EDIT state, got {entry:?}"
        );
    }

    #[test]
    fn test_track_mouse_event_arms_and_cancels() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let win = 0x6610_0001_u64;
        state.window_state().windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(win),
            title: "Tracked".to_owned(),
            width: 100,
            height: 100,
            ..Default::default()
        });
        let tme = 0x5000_u64;
        engine
            .mem_map(tme, 0x1000, wie_cpu::RwxPerms::ALL)
            .expect("map tme struct");
        // TRACKMOUSEEVENT: cbSize=24, dwFlags=TME_HOVER|TME_LEAVE, hwndTrack=win.
        engine
            .mem_write(tme, &24_u32.to_le_bytes())
            .expect("cbSize");
        engine
            .mem_write(tme + 4, &3_u32.to_le_bytes())
            .expect("flags");
        engine
            .mem_write(tme + 8, &win.to_le_bytes())
            .expect("hwndTrack");

        write_regs(&mut engine, tme, 0, 0, 0, 0);
        let r = user32::handle_track_mouse_event(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("TrackMouseEvent");
        assert_eq!(r.return_value, 1);
        assert!(
            state
                .window_state()
                .windows
                .iter()
                .find(|w| w.handle == crate::handles::Hwnd::from(win))
                .expect("tracked window exists")
                .mouse_tracking,
            "TME must arm tracking on the target window"
        );

        // TME_CANCEL releases the tracking request.
        engine
            .mem_write(tme + 4, &0x8000_0000_u32.to_le_bytes())
            .expect("cancel flags");
        write_regs(&mut engine, tme, 0, 0, 0, 0);
        let r = user32::handle_track_mouse_event(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("TrackMouseEvent cancel");
        assert_eq!(r.return_value, 1);
        assert!(
            !state
                .window_state()
                .windows
                .iter()
                .find(|w| w.handle == crate::handles::Hwnd::from(win))
                .expect("tracked window exists")
                .mouse_tracking,
            "TME_CANCEL must clear tracking"
        );
    }

    #[test]
    fn test_erase_background_fills_class_brush_and_clears_flag() {
        let mut state = default_winapi_state();
        // Register a class with the classic (HBRUSH)(COLOR_WINDOW + 1) stock
        // brush (6 = COLOR_WINDOW + 1) — the white background.
        let atom = crate::user32::register_window_class(
            &mut state,
            WindowClassRecord {
                atom: 0,
                class_name: "EraseClass".to_owned(),
                window_proc: 0x7000_0000,
                style: 0,
                instance_handle: 0,
                icon_handle: 0,
                cursor_handle: 0,
                background_brush: 6,
                small_icon_handle: 0,
                unicode: false,
            },
        )
        .expect("register class");
        let win = 0x6610_0001_u64;
        state.window_state().windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(win),
            class_atom: u16::try_from(atom).unwrap_or(0),
            title: "Erase".to_owned(),
            width: 64,
            height: 32,
            invalidated: true,
            erase_background: true,
            ..Default::default()
        });

        assert!(
            crate::user32::message::erase_window_background(&mut state, win),
            "a class brush must erase the background"
        );
        assert!(
            !state
                .window_state()
                .windows
                .iter()
                .find(|w| w.handle == crate::handles::Hwnd::from(win))
                .expect("erased window exists")
                .erase_background,
            "the erase must consume the pending-erase flag"
        );
        let surf = state
            .present()
            .surfaces
            .get(&crate::handles::Hwnd::from(win))
            .expect("erase surface exists");
        assert_eq!(surf.pixels.len(), 64 * 32);
        assert!(
            surf.pixels.iter().all(|&p| p == 0x00FF_FFFF),
            "COLOR_WINDOW brush must fill white (0RGB)"
        );

        // No class brush → no erase possible.
        let win2 = 0x6610_0002_u64;
        state.window_state().windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(win2),
            title: "NoBrush".to_owned(),
            width: 8,
            height: 8,
            ..Default::default()
        });
        assert!(
            !crate::user32::message::erase_window_background(&mut state, win2),
            "no class brush → nothing to erase"
        );
    }

    #[test]
    fn test_is_window_recognizes_real_windows() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let parent = 0x6610_0001_u64;
        let child = 0x6610_0002_u64;
        let ws = state.window_state();
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(parent),
            title: "Parent".to_owned(),
            width: 100,
            height: 100,
            visible: true,
            enabled: true,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(child),
            parent_handle: crate::handles::Hwnd::from(parent),
            control_kind: Some(crate::user32::controls::ControlClassKind::Button),
            title: "Child".to_owned(),
            visible: false,
            enabled: false,
            width: 40,
            height: 20,
            ..Default::default()
        });

        // IsWindow: real windows (including children) are valid.
        for handle in [parent, child] {
            write_regs(&mut engine, handle, 0, 0, 0, 0);
            let r = user32::handle_is_window(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            ))
            .expect("IsWindow");
            assert_eq!(r.return_value, 1, "IsWindow({handle:#x})");
        }
        write_regs(&mut engine, 0xDEAD, 0, 0, 0, 0);
        let r = user32::handle_is_window(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("IsWindow");
        assert_eq!(r.return_value, 0, "unknown handle is not a window");

        // IsWindowVisible: parent visible, child hidden.
        write_regs(&mut engine, parent, 0, 0, 0, 0);
        let r = user32::handle_is_window_visible(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("IsWindowVisible");
        assert_eq!(r.return_value, 1);
        write_regs(&mut engine, child, 0, 0, 0, 0);
        let r = user32::handle_is_window_visible(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("IsWindowVisible");
        assert_eq!(r.return_value, 0);

        // IsWindowEnabled: child disabled, parent enabled.
        write_regs(&mut engine, child, 0, 0, 0, 0);
        let r = user32::handle_is_window_enabled(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("IsWindowEnabled");
        assert_eq!(r.return_value, 0);
        write_regs(&mut engine, parent, 0, 0, 0, 0);
        let r = user32::handle_is_window_enabled(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("IsWindowEnabled");
        assert_eq!(r.return_value, 1);
    }

    #[test]
    fn test_is_dialog_message_enter_resolves_default_button() {
        use crate::user32::{VK_RETURN, WM_KEYDOWN};
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let dialog = 0x6610_0001_u64;
        let ok_button = 0x6610_0002_u64;
        let cancel_button = 0x6610_0003_u64;
        {
            let ws = state.window_state();
            ws.windows.push(WindowRecord {
                handle: crate::handles::Hwnd::from(dialog),
                dialog_proc: 0x7000_0001,
                title: "Dlg".to_owned(),
                width: 160,
                height: 60,
                visible: true,
                ..Default::default()
            });
            ws.windows.push(WindowRecord {
                handle: crate::handles::Hwnd::from(ok_button),
                parent_handle: crate::handles::Hwnd::from(dialog),
                control_kind: Some(crate::user32::controls::ControlClassKind::Button),
                control_text: "OK".to_owned(),
                menu_handle: 1,
                visible: true,
                width: 60,
                height: 20,
                ..Default::default()
            });
            ws.windows.push(WindowRecord {
                handle: crate::handles::Hwnd::from(cancel_button),
                parent_handle: crate::handles::Hwnd::from(dialog),
                control_kind: Some(crate::user32::controls::ControlClassKind::Button),
                control_text: "Cancel".to_owned(),
                menu_handle: 2,
                visible: true,
                width: 60,
                height: 20,
                ..Default::default()
            });
        }
        // OK is the BS_DEFPUSHBUTTON default; focus sits on Cancel.
        state
            .window_state()
            .control_states
            .entry(crate::handles::Hwnd::from(ok_button))
            .or_insert_with(|| crate::user32::controls::ControlClassKind::Button.new_state())
            .set_default_push(true);
        state.window_state().focus_window_handle = crate::handles::Hwnd::from(cancel_button);

        let msg_va = 0x4000;
        engine
            .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
            .expect("map msg");
        let dispatch = |engine: &mut IcedCpu, state: &mut WinApiState| {
            engine
                .mem_write(msg_va + 8, &WM_KEYDOWN.to_le_bytes())
                .expect("message");
            engine
                .mem_write(msg_va + 16, &VK_RETURN.to_le_bytes())
                .expect("wParam");
            write_regs(engine, dialog, msg_va, 0, 0, 0x3000);
            let result = crate::user32::handle_is_dialog_message_a(&mut HandlerContext::new(
                engine,
                test_environment(),
                state,
            ));
            let error = result.expect_err("Enter must be consumed by the dialog");
            *error
                .downcast_ref::<WinApiControlSignal>()
                .expect("control signal")
        };

        // Enter with a default push button activates IT (id 1), not the
        // focused Cancel (id 2).
        let signal = dispatch(&mut engine, &mut state);
        assert!(
            matches!(
                signal,
                WinApiControlSignal::GuestCallbackRequested { request }
                    if request.message == 0x0111 && request.word_parameter == 1
            ),
            "Enter must activate the BS_DEFPUSHBUTTON (id 1), got {signal:?}"
        );

        // Without a default button, Enter falls back to the focused button.
        state
            .window_state()
            .control_states
            .get_mut(&crate::handles::Hwnd::from(ok_button))
            .expect("ok state")
            .set_default_push(false);
        state.window_state().focus_window_handle = crate::handles::Hwnd::from(cancel_button);
        let signal = dispatch(&mut engine, &mut state);
        assert!(
            matches!(
                signal,
                WinApiControlSignal::GuestCallbackRequested { request }
                    if request.word_parameter == 2
            ),
            "Enter with no default must activate the focused button (id 2), got {signal:?}"
        );

        // Neither a default nor a focused button: Enter is NOT consumed
        // (IsDialogMessage returns FALSE, caller dispatches normally).
        state.window_state().focus_window_handle = crate::handles::Hwnd::NULL;
        engine
            .mem_write(msg_va + 8, &WM_KEYDOWN.to_le_bytes())
            .expect("message");
        engine
            .mem_write(msg_va + 16, &VK_RETURN.to_le_bytes())
            .expect("wParam");
        write_regs(&mut engine, dialog, msg_va, 0, 0, 0x3000);
        let r = crate::user32::handle_is_dialog_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("IsDialogMessage completes");
        assert_eq!(
            r.return_value, 0,
            "Enter with no button target must not be consumed"
        );
    }

    #[test]
    fn test_sys_color_mapping() {
        // COLOR_WINDOW (5) → white; COLOR_BTNFACE (15) → gray; these drive the
        // WM_ERASEBKGND class-brush fill and GetSysColor.
        assert_eq!(crate::user32::window::sys_color(5), 0x00FF_FFFF);
        assert_eq!(crate::user32::window::sys_color(15), 0x00F0_F0F0);
        assert_eq!(crate::user32::window::sys_color(0), 0x00C8_C8C8);
    }

    // ── Windows-fidelity tier (★15): EDIT caret/selection + LISTBOX selection ──

    /// An EDIT child of a parent with a guest WndProc, for control tests.
    fn push_edit_pair(state: &mut WinApiState) -> (u64, u64) {
        let parent = 0x6610_0011_u64;
        let edit = 0x6610_0012_u64;
        let ws = state.window_state();
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(parent),
            window_proc: 0x7000_0000,
            title: "Parent".to_owned(),
            width: 200,
            height: 100,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(edit),
            parent_handle: crate::handles::Hwnd::from(parent),
            control_kind: Some(crate::user32::controls::ControlClassKind::Edit),
            control_text: "hello".to_owned(),
            menu_handle: 12,
            visible: true,
            width: 120,
            height: 20,
            ..Default::default()
        });
        (parent, edit)
    }

    /// A LISTBOX child of a parent with a guest WndProc, for control tests.
    fn push_listbox(state: &mut WinApiState) -> (u64, u64) {
        let parent = 0x6610_0013_u64;
        let listbox = 0x6610_0014_u64;
        let ws = state.window_state();
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(parent),
            window_proc: 0x7000_0000,
            title: "Parent".to_owned(),
            width: 200,
            height: 100,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(listbox),
            parent_handle: crate::handles::Hwnd::from(parent),
            control_kind: Some(crate::user32::controls::ControlClassKind::ListBox),
            control_text: String::new(),
            menu_handle: 1,
            visible: true,
            width: 100,
            height: 60,
            ..Default::default()
        });
        (parent, listbox)
    }

    /// Test-only projection of a control's editable bits (window record
    /// interaction flags + per-kind state), so assertions can read `caret` /
    /// `sel_index` / … without matching on the variant.
    #[derive(Debug, Clone, Default)]
    struct ControlUiSnapshot {
        pressed: bool,
        focused: bool,
        default_push: bool,
        items: Vec<String>,
        caret: usize,
        sel_start: usize,
        sel_end: usize,
        sel_index: i32,
    }

    impl ControlUiSnapshot {
        fn of(state: &WinApiState, hwnd: u64) -> Self {
            use crate::user32::controls::ControlState;
            let mut snap = Self::default();
            if let Some(ws) = state.try_window_state() {
                if let Some(window) = ws
                    .windows
                    .iter()
                    .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
                {
                    snap.pressed = window.pressed;
                    snap.focused = window.focused;
                }
                match ws.control_states.get(&crate::handles::Hwnd::from(hwnd)) {
                    Some(ControlState::Button { default_push }) => {
                        snap.default_push = *default_push;
                    }
                    Some(ControlState::Edit {
                        caret,
                        sel_start,
                        sel_end,
                    }) => {
                        snap.caret = *caret;
                        snap.sel_start = *sel_start;
                        snap.sel_end = *sel_end;
                    }
                    Some(ControlState::ListBox { items, sel_index }) => {
                        snap.items = items.clone();
                        snap.sel_index = *sel_index;
                    }
                    Some(ControlState::ComboBox { items, .. }) => {
                        snap.items = items.clone();
                    }
                    Some(ControlState::Static) | None => {}
                }
            }
            snap
        }
    }

    /// The projected control UI state for a window (defaults when never touched).
    fn control_ui(state: &WinApiState, hwnd: u64) -> ControlUiSnapshot {
        ControlUiSnapshot::of(state, hwnd)
    }

    /// The control text of a window (EM_GETTEXT-side read for assertions).
    fn control_text(state: &WinApiState, hwnd: u64) -> String {
        state
            .try_window_state()
            .expect("window state")
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .map_or_else(String::new, |w| w.control_text.clone())
    }

    /// Seed a listbox's items through LB_ADDSTRING (ANSI strings in memory).
    fn seed_listbox(engine: &mut IcedCpu, state: &mut WinApiState, listbox: u64) {
        for (va, item) in [(0x4000_u64, "alpha"), (0x5000, "beta")] {
            let mut bytes = item.as_bytes().to_vec();
            bytes.push(0);
            engine.mem_write(va, &bytes).expect("write list item");
            crate::user32::controls::dispatch_control_proc(
                engine,
                state,
                listbox,
                crate::user32::LB_ADDSTRING,
                0,
                va,
            )
            .expect("add ok")
            .expect("some result");
        }
    }

    #[test]
    fn test_edit_em_setsel_getsel_packing() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_, edit) = push_edit_pair(&mut state);

        // EM_SETSEL(2, 4) → selection [2, 4), caret at the end edge (4).
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_SETSEL,
            2,
            4,
        )
        .expect("setsel ok")
        .expect("some result");
        assert_eq!(r, 1, "EM_SETSEL returns TRUE");
        let ui = control_ui(&state, edit);
        assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (2, 4, 4));

        // EM_GETSEL (no pointers) returns MAKELONG(start, end) = 2 | 4 << 16.
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_GETSEL,
            0,
            0,
        )
        .expect("getsel ok")
        .expect("some result");
        assert_eq!(r, 0x0004_0002, "EM_GETSEL packs MAKELONG(start, end)");
    }

    #[test]
    fn test_edit_em_setsel_negative_selects_all() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_, edit) = push_edit_pair(&mut state);

        // EM_SETSEL(0, -1) → select everything: [0, len).
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_SETSEL,
            0,
            u64::from(u32::MAX),
        )
        .expect("setsel ok")
        .expect("some result");
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_GETSEL,
            0,
            0,
        )
        .expect("getsel ok")
        .expect("some result");
        assert_eq!(r, 0x0005_0000, "select-all = MAKELONG(0, 5)");
    }

    #[test]
    fn test_edit_em_getsel_writes_output_pointers() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_, edit) = push_edit_pair(&mut state);
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_SETSEL,
            1,
            3,
        )
        .expect("setsel ok")
        .expect("some result");

        // EM_GETSEL with output pointers at 0x3000 (start) / 0x3004 (end).
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_GETSEL,
            0x3000,
            0x3004,
        )
        .expect("getsel ok")
        .expect("some result");
        let mut start = [0u8; 4];
        let mut end = [0u8; 4];
        engine.mem_read(0x3000, &mut start).expect("read start");
        engine.mem_read(0x3004, &mut end).expect("read end");
        assert_eq!(
            u32::from_le_bytes(start),
            1,
            "wParam pointer receives start"
        );
        assert_eq!(u32::from_le_bytes(end), 3, "lParam pointer receives end");
    }

    #[test]
    fn test_edit_wm_char_inserts_at_caret() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_, edit) = push_edit_pair(&mut state);

        // Type 'X' at caret 0 → "Xhello", caret advances to 1.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_CHAR,
            u64::from(u32::from('X')),
            0,
        )
        .expect_err("WM_CHAR that mutates the text delivers EN_CHANGE");
        assert_eq!(control_text(&state, edit), "Xhello");
        assert_eq!(control_ui(&state, edit).caret, 1);

        // Home + End, then 'Y' appends at the end → "XhelloY".
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_END,
            0,
        )
        .expect("end ok")
        .expect("some result");
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_CHAR,
            u64::from(u32::from('Y')),
            0,
        )
        .expect_err("WM_CHAR that mutates the text delivers EN_CHANGE");
        assert_eq!(control_text(&state, edit), "XhelloY");
        assert_eq!(control_ui(&state, edit).caret, 7);

        // Enter does not change the text and delivers nothing.
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_CHAR,
            0x0D,
            0,
        )
        .expect("enter ok")
        .expect("some result");
        assert_eq!(r, 0);
        assert_eq!(control_text(&state, edit), "XhelloY");
    }

    #[test]
    fn test_edit_char_replaces_selection_and_delivers_en_change() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (parent, edit) = push_edit_pair(&mut state);

        // Select "ell" (chars 1..4) of "hello".
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_SETSEL,
            1,
            4,
        )
        .expect("setsel ok")
        .expect("some result");

        // Typing 'X' replaces [1, 4) → "hXo", clears the selection.
        let result = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_CHAR,
            u64::from(u32::from('X')),
            0,
        );
        let error = result.expect_err("WM_CHAR must deliver EN_CHANGE to the parent");
        let signal = error
            .downcast_ref::<WinApiControlSignal>()
            .expect("control signal");
        assert!(
            matches!(
                signal,
                WinApiControlSignal::GuestCallbackRequested { request }
                    if request.window_handle == parent
                        && request.message == 0x0111
                        && request.word_parameter == 0x0001_000C
            ),
            "WM_CHAR must deliver WM_COMMAND(MAKEWPARAM(12, EN_CHANGE)), got {signal:?}"
        );

        assert_eq!(control_text(&state, edit), "hXo");
        let ui = control_ui(&state, edit);
        assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (2, 2, 2));
    }

    #[test]
    fn test_edit_backspace_and_delete_at_caret() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_, edit) = push_edit_pair(&mut state);

        // Home + Right + Right (caret 2), Backspace → deletes the char before
        // the caret ('e', index 1) → "hllo", caret back at 1.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_HOME,
            0,
        )
        .expect("home ok")
        .expect("some result");
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_RIGHT,
            0,
        )
        .expect("right ok")
        .expect("some result");
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_RIGHT,
            0,
        )
        .expect("right2 ok")
        .expect("some result");
        assert_eq!(control_ui(&state, edit).caret, 2);
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_CHAR,
            0x08,
            0,
        )
        .expect_err("backspace delivers EN_CHANGE");
        assert_eq!(control_text(&state, edit), "hllo");
        assert_eq!(control_ui(&state, edit).caret, 1);

        // VK_DELETE at caret 1 deletes 'l' (index 1) → "hlo".
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_DELETE,
            0,
        )
        .expect_err("delete delivers EN_CHANGE");
        assert_eq!(control_text(&state, edit), "hlo");

        // Delete past the end: no text change, no notification.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_END,
            0,
        )
        .expect("end ok")
        .expect("some result");
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_DELETE,
            0,
        )
        .expect("delete at end ok")
        .expect("some result");
        assert_eq!(r, 0);
        assert_eq!(control_text(&state, edit), "hlo");
    }

    #[test]
    fn test_edit_arrow_keys_move_caret() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_, edit) = push_edit_pair(&mut state);

        // Home → caret 0.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_HOME,
            0,
        )
        .expect("home ok")
        .expect("some result");
        assert_eq!(control_ui(&state, edit).caret, 0);

        // Right → caret 1; End → caret 5 (len of "hello"); Left → 4.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_RIGHT,
            0,
        )
        .expect("right ok")
        .expect("some result");
        assert_eq!(control_ui(&state, edit).caret, 1);
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_END,
            0,
        )
        .expect("end ok")
        .expect("some result");
        assert_eq!(control_ui(&state, edit).caret, 5);
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_LEFT,
            0,
        )
        .expect("left ok")
        .expect("some result");
        assert_eq!(control_ui(&state, edit).caret, 4);

        // Left at the start is a no-op (stays 0 after Home).
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_HOME,
            0,
        )
        .expect("home ok")
        .expect("some result");
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_LEFT,
            0,
        )
        .expect("left at start ok")
        .expect("some result");
        assert_eq!(control_ui(&state, edit).caret, 0);
    }

    #[test]
    fn test_edit_shift_arrow_extends_selection() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_, edit) = push_edit_pair(&mut state);

        // End (no shift) → caret 5, no selection; then hold Shift.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_END,
            0,
        )
        .expect("end ok")
        .expect("some result");
        assert_eq!(control_ui(&state, edit).caret, 5);
        state.window_state().keyboard_state.0[0x10] |= 0x80; // VK_SHIFT held

        // Shift+Left selects the last char: [4, 5), caret 4.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_LEFT,
            0,
        )
        .expect("shift-left ok")
        .expect("some result");
        let ui = control_ui(&state, edit);
        assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (4, 5, 4));

        // Shift+Left again extends: [3, 5), caret 3.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_LEFT,
            0,
        )
        .expect("shift-left2 ok")
        .expect("some result");
        let ui = control_ui(&state, edit);
        assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (3, 5, 3));

        // Release Shift; Right collapses the selection and moves the caret.
        state.window_state().keyboard_state.0[0x10] &= !0x80;
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KEYDOWN,
            crate::user32::VK_RIGHT,
            0,
        )
        .expect("right ok")
        .expect("some result");
        let ui = control_ui(&state, edit);
        assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (4, 4, 4));
    }

    #[test]
    fn test_listbox_setcursel_getcursel_and_lbn_selchange() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (parent, listbox) = push_listbox(&mut state);
        seed_listbox(&mut engine, &mut state, listbox);

        // Fresh listbox: no selection.
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::LB_GETCURSEL,
            0,
            0,
        )
        .expect("getcursel ok")
        .expect("some result");
        assert_eq!(r, u64::MAX, "LB_GETCURSEL = LB_ERR without a selection");

        // LB_SETCURSEL(1) selects the second item and delivers LBN_SELCHANGE.
        let result = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::LB_SETCURSEL,
            1,
            0,
        );
        let error = result.expect_err("LB_SETCURSEL must deliver LBN_SELCHANGE");
        let signal = error
            .downcast_ref::<WinApiControlSignal>()
            .expect("control signal");
        assert!(
            matches!(
                signal,
                WinApiControlSignal::GuestCallbackRequested { request }
                    if request.window_handle == parent
                        && request.message == 0x0111
                        && request.word_parameter == 0x0001_0001
            ),
            "LB_SETCURSEL must deliver WM_COMMAND(MAKEWPARAM(1, LBN_SELCHANGE)), \
             got {signal:?}"
        );

        // Now LB_GETCURSEL = 1.
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::LB_GETCURSEL,
            0,
            0,
        )
        .expect("getcursel ok")
        .expect("some result");
        assert_eq!(r, 1);

        // Re-selecting the same index does not re-notify (returns 0).
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::LB_SETCURSEL,
            1,
            0,
        )
        .expect("same setcursel ok")
        .expect("some result");
        assert_eq!(r, 0);

        // Out-of-range index → LB_ERR, selection unchanged.
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::LB_SETCURSEL,
            99,
            0,
        )
        .expect("out-of-range ok")
        .expect("some result");
        assert_eq!(r, u64::MAX, "out-of-range LB_SETCURSEL returns LB_ERR");
        assert_eq!(control_ui(&state, listbox).sel_index, 1);

        // LB_SETCURSEL(-1) clears the selection.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::LB_SETCURSEL,
            u64::from(u32::MAX),
            0,
        )
        .expect_err("clearing the selection delivers LBN_SELCHANGE");
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::LB_GETCURSEL,
            0,
            0,
        )
        .expect("getcursel ok")
        .expect("some result");
        assert_eq!(r, u64::MAX, "cleared selection reads back as LB_ERR");
    }

    #[test]
    fn test_listbox_click_selects_item_and_notifies() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (parent, listbox) = push_listbox(&mut state);
        seed_listbox(&mut engine, &mut state, listbox);

        // Click at client (2, 18): row 1 = 18 / 16 → selects "beta".
        let lparam = u64::from(2_u16) | (u64::from(18_u16) << 16);
        let result = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::WM_LBUTTONDOWN,
            0,
            lparam,
        );
        let error = result.expect_err("click must deliver LBN_SELCHANGE");
        let signal = error
            .downcast_ref::<WinApiControlSignal>()
            .expect("control signal");
        assert!(
            matches!(
                signal,
                WinApiControlSignal::GuestCallbackRequested { request }
                    if request.window_handle == parent && request.word_parameter == 0x0001_0001
            ),
            "listbox click must deliver WM_COMMAND(MAKEWPARAM(1, LBN_SELCHANGE)), \
             got {signal:?}"
        );
        assert_eq!(control_ui(&state, listbox).sel_index, 1);

        // Clicking the same row again does NOT re-notify.
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::WM_LBUTTONDOWN,
            0,
            lparam,
        )
        .expect("same click ok")
        .expect("some result");
        assert_eq!(r, 0);

        // A click below the items (y = 60) selects nothing.
        let below = u64::from(2_u16) | (u64::from(60_u16) << 16);
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::WM_LBUTTONDOWN,
            0,
            below,
        )
        .expect("below items ok")
        .expect("some result");
        assert_eq!(r, 0);
        assert_eq!(
            control_ui(&state, listbox).sel_index,
            1,
            "selection unchanged"
        );
    }

    /// A top-level window with an EDIT child, for paint tests.
    fn push_edit_paint_pair(state: &mut WinApiState) -> (u64, u64) {
        let top = 0x6610_0021_u64;
        let edit = 0x6610_0022_u64;
        let ws = state.window_state();
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(top),
            title: "Top".to_owned(),
            visible: true,
            width: 200,
            height: 100,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(edit),
            parent_handle: crate::handles::Hwnd::from(top),
            x: 10,
            y: 10,
            width: 60,
            height: 20,
            control_kind: Some(crate::user32::controls::ControlClassKind::Edit),
            control_text: "hello".to_owned(),
            visible: true,
            ..Default::default()
        });
        (top, edit)
    }

    #[test]
    fn test_edit_paint_draws_caret_and_selection_in_ancestor_surface() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (top, edit) = push_edit_paint_pair(&mut state);

        // Focus + select "el" (chars 1..3) → caret at 3.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_SETFOCUS,
            0,
            0,
        )
        .expect("focus ok")
        .expect("some result");
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_SETSEL,
            1,
            3,
        )
        .expect("setsel ok")
        .expect("some result");
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_PAINT,
            0,
            0,
        )
        .expect("paint ok")
        .expect("some result");
        // B3.5: the paint deferred its publish; flush it (the runtime drains
        // pending publishes once per message dispatch).
        state.present().drain_pending_publishes();

        let frame = state
            .present()
            .published
            .get(&crate::handles::Hwnd::from(top))
            .expect("published frame");
        assert_eq!((frame.width, frame.height), (200, 100));
        // Font-dependent pixels: assert qualitatively instead of at fixed
        // monospace positions. The selection fill (COLOR_HIGHLIGHT) must be
        // present somewhere in the edit's rows.
        let highlight_count = frame.pixels.iter().filter(|&&p| p == 0x0000_78D7).count();
        assert!(
            highlight_count > 0,
            "selected cells must be filled with COLOR_HIGHLIGHT"
        );
        let focused_frame = frame.clone();

        // KILLFOCUS + repaint: no caret, no selection fill.
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_KILLFOCUS,
            0,
            0,
        )
        .expect("killfocus ok")
        .expect("some result");
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::WM_PAINT,
            0,
            0,
        )
        .expect("paint2 ok")
        .expect("some result");
        state.present().drain_pending_publishes();
        let frame = state
            .present()
            .published
            .get(&crate::handles::Hwnd::from(top))
            .expect("published frame");
        let highlight_count = frame.pixels.iter().filter(|&&p| p == 0x0000_78D7).count();
        assert_eq!(highlight_count, 0, "no highlight without focus");
        // The caret column is font-dependent (its x is the summed advance of
        // the preceding chars, and glyph ink may reach it), so prove the
        // focus change only by the frames differing — the selection fill and
        // caret are the only things that change between them.
        let diffs = frame
            .pixels
            .iter()
            .zip(focused_frame.pixels.iter())
            .filter(|(u, f)| *u != *f)
            .count();
        assert!(diffs > 0, "losing focus must change the painted pixels");
    }

    #[test]
    fn test_listbox_paint_highlights_selected_row() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let (_parent, listbox) = push_listbox(&mut state);
        seed_listbox(&mut engine, &mut state, listbox);
        // Place the listbox as a child of a real top-level window so the
        // ancestor surface exists at a known size.
        let top = 0x6610_0023_u64;
        state.window_state().windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(top),
            title: "Top".to_owned(),
            window_proc: 0x7000_0000,
            visible: true,
            width: 200,
            height: 100,
            ..Default::default()
        });
        state
            .window_state()
            .windows
            .iter_mut()
            .find(|w| w.handle == crate::handles::Hwnd::from(listbox))
            .expect("listbox record")
            .parent_handle = crate::handles::Hwnd::from(top);
        // Position the listbox at (10, 10) so the item rows land on known
        // surface coordinates (push_listbox leaves x/y at their defaults).
        let listbox_record = state
            .window_state()
            .windows
            .iter_mut()
            .find(|w| w.handle == crate::handles::Hwnd::from(listbox))
            .expect("listbox record");
        listbox_record.x = 10;
        listbox_record.y = 10;

        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::LB_SETCURSEL,
            0,
            0,
        )
        .expect_err("LB_SETCURSEL delivers LBN_SELCHANGE to the parent");
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            listbox,
            crate::user32::WM_PAINT,
            0,
            0,
        )
        .expect("paint ok")
        .expect("some result");
        state.present().drain_pending_publishes();

        let frame = state
            .present()
            .published
            .get(&crate::handles::Hwnd::from(top))
            .expect("published frame");
        // Font-dependent rows (line height varies by system font): assert the
        // selection fill exists somewhere in the top rows and the unselected
        // region below is still COLOR_WINDOW.
        let highlight_count = frame.pixels.iter().filter(|&&p| p == 0x0000_78D7).count();
        assert!(
            highlight_count > 0,
            "selected row must be filled with COLOR_HIGHLIGHT"
        );
        let window_count = frame.pixels.iter().filter(|&&p| p == 0x00FF_FFFF).count();
        assert!(window_count > 0, "unselected rows stay COLOR_WINDOW");
    }

    #[test]
    fn test_invalidate_rect_partial_publish_region() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // Top-level 200×100 window (parentless → its own surface).
        let hwnd = 0x6610_00AA_u64;
        state.window_state().windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(hwnd),
            window_proc: 0x7000_0000,
            width: 200,
            height: 100,
            visible: true,
            ..Default::default()
        });

        // First paint: full-window fill on a fresh surface must publish a FULL
        // frame (region None) — a fresh buffer has no up-to-date pixels.
        gdi32::fill_rect_surface(
            &mut state,
            crate::handles::Hwnd::from(hwnd),
            200,
            100,
            0,
            0,
            200,
            100,
            0,
        );
        // B3.5: the fill deferred its publish; flush it (the runtime drains
        // pending publishes once per message dispatch).
        state.present().drain_pending_publishes();
        let frame = state
            .present()
            .published
            .get(&crate::handles::Hwnd::from(hwnd))
            .expect("full frame");
        assert_eq!((frame.width, frame.height), (200, 100));
        assert_eq!(frame.pixels.len(), 200 * 100);
        // Publish-model rework (ora-5): every publish is full — no region.
        // Partial InvalidateRect(hwnd, {10,20,40,60}): the WM_PAINT synthesis
        // flag is set AND the publish-side dirty rect accumulates the rect.
        write_regs(&mut engine, hwnd, 0x2000, 1, 0, 0);
        for (offset, value) in [(0_u64, 10_i32), (4, 20), (8, 40), (12, 60)] {
            engine
                .mem_write(0x2000 + offset, &value.to_le_bytes())
                .expect("write RECT field");
        }
        user32::handle_invalidate_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("InvalidateRect succeeds");
        assert!(
            state
                .window_state()
                .windows
                .iter()
                .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
                .is_some_and(|w| w.invalidated),
            "InvalidateRect must still set the WM_PAINT synthesis flag"
        );

        // The repaint fill covers only the invalidated rect (a real WM_PAINT
        // paints its update region). The publish must carry exactly that rect
        // as the frame region — and the full buffer must remain the complete
        // composite (B1 hand-back keeps accumulation intact across publishes).
        gdi32::fill_rect_surface(
            &mut state,
            crate::handles::Hwnd::from(hwnd),
            200,
            100,
            10,
            20,
            30,
            40,
            0x00FF_0000,
        );
        state.present().drain_pending_publishes();
        let frame = state
            .present()
            .published
            .get(&crate::handles::Hwnd::from(hwnd))
            .expect("partial frame");
        // Publish-model rework (ora-5): every publish is full — no region.
        assert_eq!((frame.width, frame.height), (200, 100));
        assert_eq!(frame.pixels.len(), 200 * 100);

        // A subsequent full-window repaint also publishes full — every
        // publish is full under the rework, regardless of coverage.
        gdi32::fill_rect_surface(
            &mut state,
            crate::handles::Hwnd::from(hwnd),
            200,
            100,
            0,
            0,
            200,
            100,
            0x0000_00FF,
        );
        state.present().drain_pending_publishes();
        let frame = state
            .present()
            .published
            .get(&crate::handles::Hwnd::from(hwnd))
            .expect("full frame 2");
        assert_eq!((frame.width, frame.height), (200, 100));
        assert_eq!(frame.pixels.len(), 200 * 100);
    }

    // ── P3 D3D9 software-render handlers ────────────────────────────────

    /// D3DCLEAR_TARGET (d3d9.rs keeps these private).
    const D3DCLEAR_TARGET: u32 = 0x0000_0001;
    /// D3DTS_WORLD.
    const D3DTS_WORLD: u32 = 256;
    /// D3DPT_TRIANGLELIST.
    const D3DPT_TRIANGLELIST: u32 = 4;

    #[test]
    fn test_d3d9_caps_declare_pixel_shader_pipeline() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let caps_va = 0x5000_u64;
        // GetDeviceCaps(this, adapter=0, type=HAL, pCaps).
        write_regs(&mut engine, 1, 0, 1, caps_va, 0);
        assert_return_value!(
            d3d9::handle_get_device_caps(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0 // D3D_OK
        );
        let mut read_u32_at = |offset: u64| -> u32 {
            let mut bytes = [0_u8; 4];
            engine
                .mem_read(caps_va + offset, &mut bytes)
                .expect("read caps field");
            u32::from_le_bytes(bytes)
        };
        // P5a caps honesty: the ps_2_0 interpreter is implemented, so the caps
        // report D3DPS_VERSION(2,0) and PixelShader1xMaxValue 1.0; the vertex
        // stage is still FFP (vs_2_0 execution is P5a-2), so VertexShaderVersion
        // stays 0 and MaxVertexShaderConst reports the vs_2_0 constant file.
        assert_eq!(read_u32_at(196), 0, "VertexShaderVersion must be 0.0");
        assert_eq!(
            read_u32_at(200),
            256,
            "MaxVertexShaderConst must be 256 (vs_2_0 constant file)"
        );
        assert_eq!(
            read_u32_at(204),
            0xFFFF_0200,
            "PixelShaderVersion must be D3DPS_VERSION(2,0)"
        );
        assert_eq!(
            read_u32_at(208),
            1.0_f32.to_bits(),
            "PixelShader1xMaxValue must be 1.0"
        );
    }

    #[test]
    fn test_d3d9_clear_fills_whole_backbuffer() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        {
            let d3d = state.d3d9();
            d3d.d3d9_backbuffer_width = 4;
            d3d.d3d9_backbuffer_height = 3;
            d3d.d3d9_backbuffer = vec![0_u32; 12];
        }
        // Clear(this, Count=0, pRects=NULL, Flags=TARGET, Color=0xFFC80000).
        write_regs(&mut engine, 1, 0, 0, u64::from(D3DCLEAR_TARGET), 0);
        engine
            .mem_write(STACK_TOP + 0x28, &0xFF_C8_00_00_u32.to_le_bytes())
            .expect("write clear color");
        assert_return_value!(
            d3d9::handle_clear(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let back = &state.d3d9().d3d9_backbuffer;
        assert_eq!(back.len(), 12);
        for (index, pixel) in back.iter().enumerate() {
            assert_eq!(
                *pixel, 0x00_C8_00_00,
                "backbuffer pixel {index} must be the clear color (0RGB)"
            );
        }
        assert_eq!(
            state.d3d9().d3d9_dirty,
            None,
            "Clear marks the frame full-dirty"
        );
    }

    #[test]
    fn test_d3d9_clear_fills_only_requested_rects() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        {
            let d3d = state.d3d9();
            d3d.d3d9_backbuffer_width = 4;
            d3d.d3d9_backbuffer_height = 3;
            d3d.d3d9_backbuffer = vec![0_u32; 12];
        }
        // One D3DRECT {1,1,3,3} at 0x6000.
        let rect_va = 0x6000_u64;
        for (i, v) in [1_i32, 1, 3, 3].iter().enumerate() {
            engine
                .mem_write(
                    rect_va + u64::try_from(i).unwrap_or(0) * 4,
                    &v.to_le_bytes(),
                )
                .expect("write rect field");
        }
        write_regs(&mut engine, 1, 1, rect_va, u64::from(D3DCLEAR_TARGET), 0);
        engine
            .mem_write(STACK_TOP + 0x28, &0xFF_00_00_FF_u32.to_le_bytes())
            .expect("write clear color");
        assert_return_value!(
            d3d9::handle_clear(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let back = &state.d3d9().d3d9_backbuffer;
        // Row y, column x → back[y * 4 + x].
        assert_eq!(
            back.first().copied(),
            Some(0),
            "outside rect stays unchanged"
        );
        assert_eq!(
            back.get(4 + 2).copied(),
            Some(0x00_00_00_FF),
            "inside rect filled"
        );
        assert_eq!(
            back.get(8 + 2).copied(),
            Some(0x00_00_00_FF),
            "inside rect filled"
        );
        assert_eq!(
            back.get(4).copied(),
            Some(0),
            "outside rect stays unchanged"
        );
    }

    #[test]
    fn test_d3d9_begin_end_scene_flags() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, 1, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_begin_scene(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        assert!(state.d3d9().d3d9_scene_active);
        // A second BeginScene inside a scene fails.
        write_regs(&mut engine, 1, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_begin_scene(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0x8876_086c // D3DERR_INVALIDCALL
        );
        write_regs(&mut engine, 1, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_end_scene(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        assert!(!state.d3d9().d3d9_scene_active);
        // EndScene outside a scene fails.
        write_regs(&mut engine, 1, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_end_scene(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0x8876_086c
        );
    }

    #[test]
    fn test_d3d9_set_transform_and_viewport_state() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // A known 4x4 matrix (column-major) at 0x6000.
        let matrix_va = 0x6000_u64;
        let mut matrix = [0.0_f32; 16];
        matrix[0] = 2.0;
        matrix[5] = 3.0;
        matrix[10] = 0.5;
        matrix[15] = 1.0;
        for (i, value) in matrix.iter().enumerate() {
            engine
                .mem_write(
                    matrix_va + u64::try_from(i).unwrap_or(0) * 4,
                    &value.to_le_bytes(),
                )
                .expect("write matrix element");
        }
        write_regs(&mut engine, 1, u64::from(D3DTS_WORLD), matrix_va, 0, 0);
        assert_return_value!(
            d3d9::handle_set_transform(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        assert_eq!(
            state.d3d9().d3d9_world_matrix.map(f32::to_bits),
            matrix.map(f32::to_bits),
            "SetTransform must store the matrix unchanged"
        );

        // SetViewport: D3DVIEWPORT9 {X,Y,Width,Height,MinZ,MaxZ} at 0x6400.
        let vp_va = 0x6400_u64;
        for (i, v) in [4_u32, 5, 100, 50].iter().enumerate() {
            engine
                .mem_write(vp_va + u64::try_from(i).unwrap_or(0) * 4, &v.to_le_bytes())
                .expect("write viewport field");
        }
        engine
            .mem_write(vp_va + 16, &0.25_f32.to_le_bytes())
            .expect("write MinZ");
        engine
            .mem_write(vp_va + 20, &0.75_f32.to_le_bytes())
            .expect("write MaxZ");
        write_regs(&mut engine, 1, vp_va, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_set_viewport(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let (vp_x, vp_y, vp_w, vp_h, vp_min_z, vp_max_z) = state.d3d9().d3d9_viewport;
        assert_eq!((vp_x, vp_y, vp_w, vp_h), (4, 5, 100, 50));
        // Bit-exact float compare (0.25/0.75 are exactly representable).
        assert_eq!(vp_min_z.to_bits(), 0.25_f32.to_bits());
        assert_eq!(vp_max_z.to_bits(), 0.75_f32.to_bits());

        // GetViewport writes it back.
        let out_va = 0x6800_u64;
        write_regs(&mut engine, 1, out_va, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_get_viewport(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut out = [0_u8; 24];
        engine
            .mem_read(out_va, &mut out)
            .expect("read viewport out");
        assert_eq!(u32::from_le_bytes(out[0..4].try_into().expect("x")), 4);
        assert_eq!(u32::from_le_bytes(out[8..12].try_into().expect("w")), 100);
    }

    #[test]
    fn test_d3d9_draw_primitive_up_rasterizes_triangle() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        {
            let d3d = state.d3d9();
            d3d.d3d9_backbuffer_width = 16;
            d3d.d3d9_backbuffer_height = 16;
            d3d.d3d9_backbuffer = vec![0_u32; 16 * 16];
            d3d.d3d9_viewport = (0, 0, 16, 16, 0.0, 1.0);
        }
        // FVF = XYZ | DIFFUSE.
        write_regs(&mut engine, 1, u64::from(0x0002_u32 | 0x0040_u32), 0, 0, 0);
        assert_return_value!(
            d3d9::handle_set_fvf(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        write_regs(&mut engine, 1, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_begin_scene(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        // Clear to red first.
        write_regs(&mut engine, 1, 0, 0, u64::from(D3DCLEAR_TARGET), 0);
        engine
            .mem_write(STACK_TOP + 0x28, &0xFF_C8_00_00_u32.to_le_bytes())
            .expect("write clear color");
        assert_return_value!(
            d3d9::handle_clear(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        // Full-frame triangle (NDC corners) with per-vertex colors.
        let data_va = 0x6000_u64;
        let vertices: [[f32; 3]; 3] = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [-1.0, 1.0, 0.0]];
        let colors: [u32; 3] = [0xFF_FF_00_00, 0xFF_00_FF_00, 0xFF_00_00_FF];
        for (i, vertex) in vertices.iter().enumerate() {
            for (j, component) in vertex.iter().enumerate() {
                engine
                    .mem_write(
                        data_va
                            + u64::try_from(i).unwrap_or(0) * 16
                            + u64::try_from(j).unwrap_or(0) * 4,
                        &component.to_le_bytes(),
                    )
                    .expect("write vertex position");
            }
            engine
                .mem_write(
                    data_va + u64::try_from(i).unwrap_or(0) * 16 + 12,
                    &colors.get(i).copied().unwrap_or(0).to_le_bytes(),
                )
                .expect("write vertex color");
        }
        // DrawPrimitiveUP(this, TRIANGLELIST, 1, data, stride=16).
        write_regs(&mut engine, 1, u64::from(D3DPT_TRIANGLELIST), 1, data_va, 0);
        engine
            .mem_write(STACK_TOP + 0x28, &16_u64.to_le_bytes())
            .expect("write stride");
        assert_return_value!(
            d3d9::handle_draw_primitive_up(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        // The triangle spans the bottom-left half (bounded by the diagonal
        // from (0,0) to (16,16)): inside pixels are colored, outside pixels
        // keep the clear red.
        let back = &state.d3d9().d3d9_backbuffer;
        let pixel = |x: u32, y: u32| {
            back.get(usize::try_from(y).unwrap_or(0) * 16 + usize::try_from(x).unwrap_or(0))
                .copied()
        };
        assert_eq!(
            pixel(14, 1),
            Some(0x00_C8_00_00),
            "pixels above the diagonal keep the clear color"
        );
        let red_channel = (pixel(1, 14).unwrap_or(0) >> 16) & 0xFF;
        let green_channel = (pixel(14, 14).unwrap_or(0) >> 8) & 0xFF;
        let blue_channel = pixel(1, 1).unwrap_or(0) & 0xFF;
        assert!(red_channel > 0xB0, "bottom-left corner red-dominant");
        assert!(green_channel > 0xB0, "bottom-right corner green-dominant");
        assert!(blue_channel > 0xB0, "top-left corner blue-dominant");
        // The dirty region covers the whole frame.
        assert_eq!(state.d3d9().d3d9_dirty, None, "draw keeps full-frame dirty");

        // EndScene then Present publishes the frame.
        write_regs(&mut engine, 1, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_end_scene(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        write_regs(&mut engine, 1, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_present(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
    }

    #[test]
    fn test_d3d9_present_publishes_surface_frame() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        {
            let d3d = state.d3d9();
            d3d.d3d9_backbuffer_width = 4;
            d3d.d3d9_backbuffer_height = 3;
            d3d.d3d9_backbuffer = (0_u32..12).collect();
            d3d.d3d9_present_hwnd = crate::handles::Hwnd::from(0x7777);
            d3d.d3d9_dirty = None;
        }
        // Unknown hwnd → window_client_size falls back to WindowState.
        state.window_state().window_width = 4;
        state.window_state().window_height = 3;

        write_regs(&mut engine, 1, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_present(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let frame = state
            .present()
            .published
            .get(&crate::handles::Hwnd::from(0x7777))
            .expect("Present must publish a SurfaceFrame");
        assert_eq!((frame.width, frame.height), (4, 3));
        assert_eq!(&frame.pixels[..], &(0_u32..12).collect::<Vec<u32>>()[..]);
    }

    // ── P4b texture handlers ───────────────────────────────────────────

    /// D3DFMT_A8R8G8B8.
    const D3DFMT_A8R8G8B8: u32 = 21;

    #[test]
    fn test_d3d9_texture_lock_unlock_round_trip() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // The runtime seeds the guest heap bump cursor at session init; the
        // test heap control block starts zeroed, so seed it before allocating.
        engine
            .mem_write(0x2000, &0x2000_u64.to_le_bytes())
            .expect("seed bump cursor");

        // CreateTexture(2x2, levels=1, format=A8R8G8B8) → texture at 0x7000.
        let pp_texture = 0x7000_u64;
        write_regs(&mut engine, 1, 2, 2, 1, 0);
        engine
            .mem_write(STACK_TOP + 0x30, &D3DFMT_A8R8G8B8.to_le_bytes())
            .expect("write format");
        engine
            .mem_write(STACK_TOP + 0x40, &pp_texture.to_le_bytes())
            .expect("write ppTexture");
        assert_return_value!(
            d3d9::handle_create_texture(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut tex_bytes = [0_u8; 8];
        engine
            .mem_read(pp_texture, &mut tex_bytes)
            .expect("read texture ptr");
        let texture_va = u64::from_le_bytes(tex_bytes);
        assert_ne!(texture_va, 0, "CreateTexture must return an object");

        // GetSurfaceLevel(0) → surface at 0x7100.
        let pp_surface = 0x7100_u64;
        write_regs(&mut engine, texture_va, 0, pp_surface, 0, 0);
        assert_return_value!(
            d3d9::handle_texture_get_surface_level(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut surf_bytes = [0_u8; 8];
        engine
            .mem_read(pp_surface, &mut surf_bytes)
            .expect("read surface ptr");
        let surface_va = u64::from_le_bytes(surf_bytes);
        assert_ne!(surface_va, 0, "GetSurfaceLevel must return a surface");

        // LockRect → D3DLOCKED_RECT { Pitch, pBits } at 0x7200; write texels.
        let locked_rect = 0x7200_u64;
        write_regs(&mut engine, surface_va, locked_rect, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_surface_lock_rect(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut pitch_bytes = [0_u8; 4];
        engine
            .mem_read(locked_rect, &mut pitch_bytes)
            .expect("read pitch");
        assert_eq!(u32::from_le_bytes(pitch_bytes), 8, "2x2 pitch must be 8");
        let mut bits_bytes = [0_u8; 8];
        engine
            .mem_read(locked_rect + 8, &mut bits_bytes)
            .expect("read pBits");
        let p_bits = u64::from_le_bytes(bits_bytes);
        assert_ne!(p_bits, 0, "LockRect must hand out a guest block");

        // 2x2 texels: red / green / blue / white (D3DCOLOR).
        let texels = [0xFFFF_0000_u32, 0xFF00_FF00, 0xFF00_00FF, 0xFFFF_FFFF];
        for (i, texel) in texels.iter().enumerate() {
            engine
                .mem_write(
                    p_bits + u64::try_from(i).unwrap_or(0) * 4,
                    &texel.to_le_bytes(),
                )
                .expect("write texel");
        }

        // UnlockRect → texels land in the host record.
        write_regs(&mut engine, surface_va, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_surface_unlock_rect(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let record = state
            .d3d9()
            .d3d9_textures
            .get(&texture_va)
            .expect("record exists");
        assert_eq!(record.width, 2);
        assert_eq!(record.height, 2);
        assert_eq!(
            record.pixels,
            texels.to_vec(),
            "unlock must copy the texels back"
        );
        assert_eq!(record.locked_va, 0, "lock state cleared");

        // SetTexture(0, tex) → GetTexture(0) round-trip.
        write_regs(&mut engine, 1, 0, texture_va, 0, 0);
        assert_return_value!(
            d3d9::handle_set_texture(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let out_texture = 0x7300_u64;
        write_regs(&mut engine, 1, 0, out_texture, 0, 0);
        assert_return_value!(
            d3d9::handle_get_texture(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut out_bytes = [0_u8; 8];
        engine
            .mem_read(out_texture, &mut out_bytes)
            .expect("read texture out");
        assert_eq!(u64::from_le_bytes(out_bytes), texture_va);

        // Release the texture: record gone, bindings cleared.
        write_regs(&mut engine, texture_va, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_texture_release(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            1
        );
        assert!(!state.d3d9().d3d9_textures.contains_key(&texture_va));
        assert_eq!(state.d3d9().d3d9_texture_bindings[0], 0);
    }

    #[test]
    fn test_d3d9_texture_unlock_with_rect() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // Seed the guest heap bump cursor (see the round-trip test).
        engine
            .mem_write(0x2000, &0x2000_u64.to_le_bytes())
            .expect("seed bump cursor");

        let pp_texture = 0x7000_u64;
        write_regs(&mut engine, 1, 2, 2, 1, 0);
        engine
            .mem_write(STACK_TOP + 0x30, &D3DFMT_A8R8G8B8.to_le_bytes())
            .expect("write format");
        engine
            .mem_write(STACK_TOP + 0x40, &pp_texture.to_le_bytes())
            .expect("write ppTexture");
        assert_return_value!(
            d3d9::handle_create_texture(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut tex_bytes = [0_u8; 8];
        engine
            .mem_read(pp_texture, &mut tex_bytes)
            .expect("read texture ptr");
        let texture_va = u64::from_le_bytes(tex_bytes);

        let pp_surface = 0x7100_u64;
        write_regs(&mut engine, texture_va, 0, pp_surface, 0, 0);
        assert_return_value!(
            d3d9::handle_texture_get_surface_level(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut surf_bytes = [0_u8; 8];
        engine
            .mem_read(pp_surface, &mut surf_bytes)
            .expect("read surface ptr");
        let surface_va = u64::from_le_bytes(surf_bytes);

        // Lock only the top-left texel: RECT {0,0,1,1} at 0x7400.
        let rect_ptr = 0x7400_u64;
        for (i, v) in [0_i32, 0, 1, 1].iter().enumerate() {
            engine
                .mem_write(
                    rect_ptr + u64::try_from(i).unwrap_or(0) * 4,
                    &v.to_le_bytes(),
                )
                .expect("write rect field");
        }
        let locked_rect = 0x7200_u64;
        write_regs(&mut engine, surface_va, locked_rect, rect_ptr, 0, 0);
        assert_return_value!(
            d3d9::handle_surface_lock_rect(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut bits_bytes = [0_u8; 8];
        engine
            .mem_read(locked_rect + 8, &mut bits_bytes)
            .expect("read pBits");
        let p_bits = u64::from_le_bytes(bits_bytes);
        // The rect's pBits points at the rect top-left (the block start).
        assert_ne!(p_bits, 0);

        // Write the top-left texel (red); leave the rest of the block zero.
        engine
            .mem_write(p_bits, &0xFFFF_0000_u32.to_le_bytes())
            .expect("write texel");

        write_regs(&mut engine, surface_va, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_surface_unlock_rect(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let record = state
            .d3d9()
            .d3d9_textures
            .get(&texture_va)
            .expect("record exists");
        assert_eq!(
            record.pixels.first().copied(),
            Some(0xFFFF_0000),
            "rect region texel copied back"
        );
        assert_eq!(
            record.pixels.get(1).copied(),
            Some(0),
            "outside the rect stays zero"
        );
        assert_eq!(record.pixels.get(2).copied(), Some(0));
        assert_eq!(record.pixels.get(3).copied(), Some(0));
    }

    #[test]
    fn test_d3d9_depth_surface_create_bind_and_clear() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // Seed the guest heap bump cursor (see the texture round-trip test).
        engine
            .mem_write(0x2000, &0x2000_u64.to_le_bytes())
            .expect("seed bump cursor");

        // CreateDepthStencilSurface(2x2, D16) → surface at 0x7000.
        let pp_surface = 0x7000_u64;
        write_regs(&mut engine, 1, 2, 2, 80, 0); // r9 = D3DFMT_D16 = 80
        engine
            .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
            .expect("write ppSurface");
        assert_return_value!(
            d3d9::handle_create_depth_stencil_surface(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut surf_bytes = [0_u8; 8];
        engine
            .mem_read(pp_surface, &mut surf_bytes)
            .expect("read surface ptr");
        let surface_va = u64::from_le_bytes(surf_bytes);
        assert_ne!(
            surface_va, 0,
            "CreateDepthStencilSurface must return an object"
        );

        // Unsupported format fails honestly.
        write_regs(&mut engine, 1, 2, 2, 99, 0); // unknown format
        engine
            .mem_write(STACK_TOP + 0x40, &pp_surface.to_le_bytes())
            .expect("write ppSurface");
        assert_return_value!(
            d3d9::handle_create_depth_stencil_surface(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0x8876_086c // D3DERR_INVALIDCALL
        );

        // Bind it and clear the depth buffer to 0.25 (near is 0.0).
        write_regs(&mut engine, 1, surface_va, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_set_depth_stencil_surface(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        // Clear(D3DCLEAR_ZBUFFER, z=0.25) — the Z arg is a f32 at [rsp+0x30].
        write_regs(&mut engine, 1, 0, 0, 2, 0); // flags = D3DCLEAR_ZBUFFER
        engine
            .mem_write(STACK_TOP + 0x30, &0.25_f32.to_bits().to_le_bytes())
            .expect("write clear z");
        assert_return_value!(
            d3d9::handle_clear(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let record = state
            .d3d9()
            .d3d9_depth_surfaces
            .get(&surface_va)
            .expect("depth record exists");
        assert_eq!(record.width, 2);
        assert_eq!(record.height, 2);
        assert_eq!(record.format, 80);
        assert_eq!(
            record.depth,
            vec![0.25; 4],
            "Clear(ZBUFFER) must fill the depth"
        );

        // GetDepthStencilSurface returns the binding.
        let out_surface = 0x7100_u64;
        write_regs(&mut engine, 1, out_surface, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_get_depth_stencil_surface(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            0
        );
        let mut out_bytes = [0_u8; 8];
        engine
            .mem_read(out_surface, &mut out_bytes)
            .expect("read out");
        assert_eq!(u64::from_le_bytes(out_bytes), surface_va);

        // Release the depth surface: record gone + binding cleared.
        write_regs(&mut engine, surface_va, 0, 0, 0, 0);
        assert_return_value!(
            d3d9::handle_surface_release(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state,
            )),
            1
        );
        assert!(!state.d3d9().d3d9_depth_surfaces.contains_key(&surface_va));
        assert_eq!(state.d3d9().d3d9_depth_stencil, 0, "release must unbind");
    }

    #[test]
    fn test_d3d9_render_state_typed_round_trip() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // SetRenderState(this, state, value) through the register ABI.
        let set = |engine: &mut IcedCpu, state: &mut WinApiState, state_id: u32, value: u32| {
            write_regs(engine, 1, u64::from(state_id), u64::from(value), 0, 0);
            assert_return_value!(
                d3d9::handle_set_render_state(&mut HandlerContext::new(
                    engine,
                    test_environment(),
                    state,
                )),
                0
            );
        };
        // GetRenderState(this, state, &out) returns the typed value.
        let get = |engine: &mut IcedCpu, state: &mut WinApiState, state_id: u32| -> u32 {
            let out = 0x7400_u64;
            write_regs(engine, 1, u64::from(state_id), out, 0, 0);
            assert_return_value!(
                d3d9::handle_get_render_state(&mut HandlerContext::new(
                    engine,
                    test_environment(),
                    state,
                )),
                0
            );
            let mut bytes = [0_u8; 4];
            engine
                .mem_read(out, &mut bytes)
                .expect("read GetRenderState output");
            u32::from_le_bytes(bytes)
        };

        // The D3D9 fixed-function defaults match the fragment-stage fallbacks.
        let rs = state.d3d9();
        assert!(!rs.d3d9_render_state.alpha_blend_enable);
        assert!(rs.d3d9_render_state.z_write_enable);
        assert_eq!(rs.d3d9_render_state.z_enable.as_u32(), 0);
        assert_eq!(rs.d3d9_render_state.z_func.as_u32(), 4);
        assert_eq!(rs.d3d9_render_state.src_blend.as_u32(), 2);
        assert_eq!(rs.d3d9_render_state.dest_blend.as_u32(), 1);
        assert_eq!(rs.d3d9_render_state.blend_op.as_u32(), 1);

        // Set + struct check for every supported D3DRS_*.
        set(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_ALPHABLENDENABLE,
            1,
        );
        set(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_ZENABLE,
            1,
        );
        set(&mut engine, &mut state, crate::d3d9_render::D3DRS_ZFUNC, 4);
        set(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_SRCBLEND,
            5,
        );
        set(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_DESTBLEND,
            6,
        );
        set(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_BLENDOP,
            1,
        );
        set(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_ZWRITEENABLE,
            0,
        );
        let rs = state.d3d9();
        assert!(rs.d3d9_render_state.alpha_blend_enable);
        assert!(!rs.d3d9_render_state.z_write_enable);
        assert_eq!(rs.d3d9_render_state.z_enable.as_u32(), 1);
        assert_eq!(rs.d3d9_render_state.z_func.as_u32(), 4);
        assert_eq!(rs.d3d9_render_state.src_blend.as_u32(), 5);
        assert_eq!(rs.d3d9_render_state.dest_blend.as_u32(), 6);
        assert_eq!(rs.d3d9_render_state.blend_op.as_u32(), 1);

        // GetRenderState round-trips each supported state.
        assert_eq!(
            get(
                &mut engine,
                &mut state,
                crate::d3d9_render::D3DRS_ALPHABLENDENABLE
            ),
            1
        );
        assert_eq!(
            get(&mut engine, &mut state, crate::d3d9_render::D3DRS_ZENABLE),
            1
        );
        assert_eq!(
            get(&mut engine, &mut state, crate::d3d9_render::D3DRS_ZFUNC),
            4
        );
        assert_eq!(
            get(&mut engine, &mut state, crate::d3d9_render::D3DRS_SRCBLEND),
            5
        );
        assert_eq!(
            get(&mut engine, &mut state, crate::d3d9_render::D3DRS_DESTBLEND),
            6
        );
        assert_eq!(
            get(&mut engine, &mut state, crate::d3d9_render::D3DRS_BLENDOP),
            1
        );
        assert_eq!(
            get(
                &mut engine,
                &mut state,
                crate::d3d9_render::D3DRS_ZWRITEENABLE
            ),
            0
        );

        // Unmodeled D3DRS_* are inert: setting them is a no-op and the read
        // falls back to 0 (D3D9's default for unused states).
        set(&mut engine, &mut state, 0x1FF, 7);
        assert_eq!(get(&mut engine, &mut state, 0x1FF), 0);
        // Unknown enum values round-trip through the raw register value.
        set(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DRS_SRCBLEND,
            0xDEAD,
        );
        assert_eq!(
            get(&mut engine, &mut state, crate::d3d9_render::D3DRS_SRCBLEND),
            0xDEAD
        );
    }

    #[test]
    fn test_d3d9_stage_state_typed_round_trip() {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        // SetTextureStageState(this, stage=0, slot, value) via the register ABI.
        let set_tss = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32, value: u32| {
            write_regs(engine, 1, 0, u64::from(slot), u64::from(value), 0);
            assert_return_value!(
                d3d9::handle_set_texture_stage_state(&mut HandlerContext::new(
                    engine,
                    test_environment(),
                    state,
                )),
                0
            );
        };
        // SetSamplerState(this, sampler=0, slot, value) via the register ABI.
        let set_samp = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32, value: u32| {
            write_regs(engine, 1, 0, u64::from(slot), u64::from(value), 0);
            assert_return_value!(
                d3d9::handle_set_sampler_state(&mut HandlerContext::new(
                    engine,
                    test_environment(),
                    state,
                )),
                0
            );
        };
        let get_tss = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32| -> u32 {
            let out = 0x7400_u64;
            write_regs(engine, 1, 0, u64::from(slot), out, 0);
            assert_return_value!(
                d3d9::handle_get_texture_stage_state(&mut HandlerContext::new(
                    engine,
                    test_environment(),
                    state,
                )),
                0
            );
            let mut bytes = [0_u8; 4];
            engine
                .mem_read(out, &mut bytes)
                .expect("read GetTextureStageState output");
            u32::from_le_bytes(bytes)
        };
        let get_samp = |engine: &mut IcedCpu, state: &mut WinApiState, slot: u32| -> u32 {
            let out = 0x7400_u64;
            write_regs(engine, 1, 0, u64::from(slot), out, 0);
            assert_return_value!(
                d3d9::handle_get_sampler_state(&mut HandlerContext::new(
                    engine,
                    test_environment(),
                    state,
                )),
                0
            );
            let mut bytes = [0_u8; 4];
            engine
                .mem_read(out, &mut bytes)
                .expect("read GetSamplerState output");
            u32::from_le_bytes(bytes)
        };

        // The stage-0 defaults match the legacy resolve fallbacks.
        let stage = state
            .d3d9()
            .d3d9_stage_states
            .first()
            .expect("stage 0 exists");
        assert_eq!(stage.color_op, crate::d3d9_render::D3DTOP_MODULATE);
        assert_eq!(stage.mag_filter, crate::d3d9_render::D3DTEXF_POINT);
        assert_eq!(stage.address_u, crate::d3d9_render::D3DTADDRESS_WRAP);

        // Set TSS slots → typed fields.
        set_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DTSS_COLOROP,
            crate::d3d9_render::D3DTOP_SELECTARG1,
        );
        set_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DTSS_COLORARG1,
            crate::d3d9_render::D3DTA_TEXTURE,
        );
        set_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DTSS_COLORARG2,
            crate::d3d9_render::D3DTA_DIFFUSE,
        );
        set_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DTSS_ALPHAOP,
            crate::d3d9_render::D3DTOP_MODULATE,
        );
        set_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DTSS_ALPHAARG1,
            crate::d3d9_render::D3DTA_TEXTURE,
        );
        set_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DTSS_ALPHAARG2,
            crate::d3d9_render::D3DTA_DIFFUSE,
        );
        set_tss(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DTSS_TEXCOORDINDEX,
            3,
        );
        let stage = state
            .d3d9()
            .d3d9_stage_states
            .first()
            .expect("stage 0 exists");
        assert_eq!(stage.color_op, crate::d3d9_render::D3DTOP_SELECTARG1);
        assert_eq!(stage.color_arg1, crate::d3d9_render::D3DTA_TEXTURE);
        assert_eq!(stage.color_arg2, crate::d3d9_render::D3DTA_DIFFUSE);
        assert_eq!(stage.alpha_op, crate::d3d9_render::D3DTOP_MODULATE);
        assert_eq!(stage.alpha_arg1, crate::d3d9_render::D3DTA_TEXTURE);
        assert_eq!(stage.alpha_arg2, crate::d3d9_render::D3DTA_DIFFUSE);
        assert_eq!(stage.tex_coord_index, 3);

        // Set sampler slots → typed fields (D3DSAMP_* namespace).
        set_samp(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DSAMP_MAGFILTER,
            crate::d3d9_render::D3DTEXF_LINEAR,
        );
        set_samp(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DSAMP_ADDRESSU,
            crate::d3d9_render::D3DTADDRESS_CLAMP,
        );
        set_samp(
            &mut engine,
            &mut state,
            crate::d3d9_render::D3DSAMP_ADDRESSV,
            crate::d3d9_render::D3DTADDRESS_CLAMP,
        );
        let stage = state
            .d3d9()
            .d3d9_stage_states
            .first()
            .expect("stage 0 exists");
        assert_eq!(stage.mag_filter, crate::d3d9_render::D3DTEXF_LINEAR);
        assert_eq!(stage.address_u, crate::d3d9_render::D3DTADDRESS_CLAMP);
        assert_eq!(stage.address_v, crate::d3d9_render::D3DTADDRESS_CLAMP);

        // Get* round-trips the typed slots.
        assert_eq!(
            get_tss(&mut engine, &mut state, crate::d3d9_render::D3DTSS_COLOROP),
            crate::d3d9_render::D3DTOP_SELECTARG1
        );
        assert_eq!(
            get_tss(
                &mut engine,
                &mut state,
                crate::d3d9_render::D3DTSS_COLORARG2
            ),
            crate::d3d9_render::D3DTA_DIFFUSE
        );
        assert_eq!(
            get_samp(
                &mut engine,
                &mut state,
                crate::d3d9_render::D3DSAMP_MAGFILTER
            ),
            crate::d3d9_render::D3DTEXF_LINEAR
        );
        assert_eq!(
            get_samp(
                &mut engine,
                &mut state,
                crate::d3d9_render::D3DSAMP_ADDRESSU
            ),
            crate::d3d9_render::D3DTADDRESS_CLAMP
        );

        // Unmodeled slots are preserved verbatim for the Get* round-trips.
        set_tss(&mut engine, &mut state, 99, 0xAB);
        set_samp(&mut engine, &mut state, 88, 0xCD);
        assert_eq!(get_tss(&mut engine, &mut state, 99), 0xAB);
        assert_eq!(get_samp(&mut engine, &mut state, 88), 0xCD);
        // The two namespaces stay separate despite the colliding constants:
        // TSS slot 1 is COLOROP (set to SELECTARG1 above), sampler slot 1 is
        // ADDRESSU (set to CLAMP above).
        assert_eq!(
            get_tss(
                &mut engine,
                &mut state,
                crate::d3d9_render::D3DSAMP_ADDRESSU
            ),
            crate::d3d9_render::D3DTOP_SELECTARG1
        );
        assert_eq!(
            get_samp(&mut engine, &mut state, crate::d3d9_render::D3DTSS_COLOROP),
            crate::d3d9_render::D3DTADDRESS_CLAMP
        );
    }
}
