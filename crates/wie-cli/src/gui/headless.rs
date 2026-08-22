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
///
/// `bottle_root` / `drive_d_root` are the effective `--bottle` / `--root` /
/// `--drive-d` roots from the CLI (`None` = env fallback only), threaded like
/// the windowed entry so named bottles work in screenshot mode too.
/// `app_dir` is the explicit `--app-dir` (complete-folder staging), `None`
/// keeping the exe-only default. `guest_args` are the argv entries after the
/// module name, forwarded into the guest session bootstrap exactly like the
/// GUI/micro entries (e.g. Doom Retro's `-iwad freedoom1.wad`).
pub fn run_screenshot(
    path: &Path,
    out_path: &Path,
    bottle_root: Option<&Path>,
    drive_d_root: Option<&Path>,
    app_dir: Option<&Path>,
    guest_args: &[String],
) -> Result<()> {
    // FS policy: an exe outside the bottle runs from a drive_c copy first
    // (the guest identity's `C:\…` label then maps to a real bottle file,
    // and the staged folder is the process cwd). Only the exe is staged by
    // default; `--app-dir` names a complete folder instead.
    let volumes = crate::commands::resolve_volume_config(bottle_root, drive_d_root);
    let staged = crate::commands::stage_run_source(
        path,
        &volumes,
        crate::commands::StageMode::from_run_entry(app_dir),
    )?;
    let run_t0 = std::time::Instant::now();
    let mut session = RuntimeSession::new_with_options(
        &staged.run_path,
        wie_winapi::MessageQueueIdlePolicy::YieldOnIdle,
        wie_runtime::DEFAULT_LAYOUT,
        wie_runtime::SessionOptions {
            current_directory: staged.guest_current_directory,
            // Forward guest argv after the module name (same entries the
            // GUI/micro entries thread through) so headless apps that need
            // argv (e.g. Doom Retro's `-iwad`) can be driven by a screenshot.
            guest_args: guest_args.to_vec(),
            // The staged root must reach the session (same rationale as the
            // console/persistent entries): the session would otherwise fall
            // back to `WIE_ROOT` / the global bottle.
            bottle_root: bottle_root.map(std::path::Path::to_path_buf),
            ..wie_runtime::SessionOptions::default()
        },
    )?;
    let handle = session.guest_handle();
    let control = GuiControl::new();
    control
        .wait_for_input
        .store(false, std::sync::atomic::Ordering::Relaxed);

    let outcome = run_windowed(&mut session, &control)?;

    // Finalize and print the runtime profile (publish / blit-copy frame
    // timing) so `WIE_RUNTIME_PROFILE=1 wie-cli run --screenshot …` reports
    // the new frame-time fields.
    if session.profile_enabled() {
        session.finalize_profile(run_t0.elapsed().as_nanos(), 0, 0);
        tracing::error!("{}", session.profile().report());
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
