//! Present surface infrastructure for GDI blit pipeline.
//!
//! Manages per-window compositing surfaces and frame publishing so
//! CreateDIBSection → SelectObject → BitBlt actually renders pixels.

use ahash::HashMapExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Global gate for B9 frame-timing instrumentation (publish / blit-copy /
/// host-present wall times). Set by `RuntimeSession` when `WIE_RUNTIME_PROFILE`
/// is on (or via `enable_frame_timing`). A relaxed atomic load per frame is the
/// only cost when disabled.
static FRAME_TIMING: AtomicBool = AtomicBool::new(false);

/// Enable/disable per-frame timing instrumentation.
pub fn set_frame_timing_enabled(enabled: bool) {
    FRAME_TIMING.store(enabled, Ordering::Relaxed);
}

/// Whether per-frame timing instrumentation is currently active.
#[must_use]
pub fn frame_timing_enabled() -> bool {
    FRAME_TIMING.load(Ordering::Relaxed)
}

/// Cross-thread signal for message availability.
#[derive(Debug)]
pub struct MessageSignal {
    /// Set to `true` when a message is posted; the GUI loop uses this
    /// with the condvar to wake the guest when input arrives.
    pub triggered: Mutex<bool>,
    /// Condvar for wait‑based message notification.
    pub cvar: Condvar,
}

impl MessageSignal {
    #[must_use]
    pub fn new() -> Self {
        Self {
            triggered: Mutex::new(false),
            cvar: Condvar::new(),
        }
    }
}

impl Default for MessageSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Guest message queue, behind its own mutex.
///
/// Kept separate from `WinApiState` so the host (winit thread) can post
/// input messages without ever locking the big `WinApiState` mutex that the
/// guest thread holds during API-handler execution.  Input events therefore
/// never block on guest work.
#[derive(Debug)]
pub struct MessageQueue {
    /// Queued messages in FIFO order.
    pub messages: Vec<crate::QueuedWindowMessage>,
    /// Deterministic fake message timestamp source.
    pub next_message_time: u32,
    /// Cross-thread signal: a message was posted.
    pub signal: Arc<MessageSignal>,
    /// Number of modal dialogs currently open on this queue.
    ///
    /// Incremented by `CreateDialogParamA/W`, decremented when a `WM_QUIT`
    /// (posted by `EndDialog`) is consumed. While nonzero, an empty-queue
    /// `GetMessage` must yield instead of synthesizing the regression-mode
    /// `WM_QUIT` — otherwise a dialog would close the instant it opens.
    pub dialog_depth: u32,
}

impl Default for MessageQueue {
    fn default() -> Self {
        Self {
            // Reserve the common burst up-front so PostMessage/SendMessage
            // pushes do not reallocate from an empty Vec on every burst.
            messages: Vec::with_capacity(64),
            next_message_time: 0,
            signal: Arc::new(MessageSignal::new()),
            dialog_depth: 0,
        }
    }
}

/// A frame of 0RGB pixels ready for display.
#[derive(Clone)]
pub struct SurfaceFrame {
    /// Pixel width of the frame.
    pub width: u32,
    /// Pixel height of the frame.
    pub height: u32,
    /// 0RGB pixel data, top-down.
    pub pixels: Arc<Vec<u32>>,
}

/// Per-window surface: pixel buffer + dimensions.
#[derive(Debug, Clone)]
pub struct WindowSurface {
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// 0RGB pixel buffer (top-down, length = width * height). The blit paints
    /// into this buffer; `publish` moves it into the published Arc and the next
    /// paint hands the published buffer back (see [`PresentState::ensure_surface`]),
    /// so the composite always accumulates across publishes.
    pub pixels: Vec<u32>,
}

/// Manages per-window compositing surfaces and frame publishing.
pub struct PresentState {
    /// Persistent composite surface per HWND (scratch buffer for accumulating blits).
    ///
    /// `pub(crate)`: the host reads published frames (`published`) and wake
    /// hooks; the scratch surfaces are internal to the blit pipeline.
    pub(crate) surfaces: ahash::HashMap<crate::handles::Hwnd, WindowSurface>,
    /// Last published snapshot per HWND.
    pub published: ahash::HashMap<crate::handles::Hwnd, SurfaceFrame>,
    /// Monotonically increasing generation counter.
    pub generation: u64,
    /// Optional wake callback for the host presenter.
    pub wake: Option<Box<dyn Fn() + Send>>,
    /// Signal for headless mode.
    pub(crate) record: Option<Box<SurfaceFrame>>,
    /// B9: number of published frames (frame timing enabled only).
    pub frames_published: u64,
    /// Number of zero-copy hand-backs of the published buffer in
    /// `ensure_surface` (`Arc::try_unwrap` succeeded — the host was not
    /// holding the previous frame's Arc).
    pub hand_back_unwrap: u64,
    /// Number of clone-fallback hand-backs in `ensure_surface` (the host
    /// still held the previous frame's Arc, so the buffer had to be copied).
    pub hand_back_clone: u64,
    /// B9: accumulated publish wall time (ns).
    pub publish_ns: u128,
    /// B9: duration of the most recent publish (ns).
    pub publish_ns_last: u128,
    /// B9: accumulated BitBlt mask-copy wall time (ns).
    pub blit_copy_ns: u128,
    /// B9: duration of the most recent mask copy (ns).
    pub blit_copy_ns_last: u128,
    /// B9: accumulated host present (frame upload + present) wall time (ns).
    pub present_ns: u128,
    /// B9: duration of the most recent host present (ns).
    pub present_ns_last: u128,
    /// B3.6: HWNDs with deferred (coalesced) publishes pending since the last
    /// drain. Handlers call [`Self::publish_deferred`] instead of
    /// [`Self::publish`]; the runtime drains the set once per repaint cycle at
    /// the empty-queue idle boundary (WaitingForMessage), so one WM_PAINT
    /// cycle (BitBlt + control paints + captions) emits a single frame with
    /// the union dirty region instead of one vsync-blocked present per paint
    /// call. The set is deduplicated — the surface's `dirty` accumulator
    /// already unions every write, and the pixel buffer stays in the surface
    /// until the drain, so the published frame is byte-identical to the
    /// per-call publishes it replaces.
    pub(crate) pending_publishes: std::collections::HashSet<crate::handles::Hwnd>,
}

impl std::fmt::Debug for PresentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresentState")
            .field("surface_count", &self.surfaces.len())
            .field("published_count", &self.published.len())
            .field("generation", &self.generation)
            .field("wake_is_set", &self.wake.is_some())
            .field("record_is_set", &self.record.is_some())
            .field("frames_published", &self.frames_published)
            .field("hand_back_unwrap", &self.hand_back_unwrap)
            .field("hand_back_clone", &self.hand_back_clone)
            .field("publish_ns", &self.publish_ns)
            .field("publish_ns_last", &self.publish_ns_last)
            .field("blit_copy_ns", &self.blit_copy_ns)
            .field("blit_copy_ns_last", &self.blit_copy_ns_last)
            .field("present_ns", &self.present_ns)
            .field("present_ns_last", &self.present_ns_last)
            .field("pending_publishes", &self.pending_publishes.len())
            .finish()
    }
}

impl PresentState {
    /// Create a new, empty `PresentState`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            surfaces: ahash::HashMap::new(),
            published: ahash::HashMap::new(),
            generation: 0,
            wake: None,
            record: None,
            frames_published: 0,
            hand_back_unwrap: 0,
            hand_back_clone: 0,
            publish_ns: 0,
            publish_ns_last: 0,
            blit_copy_ns: 0,
            blit_copy_ns_last: 0,
            present_ns: 0,
            present_ns_last: 0,
            pending_publishes: std::collections::HashSet::new(),
        }
    }

    /// Ensure a surface exists for `hwnd` with the given dimensions.
    /// Resizes or reallocates if dimensions changed; never shrinks.
    pub fn ensure_surface(&mut self, hwnd: crate::handles::Hwnd, width: u32, height: u32) {
        let needed = usize::try_from(width.checked_mul(height).unwrap_or(0)).unwrap_or(0);
        // B1 double-buffer hand-back: a prior publish moved the painted buffer
        // into the published Arc, leaving the surface empty. Hand the buffer
        // back as the next paint base so the composite keeps accumulating
        // across publishes — zero-copy (`Arc::try_unwrap`) when the host is
        // not holding the previous frame, a clone otherwise. This is what
        // keeps gui_blit's multi-publish paint cycle (parent BitBlt → child
        // control paints, each publishing) byte-identical.
        if let Some(surface) = self.surfaces.get_mut(&hwnd)
            && surface.pixels.is_empty()
            && let Some(frame) = self.published.remove(&hwnd)
        {
            // Zero-copy when the host is not holding the previous frame; clone
            // otherwise (the host only briefly holds it). The clone is
            // deliberate, not a missed zero-copy opportunity: the surface
            // buffer must carry the previous frame's pixels so the composite
            // keeps accumulating across publishes (see the struct doc on
            // `WindowSurface::pixels`), and allocating a fresh zeroed buffer
            // here would blank that base on the next paint. `dirty` is left
            // untouched: writes between this publish and the hand-back must
            // keep accumulating for the next region computation.
            surface.pixels = match Arc::try_unwrap(frame.pixels) {
                Ok(pixels) => {
                    self.hand_back_unwrap = self.hand_back_unwrap.saturating_add(1);
                    pixels
                }
                Err(shared) => {
                    self.hand_back_clone = self.hand_back_clone.saturating_add(1);
                    shared.to_vec()
                }
            };
        }
        let entry = self.surfaces.entry(hwnd).or_insert_with(|| WindowSurface {
            width,
            height,
            pixels: vec![0u32; needed],
        });
        // If dimensions changed, reallocate; grow when the buffer is too small
        // (matches the pre-B1 semantics — the recycled buffer may carry the
        // previous frame's size after a resize).
        if entry.width != width || entry.height != height {
            // The surface size changed — the old
            // row-major buffer does not map onto the new dimensions. Reusing
            // it (a plain `resize` keeps the first N elements) would surface
            // misaligned stale pixels in the WS_CLIPCHILDREN-clipped child
            // areas — vertical bands/lines through controls after a resize.
            // Reallocate zeroed; the caller repaints the whole window, and
            // every publish is full anyway.
            entry.width = width;
            entry.height = height;
            entry.pixels = vec![0u32; needed];
        } else if entry.pixels.len() < needed {
            // Same dimensions but the buffer is too small (length normally
            // tracks width*height); grow zeroed.
            entry.pixels.resize(needed, 0);
        }
    }

    /// Publish the current surface for `hwnd` as a snapshot.
    ///
    /// This is the immediate-publish escape hatch (`publish_now`): it moves
    /// the surface buffer into the published Arc right away and fires the wake
    /// callback synchronously. Paint handlers should prefer
    /// [`Self::publish_deferred`] so the runtime emits at most one frame per
    /// repaint cycle via [`Self::drain_pending_publishes`]; keep this
    /// immediate path only where a frame must reach the host before the
    /// dispatch ends (the D3D9 Present handler uses it directly).
    pub fn publish(&mut self, hwnd: crate::handles::Hwnd) {
        let timing = frame_timing_enabled();
        let t0 = timing.then(Instant::now);
        let Some(surface) = self.surfaces.get_mut(&hwnd) else {
            return;
        };
        if surface.pixels.is_empty() || surface.width == 0 || surface.height == 0 {
            return;
        }
        let (width, height, pixels) = {
            // B1: publish WITHOUT a pixel clone — a pointer move of the painted
            // buffer into the Arc (no 4 MB copy under the WinAPI mutex). The
            // buffer returns to the surface on the next paint via
            // `ensure_surface`'s hand-back, preserving accumulation.
            let pixels: Arc<Vec<u32>> = Arc::from(std::mem::take(&mut surface.pixels));
            (surface.width, surface.height, pixels)
        };
        // ALWAYS a full frame. The region-delta
        // contract ("changes since the last guest publish") cannot survive B2
        // present skipping — a publish that loses the event-loop race has its
        // delta permanently absent from the wgpu staging texture, which holds
        // the host's last present. Full publishes against the persistent
        // staging (~4 MB at 1280×800, dwarfed by the vsync budget) make the
        // staging invariant trivial: it always equals the last published
        // frame.
        if let Some(t0) = t0 {
            let ns = t0.elapsed().as_nanos();
            self.publish_ns = self.publish_ns.saturating_add(ns);
            self.publish_ns_last = ns;
            self.frames_published = self.frames_published.saturating_add(1);
        }
        self.generation = self.generation.wrapping_add(1);
        tracing::debug!(
            target: "wiegui",
            hwnd = hwnd.as_u64(),
            width,
            height,
            generation = self.generation,
            publish_us = match t0 {
                Some(t) => u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX),
                None => 0,
            },
            "frame published"
        );
        if let Some(record) = self.record.as_mut() {
            // The record slot holds the most recent frame only and is
            // overwritten on every publish; it is never mutated or read by the
            // host in a way that needs a private copy, so share the published
            // Arc instead of cloning up to 4 MB per recorded frame. The
            // `ensure_surface` hand-back below will see the shared Arc and
            // clone once for the next paint base — one copy per cycle either
            // way, but no extra allocation + memcpy here.
            **record = SurfaceFrame {
                width,
                height,
                pixels: Arc::clone(&pixels),
            };
        }
        self.published.insert(
            hwnd,
            SurfaceFrame {
                width,
                height,
                pixels,
            },
        );
        if let Some(wake) = &self.wake {
            wake();
        }
    }

    /// B9: record one BitBlt mask-copy (`mask_bgra_to_0rgb`) duration (ns).
    pub fn record_blit_copy(&mut self, ns: u128) {
        self.blit_copy_ns = self.blit_copy_ns.saturating_add(ns);
        self.blit_copy_ns_last = ns;
    }

    /// B9: record one host present (softbuffer copy + upload) duration (ns).
    pub fn record_present(&mut self, ns: u128) {
        self.present_ns = self.present_ns.saturating_add(ns);
        self.present_ns_last = ns;
    }

    /// B3.6: defer a publish for `hwnd` to the next
    /// [`Self::drain_pending_publishes`] (one frame per repaint cycle).
    ///
    /// The surface keeps its pixel buffer and the `dirty` accumulator keeps
    /// unioning writes until the drain, so a WM_PAINT cycle that paints in
    /// several calls (BitBlt, window-DC text, child control paints) publishes
    /// exactly once, with the union region and the fully-painted surface. The
    /// HWND is deduplicated — repeated calls in one cycle are a no-op.
    pub fn publish_deferred(&mut self, hwnd: crate::handles::Hwnd) {
        self.pending_publishes.insert(hwnd);
    }

    /// B3.6: publish every HWND that deferred a frame since the last drain.
    ///
    /// Each pending HWND is published once, in insertion order; the dirty
    /// accumulator carries the union of every write since the last publish, so
    /// the emitted region is correct. Returns how many frames were published.
    pub fn drain_pending_publishes(&mut self) -> usize {
        let pending: Vec<crate::handles::Hwnd> = self.pending_publishes.drain().collect();
        for hwnd in &pending {
            self.publish(*hwnd);
        }
        pending.len()
    }
}

impl Default for PresentState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::PresentState;
    use crate::handles::Hwnd;

    #[test]
    fn deferred_publishes_coalesce_per_dispatch() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(7);
        state.ensure_surface(hwnd, 200, 100);

        // Two writes in one dispatch (e.g. BitBlt + window-DC TextOut) each
        // defer a publish for the same HWND.
        state.publish_deferred(hwnd);
        state.publish_deferred(hwnd);

        // Nothing is published until the drain.
        assert!(state.published.is_empty());
        assert_eq!(state.pending_publishes.len(), 1);

        let published = state.drain_pending_publishes();
        assert_eq!(
            published, 1,
            "N deferred writes in one dispatch → 1 publish"
        );
        assert!(state.pending_publishes.is_empty());

        let _frame = state.published.get(&hwnd).expect("frame published");
        // Every frame is full — no region.
        // The publish moved the painted buffer into the Arc; the surface is
        // empty and will be handed back by the next ensure_surface (B1).
        assert!(
            state
                .surfaces
                .get(&hwnd)
                .is_some_and(|s| s.pixels.is_empty())
        );
    }

    #[test]
    fn deferred_publish_of_two_hwnds_emits_one_frame_each() {
        let mut state = PresentState::new();
        let a = Hwnd::from(1);
        let b = Hwnd::from(2);
        state.ensure_surface(a, 100, 100);
        state.ensure_surface(b, 100, 100);
        state.publish_deferred(a);
        state.publish_deferred(b);
        assert_eq!(state.drain_pending_publishes(), 2);
        assert!(state.published.contains_key(&a));
        assert!(state.published.contains_key(&b));
    }
}
