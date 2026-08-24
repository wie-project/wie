//! Painpoint 1 integration tests: event-driven idle parking.
//!
//! Covers the wake contract at the session seam: a `PostMessage`-style
//! injection from another host thread reaches a thread parked on the primary
//! inbox within a bounded time, teardown delivers `Shutdown` promptly, and
//! the queue→hub wiring broadcasts on every push. The pure channel /
//! registry semantics (spurious tokens, one-vs-all selection, concurrent
//! stress) live in `wie-winapi::wake` and `sync_obj` unit tests.

use std::time::{Duration, Instant};

/// Locate a mingw-built micro PE under `micro-exes/out`. Skips when absent,
/// like every other fixture-consuming test in this crate.
fn micro_exe(name: &str) -> Option<std::path::PathBuf> {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

const WM_APP: u32 = 0x8000;
const WAKE_BOUND: Duration = Duration::from_secs(5);

/// A host-thread message post (the GuestHandle seam the winit presenter uses)
/// wakes a thread parked on the session's primary inbox within a bounded
/// time — no 25 ms sleep quantum, no poll tick.
#[test]
fn post_message_from_another_thread_wakes_primary_inbox_within_bound() {
    let Some(path) = micro_exe("crt_hello.exe") else {
        return; // fixture absent: skip
    };
    // The session stays ALIVE on this thread for the whole park: its Drop
    // broadcasts Wake::Shutdown, which would otherwise answer the park with
    // a teardown token instead of the message-post token under test.
    let session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::ExitOnIdle)
            .expect("session builds");
    let handle = session.guest_handle();
    let inbox = session.primary_inbox();
    inbox.drain(); // start the park with an empty channel

    let parker = inbox.clone();
    let parked = std::thread::spawn(move || parker.wait_bounded(None, WAKE_BOUND));

    // Give the parker a moment to block, then inject from THIS thread (the
    // producer side runs on another host thread relative to the parker).
    std::thread::sleep(Duration::from_millis(30));
    let t0 = Instant::now();
    handle.post_message(0x1001, WM_APP, 0xDEAD, 0xBEEF);
    assert!(parked.join().expect("parker thread"), "wake token received");
    assert!(
        t0.elapsed() < Duration::from_millis(2_000),
        "post → wake must be prompt, took {:?}",
        t0.elapsed()
    );
}

/// Session teardown (`Drop`) broadcasts `Shutdown`: an inbox parked across
/// the drop returns promptly instead of hanging until its cap.
#[test]
fn session_teardown_shuts_down_a_parked_inbox() {
    let Some(path) = micro_exe("crt_hello.exe") else {
        return; // fixture absent: skip
    };
    let session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::ExitOnIdle)
            .expect("session builds");
    let inbox = session.primary_inbox();

    let parker = inbox.clone();
    let parked = std::thread::spawn(move || parker.wait_bounded(None, Duration::from_secs(30)));
    std::thread::sleep(Duration::from_millis(30));

    let t0 = Instant::now();
    drop(session); // Drop impl broadcasts Wake::Shutdown
    assert!(parked.join().expect("parker thread"), "shutdown received");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "teardown wake must be prompt, took {:?}",
        t0.elapsed()
    );
}

/// Every queue push (handler-side `MessageQueue::push`, wired to the hub at
/// init; host-side injection through `GuestHandle`) broadcasts a token:
/// N pushes wake a co-parked reader N times even while it keeps re-parking —
/// duplicate/racing tokens degrade to spurious wakeups, never lost ones.
#[test]
fn concurrent_queue_pushes_wake_a_reparking_reader() {
    let Some(path) = micro_exe("crt_hello.exe") else {
        return; // fixture absent: skip
    };
    let session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::ExitOnIdle)
            .expect("session builds");
    let handle = session.guest_handle();
    let hub = {
        // Public seam check: primary_inbox is stable across calls (same
        // channel), which is what makes enter/exit idempotent.
        let first = session.primary_inbox();
        let second = session.primary_inbox();
        assert!(first.same_channel(&second));
        first
    };

    const PRODUCERS: usize = 4;
    const PER_PRODUCER: usize = 50;
    let received = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let reader_inbox = hub.clone();
    let reader_received = std::sync::Arc::clone(&received);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_stop = std::sync::Arc::clone(&stop);
    let reader = std::thread::spawn(move || {
        // Park-loop shape: drain → re-check → re-park. Extra tokens are
        // spurious wakeups by design; the invariant is none are LOST.
        while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
            reader_received.fetch_add(reader_inbox.drain(), std::sync::atomic::Ordering::Relaxed);
            if reader_inbox.wait_bounded(None, Duration::from_millis(1)) {
                reader_received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        reader_received.fetch_add(reader_inbox.drain(), std::sync::atomic::Ordering::Relaxed);
    });

    let mut producers = Vec::new();
    for p in 0..PRODUCERS {
        let producer_handle = handle.clone();
        producers.push(std::thread::spawn(move || {
            for i in 0..PER_PRODUCER {
                let index = i as u64;
                producer_handle.post_message(
                    0x2000 + u64::try_from(p).unwrap_or(0),
                    WM_APP,
                    index,
                    0,
                );
            }
        }));
    }
    for producer in producers {
        producer.join().expect("producer");
    }

    let total = PRODUCERS * PER_PRODUCER;
    let deadline = Instant::now() + Duration::from_secs(10);
    while received.load(std::sync::atomic::Ordering::Relaxed) < total && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    handle.post_message(0x1, WM_APP, 0, 0); // unblock the final wait
    reader.join().expect("reader");
    assert!(
        received.load(std::sync::atomic::Ordering::Relaxed) >= total,
        "every posted token must reach the parked reader ({} observed)",
        received.load(std::sync::atomic::Ordering::Relaxed)
    );
}
