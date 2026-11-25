//! A guest can report success after one of its workers was killed by an invalid
//! access; the runtime must not repeat that claim.
//!
//! `micro-exes/out/mt_worker_fault.exe` spawns a worker that stores to an unmapped
//! address, waits for it, and then calls `ExitProcess(0)` — so the guest's own
//! verdict is "fine" while a thread it created is dead. On `mt_contention 4 64 40`
//! this happened in 12 of 14 faulting runs; a suite that only reads the exit code
//! passed while workers died.
//!
//! These tests are the witness the behaviour never had. Delete
//! `mask_a_silent_worker_fault` in `wie-runtime/src/trace.rs` and they fail — which
//! is the only way to know the branch runs, as opposed to compiling.

mod common;

use common::micro_exe;
use wie_runtime::EntryTraceTermination;

/// The masked run: a dead worker turns `ExitProcess(0)` into a `RuntimeStop` that
/// names tid, rip, address, size and exception code.
#[test]
fn a_worker_that_died_silently_fails_the_run() {
    let Some(pe) = micro_exe("mt_worker_fault.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };

    let summary = wie_runtime::run_micro_exe(&pe, 256).expect("mt_worker_fault run");

    match &summary.run.termination {
        EntryTraceTermination::RuntimeStop(detail) => {
            assert!(
                detail.contains("guest worker died on an invalid guest memory access"),
                "the stop must say what happened, got: {detail}"
            );
            // The fields that make it actionable rather than a shrug.
            for field in ["tid=", "rip=", "addr=", "size=", "exc=0xc0000005"] {
                assert!(
                    detail.contains(field),
                    "the stop must carry {field}, got: {detail}"
                );
            }
            assert!(
                detail.contains("0x7e5700000"),
                "the stop must name the faulting address, got: {detail}"
            );
        }
        other => panic!("a dead worker must not read as success, got {other:?}"),
    }
    assert_eq!(
        summary.exit_code, None,
        "a masked run carries no exit code — the guest's 0 is exactly the claim being rejected"
    );
}

/// The other half of the rule: a guest that exits **non-zero** keeps its own code.
/// The fault must not mask the guest's verdict any more than the guest masks the
/// fault, and `mt_contention` with out-of-range arguments exits non-zero on
/// purpose.
#[test]
fn a_guest_that_fails_on_its_own_keeps_its_exit_code() {
    let Some(pe) = micro_exe("mt_contention.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };

    let summary = wie_runtime::run_micro_exe_with_options(
        &pe,
        256,
        wie_runtime::MicroRunOptions {
            guest_args: vec!["99".into(), "1".into(), "1".into()],
            ..wie_runtime::MicroRunOptions::default()
        },
    )
    .expect("mt_contention run");

    assert_eq!(
        summary.exit_code,
        Some(20),
        "usage error is the guest's own verdict: {:?}",
        summary.run.termination
    );
    assert!(
        matches!(
            summary.run.termination,
            EntryTraceTermination::ExitProcess { .. }
        ),
        "a clean-but-non-zero exit stays an exit, not a runtime stop: {:?}",
        summary.run.termination
    );
}

/// A clean run must stay clean: no fault recorded, so nothing is rewritten. This is
/// what keeps the rule from turning every passing micro into a `RuntimeStop`.
#[test]
fn a_clean_run_is_not_rewritten() {
    let Some(pe) = micro_exe("crt_hello.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };

    let summary = wie_runtime::run_micro_exe(&pe, 256).expect("crt_hello run");
    assert!(
        matches!(
            summary.run.termination,
            EntryTraceTermination::ExitProcess { code: 0 }
        ),
        "a run with no worker fault keeps its exit, got {:?}",
        summary.run.termination
    );
    assert_eq!(summary.exit_code, Some(0));
}
