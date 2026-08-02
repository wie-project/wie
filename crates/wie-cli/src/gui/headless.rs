//! Headless GUI mode: runs the guest with `--screenshot`.
//!
//! Does not create a winit window.  Uses the headless driver
//! ([`run_windowed`]) and writes frames to a BMP.

use crate::bmp::write_bmp;
use anyhow::{Context, Result};
use std::path::Path;
use wie_runtime::RuntimeSession;
use wie_runtime::{GuiControl, GuiOutcome, run_windowed};

/// Run the guest headlessly and write a screenshot to `out_path`.
pub fn run_screenshot(path: &Path, out_path: &Path) -> Result<()> {
    let run_t0 = std::time::Instant::now();
    let mut session = RuntimeSession::new(path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)?;
    let handle = session.guest_handle();
    let control = GuiControl::new();
    control
        .wait_for_input
        .store(false, std::sync::atomic::Ordering::Relaxed);

    let outcome = run_windowed(&mut session, &control)?;

    // B9: finalize and print the runtime profile (publish / blit-copy frame
    // timing) so `WIE_RUNTIME_PROFILE=1 wie-cli run --screenshot …` reports
    // the new frame-time fields.
    if session.profile_enabled() {
        session.finalize_profile(run_t0.elapsed().as_nanos(), 0, 0);
        eprintln!("{}", session.profile().report());
    }

    // Try to capture a frame for screenshot.
    if let Some(hwnd) = handle.first_guest_window_handle() {
        if let Some(frame) = handle.take_frame(hwnd) {
            let w = frame.width;
            let h = frame.height;
            write_bmp(
                std::fs::File::create(out_path)
                    .with_context(|| format!("create {}", out_path.display()))?,
                w,
                h,
                &frame.pixels,
            )?;
            tracing::info!("wrote screenshot {} ({}x{})", out_path.display(), w, h);
        } else {
            tracing::warn!("no frame captured for screenshot");
        }
    } else {
        tracing::warn!("no window created");
    }

    match outcome {
        GuiOutcome::Exited(code) => {
            tracing::info!("guest exited with code {code}");
        }
        other => {
            tracing::info!("guest finished: {other:?}");
        }
    }

    Ok(())
}
