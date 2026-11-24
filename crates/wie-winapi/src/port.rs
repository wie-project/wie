//! The published surface of `wie-winapi` — the **only** path cross-crate
//! consumers may use (ADR-0006).
//!
//! # Why this module exists
//!
//! Before ADR-0006 the crate root re-exported ~60 names flat out of `state`
//! and 6 out of `dispatch_table`. Nothing distinguished "the runtime needs
//! this type" from "the runtime reaches into the emulator's guts": both looked
//! like `wie_winapi::SomeName`. A rename inside `state` was therefore a
//! breaking cross-crate change, which is what turned ADR-0005's
//! capability-traits refactor into a cross-crate edit and blocked staging it.
//!
//! `port` draws the line. Three things enforce it:
//!
//! 1. Everything a cross-crate consumer legitimately needs is re-exported
//!    **here**, once, so the surface is one reviewable list.
//! 2. `./scripts/check.sh --only winapi-port` fails when `wie-runtime` names a
//!    gated WinAPI module path (`user32`, `gdi32`, `present`, `winmm`,
//!    `kernel32`, `dll_loader`, `pthread`, `seh`) or pokes a `WinApiState`
//!    field directly, outside an explicit allow-list. A rule with no test is
//!    documentation, not enforcement.
//! 3. The crate root keeps only what `wie-cli` names (it is the other
//!    out-of-crate consumer) plus this module; the rest of `state` and
//!    `dispatch_table` is `pub(crate)`.
//!
//! # The rule
//!
//! - Need a **type or function**? It is re-exported below, in the section
//!   whose doc comment says why the boundary crosses there.
//! - Need to **read or write a field of [`WinApiState`]**? Call the accessor
//!   named in the table below. Do not reach for the field. If no accessor
//!   expresses the need, the need is a design decision: add one accessor here
//!   rather than re-widening a module.
//!
//! # Accessors — the one-to-one replacement table
//!
//! This table is the deliverable of ADR-0006's step 1. Each row is a *class*
//! of deep reach that existed in `wie-runtime`; the right column is the
//! accessor that replaces it. All of them live on [`WinApiState`].
//!
//! | Deep reach (removed) | Accessor |
//! | --- | --- |
//! | `state.heap_state.heap` | [`WinApiState::heap_arc`] |
//! | `state.heap_state.guest_fls_table_va` | [`WinApiState::set_guest_fls_table_va`] |
//! | `state.file_io.volumes` (write) | [`WinApiState::set_volumes`] |
//! | `state.file_io.volumes` (read) | [`WinApiState::volumes`] |
//! | `state.file_io.bottle_root` | [`WinApiState::set_bottle_root`] |
//! | `state.file_io.guest_io` + `.guest_file_data_next` | [`WinApiState::attach_guest_io`] |
//! | `state.file_io.stdin_{mode,bytes,cursor}` | [`WinApiState::set_stdin`] |
//! | `state.module_state.loaded_modules` | [`WinApiState::module_for_va`] |
//! | `state.module_state.import_resolver` | [`WinApiState::install_import_resolver`] |
//! | `state.display` | [`WinApiState::set_display_metrics`] |
//! | `state.process.main_module_{dialogs,menus,strings,accelerators}` | [`WinApiState::set_main_module_resources`] |
//! | `state.process.main_module_host_dir` | [`WinApiState::set_main_module_host_dir`] |
//! | `state.process.main_module_path` | [`WinApiState::main_module_path`] |
//! | `state.process.environment` | [`WinApiState::environment_mut`] |
//! | `state.process.last_error` | [`WinApiState::set_last_error`] |
//! | `state.kernel.threads.active.tid` | [`WinApiState::active_tid`] |
//! | `state.kernel.threads.active.last_error` | [`WinApiState::active_last_error`] / [`WinApiState::set_active_last_error`] |
//! | `state.kernel.threads.activate(tid)` | [`WinApiState::activate_thread`] |
//! | `state.kernel.threads.save_active()` | [`WinApiState::save_active_thread`] |
//! | `state.kernel.threads.current_tid()` | [`WinApiState::current_tid`] |
//! | `user32::handle_get_dc` (direct handler call) | [`dispatch_winapi_id`] with [`WinApiId::User32Getdc`] |
//! | `gdi32::handle_*` (direct handler calls) | [`dispatch_winapi_id`] with the matching [`WinApiId`] |
//!
//! Deliberately **not** in the table, because the ADR rejects the trade:
//! `WindowState`'s 31 public fields and `PresentState`'s fields still reach
//! the runtime through [`WinApiState::window_state`] and
//! [`WinApiState::present`]. Making those structs opaque costs 146 in-crate
//! sites for no DLL-boundary win (both sides are in-crate) — ADR-0006
//! "Alternatives considered". Their *types* are re-exported below, so a rename
//! inside them is still a cross-crate change; that is recorded debt, not an
//! oversight. `state.kernel.sync` is the third such case and the largest: see
//! [`SyncState`] below.

// ── §1 Dispatch identity and fake-VA encoding ──────────────────────────────
//
// The hot path: resolving an import to a dense id, encoding a fake target VA,
// and the trait flags that tell the runtime whether a stop can be avoided.
// This is the ~twelve-name surface ADR-0006 measured first; it is the one part
// of the boundary that was never in doubt.
pub use crate::dispatch_table::{
    WINAPI_ID_COUNT, WinApiId, WinApiTraits, dispatch_winapi, dispatch_winapi_id,
    is_winapi_implemented, is_winapi_library, resolve_winapi_id, winapi_id_export,
};
pub use crate::dynamic_apis::PREPLANTED_SOFT_APIS;
pub use crate::fake_va::{
    ComIface, ComMethod, DInput8Iface, DirectInput8Method, DirectInputDevice8Method, FAKE_API_BASE,
    FAKE_API_SIZE, FakeVa, callback_return_trampoline_va, decode as decode_fake_va,
    dll_main_return_trampoline_va, encode_alias, encode_export, encode_unresolved,
    seh_continue_trampoline_va,
};

// ── §2 The session boundary ────────────────────────────────────────────────
//
// What a `wie-runtime` session must name to drive one WinAPI dispatch: the
// state, the per-call context, the environment, and the two result shapes a
// handler can hand back (a value, or a control signal that unwinds to the
// runtime for host work).
pub use crate::kernel32::WinApiHandlerResult;
pub use crate::state::{
    GuestCallbackRequest, HandlerContext, HostParkReason, OuterReturn, WinApiControlSignal,
    WinApiEnvironment, WinApiState,
};

// ── §3 State types a session constructs or names ───────────────────────────
//
// `wie-runtime` builds one `WinApiState` per session and then talks to it
// through the §5 accessors. These are the types the construction and the
// accessor *signatures* mention, so they must be nameable — but the fields
// stay behind the accessors.
pub use crate::state::{
    DEFAULT_ENVIRONMENT, DisplayMetrics, DllStateMap, FileHandle, FileIoState, FindFileHandle,
    GuestIoRuntimeConfig, GuestStdinMode, HeapState, HostFileMount, ImportResolver, KernelState,
    ModuleHandle, ModuleState, ProcessState, RegistryKeyHandle, ResourceHandle,
};

// ── §4 Kernel objects, threads, waits, and parking ─────────────────────────
//
// The runtime's scheduler and its park loops need to name kernel objects and
// the wake/park vocabulary. Types only: the *fields* of `SyncState` are the
// largest remaining wide surface in this crate (the scheduler drives
// `state.kernel.sync.{objects,cs_waiters,multi_wait,process_dying,…}`
// directly, ~45 sites). Narrowing that needs one opaque scheduler handle, i.e.
// its own change — ADR-0006 records it as debt rather than pretending a
// re-export fixed it.
pub use crate::state::pthread_return_trampoline_va;
pub use crate::sync_obj::{
    INFINITE, KernelHandle, KernelObject, MultiWaitRequest, PendingSpawn, SyncState, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT, WaitTarget, wait_multiple, wait_multiple_step,
};
pub use crate::thread::{GuestThread, PRIMARY_THREAD_ID, ThreadState, bind_current_tid};
pub use crate::wake::{ThreadInbox, Wake, WakeHub};

// ── §5 Heap and idle policy ────────────────────────────────────────────────
pub use crate::guest_heap::{GuestHeap, HEAP_SIZE_CLASS_COUNT, LARGE_THRESHOLD, SIZE_CLASSES};
pub use crate::idle::{IdleContext, IdlePolicy};

// ── §6 Windows, menus, messages, and the presenter channel ─────────────────
//
// The window pump crosses here. `Dimension` is the geometry type winit hands
// the runtime; the `MessageQueue` / `PresentChannel` pair is the lock-free
// host↔guest seam; `MirrorWindow` / `SurfaceFrame` are what the presenter
// reads back. These were reached as `wie_winapi::present::`/`::user32::` before
// ADR-0006; they are re-exported here so the module path is no longer public
// API.
pub use crate::comdlg32::{FONT_DLG_STRIKEOUT_ID, FONT_DLG_UNDERLINE_ID};
pub use crate::handles::{Hmenu, Hwnd};
pub use crate::present::{
    CaptureStreamerHandle, MessageBoxBridge, MessageQueue, MessageSignal, MirrorWindow,
    PresentChannel, SurfaceFrame, frame_timing_enabled, set_frame_timing_enabled,
    spawn_capture_streamer,
};
pub use crate::state::{
    FileDialogBridge, FileDialogPolicy, FontDialogPolicy, MessageQueueIdlePolicy,
    PageSetupDialogBridge, PageSetupDialogPolicy, PrintDialogBridge, PrintDialogPolicy,
    PrintJobBridge, QueuedWindowMessage, WindowFlags, WindowRecord,
};
pub use crate::user32::dragdrop::FAKE_HDROP;
pub use crate::user32::{Dimension, MenuEntry, MenuRecord};

// ── §7 Platform services the runtime drives ────────────────────────────────
//
// Wall-clock and tick sources, the guest path translation layer, stdin, timers,
// and the enumeration/parse helpers a park loop needs. Each row here replaces a
// `wie_winapi::<module>::` path that the winapi-port gate now forbids.
pub use crate::bottle::{bottle_root_from_env, drive_d_from_env};
pub use crate::console::take_ctrlc_for_profile_stop;
pub use crate::exception::parse_pdata;
pub use crate::gdi32::enumerate::advance_enumeration;
pub use crate::kernel32::clock::{clock_is_fixed, clock_table_values, tick_count_32};
pub use crate::kernel32::{
    mount_host_file, open_guest_path, resolve_cs_queue, resolve_wait_target,
};
pub use crate::pthread::take_park;
pub use crate::seh::{continue_pending, dispatch_hardware_fault};
pub use crate::ucrt::{crt_data_import_va, is_ucrt_library};
pub use crate::vfs::{
    VolumeConfig, effective_bottle_root, guest_parent, guest_path_to_host, host_path_to_guest,
    seed_default_skeleton,
};
pub use crate::winmm::{DueTimer, DueTimerKind, WOM_DONE, WaveOutCallbackKind};

// ── §8 PE loading internals ────────────────────────────────────────────────
//
// **Recorded debt, not a boundary.** ADR-0006 flags these four as worse than a
// wide surface: they are PE-loading mechanics, so `wie-runtime` is reaching
// past `wie-pe`'s API rather than past `wie-winapi`'s. Re-exporting them here
// makes the reach *visible and reviewable*; it does not make it right. The fix
// is to move static-import resolution behind a named `wie-winapi` API (or into
// `wie-pe`), which is a separate change. Until then they are the only names in
// this module whose doc comment says "do not add more like this".
pub use crate::dll_loader::{
    DLL_PROCESS_ATTACH, REAL_MODULE_HANDLE_BASE, StaticDllMain, apply_pe_section_protects,
    prepare_dll_main_call, resolve_static_guest_import,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The port is a re-export layer, so the thing worth pinning is that the
    /// names the gate's allow-list assumes exist really do exist. A re-export
    /// deleted by accident breaks `wie-runtime` with a confusing error; this
    /// fails here, naming the port.
    #[test]
    fn port_publishes_the_session_boundary() {
        // §1 dispatch identity.
        let _: fn(&str) -> bool = is_winapi_library;
        let _: fn(&str, &str) -> Option<WinApiId> = resolve_winapi_id;
        let id_count: usize = WINAPI_ID_COUNT;
        assert!(id_count > 0);
        // §2 the session boundary types are nameable from outside the crate.
        // Naming them in a type position is the assertion; a deleted re-export
        // fails to compile here instead of in `wie-runtime`.
        assert!(std::mem::size_of::<&WinApiState>() > 0);
        assert!(std::mem::size_of::<&HandlerContext<'_>>() > 0);
        // §6 the window pump's own types.
        assert_eq!(Hwnd::NULL.as_u64(), 0);
        assert_eq!(Hmenu::NULL.as_u64(), 0);
    }
}
