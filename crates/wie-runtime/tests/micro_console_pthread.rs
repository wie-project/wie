//! Integration tests for console, pthread, and CRT convergence micro-exes.
//!
//! Binaries from `make -C micro-exes`. A missing binary fails the test (see `tests/common/mod.rs`).

mod common;

use common::micro_exe;

fn run_expect_zero(name: &str) {
    let Some(path) = micro_exe(name) else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };
    let summary = wie_runtime::run_micro_exe(&path, 256).expect("run_micro_exe");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "{name}: termination={:?} backend={}",
        summary.run.termination,
        summary.cpu_backend
    );
}

/// Console cell API: FillConsoleOutputCharacter, SetConsoleCursorPosition,
/// WriteConsoleA on a non-stdout buffer.
#[test]
fn console_cells_exits_zero() {
    run_expect_zero("console_cells.exe");
}

/// srand/rand compatibility: verifies UCRT RNG constants match MSVC values.
///
/// srand(42) → first rand() = 0xAF (175), second = 0x190 (400),
/// third = 0x45CD (17869). Failure would indicate wrong UCRT RNG constants.
#[test]
fn rand_test_exits_zero() {
    run_expect_zero("rand_test.exe");
}

/// UCRT coverage: wide surface of CRT functions exercised end-to-end.
#[test]
fn ucrt_coverage_exits_zero() {
    run_expect_zero("ucrt_coverage.exe");
}

/// pthread basic: two threads increment a shared counter under a mutex,
/// then return their IDs as join values. Verifies pthread_create, mutex
/// lock/unlock, pthread_join, and the tagged-id object model.
#[test]
fn pt_basic_exits_zero() {
    run_expect_zero("pt_basic.exe");
}

/// pthread condition variable: one thread waits on a condvar while another
/// signals it. Verifies pthread_cond_wait, pthread_cond_signal, and the
/// condvar state machine across the park/re-entry boundary.
#[test]
fn pt_cond_exits_zero() {
    run_expect_zero("pt_cond.exe");
}
