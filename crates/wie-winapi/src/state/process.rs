//! Process, heap, file-I/O, module, and environment state types.

use ahash::HashMap;
use std::sync::Arc;

use crate::dll_loader;
use crate::guest_heap::GuestHeap;
use crate::vfs::{self, VolumeConfig};

use super::handle_newtype;
use super::window::{FindHandle, ResourceRecord};

// ── Fake-handle newtypes (ADR-003) ─────────────────────────────────────
//
// Zero-cost wrappers over the guest-visible `u64` handle values. They exist so
// the *host-side* allocator counters and lookups cannot mix handle namespaces —
// passing a `ModuleHandle` where a `FileHandle` belongs is a compile error.
// Conversions happen exactly where a `u64` meets the typed store; the guest
// never sees these types (handlers keep raw `u64` registers).

handle_newtype! {
    /// A fake find-file handle (`FindFirstFile` / `FindNextFile`).
    FindFileHandle
}

handle_newtype! {
    /// A fake open-file handle (`CreateFile` / `OpenFile`).
    FileHandle
}

handle_newtype! {
    /// A fake resource handle (`FindResource` / `LoadResource`).
    ResourceHandle
}

handle_newtype! {
    /// A fake module handle (`LoadLibrary` / `GetModuleHandle`).
    ModuleHandle
}

handle_newtype! {
    /// A fake registry-key handle (`RegOpenKey` / `RegCreateKey`).
    RegistryKeyHandle
}

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
    /// Raw main-module file bytes, shared (Arc) — readers only slice them.
    pub executable_file_bytes: Arc<Vec<u8>>,
    pub executable_file_cursor: u64,
    pub next_find_handle: FindFileHandle,
    pub find_handles: Vec<FindHandle>,
    pub host_file_mounts: Vec<HostFileMount>,
    pub virtual_files: Vec<VirtualGuestFile>,
    pub open_files: HashMap<u64, OpenGuestFile>,
    pub next_file_handle: FileHandle,
    pub next_resource_handle: ResourceHandle,
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
    pub get_proc_address_cache: ahash::HashMap<String, GetProcAddressCacheEntry>,
    pub next_module_handle: ModuleHandle,
}

#[derive(Debug, Clone)]
pub struct ProcessState {
    pub last_error: u32,
    pub next_registry_key_handle: RegistryKeyHandle,
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
