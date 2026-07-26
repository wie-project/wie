//! N1 micro-suite: process ids + heap core (freestanding PE64).
//!
//! Binaries from `make -C micro-exes`. Skips if missing (no mingw).

use std::path::PathBuf;

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

fn run_expect_zero(name: &str) {
    let Some(path) = micro_exe(name) else {
        eprintln!("skip: micro-exes/out/{name} not built (run make -C micro-exes)");
        return;
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

#[test]
fn n1_process_ids_exits_zero() {
    run_expect_zero("process_ids.exe");
}

#[test]
fn n1_heap_alloc_exits_zero() {
    run_expect_zero("heap_alloc.exe");
}

#[test]
fn n1_heap_core_exits_zero() {
    run_expect_zero("heap_core.exe");
}

/// String-instruction coverage (MOVS/STOS/SCAS/CMPS + DF).
///
/// Previously built by the Makefile but never executed by any test, which is
/// how the inline-REP tail bug guarded by `rep_lengths` below went unnoticed.
#[test]
fn n1_cpu_string_exits_zero() {
    run_expect_zero("cpu_string.exe");
}

/// REP MOVS/STOS across every length in [1, 70].
///
/// Regression guard for the JIT inline-REP path, which accepted any length in
/// [16, 64] but only emitted full 16-byte SIMD chunks — silently dropping the
/// trailing `len & 15` bytes while still zeroing RCX and advancing RSI/RDI.
/// Verified to fail (exit 51) against the pre-fix lowering.
#[test]
fn n1_rep_lengths_exits_zero() {
    run_expect_zero("rep_lengths.exe");
}
