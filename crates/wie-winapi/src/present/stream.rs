//! The Wave 2 slice 2 capture render thread: consumes the D3D9 command
//! stream flushed at each `Present` (`d3d9/capture.rs`), replays the ops
//! into its OWN backbuffer + render-target/depth copies, and publishes the
//! frame through the channel exactly like the commit path
//! ([`super::commit`]) does.
//!
//! Ownership model: the emu thread never rasterizes on this path — handlers
//! append self-contained ops to the per-device stream and `Present` moves
//! the stream into a [`CaptureFlush`] (a `Vec` move, no pixel copies; the
//! resource inputs are `Arc` refcount bumps). The render thread owns the
//! implicit backbuffer outright (the guest has no object for it) and private
//! copies of every referenced render target / depth buffer, kept consistent
//! through the flush-input / handback protocol documented in
//! `d3d9/capture.rs`.
//!
//! Gate: off by default. The GUI host opts in via
//! `GuestHandle::enable_capture_stream` (which spawns this thread and flips
//! `capture_enabled`); headless runs and the micro-suite never spawn it, so
//! the CI hashes exercise the unchanged legacy in-handler raster path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::SurfaceFrame;
use super::commit::blit_frame_into;
use crate::d3d9::capture::{CaptureFlush, CaptureHandback, ReplayTargets};

/// The capture pipeline on the [`super::PresentChannel`]: the latest-wins
/// flush slot, the handback slot (render thread → emu thread), and counters.
pub(crate) struct CapturePipeline {
    /// Capture mode gate — handlers record + flush only when set. Flipped
    /// AFTER the render thread spawns so a flush always has a consumer.
    enabled: AtomicBool,
    /// Render-thread shutdown flag (checked alongside every condvar wait).
    stopped: AtomicBool,
    /// Latest-wins flush slot: `None` when the render thread has drained it.
    slot: std::sync::Mutex<Option<CaptureFlush>>,
    /// Wakes the render thread for a new flush or a stop request.
    signal: std::sync::Condvar,
    /// The most recent handback (`None` once the emu thread installs it).
    handback: std::sync::Mutex<Option<CaptureHandback>>,
    /// Wakes the emu thread when a handback is posted (the render-target
    /// LockRect rendezvous waits on this).
    handback_signal: std::sync::Condvar,
    /// Seq of the most recently posted handback (the flush that produced it).
    handback_seq: AtomicU64,
    /// Monotonic flush sequence number (assigned at enqueue).
    next_seq: AtomicU64,
    /// Replayed (stretched + published) frame count.
    frames: AtomicU64,
    /// Accumulated replay+publish wall time (ns, saturating).
    ns: AtomicU64,
    /// Most recent replay+publish wall time (ns).
    ns_last: AtomicU64,
}

impl CapturePipeline {
    pub(crate) const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            slot: std::sync::Mutex::new(None),
            signal: std::sync::Condvar::new(),
            handback: std::sync::Mutex::new(None),
            handback_signal: std::sync::Condvar::new(),
            handback_seq: AtomicU64::new(0),
            next_seq: AtomicU64::new(1),
            frames: AtomicU64::new(0),
            ns: AtomicU64::new(0),
            ns_last: AtomicU64::new(0),
        }
    }

    /// Enable capture mode (call only after the render thread spawned).
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// Whether the D3D9 handlers should record ops + flush instead of
    /// rasterizing inline.
    #[must_use]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Enqueue a flush (latest-wins: a superseded flush is dropped whole —
    /// its ops were never applied). Returns the flush's sequence number; the
    /// handback produced by replaying it carries the same number (the
    /// rendezvous wait's barrier).
    pub(crate) fn enqueue(&self, mut flush: CaptureFlush) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        flush.seq = seq;
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(flush);
        // Wake AFTER the slot is filled (the same ordering argument the
        // commit slot makes: a woken consumer takes whatever is latest).
        self.signal.notify_one();
        seq
    }

    /// Install the render thread's handback (post-frame target buffers) for
    /// the emu thread to pick up. Replaces any not-yet-installed handback —
    /// the newest replay state wins.
    pub(crate) fn post_handback(&self, handback: CaptureHandback, seq: u64) {
        self.handback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(handback);
        self.handback_seq.store(seq, Ordering::Release);
        self.handback_signal.notify_all();
    }

    /// Block until a handback for flush `seq` (or newer) is posted, the
    /// pipeline is stopped, or `timeout` elapses. Returns whether the
    /// handback barrier was reached — on a timeout the caller still drains
    /// whatever is posted (at shutdown that loses nothing: the guest is
    /// tearing down anyway).
    pub(crate) fn wait_for_handback(&self, seq: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut handback = self
            .handback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if self.handback_seq.load(Ordering::Acquire) >= seq {
                return true;
            }
            if self.stopped.load(Ordering::Acquire) {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return self.handback_seq.load(Ordering::Acquire) >= seq;
            }
            let (guard, _timed_out) = self
                .handback_signal
                .wait_timeout(handback, deadline.saturating_duration_since(now))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handback = guard;
        }
    }

    /// Take a pending handback, if any (emu thread, under the big lock).
    pub(crate) fn take_handback(&self) -> Option<CaptureHandback> {
        self.handback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Block until a flush arrives or the thread is stopped. `None` = stop.
    fn wait_flush(&self) -> Option<CaptureFlush> {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return None;
            }
            if let Some(flush) = slot.take() {
                return Some(flush);
            }
            // Timed wait so a stop flip without a notify cannot hang forever.
            let (guard, _timed_out) = self
                .signal
                .wait_timeout(slot, Duration::from_millis(100))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot = guard;
        }
    }

    /// Stop the render thread: wakes it and makes every future `wait_flush`
    /// return `None`. Any still-queued flush is dropped (a shutdown frame,
    /// not steady state).
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.signal.notify_all();
    }

    /// Record one replayed frame's wall time (ns) under the frame-timing
    /// gate.
    fn record_frame(&self, ns: u64) {
        if !super::frame_timing_enabled() {
            return;
        }
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.ns.store(
            self.ns.load(Ordering::Relaxed).saturating_add(ns),
            Ordering::Relaxed,
        );
        self.ns_last.store(ns, Ordering::Relaxed);
    }

    /// Replayed frame count for the profile dump.
    #[must_use]
    pub(crate) fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Accumulated replay+publish wall time (ns) for the profile dump.
    #[must_use]
    pub(crate) fn ns(&self) -> u64 {
        self.ns.load(Ordering::Relaxed)
    }

    /// Most recent replay+publish wall time (ns) for the profile dump.
    #[must_use]
    pub(crate) fn ns_last(&self) -> u64 {
        self.ns_last.load(Ordering::Relaxed)
    }
}

/// The join handle for the spawned capture render thread. Dropping it stops
/// the thread (one-frame latency at most) and joins it, so a test or a guest
/// teardown never leaks the thread (the `CommitterHandle` pattern).
pub struct CaptureStreamerHandle {
    channel: Arc<super::PresentChannel>,
    join: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for CaptureStreamerHandle {
    fn drop(&mut self) {
        self.channel.capture.stop();
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

/// Spawn the capture render thread for `channel`.
///
/// `wake` is the host presenter's frame-arrival callback (the same closure
/// shape the commit thread fires). Capture mode is NOT enabled here — the
/// caller flips `set_capture_enabled` (via
/// `GuestHandle::enable_capture_stream`) after spawning, so an enqueued
/// flush always has a live consumer. `None` when the thread cannot spawn
/// (the legacy in-handler raster path stays active).
#[must_use]
pub fn spawn_capture_streamer(
    channel: Arc<super::PresentChannel>,
    wake: Box<dyn Fn() + Send>,
) -> Option<CaptureStreamerHandle> {
    let worker_channel = Arc::clone(&channel);
    let join = std::thread::Builder::new()
        .name("wie-d3d9-capture".into())
        .spawn(move || capture_thread(worker_channel, wake))
        .ok()?;
    Some(CaptureStreamerHandle {
        channel,
        join: std::sync::Mutex::new(Some(join)),
    })
}

/// The capture render-thread loop: absorb the flush's resource inputs, replay
/// the op stream into the private targets, stretch + publish the backbuffer,
/// hand the touched targets back.
fn capture_thread(channel: Arc<super::PresentChannel>, wake: Box<dyn Fn() + Send>) {
    let mut targets = ReplayTargets::default();
    // Next publish buffer per hwnd: a recycled (displaced) frame buffer, so
    // the steady-state loop is alloc-free (the commit thread's pattern).
    let mut next_buffer: ahash::HashMap<crate::handles::Hwnd, Vec<u32>> = {
        use ahash::HashMapExt;
        ahash::HashMap::new()
    };
    while let Some(flush) = channel.capture.wait_flush() {
        let flush_seq = flush.seq;
        targets.apply_flush(&flush);
        targets.replay(&flush.ops);
        let handback = targets.take_handback();
        // A zero-sized window (minimized at flush time) has nothing to
        // publish — post the handback and drop the frame.
        if flush.win_w == 0 || flush.win_h == 0 {
            channel.capture.post_handback(handback, flush_seq);
            continue;
        }
        let t0 = super::frame_timing_enabled().then(Instant::now);
        let stride = super::padded_stride(flush.win_w);
        let dims = (
            usize::try_from(stride).unwrap_or(0),
            usize::try_from(flush.win_h).unwrap_or(0),
        );
        let needed = dims.0.saturating_mul(dims.1);
        // Take this window's recycled buffer, sized for the current dims.
        let mut buffer = next_buffer.remove(&flush.hwnd).unwrap_or_default();
        if buffer.len() != needed {
            buffer = vec![0_u32; needed];
        }
        blit_frame_into(
            &mut buffer,
            stride,
            flush.win_w,
            flush.win_h,
            targets_backbuffer(&targets),
            flush.bb_w,
            flush.bb_h,
        );
        let frame = SurfaceFrame {
            width: flush.win_w,
            stride,
            height: flush.win_h,
            pixels: Arc::new(buffer),
            background_color: flush.background,
            // A Present replaces the whole surface — full-frame upload.
            region: None,
        };
        let (displaced, should_wake) = channel.publish_frame(flush.hwnd, frame);
        // The displaced channel slot's buffer is the next frame's canvas for
        // this window (zero-copy recycling).
        if let Some(old) = displaced
            && let Ok(vec) = Arc::try_unwrap(old.pixels)
        {
            next_buffer.insert(flush.hwnd, vec);
        }
        if let Some(t0) = t0 {
            channel
                .capture
                .record_frame(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
        channel.capture.post_handback(handback, flush_seq);
        if should_wake {
            wake();
        }
    }
}

/// Read-only view of the replay targets' backbuffer (the publish source).
fn targets_backbuffer(targets: &ReplayTargets) -> &[u32] {
    targets.backbuffer()
}
