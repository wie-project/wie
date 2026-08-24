//! WIE runtime: PE64 userspace execution, WinAPI, and tracing.
//!
//! CPU backends: see [`wie_cpu`] and `docs/WIE.md` (Cranelift JIT default; iced interpreter).

mod asm_utils;
mod guest_callback;
mod guest_heap_accel;
mod guest_io;
mod guest_mbwc;
mod guest_rewire;
mod guest_stubs;
mod gui_loop;
mod hooks;
mod memory;
mod mt_runtime;
mod quantum;
mod session;
mod trace;

pub use gui_loop::{GuiControl, GuiOutcome, run_windowed};
pub use hooks::RuntimeFakeApiEntry;
pub use memory::{
    DEFAULT_LAYOUT, FAKE_API_BASE, FAKE_API_SIZE, PROCESS_HEAP_BASE, PROCESS_HEAP_HANDLE,
    PROCESS_HEAP_SIZE, RuntimeMemoryLayout,
};
pub use session::{GuestHandle, MenuNode, RuntimeProfile, RuntimeSession, SessionOptions};
pub use trace::{
    EntryTraceEvent, EntryTraceSummary, EntryTraceTermination, MicroRunOptions, MicroRunSummary,
    RuntimeRunSummary, entry_trace, run_micro_exe, run_micro_exe_with_options,
    run_micro_exe_with_root, run_persistent_until_yield, run_persistent_until_yield_with_options,
};
pub use wie_cpu::{CpuEngine, CpuError, IcedCpu, JitCpu, active_backend_name};
pub use wie_winapi::{FileDialogPolicy, IdleContext, IdlePolicy};
