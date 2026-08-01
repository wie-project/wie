//! Present surface infrastructure for GDI blit pipeline.
//!
//! Manages per-window compositing surfaces and frame publishing so
//! CreateDIBSection → SelectObject → BitBlt actually renders pixels.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::gdi32::IRect;

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
#[derive(Debug, Default)]
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

/// A frame of 0RGB pixels ready for display.
#[derive(Clone)]
pub struct SurfaceFrame {
    /// Pixel width of the frame.
    pub width: u32,
    /// Pixel height of the frame.
    pub height: u32,
    /// 0RGB pixel data, top-down.
    pub pixels: Arc<Vec<u32>>,
    /// B3: optional dirty-rect (frame pixel coords, exclusive right/bottom).
    ///
    /// `None` = the whole frame changed (the consumer must copy everything).
    /// `Some(rect)` = only `rect` is fresh; pixels outside it are stale in the
    /// consumer's buffer from the previous present and must be left alone. The
    /// buffer content is always the full composite — the region only tells the
    /// consumer which part to re-copy.
    pub region: Option<IRect>,
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
    /// B3: accumulated update region (surface coords, exclusive edges) written
    /// since the last publish. `None` = the whole surface is dirty (fresh or
    /// reallocated buffer — a partial publish would show stale pixels outside
    /// the region). `Some(rect)` = only `rect` is dirty; a degenerate (empty)
    /// rect = nothing dirty since the last publish (the buffer is fully
    /// up-to-date, so a partial publish can copy nothing).
    pub dirty: Option<IRect>,
}

/// Manages per-window compositing surfaces and frame publishing.
pub struct PresentState {
    /// Persistent composite surface per HWND (scratch buffer for accumulating blits).
    pub surfaces: std::collections::HashMap<u64, WindowSurface>,
    /// Last published snapshot per HWND.
    pub published: std::collections::HashMap<u64, SurfaceFrame>,
    /// Monotonically increasing generation counter.
    pub generation: u64,
    /// Optional wake callback for the host presenter.
    pub wake: Option<Box<dyn Fn() + Send>>,
    /// Signal for headless mode.
    pub record: Option<Box<SurfaceFrame>>,
    /// B9: number of published frames (frame timing enabled only).
    pub frames_published: u64,
    /// B9: accumulated publish wall time (ns).
    pub publish_ns: u128,
    /// B9: duration of the most recent publish (ns).
    pub publish_ns_last: u128,
    /// B9: accumulated BitBlt mask-copy wall time (ns).
    pub blit_copy_ns: u128,
    /// B9: duration of the most recent mask copy (ns).
    pub blit_copy_ns_last: u128,
    /// B9: accumulated host present (softbuffer copy + upload) wall time (ns).
    pub present_ns: u128,
    /// B9: duration of the most recent host present (ns).
    pub present_ns_last: u128,
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
            .field("publish_ns", &self.publish_ns)
            .field("publish_ns_last", &self.publish_ns_last)
            .field("blit_copy_ns", &self.blit_copy_ns)
            .field("blit_copy_ns_last", &self.blit_copy_ns_last)
            .field("present_ns", &self.present_ns)
            .field("present_ns_last", &self.present_ns_last)
            .finish()
    }
}

impl PresentState {
    /// Create a new, empty `PresentState`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            surfaces: std::collections::HashMap::new(),
            published: std::collections::HashMap::new(),
            generation: 0,
            wake: None,
            record: None,
            frames_published: 0,
            publish_ns: 0,
            publish_ns_last: 0,
            blit_copy_ns: 0,
            blit_copy_ns_last: 0,
            present_ns: 0,
            present_ns_last: 0,
        }
    }

    /// Ensure a surface exists for `hwnd` with the given dimensions.
    /// Resizes or reallocates if dimensions changed; never shrinks.
    pub fn ensure_surface(&mut self, hwnd: u64, width: u32, height: u32) {
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
            // otherwise (the host only briefly holds it). `dirty` is left
            // untouched: writes between this publish and the hand-back must
            // keep accumulating for the next region computation.
            surface.pixels = Arc::try_unwrap(frame.pixels).unwrap_or_else(|shared| shared.to_vec());
        }
        let entry = self.surfaces.entry(hwnd).or_insert_with(|| WindowSurface {
            width,
            height,
            pixels: vec![0u32; needed],
            // A brand-new buffer is blank — the whole surface must be published
            // before any partial-region publish is allowed.
            dirty: None,
        });
        // If dimensions changed, reallocate; grow when the buffer is too small
        // (matches the pre-B1 semantics — the recycled buffer may carry the
        // previous frame's size after a resize).
        if entry.width != width || entry.height != height || entry.pixels.len() < needed {
            entry.width = width;
            entry.height = height;
            entry.pixels.resize(needed, 0);
            // B3 conservative fallback: after a realloc the buffer is not
            // fully up-to-date — force a full publish so the consumer never
            // shows stale pixels outside whatever the guest repaints.
            entry.dirty = None;
        }
    }

    /// B3: accumulate `rect` (surface coords, exclusive edges) into `hwnd`'s
    /// pending update region. Unions with the existing region; a full-dirty
    /// surface (`None`) stays full (a rect can never un-dirty it). The rect is
    /// clipped to the surface bounds and degenerate rects are ignored.
    pub fn mark_dirty(&mut self, hwnd: u64, rect: IRect) {
        let Some(surface) = self.surfaces.get_mut(&hwnd) else {
            return;
        };
        let (w, h) = (
            i32::try_from(surface.width).unwrap_or(i32::MAX),
            i32::try_from(surface.height).unwrap_or(i32::MAX),
        );
        let clipped = IRect {
            left: rect.left.max(0),
            top: rect.top.max(0),
            right: rect.right.min(w),
            bottom: rect.bottom.min(h),
        };
        if clipped.left >= clipped.right || clipped.top >= clipped.bottom {
            return;
        }
        surface.dirty = match surface.dirty {
            None => None,
            // Degenerate (empty) rect = "nothing dirty yet" — start fresh.
            Some(prev) if prev.width() <= 0 || prev.height() <= 0 => Some(clipped),
            Some(prev) => Some(IRect {
                left: prev.left.min(clipped.left),
                top: prev.top.min(clipped.top),
                right: prev.right.max(clipped.right),
                bottom: prev.bottom.max(clipped.bottom),
            }),
        };
    }

    /// B3: conservative fallback — mark the whole `hwnd` surface dirty so the
    /// next publish is full. Used by writers that cannot name their exact
    /// region (e.g. text rendered straight into a window DC's surface).
    pub fn mark_dirty_full(&mut self, hwnd: u64) {
        if let Some(surface) = self.surfaces.get_mut(&hwnd) {
            surface.dirty = None;
        }
    }

    /// Publish the current surface for `hwnd` as a snapshot.
    pub fn publish(&mut self, hwnd: u64) {
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
        // B3: derive the partial-update region from the accumulated dirty rect
        // (the union of every write since the last publish). `None` = the whole
        // surface changed → full frame. A rect covering >= 50% of the surface
        // is not worth a partial copy (the consumer would copy most of the
        // frame anyway) → full frame. A degenerate rect = nothing changed →
        // copy nothing. The accumulator resets here so the next publish starts
        // clean; the P0 hand-back restores the buffer, whose content is the
        // full composite, so pixels outside the region stay up-to-date.
        let region = match surface.dirty {
            None => None,
            Some(rect) if rect.width() <= 0 || rect.height() <= 0 => Some(rect),
            Some(rect) => {
                let area = i64::from(rect.width()).saturating_mul(i64::from(rect.height()));
                let total = i64::from(surface.width).saturating_mul(i64::from(surface.height));
                if area.saturating_mul(2) >= total {
                    None
                } else {
                    Some(rect)
                }
            }
        };
        surface.dirty = Some(IRect::empty());
        if let Some(t0) = t0 {
            let ns = t0.elapsed().as_nanos();
            self.publish_ns = self.publish_ns.saturating_add(ns);
            self.publish_ns_last = ns;
            self.frames_published = self.frames_published.saturating_add(1);
        }
        self.generation = self.generation.wrapping_add(1);
        tracing::debug!(
            target: "wiegui",
            hwnd,
            width,
            height,
            region = ?region,
            generation = self.generation,
            publish_us = match t0 {
                Some(t) => u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX),
                None => 0,
            },
            "frame published"
        );
        if let Some(record) = self.record.as_mut() {
            // Headless record keeps its OWN pixel copy — never move the same
            // buffer into both the published map and the record slot.
            **record = SurfaceFrame {
                width,
                height,
                pixels: Arc::new(pixels.to_vec()),
                region,
            };
        }
        self.published.insert(
            hwnd,
            SurfaceFrame {
                width,
                height,
                pixels,
                region,
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
}

impl Default for PresentState {
    fn default() -> Self {
        Self::new()
    }
}
