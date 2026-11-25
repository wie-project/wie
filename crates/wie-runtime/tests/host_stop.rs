//! A host quit must end the session from **inside** the pump, promptly.
//!
//! Cmd+Q used to do nothing: the macOS app menu had no Quit item, so the key was
//! never bound and the process kept running until the guest quit on its own —
//! "sometimes it stops after a while". Binding the key is only half the fix; the
//! request has to unwind the run without waiting for the guest, because a GUI guest
//! may sit in a long compute stretch (or never return to its message loop) and the
//! GUI loop only regains control between quanta.
//!
//! These tests pin the mechanism the menu item drives: `request_host_stop` is
//! honoured at the next API-stop boundary even with no diagnostics knob armed, and
//! it reports `HostInterrupt` — the presenter's "the host stopped this, not the
//! guest" signal — rather than a guest exit code.

mod common;

use std::time::{Duration, Instant};

use common::micro_exe;
use wie_runtime::{GuiControl, GuiOutcome, run_windowed};

/// How long a stop may take to unwind the pump. The GUI loop's park cap is 50 ms,
/// so this covers one park plus the next pump iteration several times over; it is
/// deliberately loose because it asserts "bounded and prompt", not a benchmark.
const STOP_BOUND: Duration = Duration::from_secs(3);

/// A GUI session with a running guest, stopped by the host, returns
/// `HostInterrupt` within the bound.
///
/// The run happens on another thread and is collected through a channel so the
/// failure mode is an assertion: if the pump stops honouring the request, the guest
/// loops forever and this test fails on the timeout instead of hanging the suite.
#[test]
fn a_host_stop_ends_a_gui_session_promptly() {
    // A GUI guest with its own loop, so the stop has to reach a *running* session
    // rather than a guest that was about to exit anyway.
    let Some(path) = micro_exe("snake.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let runner = std::thread::spawn(move || {
        let built = wie_runtime::RuntimeSession::new(
            &path,
            wie_winapi::MessageQueueIdlePolicy::YieldOnIdle,
        );
        let Ok(mut session) = built else {
            drop(tx.send(Err("session build failed")));
            return;
        };
        // Interactive GUI shape: keep waiting for messages instead of returning
        // after the first paint, so this exercises the parked path a real run
        // sits in.
        let control = GuiControl::new();
        let outcome = run_windowed(&mut session, &control).map_err(|_| "run_windowed failed");
        drop(tx.send(outcome));
    });

    // Let the guest start and reach its loop, then ask the host to stop it.
    std::thread::sleep(Duration::from_millis(150));
    let t0 = Instant::now();
    wie_winapi::port::request_host_stop();

    let outcome = rx
        .recv_timeout(STOP_BOUND)
        .expect("a host stop must end the session within the bound, not run on");
    let elapsed = t0.elapsed();
    runner.join().expect("runner thread");

    let outcome = outcome.expect("the session must not fail");
    assert!(
        matches!(outcome, GuiOutcome::HostInterrupt),
        "a host stop must read as HostInterrupt, got {outcome:?}"
    );
    assert!(
        elapsed < STOP_BOUND,
        "the stop must land on the next pump boundary, took {elapsed:?}"
    );
}

/// The flag is one-shot and must not leak into the **next** run in the same
/// process: a stop requested and consumed by one session cannot make a later
/// session end before it runs.
#[test]
fn a_host_stop_does_not_leak_into_the_next_session() {
    let Some(path) = micro_exe("crt_hello.exe") else {
        return; // fixture absent: already reported (see tests/common/mod.rs)
    };

    wie_winapi::port::request_host_stop();
    // Consume it exactly as the pump would: a stop is taken, never a latch.
    assert!(
        wie_winapi::port::take_host_stop(),
        "the request is pending before it is taken"
    );
    assert!(
        !wie_winapi::port::take_host_stop(),
        "taking it clears it — a second take must not see the same request"
    );

    let summary = wie_runtime::run_micro_exe(&path, 256).expect("crt_hello run");
    assert_eq!(
        summary.exit_code,
        Some(0),
        "a consumed stop must not end an unrelated run: {:?}",
        summary.run.termination
    );
}
