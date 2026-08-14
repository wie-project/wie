//! WinAPI state-type definitions, extracted from the crate root.
//!
//! These types historically lived directly in `wie-winapi/src/lib.rs`.
//! Per-domain submodules below hold the definitions; `lib.rs` re-exports
//! every name at the crate root so the public surface is unchanged.

use std::any::Any;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use wie_cpu::CpuEngine;

use crate::console;
use crate::gdi32;
use crate::present;
use crate::pthread;
use crate::seh;
use crate::sync_obj::SyncState;
use crate::thread::ThreadState;
use crate::user32::dragdrop::DragDropState;

mod d3d9;
mod input;
mod process;
mod window;

#[cfg(test)]
mod tests;

pub use d3d9::*;
pub use input::*;
pub use process::*;
pub use window::*;

/// Shared fake-handle newtype template (ADR-003): a zero-cost `u64` wrapper
/// with the conversion accessors used by the handle allocators. Handlers keep
/// raw `u64` registers; the typed store converts at the boundary.
macro_rules! handle_newtype {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[repr(transparent)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name(u64);

        impl $name {
            /// The `NULL` handle (`0`).
            pub const NULL: Self = Self(0);

            /// Escape point: the raw handle value (return values, …).
            #[must_use]
            pub const fn as_u64(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}
pub(crate) use handle_newtype;

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
    pub seh_pending: ahash::HashMap<u32, seh::SehPending>,
}

/// Identifies a slot in [`DllStateMap`]. One variant per emulated DLL
/// that carries host-side state.
///
/// # Adding a new DLL
///
/// 1. Add a variant here.
/// 2. Add a match arm to [`DllStateMap::slot_of`].
/// 3. Add an accessor on [`WinApiState`].
///
/// That is the entire change. The slot array length derives from
/// [`DllId::COUNT`] (see the struct definition and `new()`), so a variant
/// without a slot is a compile error; [`DllId::COUNT`] itself derives from
/// the number of variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug, strum::EnumCount)]
pub enum DllId {
    Console,
    Window,
    D3D9,
    Pthread,
    Gdi,
    Present,
    /// The process clipboard (CF_TEXT only; Task 2.6).
    Clipboard,
    /// The host-side registry hive (HKCU value storage; Task 5.1).
    Registry,
    /// The shell drag-drop list behind `WM_DROPFILES` (Task 5.2).
    DragDrop,
    /// Winsock socket table (`WS2_32.dll` — guest `SOCKET` → host socket).
    Ws2,
    /// Crypto provider/hash handles (`CRYPT32.dll`).
    Crypt32,
    /// COM class registration table (`OLE32.dll` — CLSID → class factory).
    Ole32,
    /// Timer/audio handle tables (`WINMM.dll` — timeSetEvent, waveOut).
    Winmm,
    /// Internet/HTTP handle tables (`WININET.dll` — HINTERNET sessions).
    Wininet,
    /// WinHTTP handle table (`WINHTTP.dll` — SDL2 online-probe handles).
    Winhttp,
}

impl DllId {
    /// Number of variants (strum-derived) — the slot-array length, so a new
    /// variant automatically grows [`DllStateMap`] and a mismatch is a
    /// compile error.
    pub const COUNT: usize = <Self as strum::EnumCount>::COUNT;
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
/// Add a variant to [`DllId`] and a match arm to `slot_of`. The slot array
/// length is `DllId::COUNT` in both the struct definition and `new()`, so a
/// missing slot is a compile error rather than a silent empty slot.
pub struct DllStateMap {
    slots: [Option<Box<dyn Any + Send>>; DllId::COUNT],
}

// The struct field `slots: [Option<Box<dyn Any + Send>>; DllId::COUNT]`
// and the `new()` initializer share `DllId::COUNT`, so the enum and the
// array can never drift.

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
        DllId::Clipboard => 6,
        DllId::Registry => 7,
        DllId::DragDrop => 8,
        DllId::Ws2 => 9,
        DllId::Crypt32 => 10,
        DllId::Ole32 => 11,
        DllId::Winmm => 12,
        DllId::Wininet => 13,
        DllId::Winhttp => 14,
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
        6 => "clipboard",
        7 => "registry",
        8 => "dragdrop",
        9 => "ws2",
        10 => "crypt32",
        11 => "ole32",
        12 => "winmm",
        13 => "wininet",
        _ => "?",
    }
}

impl DllStateMap {
    /// All slots start unloaded. `DllId::COUNT` drives the array length, so a
    /// variant added without a matching slot here is a compile error.
    pub fn new() -> Self {
        Self {
            slots: [const { None }; DllId::COUNT],
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

/// The process clipboard (CF_TEXT only for this milestone; Task 2.6).
///
/// The clipboard is process-global but MUTABLE (`WM_COPY`/`WM_CUT` write it,
/// `WM_CLEAR` empties it), so it lives in a shared state slot — the
/// [`DllStateMap`] — behind [`WinApiState::clipboard`], not an immutable
/// process-wide `OnceLock`. macOS NSPasteboard integration is explicitly
/// YAGNI for the notepad milestone: the host `String` satisfies the
/// guest-visible contract (WM_COPY → WM_PASTE / `IsClipboardFormatAvailable`
/// within the process) and cross-app paste can follow.
#[derive(Debug, Clone, Default)]
pub struct ClipboardState {
    text: Option<String>,
}

impl ClipboardState {
    /// The clipboard text, when the clipboard holds any (CF_TEXT-available).
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Whether the clipboard holds text (`IsClipboardFormatAvailable`).
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.text.is_some()
    }

    /// Store text on the clipboard (`WM_CUT` / `WM_COPY`).
    pub fn set_text(&mut self, text: String) {
        self.text = Some(text);
    }

    /// Empty the clipboard (`WM_CLEAR` empties it like Windows' edit control).
    pub fn clear(&mut self) {
        self.text = None;
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
    ///
    /// External crates construct this once (empty map) and then reach slots
    /// only through the typed accessors below (`console()`, `window_state()`,
    /// `d3d9()`, …).
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
    /// Mutable access to window state (windows, focus/capture, menus).
    pub fn window_state(&mut self) -> &mut WindowState {
        self.dll_states.get_or_init::<WindowState>(DllId::Window)
    }

    /// Lock the guest message queue, recovering from a poisoned mutex.
    pub fn lock_message_queue(&self) -> std::sync::MutexGuard<'_, present::MessageQueue> {
        self.message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Mutable access to D3D9 state (devices, textures, surfaces).
    pub fn d3d9(&mut self) -> &mut D3D9State {
        self.dll_states.get_or_init::<D3D9State>(DllId::D3D9)
    }
    /// Mutable access to pthread state (condvars, wait queues).
    pub fn pthread(&mut self) -> &mut pthread::PthreadState {
        self.dll_states
            .get_or_init::<pthread::PthreadState>(DllId::Pthread)
    }

    /// Mutable access to GDI state (DC records, DIB sections).
    pub fn gdi_state(&mut self) -> &mut gdi32::GdiState {
        self.dll_states.get_or_init::<gdi32::GdiState>(DllId::Gdi)
    }

    /// Run `f` with the font engine taken out of GDI state, putting it back
    /// unconditionally when `f` returns — including on early `return`s inside
    /// `f`, which would silently drop the engine from `gdi_state` under the
    /// manual take/put idiom.
    ///
    /// The engine is taken out so the paint/measure/hit-test bodies can pass
    /// `&mut state` and `&mut FontEngine` side by side (a plain field cannot
    /// be split-borrowed alongside `state`); the helper makes the put-back
    /// structural. Safe under the single shared `WinApiState` mutex: every
    /// API handler — the caller and any concurrent one on another host
    /// thread — runs while holding it, so the take and the put cannot
    /// interleave.
    pub fn with_font_engine<T>(
        &mut self,
        f: impl FnOnce(&mut Self, &mut gdi32::FontEngine) -> T,
    ) -> T {
        let mut font_engine = std::mem::take(&mut self.gdi_state().font_engine);
        let result = f(self, &mut font_engine);
        self.gdi_state().font_engine = font_engine;
        result
    }

    /// Mutable access to present state (compositing surfaces, frame publishing).
    pub fn present(&mut self) -> &mut present::PresentState {
        self.dll_states
            .get_or_init::<present::PresentState>(DllId::Present)
    }

    /// Mutable access to the process clipboard (CF_TEXT only).
    pub fn clipboard(&mut self) -> &mut ClipboardState {
        self.dll_states
            .get_or_init::<ClipboardState>(DllId::Clipboard)
    }

    /// Mutable access to the registry hive state (values + persistence).
    pub fn registry(&mut self) -> &mut crate::registry::RegistryState {
        self.dll_states
            .get_or_init::<crate::registry::RegistryState>(DllId::Registry)
    }

    /// Mutable access to the shell drag-drop list (`WM_DROPFILES`).
    ///
    /// The host winit thread stores dropped paths here; the guest's
    /// `DragQueryFileA/W` / `DragQueryPoint` / `DragFinish` handlers read and
    /// clear it.
    pub fn drag_drop(&mut self) -> &mut DragDropState {
        self.dll_states
            .get_or_init::<DragDropState>(DllId::DragDrop)
    }

    /// Mutable access to the Winsock socket table (guest `SOCKET` → host
    /// socket). Lazy: allocated on first `WS2_32` call.
    pub fn ws2(&mut self) -> &mut crate::ws2_32::Ws2State {
        self.dll_states
            .get_or_init::<crate::ws2_32::Ws2State>(DllId::Ws2)
    }

    /// Mutable access to the crypto handle tables. Lazy: allocated on first
    /// `CRYPT32` call.
    pub fn crypt32(&mut self) -> &mut crate::crypt32::Crypt32State {
        self.dll_states
            .get_or_init::<crate::crypt32::Crypt32State>(DllId::Crypt32)
    }

    /// Mutable access to the COM class-registration table. Lazy: allocated
    /// on first `OLE32` call.
    pub fn ole32(&mut self) -> &mut crate::ole32::OleState {
        self.dll_states
            .get_or_init::<crate::ole32::OleState>(DllId::Ole32)
    }

    /// Mutable access to the WINMM timer/audio handle tables. Lazy:
    /// allocated on first `WINMM` call.
    pub fn winmm(&mut self) -> &mut crate::winmm::WinmmState {
        self.dll_states
            .get_or_init::<crate::winmm::WinmmState>(DllId::Winmm)
    }

    /// Mutable access to the Internet/HTTP handle tables. Lazy: allocated on
    /// first `WININET` call.
    pub fn wininet(&mut self) -> &mut crate::wininet::WininetState {
        self.dll_states
            .get_or_init::<crate::wininet::WininetState>(DllId::Wininet)
    }

    /// Mutable access to the WinHTTP handle table. Lazy: allocated on first
    /// `WINHTTP` call.
    pub fn winhttp(&mut self) -> &mut crate::winhttp::WinhttpState {
        self.dll_states
            .get_or_init::<crate::winhttp::WinhttpState>(DllId::Winhttp)
    }

    /// Read-only access — returns `None` if the state was never initialised.
    /// Use when the caller only holds `&Self`.
    pub fn try_console(&self) -> Option<&console::ConsoleState> {
        self.dll_states.get::<console::ConsoleState>(DllId::Console)
    }
    /// Read-only access — returns `None` if the state was never initialised.
    pub fn try_window_state(&self) -> Option<&WindowState> {
        self.dll_states.get::<WindowState>(DllId::Window)
    }
    /// Read-only access — returns `None` if the state was never initialised.
    pub fn try_d3d9(&self) -> Option<&D3D9State> {
        self.dll_states.get::<D3D9State>(DllId::D3D9)
    }
    /// Read-only access — returns `None` if the state was never initialised.
    pub fn try_pthread(&self) -> Option<&pthread::PthreadState> {
        self.dll_states.get::<pthread::PthreadState>(DllId::Pthread)
    }
    /// Read-only access — returns `None` if the state was never initialised.
    pub fn try_gdi_state(&self) -> Option<&gdi32::GdiState> {
        self.dll_states.get::<gdi32::GdiState>(DllId::Gdi)
    }
    /// Read-only access — returns `None` if the state was never initialised.
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

    /// Canonical handler tail: pop the guest return address, write `value`
    /// into RAX, and package the result.
    ///
    /// Replaces the two-step `engine.return_from_win64_api(v)?;` +
    /// `Ok(WinApiHandlerResult { return_address, return_value })` idiom.
    /// Errors need no per-API context here — the dispatcher already wraps
    /// handler failures as `{lib}!{name}: {error}`.
    pub fn finish(&mut self, value: u64) -> Result<crate::WinApiHandlerResult> {
        let return_address = self.engine.return_from_win64_api(value)?;
        Ok(crate::WinApiHandlerResult {
            return_address,
            return_value: value,
        })
    }
}
