//! Per-thread last-error: host-side `GuestThread` slots, the process alias,
//! and publication into each engine's GS-relative TEB slot.
//!
//! Windows stores last-error in the per-thread TEB. WIE keeps a per-thread
//! slot on the host ([`GuestThread::last_error`]) and publishes the ACTIVE
//! thread's value into the ACTIVE engine's TEB page (`gs_base() +
//! TEB_LAST_ERROR_OFFSET`): the primary engine stays bound to the fixed
//! `GS_BASE`, each worker engine is bound to its own `PerThreadTeb` page.
//! These tests pin the isolation of the per-thread slots and the per-engine
//! publication of the helpers (`absorb_guest_last_error` /
//! `publish_last_error_to_guest`).

use super::*;
use wie_cpu::guest_layout::{TEB_LAST_ERROR_OFFSET, TEB_LAST_ERROR_VA};

/// The shared test engine only maps the low region; the primary TEB page at
/// `GS_BASE` (the last-error slot lives at `GS_BASE + 0x68`) needs an explicit
/// map for the publication helpers to reach it.
fn engine_with_teb() -> IcedCpu {
    let mut cpu = test_engine();
    cpu.mem_map(wie_cpu::GS_BASE, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map primary guest TEB page");
    cpu
}

/// An engine bound to a PRIVATE TEB page at `teb_va` — the exact binding a
/// worker gets at spawn (`PerThreadTeb` page + `CpuEngine::set_gs_base`).
fn engine_bound_to(teb_va: u64) -> IcedCpu {
    let mut cpu = test_engine();
    cpu.mem_map(teb_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map worker guest TEB page");
    cpu.set_gs_base(teb_va);
    cpu
}

/// Read the last-error slot of the TEB page THIS engine is bound to.
fn read_teb_last_error(cpu: &mut IcedCpu) -> u32 {
    let mut bytes = [0_u8; 4];
    cpu.mem_read(cpu.gs_base() + TEB_LAST_ERROR_OFFSET, &mut bytes)
        .expect("read engine TEB last-error slot");
    u32::from_le_bytes(bytes)
}

/// Host-side isolation of the per-thread slots: each `GuestThread` keeps its
/// own last-error value across activation switches, and the primary's resting
/// slot is never touched by worker writes.
#[test]
fn two_guest_threads_keep_independent_last_error_slots() {
    let mut state = default_winapi_state();
    let primary_tid = state.kernel.threads.current_tid();
    let worker_a = state.kernel.threads.alloc_worker();
    let worker_b = state.kernel.threads.alloc_worker();
    assert_ne!(worker_a, worker_b, "worker TIDs are distinct");

    // Simulate handler writes on the active thread, persisted across switches
    // exactly like the runtime does (activate → handle → save_active).
    state.kernel.threads.activate(worker_a);
    state.kernel.threads.active.last_error = 42;
    state.kernel.threads.save_active();

    state.kernel.threads.activate(worker_b);
    state.kernel.threads.active.last_error = 7;
    state.kernel.threads.save_active();

    // Each thread's slot survives the other's writes.
    state.kernel.threads.activate(worker_a);
    assert_eq!(
        state.kernel.threads.active.last_error, 42,
        "thread A keeps its own value after thread B wrote a different one"
    );
    state.kernel.threads.activate(worker_b);
    assert_eq!(
        state.kernel.threads.active.last_error, 7,
        "thread B keeps its own value after thread A wrote a different one"
    );

    // The primary's resting slot was never touched.
    state.kernel.threads.activate(primary_tid);
    assert_eq!(state.kernel.threads.active.last_error, 0);
}

/// Publication is per-engine: the primary's publish lands in the fixed
/// `GS_BASE` page, the worker's in ITS OWN page — the two threads never share
/// a guest cell, so one thread's handler write cannot clobber the other's
/// guest-visible value.
#[test]
fn publish_writes_each_threads_own_teb_page() {
    let mut primary_engine = engine_with_teb();
    assert_eq!(
        primary_engine.gs_base() + TEB_LAST_ERROR_OFFSET,
        TEB_LAST_ERROR_VA,
        "the primary engine's GS-relative slot is the fixed address"
    );
    let worker_teb_va = 0x0000_7000_0040_C000_u64;
    let mut worker_engine = engine_bound_to(worker_teb_va);
    let mut state = default_winapi_state();
    let primary_tid = state.kernel.threads.current_tid();
    let worker_tid = state.kernel.threads.alloc_worker();

    // Primary's handler sets its last error and the post-dispatch publish
    // reaches the primary's OWN TEB page.
    state.process.last_error = 5;
    state.publish_last_error_to_guest(&mut primary_engine);
    assert_eq!(read_teb_last_error(&mut primary_engine), 5);

    // Worker activates: the activation-boundary refresh + publish writes the
    // worker's own (initial) value into ITS page.
    state.kernel.threads.activate(worker_tid);
    state.process.last_error = state.kernel.threads.active.last_error;
    state.publish_last_error_to_guest(&mut worker_engine);
    assert_eq!(
        read_teb_last_error(&mut worker_engine),
        0,
        "a fresh worker starts at 0 in its own page"
    );

    // Worker's handler sets its own value and publishes to its own page.
    state.process.last_error = 9;
    state.publish_last_error_to_guest(&mut worker_engine);
    assert_eq!(read_teb_last_error(&mut worker_engine), 9);

    // The primary page still holds the primary's value: no cross-thread
    // clobbering through a shared mirror.
    assert_eq!(
        read_teb_last_error(&mut primary_engine),
        5,
        "the worker's publish never touches the primary's TEB page"
    );

    // Reactivating the primary restores its own value, and the worker's host
    // slot still holds 9 (isolation through the switch).
    state.kernel.threads.activate(primary_tid);
    assert_eq!(state.kernel.threads.active.last_error, 5);
    let worker_slot = state
        .kernel
        .threads
        .by_tid
        .get(&worker_tid)
        .expect("worker slot registered");
    assert_eq!(
        worker_slot.last_error, 9,
        "the worker's value is not clobbered by the primary's publish"
    );
}

/// The pre-dispatch absorb pulls a guest-stub `SetLastError` (a pure guest
/// store into the engine's GS-relative TEB slot, no host stop) into the ACTIVE
/// thread's slot and the `process.last_error` alias that host handlers read.
#[test]
fn absorb_reads_guest_stub_writes_into_the_active_slot() {
    let mut engine = engine_with_teb();
    let mut state = default_winapi_state();

    // The in-guest SetLastError stub writes the engine's GS-relative TEB slot.
    engine
        .mem_write(
            engine.gs_base() + TEB_LAST_ERROR_OFFSET,
            &77_u32.to_le_bytes(),
        )
        .expect("write engine TEB last-error slot");

    // The next host stop absorbs it into the active thread's slot and the
    // process alias handlers read.
    state.absorb_guest_last_error(&mut engine);
    assert_eq!(state.kernel.threads.active.last_error, 77);
    assert_eq!(state.process.last_error, 77);

    // The absorb is a read-only pull; the slot is unchanged.
    assert_eq!(read_teb_last_error(&mut engine), 77);
}

/// Full dispatch discipline (absorb → handler → publish) keeps the engine's
/// GS-relative TEB slot and the active thread's slot coherent for the real
/// `SetLastError` / `GetLastError` handlers — the same sequence the pump and
/// worker loops run.
#[test]
fn set_last_error_handler_publishes_for_the_active_thread() {
    let mut engine = engine_with_teb();
    let mut state = default_winapi_state();

    state.absorb_guest_last_error(&mut engine);
    write_regs(&mut engine, 456, 0, 0, 0, 0);
    kernel32::handle_set_last_error(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("SetLastError");
    state.publish_last_error_to_guest(&mut engine);
    assert_eq!(read_teb_last_error(&mut engine), 456);
    assert_eq!(state.kernel.threads.active.last_error, 456);

    // The engine TEB slot and the active slot now feed the GetLastError handler.
    let r = kernel32::handle_get_last_error(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetLastError");
    assert_eq!(r.return_value, 456);
}

/// Handler error propagation: a failing handler writes `process.last_error`;
/// the post-dispatch publish lands that error in the ACTIVE engine's
/// GS-relative TEB slot, so the in-guest GetLastError stub (and the next
/// GetLastError dispatch) observes it.
#[test]
fn handler_error_propagates_to_the_active_engine_teb() {
    let mut engine = engine_with_teb();
    let mut state = default_winapi_state();

    // FreeLibrary(NULL) fails with ERROR_INVALID_HANDLE.
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    kernel32::handle_free_library(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("FreeLibrary(NULL)");
    assert_eq!(state.process.last_error, 6); // ERROR_INVALID_HANDLE

    // The post-dispatch publish (the quantum loop's tail) writes the error
    // into the engine's own TEB page.
    state.publish_last_error_to_guest(&mut engine);
    assert_eq!(read_teb_last_error(&mut engine), 6);
    assert_eq!(state.kernel.threads.active.last_error, 6);

    // And the per-thread slot feeds the next GetLastError dispatch.
    let r = kernel32::handle_get_last_error(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetLastError");
    assert_eq!(r.return_value, 6);
}
