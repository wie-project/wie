//! The D3D9 Present-commit render thread (implementation-plan Wave 2, review
//! Option A1 skeleton).
//!
//! A D3D9 `Present` on the emu thread used to stretch the backbuffer into the
//! pooled window surface and publish it while holding the big `WinApiState`
//! lock (the `blit_frame` tail of [`super::PresentState`]). With the commit
//! thread enabled, the handler instead swaps the finished backbuffer into a
//! latest-wins commit slot (a pointer move — no pixel copy) and hands the emu
//! thread a recycled buffer; a dedicated host thread drains the slot, performs
//! the stretch + publish, and recycles buffers through the channel's spare
//! pool. The emu thread's per-Present work drops to two map operations, and
//! the raster/stretch wall time competes for cores instead of stealing
//! guest-quantum time.
//!
//! Buffer ownership: the commit slot owns the finished backbuffer (`Arc`);
//! the emu thread draws the NEXT frame into a distinct recycled buffer, so
//! there is no aliasing. The committer reclaims the displaced channel frame
//! (or the commit slot's previous backbuffer when the committer lags) as the
//! next backbuffer — steady state is allocation-free and copy-free beyond the
//! stretch itself.
//!
//! Semantics: the committed frame is a full-surface replacement (`region`
//! `None`, background color recorded at commit time) — exactly what the
//! legacy `blit_frame` + `publish` path emitted for a pure-D3D9 window. A
//! window that mixes GDI paints with D3D9 Presents has two producers for its
//! surface (GDI through `PresentState`, Present through the committer);
//! latest-wins applies and GDI content between Presents is overwritten by the
//! next commit — the documented limitation of this slice.
//!
//! Gate: off by default everywhere. The GUI host enables it per session via
//! `GuestHandle::enable_present_commit` (which spawns this thread);
//! `WIE_PRESENT_COMMIT=0` keeps the legacy in-handler path. Headless runs and
//! the micro-suite never enable it, so the CI frame hashes exercise the
//! legacy path unchanged.

use ahash::HashMapExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{SurfaceFrame, frame_timing_enabled, padded_stride};
use crate::handles::Hwnd;

/// One committed Present: the finished backbuffer plus the metadata the
/// committer needs to stretch + publish it without touching `WinApiState`.
pub(crate) struct CommitJob {
    /// Device window that receives the frame.
    pub hwnd: Hwnd,
    /// The finished backbuffer (0RGB, top-down, `bb_w * bb_h` words).
    pub pixels: Arc<Vec<u32>>,
    /// Backbuffer dimensions (guest render resolution).
    pub bb_w: u32,
    pub bb_h: u32,
    /// Device-window client size at commit time (the surface dims).
    pub win_w: u32,
    pub win_h: u32,
    /// The window's recorded class-brush background color (0RGB).
    pub background: u32,
}

/// The commit pipeline living on the [`super::PresentChannel`]: the
/// latest-wins job slot, the backbuffer spare pool, and the commit counters.
pub(crate) struct PresentCommit {
    /// Commit mode gate — the D3D9 Present handler enqueues only when set.
    /// Flipped AFTER the committer thread spawns so no job is ever enqueued
    /// without a consumer.
    enabled: AtomicBool,
    /// Committer shutdown flag (checked alongside every condvar wait).
    stopped: AtomicBool,
    /// Latest-wins job slot: `None` when the committer has drained it.
    slot: std::sync::Mutex<Option<CommitJob>>,
    /// Wakes the committer for a new job or a stop request.
    signal: std::sync::Condvar,
    /// Recycled backbuffers (returned by the committer / displaced slots),
    /// handed back to the emu thread so the next frame draws without an alloc.
    bb_spares: std::sync::Mutex<Vec<Vec<u32>>>,
    /// Committed (stretched + published) frame count.
    commit_frames: AtomicU64,
    /// Accumulated committer stretch+publish wall time (ns, saturating).
    commit_ns: AtomicU64,
    /// Most recent committer stretch+publish wall time (ns).
    commit_ns_last: AtomicU64,
}

impl PresentCommit {
    pub(crate) const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            slot: std::sync::Mutex::new(None),
            signal: std::sync::Condvar::new(),
            bb_spares: std::sync::Mutex::new(Vec::new()),
            commit_frames: AtomicU64::new(0),
            commit_ns: AtomicU64::new(0),
            commit_ns_last: AtomicU64::new(0),
        }
    }

    /// Enable commit mode (call only after the committer thread spawned).
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// Whether the D3D9 Present handler should enqueue instead of publishing
    /// inline.
    #[must_use]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Enqueue a committed Present. Returns the backbuffer the emu thread
    /// should draw the next frame into (a recycled spare, the displaced
    /// slot's buffer, or an empty Vec when neither exists — the caller sizes
    /// an empty return).
    pub(crate) fn enqueue(&self, job: CommitJob) -> Vec<u32> {
        let displaced = {
            let mut slot = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.replace(job)
        };
        // Wake the committer AFTER the slot is filled: a woken committer takes
        // whatever is latest (latest-wins by design), and a not-yet-arrived
        // job is picked up by its next timeout re-check.
        self.signal.notify_one();
        // A displaced (already superseded) backbuffer goes straight back into
        // the spare pool — the committer never saw it.
        if let Some(old) = displaced
            && let Ok(vec) = Arc::try_unwrap(old.pixels)
        {
            self.bb_spares
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(vec);
        }
        self.take_backbuffer_spare()
    }

    /// Pop a recycled backbuffer, if any.
    fn take_backbuffer_spare(&self) -> Vec<u32> {
        self.bb_spares
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .unwrap_or_default()
    }

    /// Hand a consumed backbuffer back to the spare pool (committer side).
    pub(crate) fn return_backbuffer(&self, buffer: Vec<u32>) {
        self.bb_spares
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(buffer);
    }

    /// Block until a job arrives or the committer is stopped. `None` = stop.
    fn wait_job(&self) -> Option<CommitJob> {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return None;
            }
            if let Some(job) = slot.take() {
                return Some(job);
            }
            // Timed wait so a stop flip without a notify cannot hang forever.
            let (guard, _timed_out) = self
                .signal
                .wait_timeout(slot, Duration::from_millis(100))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot = guard;
        }
    }

    /// Stop the committer: wakes it and makes every future `wait_job` return
    /// `None`. Any still-queued job is dropped (its backbuffer is lost — a
    /// shutdown frame, not steady state).
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.signal.notify_all();
    }

    /// Record one committed frame's stretch+publish wall time (ns,
    /// saturating) under the frame-timing gate.
    fn record_commit(&self, ns: u64) {
        if !frame_timing_enabled() {
            return;
        }
        self.commit_frames.fetch_add(1, Ordering::Relaxed);
        self.commit_ns.store(
            self.commit_ns.load(Ordering::Relaxed).saturating_add(ns),
            Ordering::Relaxed,
        );
        self.commit_ns_last.store(ns, Ordering::Relaxed);
    }

    /// Accumulated committer wall time (ns) for the profile dump.
    #[must_use]
    pub(crate) fn commit_ns(&self) -> u64 {
        self.commit_ns.load(Ordering::Relaxed)
    }

    /// Most recent committer wall time (ns) for the profile dump.
    #[must_use]
    pub(crate) fn commit_ns_last(&self) -> u64 {
        self.commit_ns_last.load(Ordering::Relaxed)
    }

    /// Committed frame count for the profile dump.
    #[must_use]
    pub(crate) fn commit_frames(&self) -> u64 {
        self.commit_frames.load(Ordering::Relaxed)
    }
}

/// The join handle for the spawned commit thread. Dropping it stops the
/// committer (one-frame latency at most) and joins it, so a test or a guest
/// thread teardown never leaks the thread.
pub struct CommitterHandle {
    channel: Arc<super::PresentChannel>,
    join: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for CommitterHandle {
    fn drop(&mut self) {
        self.channel.commit.stop();
        if let Some(join) = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _joined = join.join();
        }
    }
}

/// Spawn the Present-commit render thread for `channel`.
///
/// `wake` is the host presenter's frame-arrival callback (the same closure
/// shape the guest publish path fires): the committer invokes it after a
/// publish that passes the channel's wake gate, so the winit event loop
/// redraws. Commit mode is NOT enabled here — the caller flips
/// `set_commit_enabled` (via `GuestHandle::enable_present_commit`) after
/// spawning, so an enqueued job always has a live consumer.
/// `None` when the thread cannot spawn (the caller must then keep the legacy
/// inline present path — commit mode is only enabled on `Some`).
#[must_use]
pub fn spawn_present_committer(
    channel: Arc<super::PresentChannel>,
    wake: Box<dyn Fn() + Send>,
) -> Option<CommitterHandle> {
    let worker_channel = Arc::clone(&channel);
    let join = std::thread::Builder::new()
        .name("wie-present-commit".into())
        .spawn(move || commit_thread(worker_channel, wake))
        .ok()?;
    Some(CommitterHandle {
        channel,
        join: std::sync::Mutex::new(Some(join)),
    })
}

/// The committer loop: take a committed backbuffer, stretch it into a
/// window-sized buffer, publish through the channel, recycle buffers.
fn commit_thread(channel: Arc<super::PresentChannel>, wake: Box<dyn Fn() + Send>) {
    // Next publish buffer per hwnd: a recycled (displaced) frame buffer, so
    // the steady-state loop is alloc-free (D3D9 Presents fully rewrite the
    // surface, so no accumulation state is needed).
    let mut next_buffer: ahash::HashMap<Hwnd, Vec<u32>> = ahash::HashMap::new();
    while let Some(job) = channel.commit.wait_job() {
        // A zero-sized window (minimized at commit time) has nothing to
        // publish — recycle the backbuffer and drop the frame.
        if job.win_w == 0 || job.win_h == 0 {
            if let Ok(vec) = Arc::try_unwrap(job.pixels) {
                channel.commit.return_backbuffer(vec);
            }
            continue;
        }
        let t0 = frame_timing_enabled().then(Instant::now);
        let stride = padded_stride(job.win_w);
        let dims = (
            usize::try_from(stride).unwrap_or(0),
            usize::try_from(job.win_h).unwrap_or(0),
        );
        let needed = dims.0.saturating_mul(dims.1);
        // Take this window's recycled buffer, sized for the current dims.
        let mut buffer = next_buffer.remove(&job.hwnd).unwrap_or_default();
        if buffer.len() != needed {
            buffer = vec![0_u32; needed];
        }
        blit_frame_into(
            &mut buffer,
            stride,
            job.win_w,
            job.win_h,
            &job.pixels,
            job.bb_w,
            job.bb_h,
        );
        let frame = SurfaceFrame {
            width: job.win_w,
            stride,
            height: job.win_h,
            pixels: Arc::new(buffer),
            background_color: job.background,
            // A Present replaces the whole surface — full-frame upload.
            region: None,
        };
        let (displaced, should_wake) = channel.publish_frame(job.hwnd, frame);
        // The displaced channel slot's buffer is the next frame's canvas for
        // this window (zero-copy recycling, the same reclaim the guest publish
        // path performs via `store_spare`).
        if let Some(old) = displaced
            && let Ok(vec) = Arc::try_unwrap(old.pixels)
        {
            next_buffer.insert(job.hwnd, vec);
        }
        if let Some(t0) = t0 {
            channel
                .commit
                .record_commit(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
        // The consumed commit backbuffer goes to the spare pool regardless of
        // whether a displaced slot arrived: the emu thread draws into IT next.
        if let Ok(vec) = Arc::try_unwrap(job.pixels) {
            channel.commit.return_backbuffer(vec);
        }
        if should_wake {
            wake();
        }
    }
}

/// Blit a whole 0RGB frame into a destination surface buffer — the shared
/// body of [`super::PresentState::blit_frame`] (the legacy in-handler path)
/// and the commit thread (the render-thread path), so both paths stay
/// byte-identical by construction.
///
/// A frame sized exactly like the destination copies row-major (one
/// `copy_from_slice` when the pitch matches, per-row otherwise — the surface
/// pitch may be 64-padded, the source frame is not); anything else is
/// nearest-neighbour stretched to the destination dimensions.
pub(crate) fn blit_frame_into(
    dst: &mut [u32],
    dst_stride: u32,
    dst_width: u32,
    dst_height: u32,
    frame: &[u32],
    frame_width: u32,
    frame_height: u32,
) {
    if frame_width == dst_width && frame_height == dst_height {
        if dst_stride == dst_width {
            let n = dst.len().min(frame.len());
            if let (Some(d), Some(s)) = (dst.get_mut(..n), frame.get(..n)) {
                d.copy_from_slice(s);
            }
        } else {
            // Pitched destination: copy each logical row at its stride.
            let stride = usize::try_from(dst_stride).unwrap_or(0);
            let width = usize::try_from(dst_width).unwrap_or(0);
            let height = usize::try_from(dst_height).unwrap_or(0);
            for row in 0..height {
                let src_start = row.saturating_mul(width);
                let dst_start = row.saturating_mul(stride);
                let (Some(src), Some(d)) = (
                    frame.get(src_start..src_start.saturating_add(width)),
                    dst.get_mut(dst_start..dst_start.saturating_add(width)),
                ) else {
                    break;
                };
                d.copy_from_slice(src);
            }
        }
    } else {
        wie_cpu::stretch_nearest_strided(
            dst,
            dst_stride,
            frame,
            frame_width,
            frame_height,
            dst_width,
            dst_height,
        );
    }
}
