//! WinAPI dispatcher model for WIE (generic PE64 userspace).

// Machine-checked gate for this crate's *production* code (CLAUDE.md "Lint
// policy", plan T1.2): no `unwrap`/`expect`/`panic`. A handler that panics kills
// the whole guest session, so "return an HRESULT" is the rule and this is what
// makes it a build failure rather than a review note. Test builds are exempt on
// purpose. `indexing_slicing`/`as_conversions` are NOT gated here — 1,021 and
// 689 measured sites, mostly guest-buffer reads after an explicit bounds check.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod advapi32;
pub mod bottle;
// ADR-0005: the named capabilities a handler may ask for, and the narrowed
// contexts that carry them. Private module: a capability is an in-crate seam
// (`port`, ADR-0006, is the published surface), so the `pub` items below it are
// `pub`-but-unreachable by design — the same shape as `mod state`.
#[expect(
    unreachable_pub,
    reason = "capabilities are an in-crate seam; `port` is the published surface"
)]
mod capability;
pub mod clipboard;
pub mod color;
pub mod comctl32;
pub mod comdlg32;
pub mod console;
pub mod crypt32;
pub mod d3d9;
pub mod d3d9_render;
pub mod d3d9_shader;
pub mod dbghelp;
pub mod dinput;
pub mod dll_loader;
pub mod dynamic_apis;
pub mod pthread;
pub use dynamic_apis::{DYNAMIC_FAKE_APIS, PREPLANTED_SOFT_APIS, resolve_get_proc_address};
pub mod exception;
pub mod fake_va;
pub mod gdi32;
pub mod guest_heap;
pub mod guest_io_host;
mod guest_layout;
mod guest_memory;
mod guest_string;
pub mod handles;
pub mod idle;
pub mod imm32;
pub mod kernel32;
pub mod lock_order;
pub mod mingw_dispatch;
pub mod msimg32;
pub mod msvc_eh;
pub mod ntdll;
pub mod ole32;
pub mod oleaut32;
pub mod opengl32;
pub mod present;
mod registry;
pub mod seh;
pub mod setupapi;
pub mod shell32;
mod soft_dispatch;
pub mod sync_obj;
pub mod thread;
pub mod ucrt;
pub mod urlmon;
pub mod user32;
pub mod uxtheme;
pub mod version;
pub mod vfs;
pub mod wake;
pub mod winhttp;
pub mod wininet;
pub mod winmm;
pub mod ws2_32;
pub use bottle::{bottle_root_from_env, drive_d_from_env, guest_path_to_host};
pub use exception::{RuntimeFunction, lookup_function_entry};
pub use sync_obj::{
    CsWaitQueue, FileMappingObject, INFINITE, KernelHandle, KernelObject, MAXIMUM_WAIT_OBJECTS,
    MultiWaitRequest, PendingSpawn, STILL_ACTIVE, SemaphoreObject, SyncState, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT, WaitTarget, wait_multiple, wait_multiple_step,
};
pub use vfs::{
    VolumeConfig, effective_bottle_root, global_bottle_root, host_path_to_guest,
    seed_default_skeleton,
};
pub use wake::{ThreadInbox, Wake, WakeHub};
#[cfg(test)]
mod exception_helpers;
#[cfg(test)]
mod exception_tests;
pub use fake_va::{
    ComIface, ComMethod, D3d9Iface, DInput8Iface, Device9Method, Direct3D9Method, FAKE_API_BASE,
    FAKE_API_SIZE, FakeVa, IndexBuffer9Method, PixelShader9Method, SPECIAL_CALLBACK_RETURN,
    SPECIAL_DLL_MAIN_RETURN, SPECIAL_SEH_CONTINUE, Surface9Method, Texture9Method,
    VertexBuffer9Method, VertexShader9Method, callback_return_trampoline_va,
    decode as decode_fake_va, dll_main_return_trampoline_va, encode_alias, encode_com,
    encode_export, encode_unresolved, seh_continue_trampoline_va,
};
pub use guest_heap::GuestHeap;
pub use idle::{IdleContext, IdlePolicy};
pub use kernel32::WinApiHandlerResult;
pub use thread::{FIRST_WORKER_TID, GuestThread, PRIMARY_THREAD_ID, ThreadState};

// `state` is crate-private now (ADR-0006): its types reach other crates only
// through `port`. `pub` is still required on them for those re-exports, so
// anything `port` does NOT publish is `pub`-but-unreachable — which is exactly
// what this lint reports, and exactly the intended shape. Scoped `expect` (not
// `allow`) so it has to keep being true: if a later edit makes one of them
// reachable again, this fails to compile and the surface gets re-reviewed.
#[expect(
    unreachable_pub,
    reason = "`state` is crate-private; `port` is the published surface"
)]
mod state;
// The published surface (ADR-0006). Cross-crate consumers name `port::…`;
// the two re-export blocks below are the legacy flat surface, kept only for
// the names `wie-cli` still uses (`./scripts/check.sh --only winapi-port`
// keeps `wie-runtime` off both).
pub mod port;
// `wie-cli`'s remaining root-level needs. Everything else from `state` is
// crate-visible only; see `port`.
pub use state::{
    DisplayMetrics, FileDialogPick, FileDialogPolicy, FileDialogRequest, FontDialogPolicy,
    MessageQueueIdlePolicy, PageSetupDialogPick, PageSetupDialogPolicy, PageSetupDialogRequest,
    PrintDialogPick, PrintDialogPolicy, PrintDialogRequest, PrintJobRequest,
};
// Crate-internal shorthand (`crate::WinApiState`, …). Not part of the published
// surface — `pub(crate)`, so a cross-crate name here fails to resolve. Only the
// names this crate's own modules reach through the root are listed; everything
// else `state` defines stays inside `state`.
pub(crate) use state::{
    DllId, FileHandle, FindFileHandle, FindHandle, FlsSlot, GetProcAddressCacheEntry,
    GlobalAtomRecord, GuestCallbackRequest, GuestStdinMode, HandlerContext, HostFileMount,
    HostParkReason, ModuleHandle, OpenGuestFile, OuterReturn, QueuedWindowMessage, RegistryKey,
    RegistryKeyHandle, ResourceHandle, ResourceRecord, TimerRecord, VirtualGuestFile,
    WinApiControlSignal, WinApiEnvironment, WinApiState, WindowClassRecord, WindowRecord,
    WindowsHookRecord, pthread_return_trampoline_va,
};
// Test-only: the shared fixtures below build a whole `WinApiState` by struct
// literal, so the in-crate `#[cfg(test)]` blocks name these. Test builds only —
// nothing here ships, and nothing outside the crate can see it.
#[cfg(test)]
pub(crate) use state::{
    DEFAULT_ENVIRONMENT, DllStateMap, FileDialogBridge, FileIoState, GuestIoRuntimeConfig,
    HeapState, KernelState, ModuleState, PageSetupDialogBridge, PrintDialogBridge, PrintJobBridge,
    ProcessState,
};

mod dispatch_table;
// `wie-cli` calls this one directly; `crate::port` publishes the rest.
pub use dispatch_table::is_winapi_implemented;
pub(crate) use dispatch_table::{WinApiId, resolve_winapi_id};
#[cfg(test)]
pub(crate) use dispatch_table::{dispatch_winapi, dispatch_winapi_id, is_winapi_library};
