//! SEH hardware-fault dispatch test: an unhandled fault returns the emulation error.
use super::*;

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
