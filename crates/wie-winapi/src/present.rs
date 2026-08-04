//! Present surface infrastructure for GDI blit pipeline.
//!
//! Manages per-window compositing surfaces and frame publishing so
//! CreateDIBSection → SelectObject → BitBlt actually renders pixels.

use ahash::HashMapExt;
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

/// The default frame background: `COLOR_WINDOW`-white (0RGB). Used when the
/// erase machinery has not yet recorded the owning window's class-brush
/// color — notepad's client and the common case.
const DEFAULT_BACKGROUND_COLOR: u32 = 0x00FF_FFFF;

/// A frame of 0RGB pixels ready for display.
#[derive(Clone)]
pub struct SurfaceFrame {
    /// Pixel width of the frame.
    pub width: u32,
    /// Pixel height of the frame.
    pub height: u32,
    /// 0RGB pixel data, top-down.
    pub pixels: Arc<Vec<u32>>,
    /// 0RGB background color of the owning window, recorded by the erase
    /// machinery (the class-brush color; `COLOR_WINDOW`-white by default).
    ///
    /// The host presenter clears its surface with this color before blitting
    /// the frame, so any region the frame's pixels do not cover (resize
    /// seams, the window smaller than the swapchain, pre-first-paint) reads
    /// as the window background instead of the presenter's default black.
    pub background_color: u32,
    /// The region of `pixels` that changed since the previous publish, in
    /// surface coordinates. `None` = the whole surface changed (the presenter
    /// must upload the full frame); `Some(rect)` = only that rect changed.
    ///
    /// This is a GPU-side HINT: the pixel bytes always carry the FULL frame,
    /// so headless readers (tests, the record slot) are unaffected. A
    /// degenerate (empty) rect is never emitted — `publish` treats
    /// "nothing recorded" as full, because a direct full-surface writer (the
    /// D3D9 Present handler) cannot be distinguished from a no-op cycle.
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
    /// Union of every write rect since the last publish, in surface
    /// coordinates. `None` = the whole surface changed (full upload); this is
    /// the initial state of a new surface and the state after any resize.
    /// `Some(IRect::empty())` = nothing written yet this cycle; `Some(rect)` =
    /// the union of the partial writes. `publish` snapshots it into
    /// [`SurfaceFrame::region`] and resets it to `Some(IRect::empty())`.
    ///
    /// The write primitives (`fill_rect_surface`, the BitBlt row copy, the
    /// window-DC text band) union their exact bounds into this accumulator via
    /// [`PresentState::mark_dirty`]; the control-text writes are covered by
    /// the control's full-face fill, and the D3D9 Present handler writes the
    /// whole surface without marking, which `publish` conservatively treats
    /// as full.
    pub dirty: Option<IRect>,
}

/// Host MessageBox callback: `(caption, text, mb_type)` → Win32 id.
///
/// Registered by the GUI presenter via `GuestHandle::set_message_box_bridge`;
/// invoked by the `MessageBoxA/W` handlers on the guest thread.
pub type MessageBoxBridge = Box<dyn Fn(&str, &str, u32) -> i32 + Send>;

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
    /// 0RGB background color per published HWND — the owning window's
    /// class-brush color, recorded by the erase machinery so the presenter
    /// can clear its surface with it (see [`SurfaceFrame::background_color`]).
    /// Absent HWNDs default to [`DEFAULT_BACKGROUND_COLOR`].
    pub(crate) background_colors: ahash::HashMap<crate::handles::Hwnd, u32>,
    /// Optional host MessageBox bridge, registered by the GUI presenter.
    ///
    /// When set, the `MessageBoxA/W` handlers call it with
    /// `(caption, text, mb_type)` and return its Win32 id (IDOK/IDCANCEL/
    /// IDYES/IDNO) to the guest. The bridge runs on the guest thread and
    /// blocks until the host alert is dismissed — correct MessageBox
    /// semantics. When unset (headless runs, `trace`) the handlers keep the
    /// console-echo + IDOK fallback so no guest ever hangs. Mirrors `wake`:
    /// the host stores the callback here and the winapi layer invokes it.
    pub message_box_bridge: Option<MessageBoxBridge>,
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
            .field("background_color_count", &self.background_colors.len())
            .field("generation", &self.generation)
            .field("wake_is_set", &self.wake.is_some())
            .field(
                "message_box_bridge_is_set",
                &self.message_box_bridge.is_some(),
            )
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
            background_colors: ahash::HashMap::new(),
            message_box_bridge: None,
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
            // Zero-initialized: the only safe way to size a fresh Vec, and a
            // new surface has no previous frame to hand back. The F2
            // erase-before-clear machinery repaints it before the first
            // publish, so the zeros never reach the host.
            pixels: vec![0u32; needed],
            // A new surface's content is unknown — the whole frame uploads.
            dirty: None,
        });
        // If dimensions changed, reallocate; grow when the buffer is too small
        // (matches the pre-B1 semantics — the recycled buffer may carry the
        // previous frame's size after a resize).
        if entry.width != width || entry.height != height {
            // The surface size changed — the old row-major buffer no longer
            // maps onto the new dimensions, but REUSING it (resize in place,
            // zeroing only the new tail) is safe: the F2 erase-before-clear
            // machinery repaints the whole window before the next publish
            // (resize paths set ERASE_BACKGROUND), so the stale head can never
            // reach the host. A fresh zeroed realloc (`vec![0u32; needed]`)
            // would memset the whole buffer for content that is erased anyway —
            // wasted work, and the no-black invariant was never protected by it
            // (both zeroed and stale heads are garbage that the erase covers).
            entry.width = width;
            entry.height = height;
            entry.pixels.resize(needed, 0);
            // Content is unknown at the new mapping — full upload.
            entry.dirty = None;
        } else if entry.pixels.len() < needed {
            // Same dimensions but the buffer is too small (length normally
            // tracks width*height); grow zeroed — the new tail is unknown.
            entry.pixels.resize(needed, 0);
            entry.dirty = None;
        }
    }

    /// Record the 0RGB background color of `hwnd`'s surface — the owning
    /// window's class-brush color, resolved by the erase machinery.
    ///
    /// The next published frame for `hwnd` carries it (see
    /// [`SurfaceFrame::background_color`]); the host presenter clears its
    /// surface with it so regions the frame does not cover never read black.
    pub(crate) fn set_background_color(&mut self, hwnd: crate::handles::Hwnd, color: u32) {
        self.background_colors.insert(hwnd, color);
    }

    /// Union `rect` (in surface coordinates, already clipped to the surface)
    /// into the surface's dirty accumulator — the region the next publish
    /// reports as changed. A rect outside the surface or a degenerate one is a
    /// no-op. `None` (full surface) stays `None`: a full repaint can never be
    /// narrowed by a later partial write.
    ///
    /// Called by every write primitive that reaches [`PresentState`]: the
    /// fill paths (`fill_rect_surface`), the BitBlt SRCCOPY row copy, and the
    /// window-DC text band (all in `gdi32`). The control-text glyphs are
    /// covered by the control's full-face fill, and the D3D9 Present handler
    /// writes the whole surface without marking — `publish` conservatively
    /// treats that as full.
    pub(crate) fn mark_dirty(&mut self, hwnd: crate::handles::Hwnd, rect: IRect) {
        let Some(surface) = self.surfaces.get_mut(&hwnd) else {
            return;
        };
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return;
        }
        match &mut surface.dirty {
            None => {}
            Some(acc) => {
                if acc.right <= acc.left || acc.bottom <= acc.top {
                    // The accumulator is the post-publish "nothing" state
                    // (`IRect::empty()` is (0,0,0,0), NOT a union identity —
                    // blending its zero origin into the write would widen the
                    // region to the surface's top-left). Start from the write.
                    *acc = rect;
                } else {
                    acc.left = acc.left.min(rect.left);
                    acc.top = acc.top.min(rect.top);
                    acc.right = acc.right.max(rect.right);
                    acc.bottom = acc.bottom.max(rect.bottom);
                }
            }
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
        // The presenter clears its surface with the owning window's
        // background color; the erase machinery records it per HWND, and the
        // default is COLOR_WINDOW-white.
        let background_color = self
            .background_colors
            .get(&hwnd)
            .copied()
            .unwrap_or(DEFAULT_BACKGROUND_COLOR);
        let (width, height, pixels) = {
            // B1: publish WITHOUT a pixel clone — a pointer move of the painted
            // buffer into the Arc (no 4 MB copy under the WinAPI mutex). The
            // buffer returns to the surface on the next paint via
            // `ensure_surface`'s hand-back, preserving accumulation.
            let pixels: Arc<Vec<u32>> = Arc::from(std::mem::take(&mut surface.pixels));
            (surface.width, surface.height, pixels)
        };
        // Snapshot the dirty accumulator as the frame's region hint, then
        // reset it for the next cycle. A degenerate (empty) rect means
        // "nothing recorded this cycle" — emitted as full: a direct
        // full-surface writer (the D3D9 Present handler) writes the whole
        // surface without marking partial rects, so "nothing recorded" cannot
        // be distinguished from "everything changed". The hint never under-
        // reports, which is the only direction that corrupts the GPU staging.
        // A rect covering the whole surface is normalized to `None` (full) so
        // the presenter keeps its zero-copy full-frame upload instead of
        // packing a full-size region.
        let surface_w = i32::try_from(surface.width).unwrap_or(0);
        let surface_h = i32::try_from(surface.height).unwrap_or(0);
        let region = match surface.dirty {
            None => None,
            Some(rect)
                if rect.right > rect.left
                    && rect.bottom > rect.top
                    && (rect.left > 0
                        || rect.top > 0
                        || rect.right < surface_w
                        || rect.bottom < surface_h) =>
            {
                Some(rect)
            }
            Some(_) => None,
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
                background_color,
                region,
            };
        }
        self.published.insert(
            hwnd,
            SurfaceFrame {
                width,
                height,
                pixels,
                background_color,
                region,
            },
        );
        if let Some(wake) = &self.wake {
            wake();
        }
    }

    /// Request a host-side registry sync without publishing a frame.
    ///
    /// The host reconciles its window registry on every publish wake; a guest
    /// `DestroyWindow` of a top-level window emits no frame, so the stale
    /// winit window would linger until some unrelated repaint. This fires the
    /// same stored wake the publish path uses, so the event-loop thread
    /// re-runs the Frame diff and drops the dead window (L3 wake-on-destroy).
    pub fn request_host_sync(&mut self) {
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
    use super::{DEFAULT_BACKGROUND_COLOR, PresentState, SurfaceFrame};
    use crate::gdi32::IRect;
    use crate::handles::Hwnd;
    use std::sync::Arc;

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

    #[test]
    fn request_host_sync_fires_the_stored_wake() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut state = PresentState::new();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        state.wake = Some(Box::new(move || {
            flag.store(true, Ordering::SeqCst);
        }));

        state.request_host_sync();

        assert!(
            fired.load(Ordering::SeqCst),
            "request_host_sync must fire the same stored wake the publish path fires"
        );
    }

    #[test]
    fn request_host_sync_without_wake_is_a_no_op() {
        // No wake installed (headless runs): the call must be a silent no-op.
        PresentState::new().request_host_sync();
    }

    /// An empty frame for the headless record slot.
    fn empty_frame() -> SurfaceFrame {
        SurfaceFrame {
            width: 0,
            height: 0,
            pixels: Arc::new(Vec::new()),
            background_color: DEFAULT_BACKGROUND_COLOR,
            region: None,
        }
    }

    #[test]
    fn published_frame_carries_recorded_background_color() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(9);
        state.set_background_color(hwnd, 0x00F0_F0F0); // COLOR_BTNFACE
        state.record = Some(Box::new(empty_frame()));
        state.ensure_surface(hwnd, 16, 16);
        state.publish(hwnd);

        let frame = state.published.get(&hwnd).expect("published frame");
        assert_eq!(
            frame.background_color, 0x00F0_F0F0,
            "the frame must carry the owning window's background color"
        );
        let recorded = state.record.as_ref().expect("recorded frame");
        assert_eq!(
            recorded.background_color, 0x00F0_F0F0,
            "the headless record slot carries the background too"
        );
    }

    #[test]
    fn published_frame_defaults_to_white_background() {
        // No erase ever recorded a color: the presenter falls back to
        // COLOR_WINDOW-white (notepad's client, and the headless default).
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(10);
        state.ensure_surface(hwnd, 8, 8);
        state.publish(hwnd);
        let frame = state.published.get(&hwnd).expect("published frame");
        assert_eq!(frame.background_color, 0x00FF_FFFF);
    }

    #[test]
    fn background_filled_frame_has_no_black_pixels() {
        // F2 invariant at the present level: a frame whose surface was
        // erased with its recorded background color (the erase-before-clear
        // guarantee) publishes zero 0x000000 pixels.
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(11);
        let background = 0x00FF_FFFF;
        state.set_background_color(hwnd, background);
        state.ensure_surface(hwnd, 32, 24);
        if let Some(surf) = state.surfaces.get_mut(&hwnd) {
            for px in &mut surf.pixels {
                *px = background;
            }
        }
        state.record = Some(Box::new(empty_frame()));
        state.publish(hwnd);

        let recorded = state.record.as_ref().expect("recorded frame");
        assert!(
            !recorded.pixels.contains(&0x0000_0000),
            "a background-erased frame must contain no unpainted black pixels"
        );
        assert!(
            recorded.pixels.iter().all(|&px| px == 0x00FF_FFFF),
            "every pixel is the erased background"
        );
    }

    #[test]
    fn black_background_is_legitimate_and_recorded() {
        // A window whose class brush is genuinely black (COLOR_WINDOWTEXT)
        // erases to black — that content is legitimate, and the invariant's
        // "outside legitimately black content" clause excludes it. The frame
        // must still record the black background so the presenter clears
        // with it (never the default white-on-black mismatch).
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(12);
        let black = 0x0000_0000;
        state.set_background_color(hwnd, black);
        state.ensure_surface(hwnd, 8, 8);
        if let Some(surf) = state.surfaces.get_mut(&hwnd) {
            for px in &mut surf.pixels {
                *px = black;
            }
        }
        state.publish(hwnd);
        let frame = state.published.get(&hwnd).expect("published frame");
        assert_eq!(frame.background_color, 0x0000_0000);
    }

    /// A paint that writes two disjoint sub-rects must publish a frame whose
    /// region is their union — the GPU uploads exactly the repainted area.
    ///
    /// The accumulator starts from the post-publish reset: a fresh surface is
    /// fully dirty (`None`) and a partial write must not narrow it, so the
    /// scenario seeds one full publish first, then hands the buffer back (the
    /// next paint cycle's `ensure_surface`) before the partial writes.
    #[test]
    fn publish_reports_the_union_of_partial_writes() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(20);
        state.ensure_surface(hwnd, 200, 100);
        state.publish(hwnd);
        state.ensure_surface(hwnd, 200, 100);
        // Two writes in one repaint cycle, both via the write-primitive seam
        // (`mark_dirty` is what fill_rect_surface / the BitBlt / the text
        // band call).
        state.mark_dirty(
            hwnd,
            IRect {
                left: 10,
                top: 20,
                right: 60,
                bottom: 50,
            },
        );
        state.mark_dirty(
            hwnd,
            IRect {
                left: 120,
                top: 70,
                right: 180,
                bottom: 90,
            },
        );
        state.publish(hwnd);

        let frame = state.published.get(&hwnd).expect("published frame");
        assert_eq!(
            frame.region,
            Some(IRect {
                left: 10,
                top: 20,
                right: 180,
                bottom: 90
            }),
            "the frame region is the union of the cycle's writes"
        );
        assert_eq!(
            frame.pixels.len(),
            usize::try_from(200 * 100).unwrap_or(0),
            "the pixel bytes still carry the FULL frame (region is a hint)"
        );
    }

    /// A dirty rect covering the whole surface normalizes to `None` (full):
    /// a full repaint must take the presenter's zero-copy full-frame upload,
    /// not a packed full-size region.
    #[test]
    fn full_surface_dirty_normalizes_to_none() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(21);
        state.ensure_surface(hwnd, 64, 32);
        state.mark_dirty(
            hwnd,
            IRect {
                left: 0,
                top: 0,
                right: 64,
                bottom: 32,
            },
        );
        state.publish(hwnd);
        let frame = state.published.get(&hwnd).expect("published frame");
        assert_eq!(
            frame.region, None,
            "a whole-surface repaint is a full frame"
        );
    }

    /// A publish with NO recorded writes emits a FULL frame (region None): a
    /// direct full-surface writer such as the D3D9 Present handler writes the
    /// whole surface without marking partial rects, so "nothing recorded"
    /// cannot be distinguished from "everything changed" — the hint must
    /// never under-report.
    #[test]
    fn publish_without_recorded_writes_is_full() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(22);
        state.ensure_surface(hwnd, 16, 16);
        state.publish(hwnd);
        let first = state.published.get(&hwnd).expect("published frame");
        assert_eq!(first.region, None, "a fresh surface is full");

        // A second publish with no writes (the D3D9 pattern — the surface was
        // written directly, no marks recorded) is full too: the buffer is
        // handed back and published without any `mark_dirty` call.
        state.ensure_surface(hwnd, 16, 16);
        state.publish(hwnd);
        let second = state.published.get(&hwnd).expect("published frame");
        assert_eq!(
            second.region, None,
            "an unmarked publish must fall back to full, never to an empty region"
        );
    }

    /// `publish` resets the dirty accumulator: the NEXT cycle's writes start
    /// a fresh region instead of accumulating the previous one.
    #[test]
    fn publish_resets_the_dirty_accumulator() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(23);
        state.ensure_surface(hwnd, 100, 100);
        state.publish(hwnd);
        assert_eq!(
            state.surfaces.get(&hwnd).expect("surface").dirty,
            Some(IRect::empty()),
            "the dirty accumulator resets to 'nothing' after a publish"
        );
        // The next write produces a region that starts from scratch.
        state.ensure_surface(hwnd, 100, 100);
        state.mark_dirty(
            hwnd,
            IRect {
                left: 40,
                top: 40,
                right: 55,
                bottom: 55,
            },
        );
        state.publish(hwnd);
        let frame = state.published.get(&hwnd).expect("published frame");
        assert_eq!(
            frame.region,
            Some(IRect {
                left: 40,
                top: 40,
                right: 55,
                bottom: 55
            }),
            "the second cycle's region carries only its own write"
        );
    }

    /// A new surface is dirty `None` (full): its content is unknown until the
    /// first paint, so the first publish must always upload everything.
    #[test]
    fn new_surface_is_dirty_full() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(24);
        state.ensure_surface(hwnd, 32, 32);
        assert_eq!(
            state.surfaces.get(&hwnd).expect("surface").dirty,
            None,
            "a freshly created surface is fully dirty"
        );
    }
}
