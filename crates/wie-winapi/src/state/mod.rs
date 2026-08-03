//! WinAPI state-type definitions, extracted from the crate root.
//!
//! These types historically lived directly in `wie-winapi/src/lib.rs`.
//! Per-domain submodules below hold the definitions; `lib.rs` re-exports
//! every name at the crate root so the public surface is unchanged.

use std::any::Any;
use std::sync::{Arc, Mutex};

use wie_cpu::CpuEngine;

use crate::console;
use crate::gdi32;
use crate::present;
use crate::pthread;
use crate::seh;
use crate::sync_obj::SyncState;
use crate::thread::ThreadState;

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
    /// The process clipboard (CF_TEXT only; Task 2.6).
    Clipboard,
}

impl DllId {
    /// Number of variants. The assertion at [`DLL_ID_SLOT_COUNT`] ensures
    /// it stays in sync with the slot array in [`DllStateMap::new`].
    pub const COUNT: usize = 7;
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
        DllId::Clipboard => 6,
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
        _ => "?",
    }
}

impl DllStateMap {
    /// All slots start unloaded. Add a `None` per new [`DllId`] variant.
    pub fn new() -> Self {
        Self {
            slots: [None, None, None, None, None, None, None],
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
}
