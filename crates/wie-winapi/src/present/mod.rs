//! Present surface infrastructure for GDI blit pipeline.
//!
//! Manages per-window compositing surfaces and frame publishing so
//! CreateDIBSection → SelectObject → BitBlt actually renders pixels.

use ahash::HashMapExt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

mod commit;
mod queue;
pub use commit::{CommitterHandle, spawn_present_committer};
pub use queue::{MessageQueue, MessageSignal};

use crate::WinApiState;
use crate::gdi32::{IRect, union_rect};

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

/// The default frame background: `COLOR_WINDOW`-white (0RGB). Used when the
/// erase machinery has not yet recorded the owning window's class-brush
/// color — notepad's client and the common case.
const DEFAULT_BACKGROUND_COLOR: u32 = 0x00FF_FFFF;

/// Surface pitch padding gate (ADR-0001 reversibility): `WIE_SURFACE_PAD=0`
/// allocates surfaces at the logical width (stride == width); any other
/// value or unset keeps the default 64-pixel pitch padding so a full-frame
/// host upload is always zero-copy (`stride * 4` is a multiple of 256).
fn surface_padding_enabled() -> bool {
    static PAD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PAD.get_or_init(|| !std::env::var("WIE_SURFACE_PAD").is_ok_and(|v| v == "0"))
}

/// The row pitch (in pixels) for a surface of logical `width`: padded up to a
/// multiple of 64 so `stride * 4` bytes satisfies wgpu's 256-byte
/// `bytes_per_row` alignment and the presenter can borrow the pixel buffer
/// zero-copy. `WIE_SURFACE_PAD=0` restores the unpitched layout for bisect.
#[must_use]
pub fn padded_stride(width: u32) -> u32 {
    if width == 0 {
        return 0;
    }
    if surface_padding_enabled() {
        width.div_ceil(64).saturating_mul(64)
    } else {
        width
    }
}

/// A frame of 0RGB pixels ready for display.
#[derive(Clone)]
pub struct SurfaceFrame {
    /// Logical pixel width (the guest-visible width; column count).
    pub width: u32,
    /// Row pitch of `pixels` in pixels. `stride >= width`; the tail of every
    /// row (`stride - width` pixels) is padding that carries no content.
    /// Equal to `width` for unpitched frames (tests, `WIE_SURFACE_PAD=0`).
    /// The host presenter uses it as the source `bytes_per_row` for
    /// `write_texture`, which keeps the full-frame upload zero-copy.
    pub stride: u32,
    /// Pixel height.
    pub height: u32,
    /// 0RGB pixel data, top-down, `stride * height` words (padding included).
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
    /// LOGICAL surface coordinates. `None` = the whole surface changed (the
    /// presenter must upload the full frame); `Some(rect)` = only that rect
    /// changed.
    ///
    /// This is a GPU-side HINT: the pixel bytes always carry the FULL frame,
    /// so headless readers (tests, the record slot) are unaffected. A
    /// degenerate (empty) rect is never emitted — `publish` treats
    /// "nothing recorded" as full, because a direct full-surface writer (the
    /// D3D9 Present handler) cannot be distinguished from a no-op cycle.
    pub region: Option<IRect>,
}

impl SurfaceFrame {
    /// The logical pixel at `(x, y)` — the stride-aware read every frame
    /// consumer (BMP encoder, test helpers, probes) must use instead of
    /// indexing `pixels` with `y * width + x`.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<u32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let stride = usize::try_from(self.stride).unwrap_or(0);
        let idx = usize::try_from(y)
            .unwrap_or(0)
            .saturating_mul(stride)
            .saturating_add(usize::try_from(x).unwrap_or(0));
        self.pixels.get(idx).copied()
    }
}

/// Per-window surface: pixel buffer + dimensions.
#[derive(Debug, Clone)]
pub struct WindowSurface {
    /// Logical pixel width (column count).
    pub width: u32,
    /// Row pitch of `pixels` in pixels (`>= width`, 64-aligned by default).
    /// Every row-major write must index `row * stride + x` — the padding tail
    /// of each row carries no content and the next row starts after it.
    pub stride: u32,
    /// Pixel height.
    pub height: u32,
    /// 0RGB pixel buffer (top-down, length = stride * height). The blit paints
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

/// Monotonic per-top-level content fingerprint for the pull-based repaint
/// latch: bumped by [`PresentState::request_paint`] on every visible-state
/// mutation of a window (or any of its descendant controls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRev(pub u64);

/// The revision bookkeeping [`PresentState::reconcile_and_publish`] diffs at
/// the idle boundary: what each top-level's content revision currently IS vs
/// what was last published. A window whose `content_rev` differs from its
/// `last_published_rev` is stale and gets republished.
///
/// `content_rev` entries exist only for windows a mutation touched since the
/// state was created (a never-mutated window is absent, hence never stale),
/// so the idle diff iterates the mutation set, not the whole window registry.
#[derive(Debug)]
pub struct WindowRevisions {
    /// Current content revision per top-level window (absent = untouched).
    pub content_rev: ahash::HashMap<crate::handles::Hwnd, ContentRev>,
    /// The content revision the last publish of each top-level carried
    /// (absent = never published).
    pub last_published_rev: ahash::HashMap<crate::handles::Hwnd, ContentRev>,
}

/// The host-presenter side of the publish pipeline, behind its OWN lock.
///
/// This is the Wave-1 extraction (ADR-0003): everything the host presenter
/// touches per frame — the latest published frame slot, the spare-buffer
/// pool, the z-order/window-set revision mirrors, and the presenter-side
/// timing counters — lives here behind a dedicated tiny mutex, so
/// `take_frame` / `store_spare_buffer` / `windows_rev` / `z_snapshot` /
/// `record_present_time` never acquire the big `WinApiState` mutex. Before
/// this channel, one window's paint or a D3D9 `DrawPrimitive` holding the big
/// lock stalled the presenter (and the whole macOS UI thread) for its whole
/// duration; now the presenter waits at most for one map insert/clone.
///
/// Lock ordering: guest threads take big-`WinApiState` → channel (inside
/// paint handlers and `publish`); the presenter takes channel ONLY. Nobody
/// takes channel → big, so no deadlock is possible.
///
/// Wake gating: `published_seq` counts publishes (guest side), `taken_seq`
/// counts host takes. A publish fires the wake callback only when every
/// frame published before it has already been taken (`prev == taken`) — the
/// host is idle, so without a wake it would never look again. When frames are
/// still pending the host already has a redraw in flight whose take drains
/// the latest slot (latest-wins), so skipping the wake caps the Frame-event
/// rate at the host's drain rate instead of the guest's publish rate. The
/// one race (a publish landing between the host's take and the end of its
/// redraw pass) is closed host-side by re-requesting a redraw when
/// [`Self::pending_frames`] is nonzero after a take.
pub struct PresentChannel {
    inner: Mutex<ChannelInner>,
    published_seq: AtomicU64,
    taken_seq: AtomicU64,
    /// Wave 2 (Option A1): the D3D9 Present-commit pipeline — latest-wins
    /// commit slot, backbuffer spare pool, committer counters. Own mutex
    /// inside, so an enqueue never contends the frame-slot lock.
    pub(crate) commit: commit::PresentCommit,
}

struct ChannelInner {
    /// The latest published frame per HWND (latest-wins slot; the host takes
    /// a cheap `Arc` clone of the pixels).
    latest: ahash::HashMap<crate::handles::Hwnd, SurfaceFrame>,
    /// Spare buffers recycled by the presenter after a frame is superseded,
    /// handed back to the guest paint side so `ensure_surface` reuses the
    /// allocation instead of cloning/memset (ADR-0001's zero-alloc loop).
    spare_buffers: ahash::HashMap<crate::handles::Hwnd, Vec<u32>>,
    /// Mirror of `PresentState::z_order` (read side for the host reorder).
    z_order: Vec<crate::handles::Hwnd>,
    /// Mirror of `PresentState::z_rev`.
    z_rev: u64,
    /// Mirror of `PresentState::windows_rev`.
    windows_rev: u64,
    /// Presenter-side present wall-time accumulators (written by the host
    /// presenter, read by the profile dump).
    present_ns: u128,
    present_ns_last: u128,
}

impl PresentChannel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ChannelInner {
                latest: ahash::HashMap::new(),
                spare_buffers: ahash::HashMap::new(),
                z_order: Vec::new(),
                z_rev: 0,
                windows_rev: 0,
                present_ns: 0,
                present_ns_last: 0,
            }),
            published_seq: AtomicU64::new(0),
            taken_seq: AtomicU64::new(0),
            commit: commit::PresentCommit::new(),
        }
    }

    /// Enable/disable D3D9 Present-commit mode (Wave 2). Only call after the
    /// committer thread spawned (`spawn_present_committer`), so every enqueued
    /// commit has a live consumer. `WIE_PRESENT_COMMIT=0` keeps the legacy
    /// inline present path regardless.
    pub fn set_commit_enabled(&self, enabled: bool) {
        self.commit.set_enabled(enabled);
    }

    /// Whether D3D9 `Present` enqueues a commit (render-thread path) instead
    /// of stretching + publishing inline under the big lock.
    #[must_use]
    pub fn commit_enabled(&self) -> bool {
        self.commit.enabled()
    }

    /// Accumulated committer stretch+publish wall time (ns).
    #[must_use]
    pub fn commit_ns(&self) -> u64 {
        self.commit.commit_ns()
    }

    /// Most recent committer stretch+publish wall time (ns).
    #[must_use]
    pub fn commit_ns_last(&self) -> u64 {
        self.commit.commit_ns_last()
    }

    /// Committed (render-thread published) frame count.
    #[must_use]
    pub fn commit_frames(&self) -> u64 {
        self.commit.commit_frames()
    }

    /// Publish `frame` into the latest-wins slot. Returns the displaced
    /// previous slot frame (the guest side may reclaim its buffer as a spare)
    /// and whether the host must be woken (the gate: every earlier publish
    /// was already taken).
    pub(crate) fn publish_frame(
        &self,
        hwnd: crate::handles::Hwnd,
        frame: SurfaceFrame,
    ) -> (Option<SurfaceFrame>, bool) {
        let prev = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.latest.insert(hwnd, frame)
        };
        let prev_seq = self.published_seq.fetch_add(1, Ordering::Relaxed);
        let taken = self.taken_seq.load(Ordering::Relaxed);
        (prev, prev_seq == taken)
    }

    /// Take the latest frame for `hwnd`, if any — the presenter's per-frame
    /// read path. Bumps the take counter so the publish gate sees the drain.
    #[must_use]
    pub fn take_frame(&self, hwnd: crate::handles::Hwnd) -> Option<SurfaceFrame> {
        let frame = {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.latest.get(&hwnd).cloned()
        };
        if frame.is_some() {
            self.taken_seq.fetch_add(1, Ordering::Relaxed);
        }
        frame
    }

    /// Frames published but not yet taken (saturating). The host presenter
    /// re-requests a redraw when this is nonzero after a take — closing the
    /// publish-between-take-and-redraw race the wake gate opens.
    #[must_use]
    pub fn pending_frames(&self) -> u64 {
        self.published_seq
            .load(Ordering::Relaxed)
            .saturating_sub(self.taken_seq.load(Ordering::Relaxed))
    }

    /// Recycle a superseded presented buffer into the spare pool (host side).
    pub fn store_spare(&self, hwnd: crate::handles::Hwnd, buffer: Vec<u32>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.spare_buffers.insert(hwnd, buffer);
    }

    /// Take a recycled spare buffer for `hwnd` (guest paint side).
    pub(crate) fn take_spare(&self, hwnd: crate::handles::Hwnd) -> Option<Vec<u32>> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.spare_buffers.remove(&hwnd)
    }

    /// Mirror the guest-side z-order + revisions into the host read slot.
    /// Called under the big `WinApiState` lock after every z/window-set
    /// mutation; the channel lock section is a Vec clone.
    pub(crate) fn sync_z(&self, z_rev: u64, z_order: &[crate::handles::Hwnd], windows_rev: u64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.z_rev = z_rev;
        inner.z_order.clear();
        inner.z_order.extend_from_slice(z_order);
        inner.windows_rev = windows_rev;
    }

    /// The mirrored `(z_rev, z_order)` pair — one lock, consistent snapshot.
    #[must_use]
    pub fn z_snapshot(&self) -> (u64, Vec<u64>) {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            inner.z_rev,
            inner.z_order.iter().map(|h| h.as_u64()).collect(),
        )
    }

    /// The mirrored window-set revision.
    #[must_use]
    pub fn windows_rev(&self) -> u64 {
        self.inner.lock().map_or(0, |inner| inner.windows_rev)
    }

    /// The mirrored z-order revision.
    #[must_use]
    pub fn z_rev(&self) -> u64 {
        self.inner.lock().map_or(0, |inner| inner.z_rev)
    }

    /// Record one host-present duration (ns) — presenter side, channel lock.
    pub fn record_present(&self, ns: u128) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.present_ns = inner.present_ns.saturating_add(ns);
        inner.present_ns_last = ns;
    }

    /// The most recent host-present duration (ns), for the profile dump.
    #[must_use]
    pub fn present_ns_last(&self) -> u128 {
        self.inner.lock().map_or(0, |inner| inner.present_ns_last)
    }

    /// Accumulated host-present wall time (ns), for the profile dump.
    #[must_use]
    pub fn present_ns(&self) -> u128 {
        self.inner.lock().map_or(0, |inner| inner.present_ns)
    }
}

impl Default for PresentChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages per-window compositing surfaces and frame publishing.
pub struct PresentState {
    /// Persistent composite surface per HWND (scratch buffer for accumulating blits).
    ///
    /// `pub(crate)`: the host reads published frames (`published`) and wake
    /// hooks; the scratch surfaces are internal to the blit pipeline.
    pub(crate) surfaces: ahash::HashMap<crate::handles::Hwnd, WindowSurface>,
    /// Last published snapshot per HWND — the guest-side hand-back source.
    ///
    /// The HOST presenter never reads this map: its per-frame view is the
    /// [`PresentChannel`] (`channel`), which `publish` mirrors into. This map
    /// exists for the zero-copy hand-back protocol (`ensure_surface` reclaims
    /// the buffer when the surface is empty) and tests.
    pub published: ahash::HashMap<crate::handles::Hwnd, SurfaceFrame>,
    /// Monotonically increasing generation counter.
    pub generation: u64,
    /// The host-presenter channel: latest-wins frame slot, spare pool, z/window
    /// revision mirrors, presenter timing — behind its own tiny mutex so the
    /// presenter never locks `WinApiState` per frame (ADR-0003).
    pub(crate) channel: Arc<PresentChannel>,
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
    /// Spare buffers moved here from the docs: the pool lives in the
    /// [`PresentChannel`] now (the presenter recycles into it without the big
    /// lock); the guest side consumes it in [`Self::ensure_surface`].
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
    /// Monotonic fingerprint of the guest's top-level window SET: bumped on
    /// every create/destroy of a parentless window (see
    /// [`Self::register_top_level`] / [`Self::unregister_top_level`]).
    ///
    /// The host presenter's Frame handler reconciles its winit window
    /// registry ONLY when this revision changes (the reconcile-on-change
    /// latch) — an idle repaint of an unchanged window set skips the
    /// enumerate+diff entirely. The guest-side window records stay the
    /// source of truth; this counter is a cheap change detector, monotonic
    /// and never reused. Read via `GuestHandle::windows_rev`.
    pub windows_rev: u64,
    /// Top-level guest window handles in back-to-front z-order: index 0 is
    /// the backmost window, the last element the topmost. Starts as the
    /// guest creation order ([`Self::register_top_level`] stacks each new
    /// top-level on top); `SetWindowPos` HWND_TOP / HWND_BOTTOM reorder it
    /// via [`Self::z_order_to_top`] / [`Self::z_order_to_bottom`].
    ///
    /// The host presenter mirrors this ordering into its NSWindows when
    /// [`Self::z_rev`] changes. Read via `GuestHandle::top_level_z_order`.
    pub z_order: Vec<crate::handles::Hwnd>,
    /// Monotonic fingerprint of the guest's top-level z-order: bumped on
    /// every create/destroy of a parentless window AND every `SetWindowPos`
    /// HWND_TOP/HWND_BOTTOM z-change. Read via `GuestHandle::z_rev`.
    pub z_rev: u64,
    /// Revision latch for the pull-based repaint: content revisions per
    /// top-level vs what was last published, diffed by
    /// [`Self::reconcile_and_publish`] at the idle boundary.
    pub(crate) revisions: WindowRevisions,
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
            .field("pending_publishes", &self.pending_publishes.len())
            .field("windows_rev", &self.windows_rev)
            .field("z_order_count", &self.z_order.len())
            .field("z_rev", &self.z_rev)
            .field("content_rev_count", &self.revisions.content_rev.len())
            .field(
                "last_published_rev_count",
                &self.revisions.last_published_rev.len(),
            )
            .finish()
    }
}

impl PresentState {
    /// Create a new, empty `PresentState` with its own [`PresentChannel`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            surfaces: ahash::HashMap::new(),
            published: ahash::HashMap::new(),
            generation: 0,
            channel: Arc::new(PresentChannel::new()),
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
            pending_publishes: std::collections::HashSet::new(),
            windows_rev: 0,
            z_order: Vec::new(),
            z_rev: 0,
            revisions: WindowRevisions {
                content_rev: ahash::HashMap::new(),
                last_published_rev: ahash::HashMap::new(),
            },
        }
    }

    /// The host-presenter channel Arc — `RuntimeSession::guest_handle` clones
    /// this once so presenter-side frame reads bypass the big lock entirely.
    #[must_use]
    pub fn channel_arc(&self) -> Arc<PresentChannel> {
        Arc::clone(&self.channel)
    }

    /// Accumulated host-present wall time (ns) — lives in the channel (the
    /// winit thread writes it without the big lock).
    #[must_use]
    pub fn channel_present_ns(&self) -> u128 {
        self.channel.present_ns()
    }

    /// The most recent host-present duration (ns), from the channel.
    #[must_use]
    pub fn channel_present_ns_last(&self) -> u128 {
        self.channel.present_ns_last()
    }

    /// Ensure a surface exists for `hwnd` with the given dimensions.
    /// Resizes or reallocates if dimensions changed; never shrinks.
    ///
    /// The buffer is pitch-padded: `stride = padded_stride(width)` (a
    /// multiple of 64 unless `WIE_SURFACE_PAD=0`), so `stride * 4` satisfies
    /// wgpu's 256-byte row alignment and the presenter's full-frame upload is
    /// zero-copy. Every row-major writer must index `row * stride + x`.
    pub fn ensure_surface(&mut self, hwnd: crate::handles::Hwnd, width: u32, height: u32) {
        let stride = padded_stride(width);
        let needed = usize::try_from(stride.checked_mul(height).unwrap_or(0)).unwrap_or(0);
        // B1 double-buffer hand-back: a prior publish moved the painted buffer
        // into the published Arc, leaving the surface empty. Hand the buffer
        // back as the next paint base so the composite keeps accumulating
        // across publishes — zero-copy (`Arc::try_unwrap`) when the host is
        // not holding the previous frame, a clone otherwise. This is what
        // keeps gui_blit's multi-publish paint cycle (parent BitBlt → child
        // control paints, each publishing) byte-identical.
        //
        // Lock serialization: this runs on the guest thread while it holds the
        // `WinApiState` lock (paint handlers call `ensure_surface`), and the
        // presenter's `take_frame` takes the SAME lock — the remove below and a
        // presenter take can never interleave. The remove does open a window
        // where a take sees no entry for `hwnd` (between here and the next
        // `publish`): it returns `None` and the presenter skips — correct,
        // because the old content is being replaced — and `publish` re-adds
        // its entry BEFORE firing the wake, so a take after a publish always
        // sees the fresh frame and no frame is lost.
        //
        // Refcount math for the zero-copy path: the published Arc sits at
        // refcount 1 when no `record` slot shares it and neither the
        // presenter's take nor the app's `last_presented_pixels` keep-alive
        // pins THIS frame's Arc (the keep-alive pins the last frame the
        // presenter actually presented, which may be one or more publishes
        // behind). `try_unwrap` then moves the Vec out with no copy.
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
                    // The paint base must carry the JUST-published frame's
                    // pixels. A recycled spare's CONTENT is one publish stale
                    // (it is the channel slot displaced by this publish), so
                    // the spare is only the DESTINATION allocation — the
                    // current frame is copied into it (zero-alloc, one copy).
                    // No spare → count a real clone.
                    if let Some(mut spare) = self.channel.take_spare(hwnd) {
                        spare.resize(needed, 0);
                        let src = &shared[..];
                        if let Some(dst) = spare.get_mut(0..src.len().min(needed)) {
                            dst.copy_from_slice(&src[..dst.len()]);
                        }
                        spare
                    } else {
                        self.hand_back_clone = self.hand_back_clone.saturating_add(1);
                        if needed < 1024 * 1024 {
                            shared.to_vec()
                        } else {
                            vec![0u32; needed]
                        }
                    }
                }
            };
        }
        let entry = self.surfaces.entry(hwnd).or_insert_with(|| WindowSurface {
            width,
            stride,
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
        if entry.width != width || entry.height != height || entry.stride != stride {
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
            entry.stride = stride;
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

    /// Q9/C: hand the next pooled `WindowSurface` slice to the D3D9/wGL present
    /// path as its render target. Ensures the surface and returns `&mut [u32]`
    /// to the pooled `Vec<u32>` (the one `ensure_surface` will hand back via
    /// Q2/D) so the caller can raster directly into it without an intermediate
    /// `Vec<u32>` copy. If the frame size differs from the surface, the caller
    /// must `stretch_nearest` directly into this slice (no temp alloc).
    #[allow(dead_code)]
    pub(crate) fn pooled_surface_mut(
        &mut self,
        hwnd: crate::handles::Hwnd,
        width: u32,
        height: u32,
    ) -> Option<(&mut [u32], u32)> {
        self.ensure_surface(hwnd, width, height);
        self.surfaces
            .get_mut(&hwnd)
            .map(|s| (s.pixels.as_mut_slice(), s.stride))
    }

    /// Q9/C: pooled target with dimensions — returns `(pixels, width, height, stride)`.
    #[allow(dead_code)]
    pub(crate) fn pooled_target_with_dims(
        &mut self,
        hwnd: crate::handles::Hwnd,
        width: u32,
        height: u32,
    ) -> Option<(&mut [u32], u32, u32, u32)> {
        self.ensure_surface(hwnd, width, height);
        let s = self.surfaces.get_mut(&hwnd)?;
        Some((s.pixels.as_mut_slice(), s.width, s.height, s.stride))
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
                    *acc = union_rect(*acc, rect);
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
        let (width, stride, height, pixels) = {
            // B1: publish WITHOUT a pixel clone — a pointer move of the painted
            // buffer into the Arc (no 4 MB copy under the WinAPI mutex). The
            // buffer returns to the surface on the next paint via
            // `ensure_surface`'s hand-back, preserving accumulation.
            let pixels: Arc<Vec<u32>> = Arc::from(std::mem::take(&mut surface.pixels));
            (surface.width, surface.stride, surface.height, pixels)
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
        // Sampled content probe: what is actually leaving the guest side?
        // Every 32nd publish logs three pixels (top-left, centre,
        // bottom-right) of the published frame.
        if self.generation.is_multiple_of(32) {
            let w = usize::try_from(width).unwrap_or(1).max(1);
            let stride_us = usize::try_from(stride).unwrap_or(w).max(w);
            let h = usize::try_from(height).unwrap_or(1).max(1);
            let px = |x: usize, y: usize| -> u32 {
                pixels
                    .get(y.wrapping_mul(stride_us).wrapping_add(x))
                    .copied()
                    .unwrap_or(0xDEAD_BEEF)
            };
            tracing::debug!(
                target: "wie_gdi",
                tl = format_args!("0x{:08x}", px(0, 0)),
                mid = format_args!("0x{:08x}", px(w / 2, h / 2)),
                br = format_args!("0x{:08x}", px(w - 1, h - 1)),
                "publish pixels"
            );
        }
        tracing::debug!(
            target: "wiegui",
            hwnd = hwnd.as_u64(),
            width,
            height,
            generation = self.generation,
            elapsed_us = match t0 {
                Some(t) => u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX),
                None => 0,
            },
            "frame published"
        );
        let frame = SurfaceFrame {
            width,
            stride,
            height,
            pixels,
            background_color,
            region,
        };
        if let Some(record) = self.record.as_mut() {
            // The record slot holds the most recent frame only and is
            // overwritten on every publish; it is never mutated or read by the
            // host in a way that needs a private copy, so share the published
            // Arc instead of cloning up to 4 MB per recorded frame. The
            // `ensure_surface` hand-back below will see the shared Arc and
            // clone once for the next paint base — one copy per cycle either
            // way, but no extra allocation + memcpy here.
            **record = frame.clone();
        }
        // The channel mirror shares the pixels Arc (a refcount bump, never a
        // pixel copy): the host takes from the channel without the big lock.
        let channel_frame = frame.clone();
        let old = self.published.insert(hwnd, frame);
        // Reclaim the previous published buffer if the host has released it —
        // a spare for the next `ensure_surface` so the hand-back never clones.
        if let Some(old_frame) = old
            && let Ok(vec) = Arc::try_unwrap(old_frame.pixels)
        {
            self.channel.store_spare(hwnd, vec);
        }
        // Mirror the frame into the host channel (latest-wins slot) and decide
        // the wake. The gate fires only when the host has drained every
        // earlier publish (the presenter's in-flight redraw drains the latest
        // slot anyway); the displaced channel slot's buffer is reclaimed as a
        // spare when nothing holds it. The published insert above happens
        // BEFORE this wake — a presenter taking on the wake always sees the
        // fresh frame.
        let (old_slot, should_wake) = self.channel.publish_frame(hwnd, channel_frame);
        if let Some(old_slot) = old_slot
            && let Ok(vec) = Arc::try_unwrap(old_slot.pixels)
        {
            self.channel.store_spare(hwnd, vec);
        }
        if should_wake && let Some(wake) = &self.wake {
            wake();
        }
    }

    /// Blit a whole 0RGB frame into `hwnd`'s surface and publish it — the
    /// shared tail of the D3D9 `Present` and `wglSwapBuffers` frame paths.
    ///
    /// A frame sized exactly like the ensured surface copies row-major (one
    /// `copy_from_slice` when the pitch matches, per-row otherwise — the
    /// surface pitch is 64-padded, the source frame is not); anything else is
    /// nearest-neighbour stretched to the surface dimensions. No allocation:
    /// pixels go straight into the retained surface buffer (Q9/C: pooled
    /// WindowSurface slice as render target — no intermediate `Vec<u32>`
    /// copy; stretch writes directly into the pooled slice when sizes
    /// differ).
    pub(crate) fn blit_frame(
        &mut self,
        hwnd: crate::handles::Hwnd,
        frame: &[u32],
        frame_width: u32,
        frame_height: u32,
    ) {
        if let Some(surface) = self.surfaces.get_mut(&hwnd) {
            commit::blit_frame_into(
                &mut surface.pixels,
                surface.stride,
                surface.width,
                surface.height,
                frame,
                frame_width,
                frame_height,
            );
        }
        self.publish(hwnd);
    }

    /// Wave 2 (Option A1): enqueue a D3D9 Present commit — hand the finished
    /// backbuffer to the commit slot (a pointer move into the `Arc`; no pixel
    /// copy) and return the buffer the emu thread should draw the next frame
    /// into (a recycled spare when one exists, else an empty Vec the caller
    /// sizes). The commit thread performs the stretch + publish off the big
    /// lock. Only called when [`PresentChannel::commit_enabled`].
    pub(crate) fn enqueue_present_commit(
        &mut self,
        hwnd: crate::handles::Hwnd,
        bb_w: u32,
        bb_h: u32,
        win_w: u32,
        win_h: u32,
        backbuffer: Vec<u32>,
    ) -> Vec<u32> {
        let background = self
            .background_colors
            .get(&hwnd)
            .copied()
            .unwrap_or(DEFAULT_BACKGROUND_COLOR);
        self.channel.commit.enqueue(commit::CommitJob {
            hwnd,
            pixels: Arc::new(backbuffer),
            bb_w,
            bb_h,
            win_w,
            win_h,
            background,
        })
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

    /// Bump the top-level content revision for `hwnd` and wake the presenter —
    /// the push half of the pull-based repaint latch.
    ///
    /// Every visible-state mutation routes through here (the control
    /// invalidation seams, the SetWindowText/WM_SETTEXT handlers, the
    /// font/show-window paths): the mutation marks its dirty region exactly as
    /// before, and this records that the window's content CHANGED since the
    /// last publish. The idle loop's [`Self::reconcile_and_publish`] then
    /// republishes any top-level whose revision advanced, so a visible change
    /// can never silently fail to reach the host (the stale-surface class of
    /// repaint bugs).
    ///
    /// Children resolve to their top-level ancestor — the surface that
    /// actually composites them, so a control's mutation marks the frame the
    /// host presents. Unknown windows and the legacy fake window resolve to
    /// nothing and are silently ignored: they have no surface to publish. The
    /// wake fires the same stored callback the publish path fires
    /// (`request_host_sync`'s idempotent pattern); a headless run with no wake
    /// is a silent no-op.
    pub fn request_paint(state: &mut WinApiState, hwnd: u64) {
        let Some(top) = crate::gdi32::resolve_window_ancestor(state, hwnd) else {
            return;
        };
        let present = state.present();
        let before = present.revisions.content_rev.get(&top.hwnd).copied();
        let after = ContentRev(before.map_or(0, |r| r.0).wrapping_add(1));
        assert_mutation_bumped_rev(before, after);
        present.revisions.content_rev.insert(top.hwnd, after);
        if let Some(wake) = &present.wake {
            wake();
        }
    }

    /// Sync the z-order / revision mirrors into the host channel. Call after
    /// every mutation of `z_order`, `z_rev`, or `windows_rev` — the channel
    /// clone is what the presenter actually reads.
    fn sync_channel_z(&self) {
        self.channel
            .sync_z(self.z_rev, &self.z_order, self.windows_rev);
    }

    /// Register a newly created top-level window at the TOP of the z-order.
    ///
    /// Bumps BOTH revisions: the window-set revision (the presenter
    /// reconciles its host window registry on this create) and the z-order
    /// revision (a new topmost window re-stacks the whole set). The guest
    /// `CreateWindowExA/W` handlers call this for `parent_handle == 0`
    /// windows only — children composite into their parent's surface and
    /// have no host window.
    pub fn register_top_level(&mut self, hwnd: crate::handles::Hwnd) {
        self.z_order.push(hwnd);
        self.z_rev = self.z_rev.wrapping_add(1);
        self.windows_rev = self.windows_rev.wrapping_add(1);
        self.sync_channel_z();
    }

    /// Unregister a destroyed top-level window.
    ///
    /// Always bumps the window-set revision (the presenter drops the stale
    /// host window); the z-order revision bumps only when the window was
    /// actually tracked. The guest `DestroyWindow` handler calls this for
    /// parentless windows at the same site it wakes the presenter.
    pub fn unregister_top_level(&mut self, hwnd: crate::handles::Hwnd) {
        let len = self.z_order.len();
        self.z_order.retain(|h| *h != hwnd);
        if self.z_order.len() != len {
            self.z_rev = self.z_rev.wrapping_add(1);
        }
        self.windows_rev = self.windows_rev.wrapping_add(1);
        // Drop the revision bookkeeping with the window: a destroyed top-level
        // must not stay stale (an idle reconcile would republish its ghost
        // surface) or leak its entries.
        self.revisions.content_rev.remove(&hwnd);
        self.revisions.last_published_rev.remove(&hwnd);
        self.sync_channel_z();
    }

    /// Move `hwnd` to the TOP of the z-order (`SetWindowPos` HWND_TOP).
    ///
    /// Returns whether the order changed — a window already on top (or not
    /// tracked at all) is a no-op that must NOT bump [`Self::z_rev`] (a
    /// spurious bump would re-trigger the host reorder for no change).
    pub fn z_order_to_top(&mut self, hwnd: crate::handles::Hwnd) -> bool {
        if self.z_order.last() == Some(&hwnd) {
            return false;
        }
        let Some(pos) = self.z_order.iter().position(|h| *h == hwnd) else {
            return false;
        };
        self.z_order.remove(pos);
        self.z_order.push(hwnd);
        self.z_rev = self.z_rev.wrapping_add(1);
        self.sync_channel_z();
        true
    }

    /// Move `hwnd` to the BOTTOM of the z-order (`SetWindowPos` HWND_BOTTOM).
    ///
    /// Returns whether the order changed (see [`Self::z_order_to_top`]).
    pub fn z_order_to_bottom(&mut self, hwnd: crate::handles::Hwnd) -> bool {
        if self.z_order.first() == Some(&hwnd) {
            return false;
        }
        let Some(pos) = self.z_order.iter().position(|h| *h == hwnd) else {
            return false;
        };
        self.z_order.remove(pos);
        self.z_order.insert(0, hwnd);
        self.z_rev = self.z_rev.wrapping_add(1);
        self.sync_channel_z();
        true
    }

    /// B9: record one BitBlt mask-copy (`mask_bgra_to_0rgb`) duration (ns).
    pub fn record_blit_copy(&mut self, ns: u128) {
        self.blit_copy_ns = self.blit_copy_ns.saturating_add(ns);
        self.blit_copy_ns_last = ns;
    }

    /// B9: record one host present (upload + present) duration (ns).
    ///
    /// Written by the host presenter through the channel — no big lock.
    pub fn record_present(&mut self, ns: u128) {
        self.channel.record_present(ns);
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

    /// Publish every top-level whose content revision advanced past its last
    /// published revision — the pull half of the repaint latch.
    ///
    /// Runs at the empty-queue idle boundary immediately after
    /// [`Self::drain_pending_publishes`]. A mutation that already painted and
    /// deferred a frame is a no-op here (the drain's publish moved the surface
    /// buffer into the Arc, so [`Self::publish`] early-returns on the empty
    /// buffer); a mutation whose paint produced no deferred publish still
    /// republishes the surface, so the host always sees the change. Advances
    /// `last_published_rev` for every stale window, so a caught-up window is
    /// skipped until the next mutation. Returns how many windows were
    /// considered (whether or not `publish` emitted).
    pub fn reconcile_and_publish(&mut self) -> usize {
        let stale: Vec<(crate::handles::Hwnd, ContentRev)> = self
            .revisions
            .content_rev
            .iter()
            .filter_map(|(hwnd, rev)| {
                (self.revisions.last_published_rev.get(hwnd) != Some(rev)).then_some((*hwnd, *rev))
            })
            .collect();
        let count = stale.len();
        for (hwnd, rev) in stale {
            self.publish(hwnd);
            self.revisions.last_published_rev.insert(hwnd, rev);
        }
        count
    }
}

/// Debug-only invariant: [`PresentState::request_paint`] must advance the
/// content revision. A mutation that fails to bump leaves the window's
/// revision equal to its last published revision, so the idle reconcile would
/// skip it and the visible change could never reach the host — this assertion
/// catches that silently-dropped-frame class in debug builds. The release
/// twin below is a no-op so call sites stay uniform.
#[cfg(debug_assertions)]
fn assert_mutation_bumped_rev(before: Option<ContentRev>, after: ContentRev) {
    debug_assert!(
        before.is_none_or(|r| r.0 != after.0),
        "request_paint did not advance the content revision (before {before:?}, after {after:?})"
    );
}

/// Release twin of `assert_mutation_bumped_rev`: the invariant checks only in
/// debug builds.
#[cfg(not(debug_assertions))]
fn assert_mutation_bumped_rev(_before: Option<ContentRev>, _after: ContentRev) {}

impl Default for PresentState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{ContentRev, DEFAULT_BACKGROUND_COLOR, PresentState, SurfaceFrame};
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

    /// Registering a top-level stacks it on TOP of the z-order (the last
    /// element) and bumps BOTH revisions — the window-set fingerprint the
    /// presenter's reconcile keys on, and the z-order fingerprint.
    #[test]
    fn register_top_level_stacks_top_and_bumps_both_revs() {
        let mut state = PresentState::new();
        let a = Hwnd::from(1);
        let b = Hwnd::from(2);

        state.register_top_level(a);
        assert_eq!(state.windows_rev, 1);
        assert_eq!(state.z_rev, 1);
        assert_eq!(
            state.z_order,
            vec![a],
            "the first top-level is the only row"
        );

        state.register_top_level(b);
        assert_eq!(state.windows_rev, 2);
        assert_eq!(state.z_rev, 2);
        assert_eq!(
            state.z_order,
            vec![a, b],
            "creation order, newest on top (back-to-front)"
        );
    }

    /// Unregistering a top-level removes it from the z-order and bumps the
    /// window-set revision unconditionally (the host must drop the stale
    /// window even if the destroy raced a create that never registered).
    #[test]
    fn unregister_top_level_removes_from_z_order() {
        let mut state = PresentState::new();
        let a = Hwnd::from(1);
        let b = Hwnd::from(2);
        let c = Hwnd::from(3);
        state.register_top_level(a);
        state.register_top_level(b);
        state.register_top_level(c);
        let rev_before = state.z_rev;

        state.unregister_top_level(b);

        assert_eq!(state.z_order, vec![a, c], "the destroyed window is gone");
        assert_eq!(state.windows_rev, 4);
        assert_eq!(
            state.z_rev,
            rev_before + 1,
            "a tracked top-level destroy bumps the z-order revision"
        );
    }

    /// Unregistering a window never registered still bumps the window-set
    /// revision (the host reconcile must run) but leaves the z-order
    /// revision alone (nothing re-stacked).
    #[test]
    fn unregister_unknown_window_bumps_set_rev_only() {
        let mut state = PresentState::new();
        state.register_top_level(Hwnd::from(1));
        let z_rev_before = state.z_rev;

        state.unregister_top_level(Hwnd::from(99));

        assert_eq!(state.windows_rev, 2, "the set rev always bumps");
        assert_eq!(
            state.z_rev, z_rev_before,
            "an untracked destroy cannot re-stack the z-order"
        );
    }

    /// SetWindowPos HWND_TOP moves a window to the top of the z-order; a
    /// window already on top is a no-op that must NOT bump the revision (a
    /// spurious bump would re-trigger the host reorder for no change).
    #[test]
    fn z_order_top_moves_window_to_the_top() {
        let mut state = PresentState::new();
        let a = Hwnd::from(1);
        let b = Hwnd::from(2);
        let c = Hwnd::from(3);
        state.register_top_level(a);
        state.register_top_level(b);
        state.register_top_level(c);
        let rev_before = state.z_rev;

        assert!(state.z_order_to_top(a));
        assert_eq!(state.z_order, vec![b, c, a]);
        assert_eq!(state.z_rev, rev_before + 1);

        // Repeating the same move changes nothing.
        assert!(
            !state.z_order_to_top(a),
            "a window already on top must not bump z_rev"
        );
        assert_eq!(state.z_rev, rev_before + 1);
        assert_eq!(state.z_order, vec![b, c, a]);
    }

    /// SetWindowPos HWND_BOTTOM moves a window to the back of the z-order.
    #[test]
    fn z_order_bottom_moves_window_to_the_back() {
        let mut state = PresentState::new();
        let a = Hwnd::from(1);
        let b = Hwnd::from(2);
        let c = Hwnd::from(3);
        state.register_top_level(a);
        state.register_top_level(b);
        state.register_top_level(c);
        let rev_before = state.z_rev;

        assert!(state.z_order_to_bottom(c));
        assert_eq!(state.z_order, vec![c, a, b]);
        assert_eq!(state.z_rev, rev_before + 1);

        // A window already at the back is a no-op.
        assert!(!state.z_order_to_bottom(c));
        assert_eq!(state.z_rev, rev_before + 1);
    }

    /// Z-order operations never touch the window-SET revision: they reorder
    /// existing host windows but change no membership, so the reconcile
    /// latch must not fire.
    #[test]
    fn z_reorder_leaves_windows_rev_untouched() {
        let mut state = PresentState::new();
        let a = Hwnd::from(1);
        let b = Hwnd::from(2);
        state.register_top_level(a);
        state.register_top_level(b);
        let set_rev = state.windows_rev;

        state.z_order_to_top(a);
        state.z_order_to_bottom(b);

        assert_eq!(
            state.windows_rev, set_rev,
            "a SetWindowPos z-change never changes the window SET"
        );
    }

    /// An empty frame for the headless record slot.
    fn empty_frame() -> SurfaceFrame {
        SurfaceFrame {
            width: 0,
            stride: 0,
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
        assert_eq!(frame.stride, 256, "the row pitch is 64-padded (ADR-0001)");
        assert_eq!(
            frame.pixels.len(),
            usize::try_from(frame.stride * frame.height).unwrap_or(0),
            "the pixel bytes still carry the FULL padded frame (region is a hint)"
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

    /// The hand-back keeps the composite accumulating across publishes, and
    /// in the steady state it is zero-ALLOC, not zero-copy: the presenter
    /// channel pins a clone of every published frame (the host takes it
    /// without the big lock, at any time), so the published Arc can no longer
    /// be unwrapped on the next paint. The recycled allocation is the
    /// DISPLACED channel slot: publishing frame N+1 frees the channel's
    /// frame-N clone, which lands in the spare pool and comes back as the
    /// next paint base — the very first cycle still pays one clone.
    #[test]
    fn hand_back_moves_the_published_buffer_zero_copy() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(30);
        state.ensure_surface(hwnd, 64, 32);
        // Paint a recognizable pattern, then publish: the buffer leaves the
        // surface and lands in the published Arc plus the channel's slot
        // clone (a refcount bump, not a copy).
        if let Some(surf) = state.surfaces.get_mut(&hwnd) {
            for (i, px) in surf.pixels.iter_mut().enumerate() {
                *px = u32::try_from(i % 7 + 1).unwrap_or(0);
            }
        }
        state.publish(hwnd);
        let published_ptr = state
            .published
            .get(&hwnd)
            .expect("published frame")
            .pixels
            .as_ptr();

        // First paint cycle: the channel pins its clone, so `try_unwrap`
        // fails and no spare exists yet — the fallback CLONES (one alloc,
        // first cycle only). The composite still accumulates.
        state.ensure_surface(hwnd, 64, 32);
        assert_eq!(state.hand_back_unwrap, 0, "the channel pins every frame");
        assert_eq!(
            state.hand_back_clone, 1,
            "no spare yet: one first-cycle clone"
        );
        let surface = state.surfaces.get(&hwnd).expect("surface");
        assert!(
            surface.pixels.iter().any(|&px| px != 0),
            "the reclaimed buffer carries the previously painted content"
        );
        // The published map is empty until the repaint republishes: a
        // presenter take in this window sees None and skips (the old content
        // is being replaced); the next publish re-adds before waking it.
        assert!(
            !state.published.contains_key(&hwnd),
            "the reclaimed entry is gone until the repaint republishes"
        );

        // Republish: the clone flows through a fresh Arc, and the channel's
        // displaced slot (the FIRST frame, still holding the original
        // allocation) lands in the spare pool.
        state.publish(hwnd);
        let republished = state.published.get(&hwnd).expect("republished frame");
        assert_ne!(
            republished.pixels.as_ptr(),
            published_ptr,
            "the republish wraps the cloned buffer in a new Arc"
        );

        // Second paint cycle: `try_unwrap` fails again (the channel pins the
        // new frame), but the displaced first-frame buffer is now a spare —
        // the hand-back takes it with NO clone and NO allocation, and the
        // original published allocation comes back as the paint base.
        state.ensure_surface(hwnd, 64, 32);
        assert_eq!(
            state.hand_back_clone, 1,
            "steady-state hand-backs recycle spares"
        );
        let surface = state.surfaces.get(&hwnd).expect("surface");
        assert_eq!(
            surface.pixels.as_ptr(),
            published_ptr,
            "the displaced channel slot's allocation returns as the paint base"
        );
        assert!(
            surface.pixels.iter().any(|&px| px != 0),
            "the spare carries the painted content (the composite keeps accumulating)"
        );
    }

    /// Every publish wraps its buffer in a FRESH Arc allocation: the
    /// presenter's `Arc::ptr_eq` present-skip compares consecutive takes, and
    /// a repaint must never be mistaken for an unchanged frame. Both Arcs are
    /// kept alive here so the comparison is deterministic (a freed Arc's
    /// address can be reused by the allocator, so comparing against one would
    /// be flaky).
    #[test]
    fn each_publish_wraps_the_buffer_in_a_fresh_arc() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(32);
        state.ensure_surface(hwnd, 64, 32);
        state.publish(hwnd);
        // The presenter's take_frame clone: holds the first frame's Arc alive
        // while the second publish runs (it also forces the clone fallback on
        // the reclaim below, which this test does not assert on).
        let first: Arc<Vec<u32>> =
            Arc::clone(&state.published.get(&hwnd).expect("first frame").pixels);

        state.ensure_surface(hwnd, 64, 32);
        state.publish(hwnd);
        let second = state
            .published
            .get(&hwnd)
            .expect("second frame")
            .pixels
            .clone();
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a republish always allocates a new Arc — the ptr_eq present-skip never misfires"
        );
    }

    /// The fallback CLONES when the host still holds the published Arc (the
    /// presenter's take_frame clone, or the app's last-presented keep-alive):
    /// the scratch buffer is a fresh allocation carrying the same pixels, so
    /// the composite still accumulates — at the cost of one copy.
    #[test]
    fn hand_back_clones_when_the_host_still_holds_the_frame() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(31);
        state.ensure_surface(hwnd, 64, 32);
        if let Some(surf) = state.surfaces.get_mut(&hwnd) {
            for (i, px) in surf.pixels.iter_mut().enumerate() {
                *px = u32::try_from(i % 7 + 1).unwrap_or(0);
            }
        }
        state.publish(hwnd);
        // The presenter's take_frame clones the SurfaceFrame, bumping the
        // pixel Arc's refcount — that clone (or the keep-alive) is exactly the
        // documented blocker that forces the fallback.
        let held: Arc<Vec<u32>> =
            Arc::clone(&state.published.get(&hwnd).expect("published").pixels);
        let published_ptr = state
            .published
            .get(&hwnd)
            .expect("published")
            .pixels
            .as_ptr();

        state.ensure_surface(hwnd, 64, 32);
        assert_eq!(state.hand_back_unwrap, 0);
        assert_eq!(state.hand_back_clone, 1);
        let surface = state.surfaces.get(&hwnd).expect("surface");
        assert_ne!(
            surface.pixels.as_ptr(),
            published_ptr,
            "the clone fallback allocates a fresh buffer"
        );
        assert_eq!(
            surface.pixels, *held,
            "the cloned buffer carries the same pixels (the composite keeps accumulating)"
        );
    }

    /// A stale top-level (content revision ahead of its last published
    /// revision) is republished by `reconcile_and_publish`; the last-published
    /// revision catches up, so the same window is then skipped.
    #[test]
    fn reconcile_publishes_stale_top_levels_and_advances_their_rev() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(40);
        state.ensure_surface(hwnd, 32, 16);
        // Paint recognizable content so the reconcile's republish carries it.
        if let Some(surf) = state.surfaces.get_mut(&hwnd) {
            for px in &mut surf.pixels {
                *px = 0x00FF_FFFF;
            }
        }
        state.revisions.content_rev.insert(hwnd, ContentRev(3));
        state
            .revisions
            .last_published_rev
            .insert(hwnd, ContentRev(2));

        assert_eq!(
            state.reconcile_and_publish(),
            1,
            "the stale top-level is republished once"
        );
        assert!(state.published.contains_key(&hwnd));
        assert_eq!(
            state.revisions.last_published_rev.get(&hwnd),
            Some(&ContentRev(3)),
            "the last-published revision catches up to the content revision"
        );

        assert_eq!(
            state.reconcile_and_publish(),
            0,
            "a caught-up window is skipped until the next mutation"
        );
    }

    /// A window with no `content_rev` entry was never mutated — the reconcile
    /// diff iterates the mutation set, not the window registry.
    #[test]
    fn reconcile_ignores_windows_without_a_content_revision() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(41);
        state.ensure_surface(hwnd, 8, 8);
        state.publish(hwnd);
        assert_eq!(state.reconcile_and_publish(), 0);
    }

    /// A stale top-level with NO surface still catches up: the publish is a
    /// no-op, but the revision must not stay stale forever — an idle loop
    /// would otherwise republish it on every boundary.
    #[test]
    fn reconcile_catches_up_revisions_even_without_a_surface() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(42);
        state.revisions.content_rev.insert(hwnd, ContentRev(1));

        assert_eq!(state.reconcile_and_publish(), 1);
        assert!(
            !state.published.contains_key(&hwnd),
            "no surface means nothing to publish"
        );
        assert_eq!(
            state.revisions.last_published_rev.get(&hwnd),
            Some(&ContentRev(1))
        );
        assert_eq!(state.reconcile_and_publish(), 0);
    }

    /// `unregister_top_level` drops a destroyed window's revision
    /// bookkeeping: it must not stay stale (an idle reconcile would
    /// republish its ghost surface) or leak its entries.
    #[test]
    fn unregister_top_level_drops_the_revision_bookkeeping() {
        let mut state = PresentState::new();
        let hwnd = Hwnd::from(43);
        state.revisions.content_rev.insert(hwnd, ContentRev(5));
        state
            .revisions
            .last_published_rev
            .insert(hwnd, ContentRev(5));

        state.unregister_top_level(hwnd);

        assert!(!state.revisions.content_rev.contains_key(&hwnd));
        assert!(!state.revisions.last_published_rev.contains_key(&hwnd));
    }
}
