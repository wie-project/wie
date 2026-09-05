//! Wave 2 slice 2 D3D9 command-capture test: gui_d3d9 under the capture
//! pipeline must produce the byte-identical D3D9 frame the legacy in-handler
//! raster path produces (`D3D9_RESTING_FRAME_HASH`), while the rasterization
//! runs on the capture render thread from the recorded op stream instead of
//! inside the `Draw*`/`Clear`/`Present` handlers under the big `WinApiState`
//! lock.

use crate::helpers::{D3D9_RESTING_FRAME_HASH, frame_hash, gui_suite_serialize, micro_exe};
use wie_runtime::EntryTraceTermination;

/// Run gui_d3d9 with the capture render thread enabled and assert the
/// published frame is byte-identical to the legacy path's.
///
/// The capture path never runs in the other micro-GUI tests (headless keeps
/// the legacy in-handler raster; the GUI host only spawns the streamer under
/// `WIE_CAPTURE_STREAM=1`), so this test is the hash-equivalence evidence for
/// the capture replay: same exe, same session flow, frames delivered through
/// `PresentChannel::capture` → capture render thread → channel.
#[test]
fn gui_d3d9_capture_stream_frame_matches_legacy_hash() {
    let Some(path) = micro_exe("gui_d3d9.exe") else {
        eprintln!("skip: micro-exes/out/gui_d3d9.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("GUI session starts");
    session
        .set_guest_env("WIE_SELFTEST", "1")
        .expect("inject WIE_SELFTEST");

    // Enable the capture pipeline with a no-op wake (headless: no event
    // loop to notify). The handle stops + joins the render thread on drop.
    let _streamer = session
        .guest_handle()
        .enable_capture_stream(Box::new(|| {}))
        .expect("capture streamer spawns");

    let mut iterations = 0;
    let mut saw_d3d9_frame = false;
    let exit_code = loop {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("GUI session run_until_stop");
        iterations += 1;
        assert!(
            iterations < 300,
            "gui_d3d9.exe did not exit within 300 iterations"
        );

        // The capture thread publishes asynchronously: poll the channel on a
        // deadline, checking the latest frame each round (the flag latches).
        // The commit test's deadline shape: generous after ExitProcess,
        // because the last flushed stream still has to clear the replay.
        let exited = matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        );
        if let Some(owner) = session.first_guest_window_handle() {
            let deadline = std::time::Instant::now()
                + if exited {
                    std::time::Duration::from_secs(5)
                } else {
                    std::time::Duration::from_millis(100)
                };
            while !saw_d3d9_frame && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
                let Some(frame) = session.take_frame(owner) else {
                    continue;
                };
                if frame.width >= 320
                    && frame.height >= 240
                    && frame_hash(&frame, 0, frame.height) == D3D9_RESTING_FRAME_HASH
                {
                    saw_d3d9_frame = true;
                }
            }
        }

        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => break Some(code),
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => {
                panic!("GUI session stopped unexpectedly: {other:?}");
            }
        }
    };

    // The handle stays alive to the end of the test (Drop stops the thread);
    // the explicit drop is redundant but documents the intent.
    drop(_streamer);

    assert_eq!(
        exit_code,
        Some(0),
        "gui_d3d9.exe must exit 0 under capture mode (same D3D9 call sequence)"
    );
    assert!(
        saw_d3d9_frame,
        "the capture-thread-published frame never matched D3D9_RESTING_FRAME_HASH \
         (capture replay diverged from the legacy in-handler raster path)"
    );
}
