//! Winit application handler — displays the guest windows and forwards input.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wie_runtime::GuestHandle;
use wie_runtime::MenuNode;
use wie_runtime::RuntimeSession;
use wie_runtime::{GuiControl, run_windowed};
use wie_winapi::handles::Hwnd;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::gui::input;
#[cfg(target_os = "macos")]
use crate::gui::menu_bar;

/// The `ApplicationHandler::window_event` body (input translation + the
/// per-window present/resize dispatch) lives in this submodule — see
/// [`input_events::dispatch_window_event`].
mod input_events;

/// Custom events from the guest thread to the host event loop.
///
/// `pub(crate)` so the [`menu_bar`] module can name it in its event-loop
/// proxy.
pub(crate) enum WieEvent {
    /// A frame was published and the wake callback fired. `published_at` is
    /// the wake timestamp — the event loop measures wake→redraw latency.
    Frame { published_at: Instant },
    GuestExited {
        #[allow(dead_code)]
        code: i32,
    },
    /// A macOS menu-bar item was clicked; the muda event carries the guest
    /// menu item id as a string-form [`muda::MenuId`].
    #[cfg(target_os = "macos")]
    MenuEvent(muda::MenuEvent),
}

impl WieApp {
    /// Win32 MK_* flags for mouse-message wParam: pressed buttons plus
    /// held Shift/Ctrl (Windows sends these to let apps interpret clicks).
    fn mk_flags(&self) -> u16 {
        let mut mk = self.mouse_buttons;
        if self.modifiers.shift_key() {
            mk |= input::MK_SHIFT;
        }
        if self.modifiers.control_key() {
            mk |= input::MK_CONTROL;
        }
        mk
    }

    /// The window under the cursor and its client-relative coordinates, for a
    /// mouse event delivered to `event_hwnd` (the guest HWND of the winit
    /// window the event arrived on).
    ///
    /// A window holding the mouse capture (SetCapture — a pressed BUTTON
    /// captures while down) receives every mouse message instead of the
    /// hit-tested child, matching Windows; capture is desktop-global, so the
    /// capture path runs first regardless of which top-level the event
    /// arrived on. Without capture, the hit-test descends the EVENT window's
    /// own subtree via `window_at_in` — a second top-level (a dialog in its
    /// own winit window) must resolve its own controls, not the main
    /// window's. Falls back to the primary window at (0, 0) when the guest
    /// tree yields nothing (no visible window under the cursor). `sf` is the
    /// event window's device scale factor: winit reports PHYSICAL pixels, the
    /// guest hit-test expects LOGICAL 96-DPI client pixels.
    fn mouse_target(&self, handle: &GuestHandle, event_hwnd: u64, sf: f64) -> (u64, u16, u16) {
        let (cx, cy) = self.cursor_pos;
        let (x, y) = (
            input::physical_to_logical(cx.max(0.0), sf) as i32,
            input::physical_to_logical(cy.max(0.0), sf) as i32,
        );
        if let Some((hwnd, rx, ry)) = handle.capture_target(x, y) {
            return (hwnd, rx as u16, ry as u16);
        }
        handle.window_at_in(event_hwnd, x, y).map_or(
            (self.primary_hwnd.map_or(0, |h| h.as_u64()), 0, 0),
            |(hwnd, rx, ry)| (hwnd, rx as u16, ry as u16),
        )
    }

    /// The last cursor position as integer client coordinates in LOGICAL
    /// 96-DPI pixels (for MSG.pt).
    fn cursor_pos_i32(&self) -> (i32, i32) {
        let sf = self.scale_factor();
        (
            input::physical_to_logical(self.cursor_pos.0.max(0.0), sf) as i32,
            input::physical_to_logical(self.cursor_pos.1.max(0.0), sf) as i32,
        )
    }

    /// Rebuilds the macOS menu bar when the focused guest window's menu
    /// changed.
    ///
    /// The bar mirrors the FOCUSED guest window's menu (macOS has one global
    /// bar; Windows has one per window) — `window_menu_items` resolves the
    /// focus. Runs once per `Frame` and as a best-effort immediate attempt on
    /// winit `Focused(true)`; the cheap `Vec` compare skips the AppKit work
    /// unless the menu actually changed. Clicks are forwarded back through
    /// the event-loop proxy by [`MacMenuBar`] as [`WieEvent::MenuEvent`] so
    /// the guest mutation (`WM_COMMAND`) always happens on this thread.
    #[cfg(target_os = "macos")]
    fn sync_menu_bar(&mut self) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let items = handle.window_menu_items();
        if items == self.last_menu_items {
            return;
        }
        tracing::debug!("menu items: {:?}", items);
        self.menu_bar.rebuild(items.as_slice());
        self.last_menu_items = items;
    }

    /// Mirror the guest's top-level z-order into the host NSWindows.
    ///
    /// Gated on the guest z-order revision ([`GuestHandle::z_rev`]): a change
    /// (top-level create/destroy, `SetWindowPos` HWND_TOP/HWND_BOTTOM)
    /// re-applies the stacking; idle frames skip the AppKit work. The
    /// revision is stored BEFORE the reorder so a partially-applied pass is
    /// not retried — the next z-change re-applies the full order anyway.
    #[cfg(target_os = "macos")]
    fn sync_window_z_order(&mut self) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        // The revision AND the ordered list come from ONE locked snapshot —
        // two separate reads could hand a fresh revision with a stale list if
        // a guest z-change landed between them.
        let (rev, order) = handle.z_snapshot();
        if self.last_z_rev == Some(rev) {
            return;
        }
        self.last_z_rev = Some(rev);
        // orderFront brings each window to the FRONT, so applying it in
        // back-to-front order stacks them exactly like the guest (the last
        // element ends up topmost). Windows without a host entry (top-levels
        // created through the dialog-template path, which the reconcile
        // creates but this list does not carry) are skipped — a documented
        // limitation of the minimal z-order model.
        for hwnd in order {
            let Some(window) = self
                .windows
                .values()
                .find(|rt| rt.hwnd.as_u64() == hwnd)
                .map(|rt| Arc::clone(&rt.window))
            else {
                continue;
            };
            // The cloned Arc keeps the winit window (and its NSView, which
            // `order_window_front` dereferences through a raw handle) alive
            // for the whole AppKit call — a guest DestroyWindow racing the
            // reorder can deallocate the host window only when the reconcile
            // drops the last reference, which this clone prevents.
            order_window_front(&window);
        }
    }
    /// Apply a guest-requested host-window move/resize (`SetWindowPlacement`)
    /// to the winit window.
    ///
    /// The winapi handler records the pending `(hwnd, x, y, width, height)` on
    /// the shared state and wakes the presenter; this consumes it on the
    /// event-loop thread where winit calls must run. Guest coordinates are
    /// LOGICAL 96-DPI pixels, so each value is multiplied by the window's
    /// device scale factor ([`input::logical_to_physical`]) to reach winit's
    /// physical space — the same [`MIN_GUEST_WINDOW_SIZE`] clamp guards against
    /// a degenerate or negative rect. When no winit window exists yet (the
    /// move arrived before the first published frame) the request stays
    /// pending and applies once the window is created.
    fn apply_host_geometry(&self) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        // No winit window at all: leave the request pending — the frame
        // handler re-runs this once reconciliation creates one (the startup
        // restore must not be dropped).
        if self.windows.is_empty() {
            return;
        }
        let Some((hwnd, x, y, width, height)) = handle.take_host_geometry_request() else {
            return;
        };
        // Apply to the winit window mirroring the request's hwnd; fall back
        // to the primary window when that hwnd isn't registered (a
        // SetWindowPlacement on a child/owned window has no host entry).
        let Some(rt) = self
            .windows
            .values()
            .find(|rt| rt.hwnd.as_u64() == hwnd)
            .or_else(|| self.primary_runtime())
        else {
            return;
        };
        let sf = rt.scale_factor;
        rt.window.set_outer_position(PhysicalPosition::new(
            input::logical_to_physical(x as f64, sf),
            input::logical_to_physical(y as f64, sf),
        ));
        // winit 0.30 removed set_inner_size; request_inner_size is its
        // replacement (same Into<Size> contract as with_inner_size at
        // creation, so the guest's logical size must be scaled to physical
        // here). The returned actual size is informational — the winit
        // Resized event carries the settle.
        let _ = rt.window.request_inner_size(PhysicalSize::new(
            input::logical_to_physical(width.max(MIN_GUEST_WINDOW_SIZE) as f64, sf) as u32,
            input::logical_to_physical(height.max(MIN_GUEST_WINDOW_SIZE) as f64, sf) as u32,
        ));
    }

    /// The primary window's device scale factor (physical px per logical
    /// 96-DPI px), defaulting to 1.0 before any winit window exists (no
    /// scaling has happened yet). Mouse/keyboard use the EVENT window's
    /// factor; this remains for the few paths still in primary space (the
    /// drop-point conversion).
    fn scale_factor(&self) -> f64 {
        self.primary_runtime().map_or(1.0, |rt| rt.scale_factor)
    }

    /// The runtime of the primary host window — the first window created,
    /// mirroring the guest's main window. The fallback target when an event's
    /// own window isn't registered (geometry, no-hit mouse fallback).
    fn primary_runtime(&self) -> Option<&WindowRuntime> {
        let hwnd = self.primary_hwnd?;
        self.windows.values().find(|rt| rt.hwnd == hwnd)
    }

    /// Reconcile the host window registry against the guest's top-level
    /// windows: create a winit window for each top-level that has none, and
    /// destroy the winit windows of top-levels the guest no longer has.
    ///
    /// The Frame handler calls this ONLY when the guest window-set revision
    /// changed (the reconcile-on-change latch) — an idle repaint of an
    /// unchanged window set skips the enumerate+diff entirely. On the first
    /// frame this creates the first window from the guest's own title/size
    /// (identical to the pre-registry first-window path); later revision
    /// bumps create windows for newly opened top-levels (dialogs) and
    /// destroy the winit windows of closed ones. Each created window fills
    /// its own parent slot in [`ParentWindowSlots`].
    fn reconcile_windows(&mut self, event_loop: &ActiveEventLoop) {
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let top_levels = handle.guest_top_level_windows();
        let live_hwnds: HashMap<u64, ()> =
            top_levels.iter().map(|(hwnd, ..)| (*hwnd, ())).collect();
        // Destroy winit windows for top-levels the guest no longer has.
        // Dropping the last `Arc<Window>` clone closes the winit window
        // (winit 0.30 `Window` drops the underlying window).
        let stale: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, rt)| !live_hwnds.contains_key(&rt.hwnd.as_u64()))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(rt) = self.windows.remove(&id) {
                // Drain the window's parent slot too — a stale entry would
                // otherwise keep a dead NSWindow alive and hand it to the
                // next native dialog bridge.
                if let Ok(mut slots) = self.window_slots.lock() {
                    slots.remove(&rt.hwnd.as_u64());
                }
                tracing::debug!(
                    target: "wiegui",
                    hwnd = rt.hwnd.as_u64(),
                    "destroyed host window for closed guest top-level"
                );
            }
        }
        if self.windows.is_empty() {
            self.primary_hwnd = None;
        }
        // Create winit windows for new top-levels, in guest creation order
        // (the top-level snapshot preserves the `windows` record order), so
        // the main window is created first.
        for (hwnd, title, width, height) in top_levels {
            if self.windows.values().any(|rt| rt.hwnd.as_u64() == hwnd) {
                continue;
            }
            let is_first = self.primary_hwnd.is_none();
            let width = width.max(MIN_GUEST_WINDOW_SIZE) as u32;
            let height = height.max(MIN_GUEST_WINDOW_SIZE) as u32;
            let attrs = window_attributes(&title, width, height);
            let Ok(window) = event_loop.create_window(attrs) else {
                tracing::error!(target: "wiegui", "create_window failed for a guest top-level");
                continue;
            };
            let window = Arc::new(window);
            // Fill THIS window's parent slot (keyed by its guest HWND) — the
            // native dialog bridges parent their rfd dialogs to the
            // focused/primary window's slot, so every top-level gets one,
            // not just the first.
            if let Ok(mut slots) = self.window_slots.lock() {
                slots.insert(hwnd, window.clone());
            }
            // winit reports the device scale factor (physical px per logical
            // px); the input and resize paths divide winit's physical
            // coordinates by it.
            let scale_factor = window.scale_factor();
            window.focus_window();
            let id = window.id();
            let rt = WindowRuntime {
                hwnd: Hwnd::from(hwnd),
                window: window.clone(),
                surface: init_present_backend(&window),
                last_presented_pixels: None,
                last_presented_size: None,
                pending_size: None,
                last_resize: None,
                last_sent_size: None,
                scale_factor,
                retried_once: false,
                retry_at: None,
                occluded_retries: 0,
                occluded: false,
            };
            if is_first {
                self.primary_hwnd = Some(rt.hwnd);
            }
            tracing::debug!(
                target: "wiegui",
                window_id = ?id,
                hwnd = hwnd,
                "created host window for guest top-level"
            );
            self.windows.insert(id, rt);
        }
    }
}

/// Debounce window for resize: how long without a `Resized` event before we
/// consider the drag settled and post the final `WM_SIZE` to the guest.
///
/// 50 ms: short enough that release→crisp-frame feels instant (the guest
/// repaint is the dominant cost, ~55 ms at 886×776), long enough to absorb
/// macOS's trailing `Resized` events so the guest reallocates its DIB once.
const RESIZE_SETTLE_MS: u64 = 50;

/// How often a parked (budget-exhausted, `NotDrawn`) frame is re-armed for a
/// present. A single-wake flow (the status-bar toggle publishes exactly one
/// frame) whose present is lost would otherwise stay parked until the next
/// input event re-publishes — the throttled retry recovers it without a
/// frame-per-mutation storm (at most 10 wakeups/s, only while a frame is
/// unpresented).
const PARKED_RETRY_MS: u64 = 100;
/// Slow re-arm cadence for a present skipped while the window is reported
/// occluded: the fast 100 ms cadence would be a visible spin, and parking
/// entirely (waiting for `Occluded(false)`) can strand a frame published
/// while the window was briefly covered (a menu interaction) when the
/// un-occlusion event never arrives. Bounded to a few tries, then parked.
const OCCLUDED_RETRY_MS: u64 = 1000;
/// How many slow re-arms an occluded window gets before the frame parks.
const OCCLUDED_RETRY_MAX: u8 = 3;

/// Minimum guest window dimension (LOGICAL 96-DPI px): clamps a degenerate or
/// negative rect from `SetWindowPlacement` / the guest top-level records so
/// winit never sees a zero-size window.
const MIN_GUEST_WINDOW_SIZE: i32 = 100;

/// Win32 `WHEEL_DELTA`: the per-notch distance of a mouse-wheel line delta
/// (winit `LineDelta` values are multiples of this).
const WHEEL_DELTA: f32 = 120.0;

/// Accumulate a wheel delta and return the whole 120-unit notches to emit,
/// leaving the fraction in `accumulator` for the next event.
///
/// macOS trackpads deliver pixel deltas far below one notch per event
/// (10–60 px), and the guest EDIT/LISTBOX wheel handlers truncate
/// `delta / WHEEL_DELTA` — without accumulation every trackpad flick would
/// scroll zero lines. This mirrors what a real Windows input driver does
/// before posting `WM_MOUSEWHEEL`; a physical wheel (LineDelta ±1 → ±120)
/// emits exactly one notch per event, unchanged.
#[must_use]
fn wheel_notches(accumulator: &mut f32, delta: f32) -> i32 {
    *accumulator += delta;
    let notches = (*accumulator / WHEEL_DELTA) as i32;
    *accumulator -= notches as f32 * WHEEL_DELTA;
    notches
}

/// Stack size for the guest thread (`thread::Builder::stack_size`). A GUI
/// guest (guest stubs, host-stop handlers, the winit bridge) needs a deep
/// host stack; 8 MiB is the macOS main-thread default.
const GUEST_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Per-window host state for one winit window — the payload of each entry in
/// the [`WindowRegistry`]. One entry exists per guest top-level window; the
/// "window exists iff hwnd known" invariant is now a type instead of
/// per-arm checks.
struct WindowRuntime {
    /// Guest HWND this window mirrors.
    hwnd: Hwnd,
    window: Arc<Window>,
    /// The wgpu (Metal) present backend, created at window construction.
    surface: Option<PresentBackend>,
    /// Pixels Arc of the last frame actually presented; a frame whose pixels
    /// Arc is pointer-equal to this one AND whose window size is unchanged
    /// since the last present is byte-identical — skip the re-upload.
    ///
    /// Publishes move a fresh Arc every time ([`present::publish`] wraps the
    /// painted buffer in `Arc::from`), and this entry keeps the compared-to
    /// allocation alive until the next present, so `Arc::ptr_eq` inequality
    /// exactly means the window republished. Holding the Arc also pins the
    /// allocation: a freed-and-reused header address would otherwise fake a
    /// pointer match on a genuinely republished frame.
    last_presented_pixels: Option<Arc<Vec<u32>>>,
    /// Window size at the last present (drag-stretch must not be skipped).
    last_presented_size: Option<(u32, u32)>,
    /// Latest window size from winit during a resize drag.
    pending_size: Option<(u32, u32)>,
    /// When the last `Resized` event arrived (None = not resizing).
    last_resize: Option<Instant>,
    /// The size most recently posted to the guest as WM_SIZE.  Used to skip
    /// duplicate settles (macOS fires trailing `Resized` events after the
    /// drag, each would otherwise re-trigger the guest's expensive DIB
    /// recreation at the same size — and the guest thread being busy with
    /// that recreation delays close/quit handling).  Guest sizes are
    /// LOGICAL 96-DPI pixels.
    last_sent_size: Option<(u32, u32)>,
    /// The window's device scale factor (physical pixels per logical
    /// 96-DPI pixel), read at creation and refreshed on
    /// `ScaleFactorChanged`.  The guest space is logical; every winit
    /// physical value crossing the window boundary divides by this.
    scale_factor: f64,
    /// Whether the current published frame already consumed its one
    /// immediate present retry. Reset on every publish wake (a new frame
    /// deserves a fresh attempt) and on a Drawn present.
    retried_once: bool,
    /// When a parked (budget-exhausted) frame should next be retried — the
    /// throttled re-arm for SINGLE-wake flows (the status-bar toggle fires
    /// exactly one publish wake; if that one present is `NotDrawn` the frame
    /// parks with no follow-up publish to re-arm it, so the strip stays on
    /// screen until the next input). `about_to_wait` re-requests the redraw
    /// when the deadline passes and clears it on a `Drawn`. Suspended while
    /// the window is occluded (see `occluded`).
    retry_at: Option<Instant>,
    /// Slow-re-arm attempts while occluded (see `OCCLUDED_RETRY_MS`); reset
    /// on a Drawn present or an un-occlusion.
    occluded_retries: u8,
    /// The winit-reported occlusion state (`WindowEvent::Occluded`). While
    /// occluded every surface acquire fails, so the parked-frame re-arm is
    /// throttled to a few slow tries: re-arming at the fast cadence would
    /// spin redraw → NotDrawn → re-arm forever. `Occluded(false)` re-arms by
    /// requesting one redraw instead.
    occluded: bool,
}

/// One host winit window per guest top-level window, keyed by winit
/// `WindowId`. Reconciled from the guest on every published frame (see
/// [`WieApp::reconcile_windows`]). A session starts empty — no winit window
/// exists until the first frame — and entries are created/destroyed as the
/// guest opens and closes top-level windows.
type WindowRegistry = HashMap<WindowId, WindowRuntime>;

/// Shared per-top-level parent slot: guest HWND → winit window `Arc`.
///
/// Each `WindowRuntime` entry fills ITS OWN slot (keyed by its guest HWND)
/// at creation; the native dialog bridges (MessageBox, the file panel) read
/// the FOCUSED (or primary) window's entry to parent their rfd dialogs. This
/// kills the first-window specialness — a dialog raised from a second
/// top-level parents to THAT window, not the first one. The map is shared
/// with the guest thread (the bridges run there) but only ever WRITTEN on
/// the event-loop thread, so the mutex is a read-mostly lock.
pub(crate) type ParentWindowSlots = Arc<Mutex<HashMap<u64, Arc<Window>>>>;

/// The wgpu (Metal) present backend for a window, created at window
/// construction. The guest frame is uploaded to a staging texture and blitted
/// to the swapchain; the pixel Arc is dropped right after the upload so the
/// guest can hand the surface buffer back zero-copy on the next paint.
type PresentBackend = crate::gui::present_wgpu::WgpuPresenter;

/// Bounded redraw-retry budget for one window's present loop.
///
/// The delay before the next present attempt after a `NotDrawn` skip, or
/// `None` to park the frame. Pure so the retry policy is unit-testable: the
/// first skip of a frame retries immediately, repeat skips re-arm throttled
/// (fast while visible, slow + capped while occluded), and once the occluded
/// bound is spent the frame parks until `Occluded(false)` or a new publish
/// (a Frame wake resets `retried_once`, granting a fresh immediate attempt).
fn retry_delay(retried_once: bool, occluded: bool, occluded_retries: u8) -> Option<Duration> {
    if !retried_once {
        Some(Duration::ZERO)
    } else if occluded {
        (occluded_retries < OCCLUDED_RETRY_MAX).then(|| Duration::from_millis(OCCLUDED_RETRY_MS))
    } else {
        Some(Duration::from_millis(PARKED_RETRY_MS))
    }
}

impl WindowRuntime {
    /// Schedule the next present attempt after a `NotDrawn` skip — the ONE
    /// place that decides retry timing; `about_to_wait` is the only executor.
    /// See [`retry_delay`] for the policy.
    fn schedule_present_retry(&mut self) {
        match retry_delay(self.retried_once, self.occluded, self.occluded_retries) {
            Some(delay) => {
                self.retried_once = true;
                if self.occluded {
                    self.occluded_retries += 1;
                }
                self.retry_at = Some(Instant::now() + delay);
            }
            None => self.retry_at = None,
        }
    }

    /// A frame reached the screen: the next present starts fresh.
    fn note_present_drawn(&mut self) {
        self.retried_once = false;
        self.retry_at = None;
        self.occluded_retries = 0;
    }
}

/// Initialize the wgpu present backend. wgpu init is expected to succeed on
/// macOS (Metal backend); a failure here means the host cannot present at all.
fn init_present_backend(window: &Arc<Window>) -> Option<PresentBackend> {
    crate::gui::present_wgpu::WgpuPresenter::init(window.clone()).ok()
}

/// Build the winit window attributes for the guest's first window.
///
/// The guest reports LOGICAL 96-DPI pixels; winit multiplies the
/// `LogicalSize` by the display's device scale factor, so on a Retina (2×)
/// display the guest's 640×480 window becomes a 1280×960 physical surface.
fn window_attributes(title: &str, width: u32, height: u32) -> winit::window::WindowAttributes {
    Window::default_attributes()
        .with_title(title)
        .with_inner_size(LogicalSize::new(width, height))
}

/// The guest-LOGICAL size a physical winit `inner_size` corresponds to at
/// `scale_factor`: physical ÷ sf with the crate's standard rounding (see
/// [`input::physical_to_logical`]). This is the size posted as WM_SIZE and
/// written to the guest-visible window record.
fn guest_size_from_physical(width: u32, height: u32, scale_factor: f64) -> (u32, u32) {
    (
        input::physical_to_logical(f64::from(width), scale_factor) as u32,
        input::physical_to_logical(f64::from(height), scale_factor) as u32,
    )
}

/// winit application state: bridges the guest windows to the present backend.
struct WieApp {
    handle: Option<GuestHandle>,
    /// One winit window per guest top-level window; [`WindowRegistry`] is
    /// empty until the first frame creates the first window, then reconciled
    /// from the guest on every publish (see [`WieApp::reconcile_windows`]).
    windows: WindowRegistry,
    /// Guest HWND of the primary (first-created) host window — the window
    /// that mirrors the guest's main window. Input events that are not yet
    /// per-window (keyboard, focus, and the mouse hit-test fallback — L2's
    /// input-routing lane) target it.
    primary_hwnd: Option<Hwnd>,
    /// Wake-coalescing flag, shared with the guest-thread wake callback.
    /// Set on every publish; the first `Frame` event after a publish group
    /// swaps it and requests a redraw, duplicates skip.
    pending_frame: Arc<std::sync::atomic::AtomicBool>,
    /// Per-top-level parent slots for the native dialog bridges (see
    /// [`ParentWindowSlots`]); filled at window creation, drained on destroy.
    window_slots: ParentWindowSlots,
    /// Cached guest top-level window-SET revision ([`GuestHandle::windows_rev`]).
    ///
    /// The reconcile-on-change latch: the Frame handler reconciles the winit
    /// window registry ONLY when this changes, so an idle repaint of an
    /// unchanged window set skips the enumerate+diff entirely. `None` before
    /// the first reconcile — the first frame always reconciles (that is the
    /// window-creation event).
    last_windows_rev: Option<u64>,
    /// Cached guest z-order revision ([`GuestHandle::z_rev`]): a change
    /// re-orders the host NSWindows to mirror the guest stacking.
    #[cfg(target_os = "macos")]
    last_z_rev: Option<u64>,
    /// Currently pressed mouse buttons (MK_* bits) — from MouseInput events.
    mouse_buttons: u16,
    /// Last reported cursor position in client coords (x, y).
    cursor_pos: (f64, f64),
    /// Residual wheel deltas (x, y) not yet emitted as a whole 120-unit
    /// notch — macOS trackpads deliver sub-notch pixel deltas per event,
    /// and the guest wheel handlers truncate partial deltas to zero (see
    /// [`wheel_notches`]).
    wheel_accum_x: f32,
    wheel_accum_y: f32,
    /// The last left-button press (time, position, target window), for
    /// double-click detection — a second press on the same window within the
    /// time window and slop rectangle posts WM_LBUTTONDBLCLK instead of
    /// WM_LBUTTONDOWN (the message Windows itself would synthesize).
    last_left_press: Option<(Instant, f64, f64, u64)>,
    /// Currently held modifier keys (shift/ctrl/alt) — from ModifiersChanged.
    modifiers: winit::keyboard::ModifiersState,
    /// macOS application menu bar mirroring the guest window's menu.
    #[cfg(target_os = "macos")]
    menu_bar: crate::gui::menu_bar::MacMenuBar,
    /// Menu items the menu bar was last rebuilt with (cheap change check).
    #[cfg(target_os = "macos")]
    last_menu_items: Arc<Vec<MenuNode>>,
}

impl ApplicationHandler<WieEvent> for WieApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // Window is created lazily in the Frame handler with the guest's
        // actual title and size (from CreateWindowExA parameters).
        tracing::debug!("resumed: waiting for guest window");
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        self.dispatch_window_event(event_loop, window_id, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Debounced resize, per window: when no Resized event arrived for the
        // settle window, the drag has ended — post the final WM_SIZE (and a
        // WM_PAINT so the guest reallocates its DIB exactly once, at the
        // final size). Each window settles independently (a dialog can be
        // settling while the main window idles).
        let mut any_settled = false;
        for rt in self.windows.values_mut() {
            let Some(start) = rt.last_resize else {
                continue;
            };
            if start.elapsed() < Duration::from_millis(RESIZE_SETTLE_MS) {
                continue;
            }
            rt.last_resize = None;
            any_settled = true;
            let Some((w, h)) = rt.pending_size.take() else {
                continue;
            };
            // The guest DIB is LOGICAL 96-DPI: post the PHYSICAL inner
            // size divided by the device scale factor.
            let (lw, lh) = guest_size_from_physical(w, h, rt.scale_factor);
            tracing::debug!(
                "settle: pending={}x{} guest={}x{} last_sent={:?}",
                w,
                h,
                lw,
                lh,
                rt.last_sent_size
            );
            // Skip if the size hasn't changed since the last posted
            // WM_SIZE — macOS fires trailing Resized events after the
            // drag, and re-posting the same size would re-trigger the
            // guest's expensive DIB recreation (which also delays
            // close/quit handling).
            if rt.last_sent_size != Some((lw, lh)) {
                let hwnd = rt.hwnd;
                if let Some(handle) = self.handle.as_ref() {
                    // Update the guest-visible record now — together with
                    // the WM_SIZE post — so GetClientRect matches the size
                    // the guest is about to recreate its DIB at.
                    handle.resize_window(hwnd.as_u64(), lw, lh);
                    let lparam = input::make_lparam(lw as u16, lh as u16);
                    handle.post_message(hwnd.as_u64(), input::WM_SIZE, 0, lparam);
                    handle.post_message(hwnd.as_u64(), input::WM_PAINT, 0, 0);
                    tracing::debug!("resize settled: WM_SIZE {}x{}", lw, lh);
                }
                rt.last_sent_size = Some((lw, lh));
            }
        }
        if any_settled {
            event_loop.set_control_flow(ControlFlow::Wait);
        }

        // Re-arm parked presents. A frame whose present was `NotDrawn` and
        // whose retry bound is spent stays parked (no follow-up wake for a
        // single-publish flow); when its deadline passes, re-request the
        // redraw. Bounded: at most one wakeup per retry cadence per parked
        // window, and it stops the moment the frame draws.
        let now = Instant::now();
        let mut earliest_retry: Option<Instant> = None;
        for rt in self.windows.values_mut() {
            let Some(at) = rt.retry_at else {
                continue;
            };
            if at <= now {
                rt.retry_at = None;
                rt.window.request_redraw();
            } else {
                earliest_retry = Some(earliest_retry.map_or(at, |e| e.min(at)));
            }
        }
        if let Some(at) = earliest_retry {
            event_loop.set_control_flow(ControlFlow::WaitUntil(at));
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: WieEvent) {
        match event {
            WieEvent::Frame { published_at } => {
                // Wake → event-loop latency.
                tracing::debug!(
                    target: "wiegui",
                    wake_to_user_us = u64::try_from(published_at.elapsed().as_micros())
                        .unwrap_or(u64::MAX),
                    "Frame event"
                );
                // Reconcile the host window registry against the guest's
                // top-level windows — but ONLY when the guest window SET
                // changed (the reconcile-on-change latch). The create/destroy
                // handlers bump the revision, so an idle repaint of an
                // unchanged window set skips the enumerate+diff entirely. The
                // first frame always reconciles (the `None` cache) — that is
                // the first-window creation event. The cached value is the
                // rev we COMPARED against, so a create landing between the
                // read and the store is caught by the next frame.
                if let Some(handle) = self.handle.as_ref() {
                    let rev = handle.windows_rev();
                    if self.last_windows_rev != Some(rev) {
                        self.reconcile_windows(event_loop);
                        self.last_windows_rev = Some(rev);
                    }
                } else {
                    self.reconcile_windows(event_loop);
                }
                // Request a redraw for EVERY publish wake, without the
                // coalescing drop. The flag previously gated the request to
                // the FIRST Frame event of a publish group, which could run
                // BEFORE the group's final publish landed (the guest thread
                // publishes at the idle boundary while the host event loop
                // processes the earlier wake) — the final frame's wake was
                // then coalesced away and the new surface never reached the
                // OS window until the next input ("the status bar persists
                // until a click"). `request_redraw` is cheap and winit
                // coalesces it; each window's RedrawRequested additionally
                // skips via the per-window ptr_eq compare, so a genuinely
                // unchanged frame does no GPU work.
                let _ = self
                    .pending_frame
                    .swap(false, std::sync::atomic::Ordering::SeqCst);
                for rt in self.windows.values_mut() {
                    // A publish wake means a genuinely new frame is in the
                    // slot: the next present starts with a fresh immediate
                    // retry (see `schedule_present_retry`).
                    rt.retried_once = false;
                    rt.window.request_redraw();
                }
                // Apply any guest-requested geometry (SetWindowPlacement) to
                // the host window. The winapi handler set a pending request
                // and woke the presenter, so the move lands even without a
                // repaint; a cheap no-op when nothing is pending. Runs on the
                // first frame too — the window was just created above, so a
                // startup restore is applied immediately.
                self.apply_host_geometry();
                #[cfg(target_os = "macos")]
                {
                    // Mirror the guest's z-order (gated on its revision) and
                    // the focused window's menu bar — both cheap no-ops on an
                    // unchanged state.
                    self.sync_window_z_order();
                    self.sync_menu_bar();
                }
            }
            #[cfg(target_os = "macos")]
            WieEvent::MenuEvent(menu_event) => {
                // Decode the string-form muda id back to the guest item id and
                // deliver WM_COMMAND with wParam = MAKEWPARAM(id, 0). The menu
                // bar mirrors the primary window's menu (menu polish is L4's
                // lane), so the command goes to the primary window's hwnd.
                let id = menu_bar::menu_id_to_guest_id(menu_event.id());
                tracing::debug!(
                    target: "wiegui",
                    menu_id = menu_event.id().0,
                    guest_id = id,
                    "MenuEvent"
                );
                if let Some(handle) = self.handle.as_ref()
                    && let Some(hwnd) = self.primary_hwnd.map_or_else(
                        || handle.first_guest_window_handle(),
                        |hwnd| Some(hwnd.as_u64()),
                    )
                {
                    handle.post_message(hwnd, input::WM_COMMAND, u64::from(id), 0);
                }
            }
            WieEvent::GuestExited { code: _ } => {
                event_loop.exit();
            }
        }
    }
}

/// Win32 `MB_*` flag bits (the low nibble of `MB_ICONMASK`/`MB_TYPEMASK` picks
/// the button set; the next nibble the icon). The rfd bridge receives the raw
/// flag word from the guest's MessageBoxA/W call.
#[cfg(target_os = "macos")]
const MB_TYPEMASK: u32 = 0x0000_000F;
#[cfg(target_os = "macos")]
const MB_ICONMASK: u32 = 0x0000_00F0;
#[cfg(target_os = "macos")]
const MB_OK: u32 = 0x0000_0000;
#[cfg(target_os = "macos")]
const MB_OKCANCEL: u32 = 0x0000_0001;
#[cfg(target_os = "macos")]
const MB_YESNOCANCEL: u32 = 0x0000_0003;
#[cfg(target_os = "macos")]
const MB_YESNO: u32 = 0x0000_0004;
#[cfg(target_os = "macos")]
const MB_ICONERROR: u32 = 0x0000_0010;
#[cfg(target_os = "macos")]
const MB_ICONQUESTION: u32 = 0x0000_0020;
#[cfg(target_os = "macos")]
const MB_ICONWARNING: u32 = 0x0000_0030;
#[cfg(target_os = "macos")]
const MB_ICONINFORMATION: u32 = 0x0000_0040;

/// Win32 standard dialog-command ids (`WM_COMMAND` wParam lows).
#[cfg(target_os = "macos")]
const IDOK: i32 = 1;
#[cfg(target_os = "macos")]
const IDCANCEL: i32 = 2;
#[cfg(target_os = "macos")]
const IDYES: i32 = 6;
#[cfg(target_os = "macos")]
const IDNO: i32 = 7;

/// Map Win32 `MB_*` flag bits to rfd's dialog shape.
///
/// The low nibble selects the button set, the next nibble the icon. rfd has no
/// `Question` level, so `MB_ICONQUESTION` falls back to `Info`. Unknown bits
/// fall back to the MB_OK / no-icon defaults, matching real MessageBox.
#[cfg(target_os = "macos")]
fn map_message_box_buttons(mb_type: u32) -> (rfd::MessageButtons, rfd::MessageLevel) {
    let buttons = match mb_type & MB_TYPEMASK {
        MB_OK => rfd::MessageButtons::Ok,
        MB_OKCANCEL => rfd::MessageButtons::OkCancel,
        MB_YESNOCANCEL => rfd::MessageButtons::YesNoCancel,
        MB_YESNO => rfd::MessageButtons::YesNo,
        // Unknown button bits fall back to Ok (matching real MessageBox).
        _ => rfd::MessageButtons::Ok,
    };
    let level = match mb_type & MB_ICONMASK {
        MB_ICONERROR => rfd::MessageLevel::Error,
        MB_ICONWARNING => rfd::MessageLevel::Warning,
        MB_ICONINFORMATION | MB_ICONQUESTION => rfd::MessageLevel::Info,
        // No icon bits (plain MB_OK) also read as Info.
        _ => rfd::MessageLevel::Info,
    };
    (buttons, level)
}

/// Map an rfd alert result to the Win32 id the guest expects (IDOK, IDCANCEL,
/// IDYES, IDNO).
#[cfg(target_os = "macos")]
fn map_alert_result(result: rfd::MessageDialogResult) -> i32 {
    match result {
        rfd::MessageDialogResult::Ok => IDOK,
        rfd::MessageDialogResult::Cancel => IDCANCEL,
        rfd::MessageDialogResult::Yes => IDYES,
        rfd::MessageDialogResult::No => IDNO,
        rfd::MessageDialogResult::Custom(_) => IDCANCEL,
    }
}

/// Show a MessageBox with NO parent winit window via a direct NSAlert.
///
/// rfd's unparented `MessageDialog::show()` falls back to the legacy
/// `CFUserNotificationDisplayAlert` API on macOS, which prints a
/// "called from main application thread, will block waiting for a response"
/// line to stderr. A bare NSAlert (the same modern API rfd uses internally
/// once a parent exists) never touches that path — a MessageBox raised
/// before the first frame created a winit window stays silent.
#[cfg(target_os = "macos")]
#[expect(unsafe_code)]
fn show_unparented_ns_alert(caption: &str, text: &str, mb_type: u32) -> i32 {
    use objc2_app_kit::{
        NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAlertStyle,
        NSAlertThirdButtonReturn,
    };
    use objc2_foundation::{NSString, run_on_main};

    let (buttons, level) = map_message_box_buttons(mb_type);
    run_on_main(|mtm| {
        // SAFETY: `new(mtm)` is the main-thread-only constructor (NSAlert is
        // a MainThreadOnly class); the returned Retained owns the alert.
        let alert = unsafe { NSAlert::new(mtm) };
        // SAFETY: plain property setters on the owned alert.
        unsafe {
            alert.setMessageText(&NSString::from_str(caption));
            alert.setInformativeText(&NSString::from_str(text));
            let style = match level {
                rfd::MessageLevel::Error => NSAlertStyle::Critical,
                rfd::MessageLevel::Warning => NSAlertStyle::Warning,
                rfd::MessageLevel::Info => NSAlertStyle::Informational,
            };
            alert.setAlertStyle(style);
        }
        // First added button is the default (rightmost, Enter). The order
        // mirrors the Win32 button set so the response index maps to the id.
        let titles: &[&str] = match buttons {
            rfd::MessageButtons::Ok => &["OK"],
            rfd::MessageButtons::OkCancel => &["OK", "Cancel"],
            rfd::MessageButtons::YesNoCancel => &["Yes", "No", "Cancel"],
            rfd::MessageButtons::YesNo => &["Yes", "No"],
            // Unknown button bits fall back to Ok (matching real MessageBox).
            _ => &["OK"],
        };
        for title in titles {
            // SAFETY: appends a button to the owned alert; the returned
            // Retained<NSButton> is dropped (the alert retains it).
            unsafe {
                let _ = alert.addButtonWithTitle(&NSString::from_str(title));
            }
        }
        // SAFETY: runModal on the main thread (we are inside run_on_main)
        // blocks until the user clicks — MessageBox semantics; the guest
        // thread is blocked in the bridge.
        let response = unsafe { alert.runModal() };
        if response == NSAlertFirstButtonReturn {
            match buttons {
                rfd::MessageButtons::YesNo | rfd::MessageButtons::YesNoCancel => IDYES,
                _ => IDOK,
            }
        } else if response == NSAlertSecondButtonReturn {
            match buttons {
                rfd::MessageButtons::YesNo | rfd::MessageButtons::YesNoCancel => IDNO,
                _ => IDCANCEL,
            }
        } else if response == NSAlertThirdButtonReturn {
            IDCANCEL
        } else {
            IDCANCEL
        }
    })
}

/// Resolve the winit window a native dialog (MessageBox, file panel) should
/// parent to.
///
/// The FOCUSED window's top-level is the natural parent — a MessageBox raised
/// while a second top-level is active parents to THAT window, not the first
/// one (the per-window-slot fix). Falls back to the primary window (the
/// first guest window), then to any live host window. `None` when no host
/// window exists yet (the bridge runs before the first frame — rfd then uses
/// its unparented fallback).
#[cfg(target_os = "macos")]
pub(crate) fn resolve_dialog_parent(
    handle: &GuestHandle,
    slots: &ParentWindowSlots,
) -> Option<Arc<Window>> {
    let preferred = handle
        .focused_top_level()
        .or_else(|| handle.first_guest_window_handle());
    let Ok(map) = slots.lock() else {
        return None;
    };
    preferred
        .and_then(|hwnd| map.get(&hwnd).cloned())
        .or_else(|| map.values().next().cloned())
}

/// Bring `window`'s NSWindow to the front of its window level.
///
/// winit 0.30 exposes no window-ordering API, so the guest z-order is
/// mirrored through AppKit directly: winit's raw window handle exposes the
/// backing NSView, whose owning NSWindow answers `orderFront`. Best-effort —
/// a window whose raw handle is not AppKit, or whose view is not yet in a
/// window, is skipped. Runs on the main (event-loop) thread, where AppKit
/// ordering is valid.
#[cfg(target_os = "macos")]
#[expect(unsafe_code)]
fn order_window_front(window: &Arc<Window>) {
    use objc2_app_kit::NSView;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: `ns_view` is the live NSView backing this winit window — winit
    // retains it for the window's lifetime, and this runs on the main thread
    // where the AppKit view hierarchy is valid.
    let view: &NSView = unsafe { &*appkit.ns_view.as_ptr().cast::<NSView>() };
    if let Some(ns_window) = view.window() {
        ns_window.orderFront(None);
    }
}

/// Resolve the run source for the windowed GUI entry under the FS bottle
/// policy: an exe outside the bottle runs from a `drive_c` copy (see
/// [`crate::commands::ensure_exe_in_bottle`]), so the guest identity's
/// `C:\{name}` label maps back to a real bottle file. The roots are threaded
/// the same way the other run entries thread them — `run_gui_windowed`
/// propagates the CLI's `--root`/`--drive-d` into `WIE_ROOT`/`WIE_DRIVE_D`,
/// which this resolution (and the winapi volume config) read. Explicit roots
/// keep the wiring testable without mutating the process environment.
fn resolve_gui_run_source(
    path: &std::path::Path,
    bottle_root: Option<&std::path::Path>,
    drive_d_root: Option<&std::path::Path>,
) -> Result<std::path::PathBuf> {
    crate::commands::ensure_exe_in_bottle(path, bottle_root, drive_d_root)
}

/// Run the guest with a winit window.
///
/// `input_script` is a parsed input-script path (see
/// [`crate::gui::input_script`]); `None` runs without scripted input. The
/// script is read and parsed up front so a bad path or syntax fails before
/// the guest thread and event loop start.
pub fn run_gui_windowed(
    path: &std::path::Path,
    input_script: Option<std::path::PathBuf>,
    bottle_root: Option<&std::path::Path>,
    drive_d_root: Option<&std::path::Path>,
) -> Result<()> {
    // The winapi volume config and the run-source resolution read the bottle
    // from WIE_ROOT/WIE_DRIVE_D. Propagate the GUI-entry flags into the
    // environment so `--root`/`--drive-d` work in GUI mode too (the console
    // entries consume the flags directly; this mirrors their env-channel).
    let resolved_root = bottle_root
        .map(std::path::Path::to_path_buf)
        .or_else(wie_winapi::bottle_root_from_env);
    let resolved_drive_d = drive_d_root
        .map(std::path::Path::to_path_buf)
        .or_else(wie_winapi::drive_d_from_env);
    if let Some(root) = resolved_root.as_deref() {
        // SAFETY: set before the guest thread or any session reads it.
        unsafe { std::env::set_var("WIE_ROOT", root) };
    }
    if let Some(d) = resolved_drive_d.as_deref() {
        // SAFETY: set before the guest thread or any session reads it.
        unsafe { std::env::set_var("WIE_DRIVE_D", d) };
    }
    let script_steps = match &input_script {
        Some(script_path) => Some(crate::gui::input_script::read_script(script_path)?),
        None => None,
    };
    let event_loop = EventLoop::<WieEvent>::with_user_event()
        .build()
        .context("build event loop")?;
    let proxy = event_loop.create_proxy();
    // Wake-coalescing flag, shared between the guest-thread wake callback
    // and the host event loop. Cloned for the guest thread; the original moves
    // into `WieApp`.
    let pending_frame = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pending_frame_guest = pending_frame.clone();
    // Per-top-level parent slots for the native dialog bridges: the bridges
    // (registered on the guest thread BEFORE any window exists) read the
    // focused/primary window's slot to parent their rfd dialogs; `WieApp`
    // fills each slot when its window is created. Clone for the guest
    // thread; the original moves into `WieApp`.
    let window_slots: ParentWindowSlots = Arc::new(Mutex::new(HashMap::new()));
    let window_slots_guest = window_slots.clone();

    // FS policy: an exe outside the bottle runs from a drive_c copy so the
    // guest identity's `C:\{name}` label maps back to a real bottle file
    // (the non-GUI run entries wire `ensure_exe_in_bottle` at the same
    // point, before the session build).
    let run_path = resolve_gui_run_source(
        path,
        wie_winapi::bottle_root_from_env().as_deref(),
        wie_winapi::drive_d_from_env().as_deref(),
    )?;

    let handle_rx = {
        let (tx, rx) = mpsc::channel::<GuestHandle>();
        let proxy = proxy.clone();
        let control = Arc::new(GuiControl::new());

        thread::Builder::new()
            .name("wie-guest-primary".into())
            .stack_size(GUEST_THREAD_STACK_BYTES)
            .spawn(move || {
                match RuntimeSession::new(
                    &run_path,
                    wie_winapi::MessageQueueIdlePolicy::YieldOnIdle,
                ) {
                    Ok(mut session) => {
                        // Interactive file dialogs: GetOpenFileNameW /
                        // GetSaveFileNameW build the host dialog (path EDIT +
                        // directory LISTBOX + OK/Cancel) and run its in-guest
                        // modal loop instead of returning a scripted answer.
                        crate::gui::file_dialog::enable_interactive_file_dialogs(&mut session);
                        // FindTextW / ReplaceTextW are modeless and always
                        // enabled (see gui/find_dialog.rs); this call pins the
                        // wiring point next to the file dialog's.
                        crate::gui::find_dialog::enable_interactive_find_dialogs(&mut session);
                        // ChooseFontW builds the host font dialog (family +
                        // size + effects + OK/Cancel) and runs the file
                        // dialog's in-guest modal loop.
                        crate::gui::font_dialog::enable_interactive_font_dialogs(&mut session);
                        // PrintDlgW shows the native macOS print panel (the
                        // OS-equivalent of Windows' print dialog) when the
                        // bridge is registered; headless runs keep Cancel.
                        crate::gui::print::enable_interactive_print_dialogs(&mut session);
                        // PageSetupDlgW shows the native macOS page-layout
                        // panel (the OS-equivalent of Windows' Page Setup
                        // dialog) when the bridge is registered; headless
                        // runs keep Cancel.
                        crate::gui::print::enable_interactive_page_setup_dialogs(&mut session);
                        let handle = session.guest_handle();

                        // Register wake callback.
                        {
                            let proxy = proxy.clone();
                            let pending = pending_frame_guest.clone();
                            handle.set_wake(Box::new(move || {
                                // Mark the pending frame BEFORE sending so
                                // the first Frame event always does the work.
                                pending.store(true, std::sync::atomic::Ordering::SeqCst);
                                let _ = proxy.send_event(WieEvent::Frame {
                                    published_at: Instant::now(),
                                });
                            }));
                        }

                        // Native file dialogs: GetOpenFileName / GetSaveFileName
                        // show the real macOS Open/Save panel (rfd NSOpenPanel/
                        // NSSavePanel) instead of the in-app emulated dialog —
                        // the OS-equivalent of Windows' common dialogs. The
                        // guest thread blocks in the bridge until the user
                        // picks; a pick outside the bottle cancels at accept.
                        #[cfg(target_os = "macos")]
                        crate::gui::file_dialog::register_native_file_dialog_bridge(
                            &handle,
                            window_slots_guest.clone(),
                        );

                        // Native print dialogs: PrintDlgW shows the real macOS
                        // print panel (NSPrintPanel) instead of returning a
                        // scripted cancel — the OS-equivalent of Windows'
                        // PrintDlg. The guest thread blocks in the bridge until
                        // the user picks; the pick's NSPrintInfo is registered
                        // in the session-scoped id-table (the EndDoc print
                        // operation consumes the entry — both bridges share
                        // ONE table so the panel's settings survive to the
                        // operation).
                        #[cfg(target_os = "macos")]
                        {
                            let print_info_table = crate::gui::print::PrintInfoTable::default();
                            crate::gui::print::register_native_print_dialog_bridge(
                                &handle,
                                print_info_table.clone(),
                            );
                            // Native print operations: EndDoc hands the
                            // completed pages to a real NSPrintOperation
                            // (printer / "Save as PDF" — the user's destination
                            // from the panel) instead of writing the
                            // WIE_PRINT_TO BMP oracle. The guest thread blocks
                            // until the operation finishes; its success flag is
                            // the EndDoc return value.
                            crate::gui::print::register_native_print_job_bridge(
                                &handle,
                                print_info_table,
                            );
                            // Native page setup: PageSetupDlgW shows the real
                            // macOS page-layout panel (NSPageLayout) instead
                            // of returning a scripted cancel. No id-table —
                            // the pick's paper/orientation reach the later
                            // PrintDlgW panel through the guest DEVMODE.
                            crate::gui::print::register_native_page_setup_dialog_bridge(&handle);
                        }

                        // Register the native-alert MessageBox bridge. The
                        // MessageBoxA/W handlers never call this directly:
                        // they return MessageBoxBridgeRequested, the runtime
                        // drops the shared state lock, and THIS callback runs
                        // on the guest thread — rfd's parented dialog uses the
                        // modern NSAlert API (dispatched to the main thread;
                        // the guest thread blocks until the user clicks —
                        // MessageBox semantics) and maps the result to the
                        // Win32 id. With no parent (a MessageBox raised before
                        // the first frame created a winit window), rfd would
                        // fall back to the legacy CFUserNotificationDisplayAlert
                        // path, which prints "will block waiting for a
                        // response" on the main thread — use a bare NSAlert
                        // instead (the same modern API, no legacy print).
                        #[cfg(target_os = "macos")]
                        handle.set_message_box_bridge(Box::new({
                            let handle = handle.clone();
                            let window_slots = window_slots_guest.clone();
                            move |caption, text, mb_type| {
                                tracing::info!(
                                    target: "wiegui",
                                    "MessageBox: {caption}: {text} (type 0x{mb_type:x})"
                                );
                                // Parent to the FOCUSED (or primary) window's
                                // slot — a MessageBox raised while a second
                                // top-level is active parents to THAT window,
                                // not the first one (the per-window slot fix).
                                // set_parent consumes the builder, so apply it
                                // before the chain.
                                let Some(parent) = resolve_dialog_parent(&handle, &window_slots)
                                else {
                                    return show_unparented_ns_alert(caption, text, mb_type);
                                };
                                let (buttons, level) = map_message_box_buttons(mb_type);
                                let result = rfd::MessageDialog::new()
                                    .set_parent(parent.as_ref())
                                    .set_title(caption.to_owned())
                                    .set_description(text.to_owned())
                                    .set_level(level)
                                    .set_buttons(buttons)
                                    .show();
                                map_alert_result(result)
                            }
                        }));

                        let _ = tx.send(handle);

                        // Run the guest.
                        let _result = run_windowed(&mut session, &control);
                        let code = control.exit_code.load(std::sync::atomic::Ordering::SeqCst);
                        let _ = proxy.send_event(WieEvent::GuestExited { code });
                    }
                    Err(e) => tracing::error!("session: {e}"),
                }
            })
            .context("spawn guest thread")?;

        rx
    };

    let handle = handle_rx.recv().context("recv handle")?;

    // Scripted input driver: posts WM_KEYDOWN/WM_CHAR/WM_COMMAND to the guest
    // on its own schedule once the guest window exists.
    if let Some(steps) = script_steps {
        crate::gui::input_script::spawn(handle.clone(), steps)?;
    }

    // Windows are created lazily when frames arrive (in user_event):
    // reconciliation creates a winit window per guest top-level.
    let mut app = WieApp {
        handle: Some(handle),
        windows: WindowRegistry::new(),
        primary_hwnd: None,
        pending_frame,
        window_slots,
        last_windows_rev: None,
        #[cfg(target_os = "macos")]
        last_z_rev: None,
        mouse_buttons: 0,
        cursor_pos: (0.0, 0.0),
        wheel_accum_x: 0.0,
        wheel_accum_y: 0.0,
        last_left_press: None,
        modifiers: winit::keyboard::ModifiersState::default(),
        #[cfg(target_os = "macos")]
        menu_bar: crate::gui::menu_bar::MacMenuBar::new(proxy.clone()),
        #[cfg(target_os = "macos")]
        last_menu_items: Arc::new(Vec::new()),
    };

    event_loop
        .run_app(&mut app)
        .map_err(|e| anyhow::anyhow!("event loop: {e}"))
}

#[cfg(all(test, target_os = "macos"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        OCCLUDED_RETRY_MAX, OCCLUDED_RETRY_MS, PARKED_RETRY_MS, guest_size_from_physical,
        map_alert_result, map_message_box_buttons, resolve_gui_run_source, retry_delay,
        wheel_notches, window_attributes,
    };

    /// A physical wheel notch (LineDelta ±1 → ±120) emits exactly one notch
    /// per event and leaves no residue.
    #[test]
    fn wheel_notches_full_notch_emits_immediately() {
        let mut accum = 0.0_f32;
        assert_eq!(wheel_notches(&mut accum, 120.0), 1);
        assert_eq!(accum, 0.0);
        assert_eq!(wheel_notches(&mut accum, -120.0), -1);
        assert_eq!(accum, 0.0);
    }

    /// Trackpad flicks (PixelDelta, 10–60 px per event) accumulate across
    /// events until a whole 120-unit notch forms — a 3×40 flick scrolls.
    #[test]
    fn wheel_notches_accumulates_trackpad_flicks() {
        let mut accum = 0.0_f32;
        assert_eq!(wheel_notches(&mut accum, 40.0), 0);
        assert_eq!(accum, 40.0);
        assert_eq!(wheel_notches(&mut accum, 40.0), 0);
        assert_eq!(accum, 80.0);
        assert_eq!(wheel_notches(&mut accum, 40.0), 1);
        assert_eq!(accum, 0.0);
    }

    /// Negative accumulation mirrors the positive case (scrolling down).
    #[test]
    fn wheel_notches_accumulates_downward_flicks() {
        let mut accum = 0.0_f32;
        assert_eq!(wheel_notches(&mut accum, -60.0), 0);
        assert_eq!(accum, -60.0);
        assert_eq!(wheel_notches(&mut accum, -60.0), -1);
        assert_eq!(accum, 0.0);
    }

    /// A fast flick that exceeds a notch emits the whole-notch part and
    /// carries the remainder into the next event.
    #[test]
    fn wheel_notches_emits_whole_notches_keeps_remainder() {
        let mut accum = 0.0_f32;
        assert_eq!(wheel_notches(&mut accum, 250.0), 2);
        assert_eq!(accum, 10.0);
        assert_eq!(wheel_notches(&mut accum, 110.0), 1);
        assert_eq!(accum, 0.0);
    }

    /// `MB_*` button bits select the rfd button set; the bridge receives the
    /// raw flag word, so this mapping is the winapi crate's documented seam.
    #[test]
    fn map_message_box_buttons_selects_button_set() {
        assert!(matches!(
            map_message_box_buttons(0x00).0,
            rfd::MessageButtons::Ok
        ));
        assert!(matches!(
            map_message_box_buttons(0x01).0,
            rfd::MessageButtons::OkCancel
        ));
        assert!(matches!(
            map_message_box_buttons(0x04).0,
            rfd::MessageButtons::YesNo
        ));
        assert!(matches!(
            map_message_box_buttons(0x03).0,
            rfd::MessageButtons::YesNoCancel
        ));
        // Icon bits must not disturb the button set.
        assert!(matches!(
            map_message_box_buttons(0x04 | 0x20).0,
            rfd::MessageButtons::YesNo
        ));
        // Unknown button bits fall back to Ok.
        assert!(matches!(
            map_message_box_buttons(0x02).0,
            rfd::MessageButtons::Ok
        ));
    }

    /// `MB_ICON*` bits select the rfd level. rfd has no Question level, so
    /// MB_ICONQUESTION reads as Info.
    #[test]
    fn map_message_box_buttons_selects_level() {
        assert!(matches!(
            map_message_box_buttons(0x10).1,
            rfd::MessageLevel::Error
        ));
        assert!(matches!(
            map_message_box_buttons(0x30).1,
            rfd::MessageLevel::Warning
        ));
        assert!(matches!(
            map_message_box_buttons(0x40).1,
            rfd::MessageLevel::Info
        ));
        assert!(matches!(
            map_message_box_buttons(0x20).1,
            rfd::MessageLevel::Info
        ));
        // No icon bits (plain MB_OK) also reads as Info.
        assert!(matches!(
            map_message_box_buttons(0x00).1,
            rfd::MessageLevel::Info
        ));
    }

    /// The user's alert choice maps to the Win32 id the guest sees.
    #[test]
    fn map_alert_result_maps_to_win32_ids() {
        assert_eq!(map_alert_result(rfd::MessageDialogResult::Ok), 1, "IDOK");
        assert_eq!(
            map_alert_result(rfd::MessageDialogResult::Cancel),
            2,
            "IDCANCEL"
        );
        assert_eq!(map_alert_result(rfd::MessageDialogResult::Yes), 6, "IDYES");
        assert_eq!(map_alert_result(rfd::MessageDialogResult::No), 7, "IDNO");
        assert_eq!(
            map_alert_result(rfd::MessageDialogResult::Custom("x".to_owned())),
            2,
            "an unknown custom result cancels"
        );
    }

    // -----------------------------------------------------------------------
    // F1 scale plumbing (fidelity lane 1): the guest space is LOGICAL 96-DPI
    // pixels; winit reports PHYSICAL. These pin the window-boundary
    // conversions the input/resize/geometry paths share.
    // -----------------------------------------------------------------------

    /// The guest's first window is created at its LOGICAL 96-DPI size —
    /// winit multiplies by the device scale factor itself.
    #[test]
    fn window_attributes_use_logical_size() {
        let attrs = window_attributes("notepad", 640, 480);
        assert_eq!(
            attrs.inner_size,
            Some(winit::dpi::Size::Logical(winit::dpi::LogicalSize::new(
                640.0, 480.0
            )))
        );
        let attrs = window_attributes("t", 100, 100);
        assert_eq!(
            attrs.inner_size,
            Some(winit::dpi::Size::Logical(winit::dpi::LogicalSize::new(
                100.0, 100.0
            )))
        );
    }

    /// The WM_SIZE settle posts the guest LOGICAL size: physical inner_size
    /// ÷ sf with the crate's rounding.
    #[test]
    fn settle_posts_logical_size() {
        // Retina 2×: a 1280×960 physical window is a 640×480 logical window.
        assert_eq!(guest_size_from_physical(1280, 960, 2.0), (640, 480));
        // Non-integer division rounds half away from zero (320.5 → 321).
        assert_eq!(guest_size_from_physical(641, 481, 2.0), (321, 241));
        // Scale factor 1.0 reproduces today's physical-as-logical posting.
        assert_eq!(guest_size_from_physical(640, 480, 1.0), (640, 480));
        assert_eq!(guest_size_from_physical(886, 776, 1.0), (886, 776));
    }

    /// The retry policy for a `NotDrawn` present: the first skip of a frame
    /// retries immediately; repeat skips re-arm throttled — fast while
    /// visible, slow while occluded, and at most `OCCLUDED_RETRY_MAX` slow
    /// tries before the frame parks. A new frame (`retried_once` reset by a
    /// publish wake) always gets a fresh immediate attempt, even while
    /// parked. This is what bounds the occluded-window spin — the pre-bound
    /// app re-requested unconditionally, so a window that stayed occluded
    /// (acquire keeps timing out) looped forever.
    #[test]
    fn present_retry_delay_policy() {
        // First skip of a frame: immediate.
        assert_eq!(retry_delay(false, false, 0), Some(Duration::ZERO));
        assert_eq!(retry_delay(false, true, 0), Some(Duration::ZERO));
        // Repeat skip, visible: fast cadence.
        assert_eq!(
            retry_delay(true, false, 0),
            Some(Duration::from_millis(PARKED_RETRY_MS))
        );
        // Repeat skip, occluded: slow cadence, capped at the bound.
        assert_eq!(
            retry_delay(true, true, 0),
            Some(Duration::from_millis(OCCLUDED_RETRY_MS))
        );
        assert_eq!(
            retry_delay(true, true, OCCLUDED_RETRY_MAX - 1),
            Some(Duration::from_millis(OCCLUDED_RETRY_MS))
        );
        assert_eq!(
            retry_delay(true, true, OCCLUDED_RETRY_MAX),
            None,
            "parked once the occluded bound is spent"
        );
        // A new frame always gets a fresh immediate attempt, even while
        // parked (the publish wake reset `retried_once`).
        assert_eq!(
            retry_delay(false, true, OCCLUDED_RETRY_MAX),
            Some(Duration::ZERO)
        );
    }

    // -----------------------------------------------------------------------
    // FS bottle-policy wiring (`resolve_gui_run_source`): the windowed GUI
    // entry's copy happens before the session build, same as the other run
    // entries. `run_gui_windowed` itself opens a real winit window, so these
    // pin the copy + identity half of that wiring headlessly.
    // -----------------------------------------------------------------------

    /// Unique temp dir under the system temp dir; removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("wie-gui-bottle-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A real file `ensure_exe_in_bottle` can copy.
    fn fake_exe(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"MZ\x90\x00").expect("write fake exe");
        path
    }

    /// The GUI entry's copy + identity wiring, mirroring
    /// `run_micro_runs_outside_exe_from_bottle_copy`: an exe outside the
    /// bottle resolves to a `drive_c` copy, and that copy's identity is its
    /// Program Files guest label — the reason the copy must happen before the
    /// session build.
    #[test]
    fn gui_run_source_copies_outside_exe_into_bottle() {
        let outside = TempDir::new("copy-src");
        let src_exe = fake_exe(outside.path(), "app.exe");
        let bottle = TempDir::new("copy-bottle");

        let resolved = resolve_gui_run_source(&src_exe, Some(bottle.path()), None)
            .expect("copy into the bottle should succeed");
        let expected = bottle
            .path()
            .join("drive_c")
            .join("Program Files")
            .join("app")
            .join("app.exe");
        assert_eq!(
            resolved, expected,
            "the GUI run source is the Program Files copy"
        );
        assert!(expected.is_file(), "bottle copy must exist");
        assert!(
            src_exe.is_file(),
            "the copy is non-destructive: the source stays"
        );

        let identity = wie_pe::process_identity_from_host_path_with_args(&resolved, &[]);
        assert_eq!(identity.module_file_name, "app.exe");
        // The runtime remaps the loader's `C:\{name}` through the volume
        // config, so the copy's guest module path is its real location.
        let volumes = wie_winapi::VolumeConfig::from_parts(Some(bottle.path().to_path_buf()), None);
        assert_eq!(
            wie_winapi::host_path_to_guest(&volumes, &resolved).as_deref(),
            Some(r"C:\Program Files\app\app.exe"),
            "the copy's guest module path is its Program Files location"
        );
        assert_eq!(identity.current_directory, r"C:\");
    }

    /// An in-bottle GUI source passes through unchanged (no re-copy).
    #[test]
    fn gui_run_source_passes_in_bottle_exe_through() {
        let bottle = TempDir::new("inside-bottle");
        let drive_c = bottle.path().join("drive_c");
        std::fs::create_dir_all(&drive_c).expect("create drive_c");
        let exe = fake_exe(&drive_c, "app.exe");

        let resolved = resolve_gui_run_source(&exe, Some(bottle.path()), None)
            .expect("in-bottle exe passes through");
        assert_eq!(resolved, exe, "the in-bottle exe is its own run source");
    }
}
