//! WinAPI dispatcher model for WIE (generic PE64 userspace).

pub mod advapi32;
pub mod bottle;
pub mod clipboard;
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
mod guest_layout;
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
mod registry;
pub mod seh;
pub mod shell32;
pub mod sync_obj;
pub mod thread;
pub mod ucrt;
pub mod user32;
pub mod uxtheme;
pub mod version;
pub mod vfs;
pub mod winmm;
pub use bottle::{bottle_root_from_env, drive_d_from_env, guest_path_to_host};
pub use exception::{RuntimeFunction, lookup_function_entry};
pub use sync_obj::{
    CsWaitQueue, FileMappingObject, INFINITE, KernelHandle, KernelObject, MAXIMUM_WAIT_OBJECTS,
    MultiWaitRequest, PendingSpawn, STILL_ACTIVE, SemaphoreObject, SyncState, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT, WaitTarget, wait_multiple,
};
pub use vfs::{VolumeConfig, ensure_bottle_skeleton, host_path_to_guest};
#[cfg(test)]
mod exception_helpers;
#[cfg(test)]
mod exception_tests;
pub use fake_va::{
    ComMethod, D3d9Iface, Device9Method, Direct3D9Method, FAKE_API_BASE, FAKE_API_SIZE, FakeVa,
    IndexBuffer9Method, PixelShader9Method, SPECIAL_CALLBACK_RETURN, SPECIAL_SEH_CONTINUE,
    Surface9Method, Texture9Method, VertexBuffer9Method, VertexShader9Method,
    callback_return_trampoline_va, decode as decode_fake_va, encode_alias, encode_com,
    encode_export, encode_unresolved, seh_continue_trampoline_va,
};
pub use guest_heap::GuestHeap;
pub use idle::{IdleContext, IdlePolicy};
pub use kernel32::WinApiHandlerResult;
pub use thread::{FIRST_WORKER_TID, GuestThread, PRIMARY_THREAD_ID, ThreadState};

mod state;
pub use state::{
    ClipboardState, D3D9State, DEFAULT_ENVIRONMENT, DllId, DllStateMap, FileDialogBridge,
    FileDialogFilter, FileDialogPick, FileDialogPolicy, FileDialogRequest, FileHandle, FileIoState,
    FindFileHandle, FindHandle, FlsSlot, FontDialogPolicy, GetProcAddressCacheEntry,
    GlobalAtomRecord, GuestCallbackRequest, GuestIoRuntimeConfig, GuestStdinMode, HandlerContext,
    HeapAllocation, HeapState, HostFileMount, HostParkReason, ImportResolver, KernelState,
    KeyboardState, MessageQueueIdlePolicy, ModuleHandle, ModuleState, OpenGuestFile, OuterReturn,
    PTHREAD_RETURN_TRAMPOLINE_VA, PageSetupDialogBridge, PageSetupDialogPick,
    PageSetupDialogPolicy, PageSetupDialogRequest, PendingNativePrintJob, PrintDialogBridge,
    PrintDialogPick, PrintDialogPolicy, PrintDialogRequest, PrintJobBridge, PrintJobRequest,
    ProcessState, QueuedWindowMessage, RegistryKey, RegistryKeyHandle, ResourceHandle,
    ResourceRecord, TimerRecord, VirtualGuestFile, WinApiControlSignal, WinApiEnvironment,
    WinApiState, WindowClassRecord, WindowFlags, WindowRecord, WindowState, WindowsHookRecord,
    pthread_return_trampoline_va,
};

mod dispatch_table;
pub use dispatch_table::{
    WINAPI_ID_COUNT, WinApiId, WinApiTraits, dispatch_winapi, dispatch_winapi_id,
    is_winapi_implemented, resolve_winapi_id, winapi_id_export,
};
