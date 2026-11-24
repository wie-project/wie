//! WIE runtime: PE64 userspace execution, WinAPI, and tracing.
//!
//! CPU backends: see [`wie_cpu`] and `docs/WIE.md` (Cranelift JIT default; iced interpreter).

// Machine-checked gate for this crate's *production* code (CLAUDE.md "Lint
// policy", plan T1.2): no `unwrap`/`expect`/`panic`. This is the run loop: a
// panic here ends the guest session with no HRESULT and no unwind. Test builds
// are exempt on purpose. `indexing_slicing`/`as_conversions` are NOT gated — the
// 14 and 72 measured production sites are fixed-offset writes into guest-stub
// templates and bit-width arithmetic, where the checked spelling is longer
// without being safer.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

mod asm_utils;
mod guest_callback;
mod guest_heap_accel;
mod guest_io;
mod guest_mbwc;
mod guest_rewire;
mod guest_stubs;
mod gui_loop;
mod hooks;
mod knobs;
mod memory;
mod mt_runtime;
mod quantum;
mod session;
mod trace;

pub use gui_loop::{GuiControl, GuiOutcome, run_windowed};
// The host-facing surface (ADR-0006 decision 3): every `wie-winapi` name the
// CLI is allowed to say, plus the `GuestHandle` entry points for host work
// that used to call the emulator directly.
pub mod host;
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
pub use wie_winapi::port::{FileDialogPolicy, IdleContext, IdlePolicy};
