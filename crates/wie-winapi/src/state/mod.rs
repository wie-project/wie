//! WinAPI state-type definitions, extracted from the crate root.
//!
//! These types historically lived directly in `wie-winapi/src/lib.rs`.
//! Per-domain submodules below hold the definitions; `lib.rs` re-exports
//! every name at the crate root so the public surface is unchanged.

use ahash::HashMapExt as _;
use std::any::Any;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use strum::IntoEnumIterator;
use wie_cpu::CpuEngine;
use wie_cpu::guest_layout::TEB_LAST_ERROR_OFFSET;

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

// `pub(crate)` so the shared test fixtures (`winapi_state_default` and its
// heap-seeded variant) are reachable from the `#[cfg(test)]` blocks in
// non-test modules (version.rs, ole_clipboard.rs, kernel32/file_io/path.rs,
// advapi32/tests.rs). Test-only, so it does not ship.
#[cfg(test)]
pub(crate) mod tests;

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
/// 2. Add a match arm to [`dll_slot`].
/// 3. Add an accessor on [`WinApiState`].
///
/// That is the entire change. The slot array length derives from
/// [`DllId::COUNT`] (see the struct definition and `new()`), so a variant
/// without a slot is a compile error; [`DllId::COUNT`] itself derives from
/// the number of variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug, strum::EnumCount, strum::EnumIter)]
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
    /// DirectInput8 COM objects + per-device reports (`dinput8.dll`).
    DInput8,
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
/// Add a variant to [`DllId`] and a match arm to [`dll_slot`]. The slot array
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

/// Map a [`DllId`] to its slot: the index into [`DllStateMap::slots`] and the
/// debug label, both from one match arm so they can never disagree.
///
/// This is the single authoritative `DllId` ↔ slot mapping — it replaces the
/// old `dll_index` + `slot_of` pair, which drifted (slot 14, `Winhttp`, had no
/// label and fell into `slot_of`'s `_ => "?"` arm). The match is exhaustive,
/// so a new variant without an arm is a compile error; the slot-array length
/// is [`DllId::COUNT`] in both the struct definition and `new()`, so the enum
/// and the array cannot drift either.
const fn dll_slot(id: DllId) -> (usize, &'static str) {
    match id {
        DllId::Console => (0, "console"),
        DllId::Window => (1, "window"),
        DllId::D3D9 => (2, "d3d9"),
        DllId::Pthread => (3, "pthread"),
        DllId::Gdi => (4, "gdi"),
        DllId::Present => (5, "present"),
        DllId::Clipboard => (6, "clipboard"),
        DllId::Registry => (7, "registry"),
        DllId::DragDrop => (8, "dragdrop"),
        DllId::Ws2 => (9, "ws2"),
        DllId::Crypt32 => (10, "crypt32"),
        DllId::Ole32 => (11, "ole32"),
        DllId::Winmm => (12, "winmm"),
        DllId::Wininet => (13, "wininet"),
        DllId::Winhttp => (14, "winhttp"),
        DllId::DInput8 => (15, "dinput8"),
    }
}

impl std::fmt::Debug for DllStateMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Iterate the enum (declaration order equals slot order) so every
        // loaded slot is labelled by its own `dll_slot` arm — one source of
        // truth for both index and name.
        let loaded: Vec<&str> = DllId::iter()
            .filter_map(|id| {
                let (index, label) = dll_slot(id);
                self.slots
                    .get(index)
                    .and_then(Option::as_ref)
                    .map(|_| label)
            })
            .collect();
        f.debug_struct("DllStateMap")
            .field("loaded", &loaded)
            .finish()
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

    /// Fallible core of [`DllStateMap::get_or_init`]: heap-allocates a
    /// default on first call, like `get_or_init`, but reports the two
    /// programming-error cases — a slot index outside the array, or a
    /// [`DllId`] reused for a different state type — as `Err` instead of
    /// terminating mid-call. Valid callers never see them: `dll_slot` is
    /// exhaustive over in-range indices and each slot holds exactly one type,
    /// invariants pinned by the `dll_slot_round_trip_covers_every_variant`
    /// test.
    fn try_get_or_init<T: Default + Send + 'static>(&mut self, id: DllId) -> Result<&mut T> {
        let (index, label) = dll_slot(id);
        let slot = self.slots.get_mut(index).ok_or_else(|| {
            anyhow!(
                "{id:?} ({label}) maps to slot index {index}, outside the {count}-slot array",
                count = DllId::COUNT
            )
        })?;
        let boxed = slot.get_or_insert_with(|| Box::new(T::default()));
        boxed
            .as_mut()
            .downcast_mut::<T>()
            .ok_or_else(|| anyhow!("{id:?} ({label}) slot {index} holds a different state type"))
    }

    /// Access the state for `id`, heap-allocating a default on first call.
    ///
    /// # Aborts
    /// On the two programming errors [`DllStateMap::try_get_or_init`]
    /// reports — a [`DllId`] variant mapped outside the slot array, or reused
    /// for a different state type. Both are impossible for a valid build; the
    /// infallible signature is what the [`WinApiState`] accessors need.
    pub fn get_or_init<T: Default + Send + 'static>(&mut self, id: DllId) -> &mut T {
        match self.try_get_or_init(id) {
            Ok(state) => state,
            Err(_) => std::process::abort(),
        }
    }

    /// Read-only access — returns `None` if the slot was never initialised.
    pub fn get<T: 'static>(&self, id: DllId) -> Option<&T> {
        let boxed = self.slots.get(dll_slot(id).0)?.as_ref()?;
        boxed.as_ref().downcast_ref::<T>()
    }

    /// Mutable read access — `None` if the state was never initialized.
    pub fn get_mut<T: 'static>(&mut self, id: DllId) -> Option<&mut T> {
        let boxed = self.slots.get_mut(dll_slot(id).0)?.as_mut()?;
        boxed.as_mut().downcast_mut::<T>()
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

/// Guest-visible primary-display dimensions.
///
/// The host window is created by winit in LOGICAL points on macOS (Retina),
/// so the honest guest contract is the monitor's point size: a frame rendered
/// at these dimensions is GPU-upscaled by the presenter across the full
/// physical window surface, exactly like a native HiDPI game rendering at
/// backing-store resolution would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMetrics {
    /// Primary-monitor width in logical points.
    pub width: i32,
    /// Primary-monitor height in logical points.
    pub height: i32,
}

impl DisplayMetrics {
    /// The historical fake desktop (`write_monitor_info` reported this before
    /// real host metrics were plumbed). Headless/persistent runs keep it so
    /// the micro-suite stays deterministic.
    pub const DEFAULT: Self = Self::new(1920, 1080);

    pub const fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }

    /// Screen width as the unsigned value the SM_*/caps tables return
    /// (`u64` because every metric arm returns `u64`; negative dimensions are
    /// impossible from winit, so the fallback of 0 matches Windows' behavior
    /// for degenerate metrics).
    pub fn width_metric(self) -> u64 {
        u64::try_from(self.width).unwrap_or(0)
    }

    /// Screen height as the unsigned value the SM_*/caps tables return.
    pub fn height_metric(self) -> u64 {
        u64::try_from(self.height).unwrap_or(0)
    }

    /// HORZSIZE: screen width in millimeters at the 96-dpi logical baseline
    /// (`px * 25.4 / 96`, rounded half-up so the default 1920 keeps 508).
    pub fn width_mm(self) -> u64 {
        let product = i64::from(self.width) * 254;
        u64::try_from((product + 480) / 960).unwrap_or(0)
    }

    /// VERTSIZE: screen height in millimeters at the 96-dpi logical baseline
    /// (rounded half-up so the default 1080 keeps 286).
    pub fn height_mm(self) -> u64 {
        let product = i64::from(self.height) * 254;
        u64::try_from((product + 480) / 960).unwrap_or(0)
    }
}

impl Default for DisplayMetrics {
    fn default() -> Self {
        Self::DEFAULT
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
    /// Guest-visible primary display dimensions (logical points on Retina).
    ///
    /// Set once at session init from the winit primary monitor; every fake
    /// dimension source (monitor info, system metrics, device caps, DEVMODE,
    /// clip cursor) reads it so the whole surface reports one geometry.
    pub display: DisplayMetrics,
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
            .field("display", &self.display)
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
    /// Absorb the ACTIVE engine's GS-relative TEB last-error slot into the
    /// ACTIVE thread's per-thread slot and refresh the `process.last_error`
    /// alias.
    ///
    /// Called before host API dispatch, under the WinAPI lock with `active`
    /// set to the thread that owns the engine: the in-guest `SetLastError`
    /// stub writes the engine's TEB slot directly (a pure guest memory store,
    /// no host stop), so the slot may hold a newer value than this thread last
    /// published. Host handlers that READ `process.last_error` as input
    /// (`WSAGetLastError`, `GetLastError` when not stubbed, …) must observe
    /// that guest-side value. The engine's GS base binds the read to the
    /// ACTIVE thread's own TEB page — the primary engine keeps the fixed
    /// `GS_BASE`, each worker engine is bound to its own `PerThreadTeb` page
    /// at spawn — so a peer thread's stub write can never race this read.
    pub fn absorb_guest_last_error(&mut self, engine: &mut dyn CpuEngine) {
        let teb_va = engine.gs_base();
        let mut teb_err = [0_u8; 4];
        if engine
            .mem_read(teb_va + TEB_LAST_ERROR_OFFSET, &mut teb_err)
            .is_err()
        {
            // Best-effort: the TEB page is always mapped for the real layout.
            return;
        }
        let value = u32::from_le_bytes(teb_err);
        self.kernel.threads.active.last_error = value;
        self.process.last_error = value;
    }

    /// Publish the ACTIVE thread's last-error into the ACTIVE engine's
    /// GS-relative TEB slot.
    ///
    /// Folds any `process.last_error` handler writes into the active thread's
    /// slot first, then writes the slot value into the TEB page the engine is
    /// bound to (`engine.gs_base() + TEB_LAST_ERROR_OFFSET`). Called after
    /// host API dispatch; at thread-activation boundaries callers first
    /// refresh the alias from the active slot (`process.last_error =
    /// threads.active.last_error`) because the alias still holds the PREVIOUS
    /// thread's value there and must not overwrite the newly active thread's
    /// slot. Every write lands in the ACTIVE thread's own TEB page: the
    /// primary engine writes the fixed `GS_BASE` page, each worker engine its
    /// own `PerThreadTeb` page — never one shared mirror.
    pub fn publish_last_error_to_guest(&mut self, engine: &mut dyn CpuEngine) {
        let threads = &mut self.kernel.threads;
        threads.active.last_error = self.process.last_error;
        let value = threads.active.last_error;
        let teb_va = engine.gs_base();
        let bytes = value.to_le_bytes();
        // Best-effort: the TEB page is always mapped for the real layout.
        drop(engine.mem_write(teb_va + TEB_LAST_ERROR_OFFSET, &bytes));
    }

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

    /// Mutable read-only-fallback access — `None` if never initialized (the
    /// mirror sync's write-back path).
    pub fn try_window_state_mut(&mut self) -> Option<&mut WindowState> {
        self.dll_states.get_mut::<WindowState>(DllId::Window)
    }

    /// Lock the guest message queue, recovering from a poisoned mutex.
    ///
    /// Records the acquisition for the debug-only lock-order assertion
    /// ([`crate::lock_order`]). The guest-side route is always taken with the
    /// big `WinApiState` already held (`&self`), which is what makes
    /// `State → Queue` the order rather than a convention; the host-side post
    /// takes the bare `Arc` instead (`RuntimeSession::post_message`) and so
    /// records only `Queue`, which is legal.
    pub fn lock_message_queue(&self) -> crate::lock_order::Ordered<'_, present::MessageQueue> {
        let scope = crate::lock_order::enter(crate::lock_order::Lock::Queue);
        let guard = self
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::lock_order::Ordered::wrap(scope, guard)
    }

    /// Guest thread that owns `window_handle` — the queue its posted messages
    /// land in. `None` when the window is unknown or was created without a
    /// recorded owner (`WindowRecord::owner_tid == 0`), in which case posting
    /// falls back to the calling thread's own queue.
    #[must_use]
    pub fn window_owner_tid(&self, window_handle: crate::handles::Hwnd) -> Option<u32> {
        self.try_window_state()
            .and_then(|ws| {
                ws.windows
                    .iter()
                    .find(|window| window.handle == window_handle)
            })
            .map(|window| window.owner_tid)
            .filter(|owner_tid| *owner_tid != 0)
    }

    /// Queue one message for the thread that owns `window_handle`, falling
    /// back to the calling thread's own queue.
    ///
    /// The single guest-side posting entry point: the owner tid comes from the
    /// window records (authoritative) and NOT from the queue's routing mirror,
    /// which exists only for host posts that cannot take this lock.
    pub fn post_message(
        &mut self,
        window_handle: crate::handles::Hwnd,
        message: u32,
        word_parameter: u64,
        long_parameter: u64,
    ) -> anyhow::Result<()> {
        let current_tid = self.kernel.threads.current_tid();
        let tid = self.window_owner_tid(window_handle).unwrap_or(current_tid);
        self.lock_message_queue().push_to(
            tid,
            window_handle,
            message,
            word_parameter,
            long_parameter,
        )
    }

    /// Point the message queue's wake hub at [`SyncState::wake_hub`].
    ///
    /// Called once at session init: after this, EVERY handler-side
    /// `queue.push` (PostMessage, PostQuitMessage, synthesized WM_TIMER /
    /// WM_PAINT, …) delivers a wake token to any parked guest thread without
    /// touching per-push call sites. Idempotent.
    pub fn wire_wake_hub(&mut self) {
        let hub = self.kernel.sync.wake_hub.clone();
        self.lock_message_queue().wake = hub;
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
    /// Borrow-split access to the font engine: hand out `&mut WinApiState` and
    /// `&mut FontEngine` at once, which the borrow checker cannot express
    /// directly, by taking the engine out and putting it back.
    ///
    /// Not unwind-safe, and deliberately so: the take/put has no RAII guard, so a
    /// panicking `f` would drop the engine's four caches and leave defaults
    /// behind. That cannot happen in a shipped binary — `[profile.release]` sets
    /// `panic = "abort"`, so a panic aborts the process rather than unwinding,
    /// and nothing in production wraps a handler in `catch_unwind`. The caches
    /// die with the process.
    ///
    /// Do not "fix" this with a drop guard without first removing
    /// `panic = "abort"`: that would trade a real property (a handler panic is
    /// never survivable) for one that is not needed.
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

    /// Pop due WINMM timers for `now_tick`.
    ///
    /// Delegates to [`crate::winmm::WinmmState::pop_due_timers`]. One-shot
    /// timers are removed, periodic timers are re-armed (`due += delay`).
    /// See `winmm::WinmmTimerRecord` for reentrancy notes.
    pub fn pop_due_timers(&mut self, now_tick: u32) -> Vec<crate::winmm::DueTimer> {
        self.winmm().pop_due_timers(now_tick)
    }

    /// Read-only WINMM state (timer wheel + period), `None` before the DLL
    /// state is initialized. The pump's park loops use this to bound their
    /// waits by the next due guest timer without taking `&mut`.
    #[must_use]
    pub fn winmm_ref(&self) -> Option<&crate::winmm::WinmmState> {
        self.dll_states
            .get::<crate::winmm::WinmmState>(crate::DllId::Winmm)
    }

    /// Pop the next due WINMM timer for `now_tick`, if any.
    ///
    /// The pump uses this to dispatch at most one timer per boundary while a
    /// guest callback is in flight.
    pub fn pop_next_due_timer(&mut self, now_tick: u32) -> Option<crate::winmm::DueTimer> {
        self.winmm().pop_next_due_timer(now_tick)
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

    /// Mutable access to the DirectInput8 COM objects, per-device data
    /// formats, and in-flight enumerations. Lazy: allocated on the first
    /// `dinput8.dll` call.
    pub fn dinput8(&mut self) -> &mut crate::dinput::DInputState {
        self.dll_states
            .get_or_init::<crate::dinput::DInputState>(DllId::DInput8)
    }

    /// Read-only DirectInput8 state — `None` before the DLL is initialised.
    #[must_use]
    pub fn try_dinput8(&self) -> Option<&crate::dinput::DInputState> {
        self.dll_states
            .get::<crate::dinput::DInputState>(DllId::DInput8)
    }

    /// Rebuild the presenter-side window mirror from the current window
    /// state — called from [`Self::sync_window_mirror_if_dirty`] (the
    /// `HandlerContext::finish` seam) and by host-side mutators that bypass
    /// the handler path (the GUI `resize_window`). See
    /// `present::window_mirror`.
    ///
    /// Mirrors only what the host reads: geometry/visibility/title/menu/
    /// tracking per record, focus, capture, and the menu-dirty gate. The
    /// projection rebuild runs only after a mutation site bumped the rev.
    pub fn sync_window_mirror_if_dirty(&mut self) {
        let dirty = self
            .try_window_state()
            .is_some_and(|ws| ws.window_mirror_rev != ws.window_mirror_synced_rev);
        if !dirty {
            return;
        }
        self.sync_window_mirror();
    }

    /// Unconditionally rebuild the presenter-side window mirror from the
    /// current window state (see [`Self::sync_window_mirror_if_dirty`]).
    pub fn sync_window_mirror(&mut self) {
        let Some(ws) = self.try_window_state() else {
            return;
        };
        let windows: Vec<present::MirrorWindow> = ws
            .windows
            .iter()
            .map(|w| present::MirrorWindow {
                handle: w.handle,
                parent: w.parent_handle,
                x: w.x,
                y: w.y,
                width: w.width,
                height: w.height,
                visible: w.visible,
                title: w.title.clone(),
                menu_handle: w.menu_handle,
                mouse_tracking: w.mouse_tracking,
            })
            .collect();
        let (focus, capture, menu_dirty) = (
            ws.focus_window_handle,
            ws.capture_window_handle,
            ws.menu_dirty,
        );
        let synced_rev = ws.window_mirror_rev;
        self.present()
            .channel
            .sync_window_mirror(windows, focus, capture, menu_dirty);
        if let Some(ws) = self.try_window_state_mut() {
            ws.window_mirror_synced_rev = synced_rev;
        }
    }

    /// Drain the host's pending keyboard writes into the guest keyboard-state
    /// array — called by the guest keyboard-state readers (under the big
    /// lock they already hold) so host `set_key_state` writes reach the
    /// guest without ever taking that lock host-side.
    pub fn drain_key_writes(&mut self) {
        let writes = self.present().channel.drain_key_writes();
        if writes.is_empty() {
            return;
        }
        let ws = self.window_state();
        for (vk, pressed) in writes {
            if let Some(key) = ws.keyboard_state.get_mut(usize::from(vk)) {
                if pressed {
                    *key |= 0x80;
                } else {
                    *key &= !0x80;
                }
            }
        }
    }

    /// Read the latest host-pushed cursor position in guest-logical screen
    /// pixels — called by the `GetCursorPos` reader under the big lock it
    /// already holds. A copy, NOT a drain (`None` until the first host
    /// push, which the handler reports as the legacy `(0, 0)`).
    pub fn cursor_pos(&mut self) -> Option<(i32, i32)> {
        self.present().channel.cursor_pos()
    }

    /// Read the host-pushed mouse-button mask (Win32 `MK_*` bits: `MK_LBUTTON`
    /// 0x0001, `MK_RBUTTON` 0x0002, `MK_MBUTTON` 0x0010, `MK_XBUTTON1`
    /// 0x0020, `MK_XBUTTON2` 0x0040) — called by the DirectInput mouse report
    /// path under the big lock it already holds.
    ///
    /// Level state: a copy, NOT a drain, so a held button reads down on every
    /// `GetDeviceState` until the host pushes a newer mask. `0` before the
    /// first host push is the truthful "nothing pressed", not "unknown".
    pub fn mouse_buttons(&mut self) -> u16 {
        self.present().channel.mouse_buttons()
    }

    /// Drain the host's pending wheel movement as whole notches,
    /// `(horizontal, vertical)` — called by the DirectInput mouse report path
    /// under the big lock it already holds.
    ///
    /// Relative state, so a drain: `lZ` is a relative axis, and a guest that
    /// polls `GetDeviceState` in a loop must see each notch once, not once per
    /// poll. Positive is away from the user, negative toward.
    pub fn drain_wheel_notches(&mut self) -> (i32, i32) {
        self.present().channel.drain_wheel_notches()
    }
}

// ── Cross-crate accessors (ADR-0006) ───────────────────────────────────────
//
// `wie-runtime` builds one `WinApiState` per session and then configures it.
// Before ADR-0006 that configuration was ~23 direct field writes
// (`state.file_io.volumes = …`, `state.process.main_module_dialogs = …`, …),
// which made every field of the five state structs part of the published
// surface whether or not anyone meant it. These methods are the replacement:
// one narrow verb per need, so a field can be renamed inside this module
// without touching another crate.
//
// Deliberately absent: a general `&mut FileIoState` / `&mut ProcessState`
// escape hatch. That is the shape that produced the problem. If a new need
// does not fit one of these verbs, add a verb — see `crate::port`'s table.
impl WinApiState {
    /// The session heap shard, cloned out of the state.
    ///
    /// The heap lives behind its own mutex so lock ordering is
    /// `global -> heap`, never the reverse; a caller that holds this must not
    /// take the `WinApiState` lock while holding it.
    #[must_use]
    pub fn heap_arc(&self) -> Arc<Mutex<crate::GuestHeap>> {
        Arc::clone(&self.heap_state.heap)
    }

    /// Install the guest VA of the FLS value table (`TlsAlloc`/`FlsAlloc` use it).
    pub fn set_guest_fls_table_va(&mut self, table_va: u64) {
        self.heap_state.guest_fls_table_va = table_va;
    }

    /// Replace the session's volume table (guest drive → host root mapping).
    pub fn set_volumes(&mut self, volumes: crate::vfs::VolumeConfig) {
        self.file_io.volumes = volumes;
    }

    /// The session's volume table.
    #[must_use]
    pub fn volumes(&self) -> &crate::vfs::VolumeConfig {
        &self.file_io.volumes
    }

    /// Replace the legacy single C: bottle root.
    ///
    /// Predates [`Self::set_volumes`]; both are still written because VFS
    /// consumers refresh the shared view from it. Prefer `set_volumes` in new
    /// code.
    pub fn set_bottle_root(&mut self, root: Option<std::path::PathBuf>) {
        self.file_io.bottle_root = root;
    }

    /// Publish the guest I/O layout and the first free file-data arena slot.
    ///
    /// One call because the two are set together at session init and a
    /// half-configured pair would let the guest's handle table point into an
    /// arena the host had not placed yet.
    pub fn attach_guest_io(&mut self, config: GuestIoRuntimeConfig, file_data_next: u64) {
        self.file_io.guest_io = Some(config);
        self.file_io.guest_file_data_next = file_data_next;
    }

    /// Install the console stdin buffer and its source policy.
    ///
    /// Cursor starts at zero: this is the session-init entry point, not a
    /// mid-run refill.
    pub fn set_stdin(&mut self, bytes: Vec<u8>, mode: GuestStdinMode) {
        self.file_io.stdin_bytes = bytes;
        self.file_io.stdin_cursor = 0;
        self.file_io.stdin_mode = mode;
    }

    /// Replace the guest-visible display geometry (logical points on Retina).
    pub fn set_display_metrics(&mut self, metrics: DisplayMetrics) {
        self.display = metrics;
    }

    /// Install the main module's parsed resource tables.
    ///
    /// The main EXE does not go through `dll_loader::load_dll`, so its
    /// RT_DIALOG/RT_MENU/RT_STRING/RT_ACCELERATOR templates are parsed by the
    /// runtime at session init and handed over here. One call rather than four
    /// writes: `DialogBoxParam` can arrive between two of them in principle,
    /// and an empty-but-parsed table is indistinguishable from "not parsed yet".
    pub fn set_main_module_resources(
        &mut self,
        dialogs: Vec<wie_pe::resources::DialogTemplate>,
        menus: Vec<wie_pe::resources::MenuTemplate>,
        strings: Vec<wie_pe::resources::StringBlock>,
        accelerators: Vec<wie_pe::resources::AccelTemplate>,
    ) {
        self.process.main_module_dialogs = dialogs;
        self.process.main_module_menus = menus;
        self.process.main_module_strings = strings;
        self.process.main_module_accelerators = accelerators;
    }

    /// Set the host directory searched for the main module's DLL dependencies.
    pub fn set_main_module_host_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.process.main_module_host_dir = dir;
    }

    /// Guest path of the main module, as the identity APIs report it.
    #[must_use]
    pub fn main_module_path(&self) -> &str {
        &self.process.main_module_path
    }

    /// The Win32 process environment block, as `(name, value)` in insertion order.
    pub fn environment_mut(&mut self) -> &mut Vec<(String, String)> {
        &mut self.process.environment
    }

    /// Overwrite the active thread's last-error value (and its process alias).
    pub fn set_last_error(&mut self, error: u32) {
        self.kernel.threads.active.last_error = error;
        self.process.last_error = error;
    }

    /// Guest TID of the thread currently owning the engine.
    #[must_use]
    pub fn active_tid(&self) -> u32 {
        self.kernel.threads.active.tid
    }

    /// The active thread's own last-error slot (authoritative per-thread store).
    #[must_use]
    pub fn active_last_error(&self) -> u32 {
        self.kernel.threads.active.last_error
    }

    /// Overwrite the active thread's last-error slot only.
    ///
    /// Unlike [`Self::set_last_error`] this does not touch the process alias —
    /// for the activation seam, where the alias deliberately still holds the
    /// PREVIOUS thread's value.
    pub fn set_active_last_error(&mut self, error: u32) {
        self.kernel.threads.active.last_error = error;
    }

    /// Switch the active guest thread, saving the outgoing one's registers.
    pub fn activate_thread(&mut self, tid: u32) {
        self.kernel.threads.activate(tid);
    }

    /// Spill the active thread's registers back into its `by_tid` slot.
    pub fn save_active_thread(&mut self) {
        self.kernel.threads.save_active();
    }

    /// The active thread's TID as the guest sees it (`GetCurrentThreadId`).
    #[must_use]
    pub fn current_tid(&self) -> u32 {
        self.kernel.threads.current_tid()
    }

    /// The loaded module whose image contains `va`, as `(name, offset)`.
    ///
    /// Replaces iterating `module_state.loaded_modules` to answer "which
    /// module is this address in" — the runtime's only reason to look at the
    /// module table directly.
    #[must_use]
    pub fn module_for_va(&self, va: u64) -> Option<(&str, u64)> {
        self.module_state
            .loaded_modules
            .values()
            .find(|module| {
                va >= module.image_base
                    && va < module.image_base.saturating_add(module.image_size as u64)
            })
            .map(|module| (module.name.as_str(), va - module.image_base))
    }

    /// Install the guest IAT resolver used by dynamic `LoadLibrary` imports.
    pub fn install_import_resolver(&mut self, resolver: ImportResolver) {
        self.module_state.import_resolver = Some(resolver);
    }

    /// Replace the legacy D: root (both the volume table and its own field).
    pub fn set_drive_d_root(&mut self, root: Option<std::path::PathBuf>) {
        self.file_io.volumes.drive_d_root = root;
    }

    /// The open guest file for `handle`, if any.
    ///
    /// Replaces reading `file_io.open_files` directly — the runtime's guest-file
    /// helpers (size / peek / cursor) are the only outside readers.
    #[must_use]
    pub fn open_guest_file(&self, handle: u64) -> Option<&OpenGuestFile> {
        self.file_io.open_files.get(&handle)
    }

    /// Every open guest file, for a diagnostic snapshot.
    #[must_use]
    pub fn open_guest_files(&self) -> &ahash::HashMap<u64, OpenGuestFile> {
        &self.file_io.open_files
    }

    /// The guest message queue shard, cloned out of the state.
    ///
    /// Its own mutex: the host posts input through this without ever taking
    /// the `WinApiState` lock the guest holds during a handler.
    #[must_use]
    pub fn message_queue_arc(&self) -> Arc<Mutex<present::MessageQueue>> {
        Arc::clone(&self.message_queue)
    }

    /// The guest current directory as UTF-16 code units (Windows form).
    #[must_use]
    pub fn current_directory_wide(&self) -> &[u16] {
        &self.file_io.current_directory_wide
    }

    /// The opaque scheduler handle (ADR-0006).
    ///
    /// `SyncState` is the one struct in this crate that a consumer drives
    /// rather than reads, and handing out `&mut SyncState` is what made its 14
    /// fields published surface. [`crate::port::Scheduler`] exposes the 15
    /// operations the session's scheduler and park loops actually perform;
    /// there is no way to get the struct back out of it.
    pub fn scheduler(&mut self) -> crate::port::Scheduler<'_> {
        crate::port::Scheduler::new(&mut self.kernel.sync)
    }

    /// Whether `ExitProcess` has been requested.
    ///
    /// On `WinApiState` rather than on the [`Self::scheduler`] handle because
    /// several park loops hold only `&WinApiState`.
    #[must_use]
    pub fn is_process_dying(&self) -> bool {
        self.kernel.sync.process_dying
    }

    /// Resolve `handles` to live wait targets, or `None` if any is unknown.
    #[must_use]
    pub fn wait_targets(&self, handles: &[u64]) -> Option<Vec<crate::sync_obj::WaitTarget>> {
        self.kernel.sync.wait_targets(handles)
    }

    /// The saved CPU context for `tid`, while it is not running on the engine.
    #[must_use]
    pub fn saved_thread_context(&self, tid: u32) -> Option<wie_cpu::ThreadContext> {
        self.kernel.sync.saved_thread_context(tid)
    }

    /// Finish the guest thread object whose TID is `tid`, waking joiners.
    ///
    /// `&self`: the exit code lives in an `Arc`'d atomic on the object, so this
    /// reads the table and signals, it does not restructure it.
    pub fn finish_thread_object(&self, tid: u32, code: u32) {
        self.kernel.sync.finish_thread_object(tid, code);
    }

    /// The session's wake hub (clone of the `Arc`-backed registry).
    #[must_use]
    pub fn wake_hub(&self) -> crate::wake::WakeHub {
        self.kernel.sync.wake_hub.clone()
    }
}

// ── Session construction (ADR-0006) ────────────────────────────────────────
//
// `wie-runtime` used to build a `WinApiState` with a 44-field struct literal.
// That literal is why these five state structs could not be opaque: a literal
// names every field, so every field was published surface whether or not
// anybody meant it.
//
// The replacement is not a builder. A builder with one setter per field would
// be the same surface in method syntax — the honest objection, and it is
// correct. What the runtime actually has is *session inputs*: a heap range, the
// mapped exe, the module identity, two guest VAs, an environment, and two
// bottle roots. [`SessionInit`] is that list (11 values), and every remaining
// field of the state is a constant or a derivative that belongs here rather
// than in the caller. [`WinApiState::empty`] is the same idea for the bare
// fixture state a handler-level test needs.

/// The session inputs [`WinApiState::for_session`] needs.
///
/// Everything here is a value the session genuinely varies; everything else in
/// the state is a fixed default or derived from one of these.
#[derive(Debug, Clone)]
pub struct SessionInit {
    /// Guest VA the process heap starts at.
    pub heap_base: u64,
    /// Size of the process heap in bytes.
    pub heap_size: u64,
    /// Raw main-module file bytes, shared (`Arc`) — readers only slice them.
    pub executable_file_bytes: Arc<Vec<u8>>,
    /// Main module's base file name (`GetModuleFileName` short form).
    pub module_file_name: String,
    /// Main module's full guest path.
    pub module_path: String,
    /// Guest current directory (Windows form, not yet UTF-16).
    pub current_directory: String,
    /// Guest VA of the FLS value table.
    pub guest_fls_table_va: u64,
    /// Guest VA of the file-content mirror arena.
    pub guest_file_data_va: u64,
    /// Win32 process environment, `(name, value)` in insertion order.
    pub environment: Vec<(String, String)>,
    /// Legacy single C: bottle root.
    pub bottle_root: Option<std::path::PathBuf>,
    /// Host bridge root for guest `D:`.
    pub drive_d_root: Option<std::path::PathBuf>,
}

impl WinApiState {
    /// Build the state for one session.
    ///
    /// Seeds the C: folder skeleton as a side effect (idempotent mkdirs), exactly
    /// where the old literal did, so the on-disk result of a session start is
    /// unchanged.
    pub fn for_session(init: SessionInit) -> Result<Self> {
        let heap_end = init
            .heap_base
            .checked_add(init.heap_size)
            .context("heap end overflow")?;
        let executable_file_size = u64::try_from(init.executable_file_bytes.len())
            .context("executable file size does not fit u64")?;
        let volumes = {
            let drive_d = init.drive_d_root.clone();
            if let Some(ref root) = init.bottle_root {
                drop(crate::vfs::seed_default_skeleton(root));
            }
            crate::vfs::VolumeConfig::from_parts(init.bottle_root.clone(), drive_d)
        };
        Ok(Self {
            display: DisplayMetrics::default(),
            heap_state: HeapState {
                heap: Arc::new(Mutex::new(crate::guest_heap::GuestHeap::new(
                    init.heap_base,
                    heap_end,
                ))),
                next_fls_index: 1,
                fls_slots: Vec::new(),
                guest_fls_table_va: init.guest_fls_table_va,
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: RegistryKeyHandle::from(0x0000_0000_7000_0000),
                registry_keys: Vec::new(),
                main_module_file_name: init.module_file_name,
                main_module_path: init.module_path,
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: ahash::HashMap::new(),
                environment: init.environment,
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: ahash::HashMap::new(),
            },
            file_io: FileIoState {
                executable_file_size,
                executable_file_bytes: init.executable_file_bytes,
                executable_file_cursor: 0,
                next_find_handle: FindFileHandle::from(0x0000_0000_6200_0000),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: ahash::HashMap::new(),
                next_file_handle: FileHandle::from(0x0000_0000_6700_0001),
                next_resource_handle: ResourceHandle::from(0x0000_0000_6300_0000),
                resources: Vec::new(),
                current_directory_wide: init.current_directory.encode_utf16().collect(),
                bottle_root: init.bottle_root,
                volumes,
                guest_file_data_next: init.guest_file_data_va,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: GuestStdinMode::InjectOnly,
                ucrt_files: ahash::HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: ahash::HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: ahash::HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: ahash::HashMap::with_capacity(64),
                next_module_handle: ModuleHandle::from(crate::dll_loader::REAL_MODULE_HANDLE_BASE),
            },
        })
    }

    /// The bare state a handler-level test starts from.
    ///
    /// Every counter and every collection is at its zero value: no volumes, no
    /// environment, no volume skeleton on disk, and handle bases of `0`. That is
    /// deliberately *not* what a session gets — [`Self::for_session`] is — so a
    /// test that needs the production seeding should call that instead. Provided
    /// because the alternative was a 44-field literal in an integration test,
    /// which is what kept these structs un-opaque.
    #[must_use]
    pub fn empty(heap_base: u64, heap_end: u64) -> Self {
        Self {
            display: DisplayMetrics::default(),
            heap_state: HeapState {
                heap: Arc::new(Mutex::new(crate::guest_heap::GuestHeap::new(
                    heap_base, heap_end,
                ))),
                next_fls_index: 0,
                fls_slots: Vec::new(),
                guest_fls_table_va: 0,
            },
            process: ProcessState {
                last_error: 0,
                next_registry_key_handle: RegistryKeyHandle::from(0),
                registry_keys: Vec::new(),
                main_module_file_name: String::new(),
                main_module_path: String::new(),
                main_module_host_dir: None,
                error_mode: 0,
                suspended_threads: ahash::HashMap::new(),
                environment: Vec::new(),
                main_module_dialogs: Vec::new(),
                main_module_menus: Vec::new(),
                main_module_strings: Vec::new(),
                main_module_accelerators: Vec::new(),
            },
            kernel: KernelState {
                threads: ThreadState::primary(),
                sync: SyncState::new(),
                seh_pending: ahash::HashMap::new(),
            },
            file_io: FileIoState {
                executable_file_size: 0,
                executable_file_bytes: Arc::new(Vec::new()),
                executable_file_cursor: 0,
                next_find_handle: FindFileHandle::from(0),
                find_handles: Vec::new(),
                host_file_mounts: Vec::new(),
                virtual_files: Vec::new(),
                open_files: ahash::HashMap::new(),
                next_file_handle: FileHandle::from(0),
                next_resource_handle: ResourceHandle::from(0),
                resources: Vec::new(),
                current_directory_wide: Vec::new(),
                bottle_root: None,
                volumes: crate::vfs::VolumeConfig::default(),
                guest_file_data_next: 0,
                guest_io: None,
                stdin_bytes: Vec::new(),
                stdin_cursor: 0,
                stdin_mode: GuestStdinMode::InjectOnly,
                ucrt_files: ahash::HashMap::new(),
                ucrt_next_file_va: 0x0000_0000_6900_0000,
                cached_streams: ahash::HashMap::new(),
            },
            dll_states: DllStateMap::new(),
            message_queue: Arc::new(Mutex::new(present::MessageQueue::default())),
            module_state: ModuleState {
                loaded_modules: ahash::HashMap::new(),
                import_resolver: None,
                get_proc_address_cache: ahash::HashMap::new(),
                next_module_handle: ModuleHandle::from(crate::dll_loader::REAL_MODULE_HANDLE_BASE),
            },
        }
    }
}

/// Bundle of everything a WinAPI handler may need.
///
/// Passed as `HandlerContext` to every handler so adding new context
/// fields doesn't touch handler signatures and the dispatch table is uniform.
///
/// The heap lives in a dedicated `Arc<Mutex<GuestHeap>>` shard at the
/// session level, next to the `Mutex<WinApiState>`; handlers that need the
/// heap lock it via `ctx.heap` (order `global -> heap` when both are taken).
pub struct HandlerContext<'a> {
    /// CPU engine (mem_read / mem_write / register access).
    pub engine: &'a mut dyn CpuEngine,
    /// Session environment (image base, command line, heap handle, …).
    pub environment: WinApiEnvironment,
    /// Full emulator state.
    pub state: &'a mut WinApiState,
    /// Session-level heap shard (`Arc<Mutex<GuestHeap>>` in `wie-runtime`).
    pub heap: Arc<Mutex<crate::GuestHeap>>,
}

impl<'a> HandlerContext<'a> {
    pub fn new(
        engine: &'a mut dyn CpuEngine,
        environment: WinApiEnvironment,
        state: &'a mut WinApiState,
    ) -> Self {
        let heap = std::sync::Arc::clone(&state.heap_state.heap);
        Self {
            engine,
            environment,
            state,
            heap,
        }
    }

    /// The console capability: the console's own state, its stdin buffer, and
    /// the CPU port — and nothing else (ADR-0005).
    ///
    /// Returned with the engine attached rather than as a bare `&mut dyn
    /// ConsoleAccess` because a console handler reads its arguments from
    /// registers; keeping the two as separate fields of the returned struct is
    /// what lets `cx.engine.read_rcx()?` and `cx.state.console()` coexist under
    /// the borrow checker, exactly as `ctx.engine` / `ctx.state` did.
    ///
    /// This is the seam the console handlers are type-checked against: inside
    /// [`crate::console`] there is no `HandlerContext`, so
    /// `cx.state.window_state()` or `cx.state.process` is a compile error
    /// rather than a review comment.
    pub fn console(&mut self) -> crate::capability::ConsoleCall<'_> {
        crate::capability::ConsoleCall {
            engine: &mut *self.engine,
            state: &mut *self.state,
        }
    }

    /// The common-dialog capability: the dialog session plumbing `comdlg32`
    /// needs off `WindowState`, and nothing else (ADR-0005).
    ///
    /// A plain `&mut dyn` rather than a context struct like
    /// [`Self::console`], because no `comdlg32` handler is dialog-only: each one
    /// also builds or writes guest windows, which is `user32`'s machinery and
    /// stays on `state`. Handlers therefore bind `ctx.state` and pass it to the
    /// capability-typed helpers below it, and *those* are what the compiler
    /// holds to the dialog surface.
    pub fn dialogs(&mut self) -> &mut dyn crate::capability::WindowDialogAccess {
        self.state
    }

    /// Canonical handler tail: pop the guest return address, write `value`
    /// into RAX, and package the result.
    ///
    /// Replaces the two-step `engine.return_from_win64_api(v)?;` +
    /// `Ok(WinApiHandlerResult { return_address, return_value })` idiom.
    /// Errors need no per-API context here — the dispatcher already wraps
    /// handler failures as `{lib}!{name}: {error}`.
    pub fn finish(&mut self, value: u64) -> Result<crate::WinApiHandlerResult> {
        // Wave 2 Step 2: refresh the presenter-side window mirror after every
        // handler dispatch. Rev-gated: the common case (no window mutation)
        // is two integer compares — the projection rebuild only runs after a
        // site bumped `window_mirror_rev`. Handlers that bail with a control
        // signal before `finish` sync on their next successful dispatch.
        self.state.sync_window_mirror_if_dirty();
        let return_address = self.engine.return_from_win64_api(value)?;
        Ok(crate::WinApiHandlerResult {
            return_address,
            return_value: value,
        })
    }
}

#[cfg(test)]
mod display_metrics_tests {
    use super::DisplayMetrics;

    #[test]
    fn mm_sizes_match_the_96_dpi_logical_baseline() {
        // Defaults preserve the historical table (1920→508, 1080→286).
        let default = DisplayMetrics::default();
        assert_eq!(default.width_metric(), 1920);
        assert_eq!(default.height_metric(), 1080);
        assert_eq!(default.width_mm(), 508);
        assert_eq!(default.height_mm(), 286);
        // A 1728×1117 point monitor scales proportionally (rounded half-up).
        let custom = DisplayMetrics::new(1728, 1117);
        assert_eq!(custom.width_metric(), 1728);
        assert_eq!(custom.height_metric(), 1117);
        assert_eq!(custom.width_mm(), 457);
        assert_eq!(custom.height_mm(), 296);
    }
}
