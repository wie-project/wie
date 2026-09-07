//! `GuestHandle` window-tree access: hit-testing, capture, and message posting.

use super::menu::{MenuNode, MenuTreeCache, build_menu_tree};
use std::sync::Arc;
use wie_winapi::user32::Dimension;
use wie_winapi::{WindowFlags, handles::Hmenu, handles::Hwnd};

/// Host-side handle to the WinAPI state for cross-thread access.
///
/// The presenter calls methods on this handle to post messages and read
/// frames.  Posting locks ONLY the dedicated message-queue mutex — never the
/// big `WinApiState` mutex the guest thread holds during API-handler
/// execution — so input events never block on guest work.
#[derive(Clone)]
pub struct GuestHandle {
    pub(super) state: std::sync::Arc<std::sync::Mutex<wie_winapi::WinApiState>>,
    pub(super) queue: std::sync::Arc<std::sync::Mutex<wie_winapi::present::MessageQueue>>,
    /// The host-presenter frame channel (ADR-0003): latest-wins frame slot,
    /// spare pool, z/window revision mirrors, presenter timing. Per-frame
    /// reads/writes (`take_frame`, `store_spare_buffer`, `windows_rev`,
    /// `z_snapshot`, `record_present_time`) lock ONLY this — never the big
    /// `WinApiState` mutex the guest thread holds during paint handlers.
    pub(super) present_channel: Arc<wie_winapi::present::PresentChannel>,
    /// Cached menu-bar tree, keyed by the menu handle it was built for.
    ///
    /// `window_menu_items` rebuilds only when `WindowState.menu_dirty` flips
    /// or the first menu-bearing window's handle changes, so the host frame
    /// loop stops reconstructing the tree (and locking the big mutex) on
    /// every frame.
    pub(super) menu_tree_cache: MenuTreeCache,
    /// Shared `shared_winapi` wait accumulators (Task 7) — the presenter
    /// side of the same counters the guest threads write.
    pub(super) lock_wait_stats: Arc<crate::mt_runtime::LockWaitStats>,
}

/// Descend the child hierarchy of `root` with z-order hit-testing: at each
/// level pick the topmost visible child containing `(rel_x, rel_y)`, then
/// recurse into it. Coordinates stay child-relative at every step, so the
/// returned point is relative to the deepest hit window. Returns the root
/// itself (with the given coordinates) when no child contains the point.
///
/// Wave 2 Step 2: runs over the presenter-side window mirror
/// (`present::MirrorWindow`) — never the big `WinApiState` lock.
fn hit_test_subtree(
    windows: &[wie_winapi::present::MirrorWindow],
    mut current: wie_winapi::handles::Hwnd,
    mut rel_x: i32,
    mut rel_y: i32,
) -> Option<(u64, u32, u32)> {
    loop {
        let hit = windows.iter().rev().find(|w| {
            w.parent == current
                && w.visible
                && rel_x >= w.x
                && rel_y >= w.y
                && rel_x < w.x.saturating_add(w.width)
                && rel_y < w.y.saturating_add(w.height)
        });
        let Some(child) = hit else {
            return Some((current.as_u64(), rel_x.max(0) as u32, rel_y.max(0) as u32));
        };
        current = child.handle;
        rel_x -= child.x;
        rel_y -= child.y;
    }
}

impl GuestHandle {
    /// Acquire the shared WinAPI state lock for a presenter-side access,
    /// timing the wait when runtime profiling is enabled.
    ///
    /// Mirrors the callers' poison policy: a poisoned lock reads as `None`
    /// (fail soft) exactly like the previous `.lock().ok()?` patterns, so
    /// poison behavior is unchanged. Disabled path pays one relaxed atomic
    /// load before the plain lock.
    pub(super) fn lock_state(&self) -> Option<std::sync::MutexGuard<'_, wie_winapi::WinApiState>> {
        if !self.lock_wait_stats.enabled() {
            return self.state.lock().ok();
        }
        let t0 = std::time::Instant::now();
        let guard = self.state.lock().ok()?;
        self.lock_wait_stats
            .record_presenter(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
        Some(guard)
    }

    /// Take the latest published frame for `hwnd`, if any.
    ///
    /// Channel-only: never touches the big `WinApiState` mutex, so a guest
    /// paint handler or D3D9 draw holding that lock cannot stall the
    /// presenter (and the macOS UI thread) — the whole point of the channel.
    #[must_use]
    pub fn take_frame(&self, hwnd: u64) -> Option<wie_winapi::present::SurfaceFrame> {
        self.present_channel
            .take_frame(wie_winapi::handles::Hwnd::from(hwnd))
    }

    /// Frames published but not yet taken — the presenter re-arms its redraw
    /// when this is nonzero after a take (closing the publish gate's one
    /// race). Lock-free.
    #[must_use]
    pub fn pending_frames(&self) -> u64 {
        self.present_channel.pending_frames()
    }

    pub fn store_spare_buffer(&self, hwnd: u64, buffer: Vec<u32>) {
        self.present_channel
            .store_spare(wie_winapi::handles::Hwnd::from(hwnd), buffer);
    }

    /// The EDIT control's caret/selection for `hwnd`: `(caret, sel_start,
    /// sel_end)` in character indices. `None` for a non-EDIT window or an
    /// unseeded control. Read by the GUI micro-tests to observe the caret
    /// after a guest `EM_SETSEL`/`EM_SCROLLCARET`.
    ///
    /// Wave 2 Step 2 note: legitimately still takes the big lock — this is a
    /// TEST-ONLY observation accessor (the selection state lives in the
    /// built-in control records, which have no host-side consumer), so
    /// mirroring it would be pure overhead.
    #[must_use]
    pub fn edit_selection(&self, hwnd: u64) -> Option<(usize, usize, usize)> {
        let state = self.lock_state()?;
        state.try_window_state()?.edit_selection(hwnd)
    }

    /// The control-text buffer of `hwnd` (what `SetWindowText`/`WM_SETTEXT`
    /// maintain for built-in controls). Read by the GUI micro-tests to check
    /// a dialog field's contents.
    ///
    /// Wave 2 Step 2 note: legitimately still takes the big lock — test-only
    /// observation accessor (same reasoning as [`Self::edit_selection`]).
    #[must_use]
    pub fn control_text(&self, hwnd: u64) -> Option<String> {
        let state = self.lock_state()?;
        state
            .try_window_state()?
            .control_text(hwnd)
            .map(str::to_owned)
    }

    /// The text of status-bar part `part` (`SB_SETTEXT`), when `hwnd` is a
    /// STATUSCLASSNAMEW window. Read by the GUI micro-tests to observe the
    /// Ln/Col indicator text after a guest mutation.
    ///
    /// Wave 2 Step 2 note: legitimately still takes the big lock — test-only
    /// observation accessor (same reasoning as [`Self::edit_selection`]).
    #[must_use]
    pub fn status_bar_part_text(&self, hwnd: u64, part: usize) -> Option<String> {
        let state = self.lock_state()?;
        state.try_window_state()?.status_bar_part_text(hwnd, part)
    }

    /// Whether frame timing instrumentation is active (lock-free gate).
    #[must_use]
    pub fn frame_timing_enabled(&self) -> bool {
        wie_winapi::present::frame_timing_enabled()
    }

    /// Record one host present (upload + present) wall time (ns).
    /// Channel-only (the internal gate makes this a no-op when timing is
    /// disabled) — never the big lock.
    pub fn record_present_time(&self, ns: u128) {
        if !wie_winapi::present::frame_timing_enabled() {
            return;
        }
        self.present_channel.record_present(ns);
    }

    /// Publish duration of the most recent frame (ns; 0 when timing disabled).
    ///
    /// Wave 2 Step 2 note: legitimately still takes the big `WinApiState`
    /// lock — a diagnostics-only read (profile logging), not on any hot path.
    #[must_use]
    pub fn present_publish_ns_last(&self) -> u128 {
        let Some(state) = self.lock_state() else {
            return 0;
        };
        state.try_present().map_or(0, |p| p.publish_ns_last)
    }

    /// Return the first (and typically only) guest-created window handle.
    ///
    /// Wave 2 Step 2: reads the presenter-side window mirror — never the big
    /// `WinApiState` lock.
    #[must_use]
    pub fn first_guest_window_handle(&self) -> Option<u64> {
        self.present_channel
            .mirror_windows(|windows| windows.first().map(|w| w.handle.as_u64()))
            .flatten()
    }

    /// Hit-test a point in the first top-level window's client area.
    ///
    /// Returns `(hwnd, rel_x, rel_y)` for the topmost visible child containing
    /// the point, or the top-level window itself (with client-relative
    /// coordinates) when no child is hit. `None` when no top-level window
    /// exists. The top-level window is the first parentless record (the main
    /// guest window); children are tested in reverse creation order, matching
    /// Windows' z-order hit-testing, and the descent recurses through the
    /// whole child hierarchy — a modal dialog's own controls (buttons, edits)
    /// are children of the dialog, not of the top-level window, so a click on
    /// a dialog button must resolve to the button, not the dialog.
    ///
    /// The GUI presenter uses [`Self::window_at_in`] instead — this roots at
    /// the first parentless record, so it cannot resolve a SECOND top-level's
    /// controls (a dialog presented in its own winit window). Kept for the
    /// headless/screenshot callers.
    ///
    /// Wave 2 Step 2: reads the presenter-side window mirror — never the big
    /// `WinApiState` lock (headless/screenshot callers only; the GUI
    /// presenter uses [`Self::window_at_in`]).
    #[must_use]
    pub fn window_at(&self, x: i32, y: i32) -> Option<(u64, u32, u32)> {
        let (top, result) = self
            .present_channel
            .mirror_windows(|windows| {
                let top = windows
                    .iter()
                    .find(|w| w.parent == wie_winapi::handles::Hwnd::NULL)?
                    .handle;
                Some((top, hit_test_subtree(windows, top, x, y)))
            })
            .flatten()?;
        let _ = top;
        result
    }

    /// Hit-test a point in the subtree rooted at `hwnd` (a top-level window).
    ///
    /// Same descent as [`Self::window_at`] (topmost visible child per level,
    /// child-relative coordinates throughout) but rooted at the caller's
    /// window, so a mouse event delivered to a SECOND top-level (a dialog in
    /// its own winit window) resolves that window's own controls instead of
    /// the main window's. Returns `None` when `hwnd` is not a live guest
    /// window (a destroyed handle must not hit-test stale children).
    ///
    /// Wave 2 Step 2: reads the presenter-side window mirror — every mouse
    /// event runs this without ever taking the big `WinApiState` lock.
    #[must_use]
    pub fn window_at_in(&self, hwnd: u64, x: i32, y: i32) -> Option<(u64, u32, u32)> {
        let root = wie_winapi::handles::Hwnd::from(hwnd);
        self.present_channel
            .mirror_windows(|windows| {
                windows.iter().find(|w| w.handle == root)?;
                hit_test_subtree(windows, root, x, y)
            })
            .flatten()
    }

    /// Resolve the destination for a mouse message under active capture.
    ///
    /// Returns `(hwnd, rel_x, rel_y)` for the window holding the mouse capture
    /// (SetCapture — a pressed BUTTON captures until its `WM_LBUTTONUP`), with
    /// coordinates relative to that window, or `None` when no window captures
    /// (the caller falls back to hit-testing via [`Self::window_at`]).
    ///
    /// The relative coordinates accumulate the whole ancestor chain — a
    /// button inside a modal dialog sits at `dialog.x + button.x` in the
    /// top-level client space, not just `button.x`.
    ///
    /// Wave 2 Step 2: reads the presenter-side window mirror — every mouse
    /// event runs this without ever taking the big `WinApiState` lock.
    #[must_use]
    pub fn capture_target(&self, x: i32, y: i32) -> Option<(u64, u32, u32)> {
        let capture = self.present_channel.mirror_meta()?.1;
        if capture == wie_winapi::handles::Hwnd::NULL {
            return None;
        }
        self.present_channel
            .mirror_windows(|windows| {
                windows.iter().find(|w| w.handle == capture)?;
                let (mut offset_x, mut offset_y) = (0_i32, 0_i32);
                let mut current = capture;
                while let Some(w) = windows.iter().find(|w| w.handle == current) {
                    if w.parent == wie_winapi::handles::Hwnd::NULL {
                        break;
                    }
                    offset_x = offset_x.saturating_add(w.x);
                    offset_y = offset_y.saturating_add(w.y);
                    current = w.parent;
                }
                Some((
                    capture.as_u64(),
                    x.saturating_sub(offset_x).max(0) as u32,
                    y.saturating_sub(offset_y).max(0) as u32,
                ))
            })
            .flatten()
    }

    /// The guest's top-level window-SET revision — bumped by every top-level
    /// create/destroy (see [`wie_winapi::present::PresentState::windows_rev`]).
    ///
    /// The Frame handler's reconcile-on-change latch: the winit window
    /// registry is reconciled only when this changes, so an idle repaint of
    /// an unchanged window set skips the enumerate+diff entirely.
    #[must_use]
    pub fn windows_rev(&self) -> u64 {
        self.present_channel.windows_rev()
    }

    /// The guest's top-level z-order revision — bumped by every top-level
    /// create/destroy AND `SetWindowPos` HWND_TOP/HWND_BOTTOM z-change (see
    /// [`wie_winapi::present::PresentState::z_rev`]).
    ///
    /// The Frame handler re-orders its NSWindows only when this changes.
    #[must_use]
    pub fn z_rev(&self) -> u64 {
        self.present_channel.z_rev()
    }

    /// Atomic snapshot of the guest top-level z-order: the revision AND the
    /// ordered list read under ONE lock.
    ///
    /// Separate locked reads (revision, then list) would hand the caller a
    /// fresh revision with a stale list (or the reverse). The Frame handler
    /// uses this combined accessor so the reorder always applies the list the
    /// revision it compared actually describes.
    #[must_use]
    pub fn z_snapshot(&self) -> (u64, Vec<u64>) {
        self.present_channel.z_snapshot()
    }

    /// The top-level (parentless) ancestor of the focused window.
    ///
    /// The window a modal host dialog (the MessageBox bridge) should parent
    /// to: a MessageBox opened while a dialog is focused parents to that
    /// dialog's owner top-level. `None` when nothing is focused — the caller
    /// falls back to the primary window.
    ///
    /// Wave 2 Step 2: reads the presenter-side window mirror (focus handle
    /// from `mirror_meta`, ancestry from the mirrored windows) — never the
    /// big `WinApiState` lock. Called only when a MessageBox opens, so the
    /// mirror is fresh from the last handler's `finish()`.
    #[must_use]
    pub fn focused_top_level(&self) -> Option<u64> {
        let mut current = self.present_channel.mirror_meta()?.0;
        if current == wie_winapi::handles::Hwnd::NULL {
            return None;
        }
        self.present_channel
            .mirror_windows(|windows| {
                loop {
                    let window = windows.iter().find(|w| w.handle == current)?;
                    if window.parent == wie_winapi::handles::Hwnd::NULL {
                        return Some(current.as_u64());
                    }
                    current = window.parent;
                }
            })
            .flatten()
    }

    /// The window with keyboard focus (what `GetFocus` returns in-guest).
    ///
    /// Wave 2 Step 2 note: legitimately still takes the big `WinApiState`
    /// lock — a rare host-side read (focus changes are rare), and the mirror
    /// carries focus only as a side channel of `mirror_meta`. Not on any hot
    /// path; left as-is.
    #[must_use]
    pub fn focus_window(&self) -> Option<u64> {
        let state = self.lock_state()?;
        let focus = state.try_window_state()?.focus_window_handle;
        (focus != wie_winapi::handles::Hwnd::NULL).then_some(focus.as_u64())
    }

    /// Whether `TrackMouseEvent` armed hover/leave tracking for `hwnd`.
    ///
    /// The host forwards `WM_MOUSEHOVER` / `WM_MOUSELEAVE` only for tracked
    /// windows — without a `TrackMouseEvent` call Windows sends neither.
    ///
    /// Wave 2 Step 2: reads the presenter-side window mirror — every mouse
    /// event runs this without ever taking the big `WinApiState` lock. A
    /// poisoned mirror mutex (a panicked syncer) reads as "not tracking",
    /// matching the lock-failure fallback it replaces.
    #[must_use]
    pub fn mouse_tracking(&self, hwnd: u64) -> bool {
        let target = wie_winapi::handles::Hwnd::from(hwnd);
        self.present_channel
            .mirror_windows(|windows| {
                windows
                    .iter()
                    .any(|w| w.handle == target && w.mouse_tracking)
            })
            .unwrap_or(false)
    }

    /// Update one virtual key's pressed state in the guest keyboard-state
    /// array (the 256-byte table `GetKeyState` / `GetAsyncKeyState` /
    /// `IsDialogMessage`'s Shift+Tab read).
    ///
    /// `pressed` sets or clears bit 0x80 (the "key is down" flag) for `vk`.
    ///
    /// Wave 2 Step 2: pushes a `(vk, pressed)` event onto the presenter-side
    /// window mirror — no big `WinApiState` lock here (this runs per key
    /// event on the presenter thread while the guest holds that lock for
    /// whole handler batches). The guest-side readers (`GetKeyState`,
    /// `GetAsyncKeyState`, `GetKeyboardState`, `IsDialogMessage`, EDIT
    /// caret/scroll, accelerator translation) drain the events into the real
    /// `keyboard_state` under the big lock they already hold.
    pub fn set_key_state(&self, vk: u16, pressed: bool) {
        self.present_channel.push_key_write(vk, pressed);
    }

    /// Record the latest host cursor position in guest-logical screen pixels
    /// for the guest's `GetCursorPos` reader.
    ///
    /// Wave 5 (P4): pushes onto the presenter-side window mirror — no big
    /// `WinApiState` lock here (this runs per cursor move on the event-loop
    /// thread). The guest reader copies the latest value out under the big
    /// lock it already holds; the read is a copy, NOT a drain, because
    /// cursor position is level-triggered.
    pub fn set_cursor_pos(&self, x: i32, y: i32) {
        self.present_channel.push_cursor_pos(x, y);
    }

    /// Guest-logical screen origin of the top-level window owning `hwnd`
    /// (that record's `(x, y)`).
    ///
    /// Wave 5 (P4): `GetCursorPos` reports guest-screen coordinates, so the
    /// host adds the owning top-level's record origin to the
    /// client-relative cursor position before pushing it (see
    /// [`Self::set_cursor_pos`]). Reads the presenter-side window mirror —
    /// never the big `WinApiState` lock. `None` when `hwnd` is not a live
    /// guest window.
    #[must_use]
    pub fn top_level_origin(&self, hwnd: u64) -> Option<(i32, i32)> {
        let start = wie_winapi::handles::Hwnd::from(hwnd);
        self.present_channel
            .mirror_windows(|windows| {
                let mut current = start;
                // Ascend the parent chain to the top-level ancestor. The
                // depth is bounded by the record count so a corrupt parent
                // cycle cannot spin the event loop.
                for _ in 0..windows.len().saturating_add(1) {
                    let record = windows.iter().find(|w| w.handle == current)?;
                    if record.parent == wie_winapi::handles::Hwnd::NULL {
                        return Some((record.x, record.y));
                    }
                    current = record.parent;
                }
                None
            })
            .flatten()
    }

    /// Return info for the first guest window: (hwnd, title, width, height).
    ///
    /// Wave 2 Step 2: reads the presenter-side window mirror — never the big
    /// `WinApiState` lock.
    #[must_use]
    pub fn first_guest_window_info(&self) -> Option<(u64, String, i32, i32)> {
        self.present_channel
            .mirror_windows(|windows| {
                windows
                    .first()
                    .map(|w| (w.handle.as_u64(), w.title.clone(), w.width, w.height))
            })
            .flatten()
    }

    /// Snapshot of the menu the macOS bar should mirror, as a tree.
    ///
    /// macOS has ONE global menu bar; Windows has one menu per window, so the
    /// bar mirrors the FOCUSED guest window's menu (the dynamic-menu pattern)
    /// — the guest's `SetFocus` state is authoritative, and focus can sit on
    /// a child (an EDIT inside the focused top-level), so the selection
    /// ascends to that child's top-level ancestor, which carries the menu.
    /// Falls back to the first menu-bearing top-level window when nothing is
    /// focused or the focused top-level has no menu (the pre-focus behavior,
    /// e.g. before any window has focus).
    ///
    /// Walks the native `MenuRecord` tree once and caches the result: while
    /// `menu_dirty` is false AND the selected menu handle is unchanged the
    /// cache is returned without touching the menu records.
    ///
    /// Wave 2 Step 2: the fast path checks the presenter-side window mirror
    /// first — when the mirror reports no menu mutation since the last sync
    /// AND the mirror-resolved menu handle matches the cached handle, the
    /// cached tree is returned WITHOUT the big `WinApiState` lock (this runs
    /// per Frame on the presenter thread). Only a cache miss or a dirty
    /// mirror takes the lock to rebuild the tree (then re-syncs the mirror
    /// so the next Frame is clean again). A poisoned mirror mutex falls
    /// through to the locked path. The mirror can be one handler behind
    /// (read racing the mutating handler's `finish()`), so a menu change
    /// lands at most one Frame late — the same cadence the presenter paints
    /// at.
    #[must_use]
    pub fn window_menu_items(&self) -> Arc<Vec<MenuNode>> {
        if let Some((focus, _, menu_dirty)) = self.present_channel.mirror_meta()
            && !menu_dirty
        {
            let resolved: Option<wie_winapi::handles::Hmenu> = self
                .present_channel
                .mirror_windows(|windows| {
                    // The focused window's menu wins; a child window's
                    // menu slot holds its child id, so ascend to the
                    // focus's parentless top-level first.
                    let mut focus_top = focus;
                    if focus_top != wie_winapi::handles::Hwnd::NULL {
                        loop {
                            let Some(w) = windows.iter().find(|w| w.handle == focus_top) else {
                                focus_top = wie_winapi::handles::Hwnd::NULL;
                                break;
                            };
                            if w.parent == wie_winapi::handles::Hwnd::NULL {
                                break;
                            }
                            focus_top = w.parent;
                        }
                    }
                    windows
                        .iter()
                        .find(|w| w.handle == focus_top && w.menu_handle != 0)
                        .map(|w| w.menu_handle)
                        .or_else(|| {
                            windows.iter().find_map(|w| {
                                (w.parent == wie_winapi::handles::Hwnd::NULL && w.menu_handle != 0)
                                    .then_some(w.menu_handle)
                            })
                        })
                        .map(wie_winapi::handles::Hmenu::from)
                })
                .flatten();
            let Some(menu_handle) = resolved else {
                // Mirror-resolved: no menu anywhere — same answer the
                // locked path returns, without the lock.
                return Arc::new(Vec::new());
            };
            let cached = self.menu_tree_cache.read().ok();
            if let Some(cached) = cached
                && let Some((cached_handle, tree)) = cached.as_ref()
                && *cached_handle == menu_handle
            {
                // Cache hit: refcount bump — no per-frame tree deep clone,
                // no big lock.
                return Arc::clone(tree);
            }
        }
        self.window_menu_items_locked()
    }

    /// The locked rebuild path for [`Self::window_menu_items`] — the original
    /// big-lock body, kept for cache misses, dirty mirrors, poisoned mirrors,
    /// and tests that seed state directly.
    fn window_menu_items_locked(&self) -> Arc<Vec<MenuNode>> {
        let Some(mut state) = self.lock_state() else {
            return Arc::new(Vec::new());
        };
        let Some(ws) = state.try_window_state() else {
            return Arc::new(Vec::new());
        };
        // The focused window's menu wins; `menu_handle != 0` alone is NOT a
        // menu — a child window's slot holds its child id — so first ascend
        // to the focus's parentless top-level, then check ITS handle.
        let mut focus_top = ws.focus_window_handle;
        if focus_top != wie_winapi::handles::Hwnd::NULL {
            loop {
                let Some(w) = ws.windows.iter().find(|w| w.handle == focus_top) else {
                    focus_top = wie_winapi::handles::Hwnd::NULL;
                    break;
                };
                if w.parent_handle == wie_winapi::handles::Hwnd::NULL {
                    break;
                }
                focus_top = w.parent_handle;
            }
        }
        let Some(menu_handle) = ws
            .windows
            .iter()
            .find(|w| w.handle == focus_top && w.menu_handle != 0)
            .map(|w| w.menu_handle)
            .or_else(|| {
                ws.windows.iter().find_map(|w| {
                    (w.parent_handle == wie_winapi::handles::Hwnd::NULL && w.menu_handle != 0)
                        .then_some(w.menu_handle)
                })
            })
        else {
            return Arc::new(Vec::new());
        };
        // The window record stores the raw u64 handle; the cache is keyed by
        // the typed menu handle so a stale build (different handle) is a
        // compile-time type mismatch, not a silent u64 aliasing.
        let menu_handle = Hmenu::from(menu_handle);
        if !ws.menu_dirty {
            let cached = self.menu_tree_cache.read().ok();
            if let Some(cached) = cached
                && let Some((cached_handle, tree)) = cached.as_ref()
                && *cached_handle == menu_handle
            {
                // Cache hit: refcount bump — no per-frame tree deep clone.
                return Arc::clone(tree);
            }
        }
        let tree = Arc::new(build_menu_tree(&ws.menus, menu_handle.as_u64()));
        if let Ok(mut cache) = self.menu_tree_cache.write() {
            *cache = Some((menu_handle, Arc::clone(&tree)));
        }
        // Re-arm the cache: `menu_dirty` is set by the winapi menu handlers on
        // every mutation (LoadMenu, AppendMenu, EnableMenuItem, ...) but is
        // never cleared there, so WITHOUT this reset every call would bypass
        // the cache and rebuild the tree. That makes the host bar's
        // per-Frame `sync_menu_bar` see spurious content differences whenever
        // the guest touches its menu state (notepad re-enables Find/FindNext
        // on selection changes), triggering a full native-menu teardown +
        // reinstall (`remove_for_nsapp` sets the main menu to None) — which
        // destroys any menu interaction in flight. The tree built here
        // already reflects every mutation that set the flag, so clearing it
        // is exactly the "consumed since last mutation" contract; the next
        // guest mutation re-sets it and the cache correctly invalidates.
        state.window_state().menu_dirty = false;
        // Re-sync the mirror (fresh projection + cleared mirror menu_dirty)
        // so the next per-Frame call takes the lock-free fast path again.
        state.sync_window_mirror();
        tree
    }

    /// Set the host file-dialog bridge — called by the `GetOpenFileNameA/W`
    /// / `GetSaveFileNameA/W` handlers (under [`wie_winapi::FileDialogPolicy::Interactive`])
    /// with the request built from the guest's `OPENFILENAME`; the returned
    /// pick's HOST path is written back into the guest buffer.
    ///
    /// Mirrors [`Self::set_message_box_bridge`]: the GUI presenter registers
    /// the native-panel callback (rfd NSOpenPanel/NSSavePanel) here once at
    /// startup, and the guest thread invokes it from the handler. The callback
    /// blocks until the user picks (the guest thread parks inside the
    /// handler), which is dialog semantics. The handler confines the returned
    /// host path to a guest volume at accept — a pick outside the bottle
    /// cancels. When no bridge is registered the handlers keep the in-app
    /// emulated dialog, so headless runs and `trace` never hang.
    pub fn set_file_dialog_bridge(&self, cb: wie_winapi::FileDialogBridge) {
        if let Some(mut state) = self.lock_state() {
            state.window_state().file_dialog_bridge = Some(cb);
        }
    }

    /// Set the host print-dialog bridge — called by the `PrintDlgW` handler
    /// (under [`wie_winapi::PrintDialogPolicy::Interactive`]) with the
    /// request seeded from the guest's `PRINTDLG`/DEVMODE; the returned
    /// pick's settings are written back into the guest `PRINTDLG`.
    ///
    /// Mirrors [`Self::set_file_dialog_bridge`]: the GUI presenter registers
    /// the native-panel callback (macOS NSPrintPanel) here once at startup,
    /// and the guest thread invokes it from the handler. The callback blocks
    /// until the user picks (the guest thread parks inside the handler),
    /// which is dialog semantics. When no bridge is registered the handler
    /// cancels, so headless runs and `trace` never hang.
    pub fn set_print_dialog_bridge(&self, cb: wie_winapi::PrintDialogBridge) {
        if let Some(mut state) = self.lock_state() {
            state.window_state().print_dialog_bridge = Some(cb);
        }
    }

    /// Set the host page-setup bridge — called by the `PageSetupDlgW` handler
    /// (under [`wie_winapi::PageSetupDialogPolicy::Interactive`]) with the
    /// request seeded from the guest's `PAGESETUPDLG`/DEVMODE; the returned
    /// pick's paper/orientation are written back into the guest
    /// `PAGESETUPDLG`.
    ///
    /// Mirrors [`Self::set_print_dialog_bridge`]: the GUI presenter registers
    /// the native-panel callback (macOS NSPageLayout) here once at startup,
    /// and the guest thread invokes it from the handler. The callback blocks
    /// until the user picks (the guest thread parks inside the handler),
    /// which is dialog semantics. When no bridge is registered the handler
    /// cancels, so headless runs and `trace` never hang.
    pub fn set_page_setup_dialog_bridge(&self, cb: wie_winapi::PageSetupDialogBridge) {
        if let Some(mut state) = self.lock_state() {
            state.window_state().page_setup_dialog_bridge = Some(cb);
        }
    }

    /// Set the host print-operation bridge — called by the gdi32 `EndDoc`
    /// handler (with the completed [`wie_winapi::PrintJobRequest`], pages
    /// moved in) to run the native print pipeline (macOS NSPrintOperation);
    /// the returned success flag becomes the `EndDoc` return value.
    ///
    /// Mirrors [`Self::set_print_dialog_bridge`]: the GUI presenter registers
    /// the native-operation callback here once at startup, and the guest
    /// thread invokes it from the handler. The callback blocks until the
    /// operation finishes (the guest thread parks inside the handler), which
    /// is print semantics. When no bridge is registered the handler keeps the
    /// `WIE_PRINT_TO` BMP oracle, so headless runs and `trace` never block.
    pub fn set_print_job_bridge(&self, cb: wie_winapi::PrintJobBridge) {
        if let Some(mut state) = self.lock_state() {
            state.window_state().print_job_bridge = Some(cb);
        }
    }

    /// Set the wake callback — called when a new frame is published.
    pub fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
        if let Some(mut state) = self.lock_state() {
            state.present().wake = Some(cb);
        }
    }

    /// Enable D3D9 Present-commit mode (implementation-plan Wave 2, Option
    /// A1): spawns the Present-committer render thread on this session's
    /// channel and flips the commit gate, so `IDirect3DDevice9::Present`
    /// hands its finished backbuffer to the render thread instead of
    /// stretching + publishing inline under the big `WinApiState` lock.
    ///
    /// `wake` is the frame-arrival callback the committer fires after a
    /// publish that passes the channel wake gate — the same closure shape
    /// [`Self::set_wake`] registers for the guest publish path.
    /// `WIE_PRESENT_COMMIT=0` disables the feature entirely (the legacy
    /// inline path stays).
    ///
    /// Keep the returned handle alive for the session's lifetime: dropping it
    /// stops and joins the render thread (its `Drop`). `None` = the thread
    /// could not spawn — the legacy path stays active.
    pub fn enable_present_commit(
        &self,
        wake: Box<dyn Fn() + Send + 'static>,
    ) -> Option<wie_winapi::present::CommitterHandle> {
        if std::env::var("WIE_PRESENT_COMMIT").is_ok_and(|v| v == "0") {
            return None;
        }
        let committer =
            wie_winapi::present::spawn_present_committer(Arc::clone(&self.present_channel), wake)?;
        // Flip the gate only after the consumer exists.
        self.present_channel.set_commit_enabled(true);
        Some(committer)
    }

    /// Enable the D3D9 command-capture pipeline (implementation-plan Wave 2
    /// slice 2): spawns the capture render thread on this session's channel
    /// and flips the capture gate, so `Draw*`/`Clear` handlers record ops
    /// instead of rasterizing inline and `Present` flushes the stream to the
    /// render thread (see `wie_winapi::d3d9::capture` for the op model and
    /// the render-target/depth round-trip protocol).
    ///
    /// The caller decides the opt-in policy: the GUI host only calls this
    /// when `WIE_CAPTURE_STREAM=1` (default off during bring-up — flip to
    /// default-on after the hash-equivalence gate passes). Headless runs and
    /// the other micro-suite tests never call it, so the CI hashes exercise
    /// the legacy in-handler raster path unchanged (the dedicated capture
    /// test spawns it explicitly).
    ///
    /// `wake` is the frame-arrival callback the render thread fires after a
    /// publish that passes the channel wake gate — the same closure shape
    /// [`Self::set_wake`] and [`Self::enable_present_commit`] register.
    ///
    /// Keep the returned handle alive for the session's lifetime: dropping
    /// it stops and joins the render thread (its `Drop`). `None` = the
    /// thread could not spawn — the legacy path stays active.
    pub fn enable_capture_stream(
        &self,
        wake: Box<dyn Fn() + Send + 'static>,
    ) -> Option<wie_winapi::present::CaptureStreamerHandle> {
        let streamer =
            wie_winapi::present::spawn_capture_streamer(Arc::clone(&self.present_channel), wake)?;
        // Flip the gate only after the consumer exists.
        self.present_channel.set_capture_enabled(true);
        Some(streamer)
    }

    /// Set the host MessageBox bridge — called by the `MessageBoxA/W`
    /// handlers with `(caption, text, mb_type)`; the returned Win32 id
    /// (IDOK/IDCANCEL/IDYES/IDNO) is returned to the guest.
    ///
    /// Mirrors [`Self::set_wake`]: the GUI presenter registers the native-alert
    /// callback here once at startup. The handlers never invoke it directly —
    /// they return [`wie_winapi::WinApiControlSignal::MessageBoxBridgeRequested`]
    /// and the runtime runs this callback WITHOUT the shared state lock (the
    /// winit event loop needs that lock to service frame events while the
    /// alert is up), then the handler's re-entry returns the chosen id to the
    /// guest. When no bridge is registered the handlers keep the
    /// console-echo + IDOK fallback, so headless runs and `trace` never hang.
    pub fn set_message_box_bridge(&self, cb: wie_winapi::present::MessageBoxBridge) {
        if let Some(mut state) = self.lock_state() {
            state.present().message_box_bridge = Some(cb);
        }
    }

    /// Store a host-side file drop (winit `DroppedFile`) as the guest drop
    /// list and return the fake `HDROP` the caller posts as the `WM_DROPFILES`
    /// `wParam`.
    ///
    /// Host paths are translated to guest-visible `C:\…` / `D:\…` paths
    /// through the session's volume config ([`wie_winapi::host_path_to_guest`]
    /// — the inverse of the guest → host bottle mapping); a dropped file
    /// outside both volumes is skipped, because the guest filesystem cannot
    /// see it. When NO path maps into a volume the whole drop is skipped —
    /// the function returns 0 and the caller posts no `WM_DROPFILES` (a fake
    /// HDROP over an empty list would make the guest open an empty path).
    /// `point` is the drop point in client coordinates, which the guest
    /// reads back via `DragQueryPoint`.
    #[must_use]
    pub fn set_drop_files(&self, paths: Vec<std::path::PathBuf>, point: (i32, i32)) -> u64 {
        let Some(mut state) = self.lock_state() else {
            return 0;
        };
        let volumes = state.file_io.volumes.clone();
        let guest_paths: Vec<String> = paths
            .iter()
            .filter_map(|p| wie_winapi::host_path_to_guest(&volumes, p))
            .collect();
        if guest_paths.is_empty() {
            tracing::info!(
                target: "wiegui",
                host_path = %paths.first().map_or("", |p| p.to_str().unwrap_or("<non-utf8>")),
                "drop skipped: no guest volume maps the host path"
            );
            return 0;
        }
        tracing::info!(target: "wiegui", guest_paths = ?guest_paths, "drop stored for WM_DROPFILES");
        state.drag_drop().set_drop(guest_paths, point);
        wie_winapi::user32::dragdrop::FAKE_HDROP
    }

    /// Update the guest-visible window size (host window was resized).
    ///
    /// Updates the `WindowRecord` in place — no allocation.  Uses `try_lock`
    /// so the per-`Resized`-event hot path NEVER blocks on the guest thread
    /// mid-API-call: if the record can't be locked right now, the settled
    /// `WM_SIZE` carries the final size anyway (and the guest's own
    /// `recreate_dib` reads `lParam`, not the record).
    pub fn resize_window(&self, hwnd: Hwnd, size: Dimension) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        let ws = state.window_state();
        if let Some(window) = ws.windows.iter_mut().find(|w| w.handle == hwnd) {
            window.width = size.width;
            window.height = size.height;
            window.client_rect = (0, 0, window.width, window.height);
            // The guest DIB is reallocated ZEROED on resize, so the resized
            // window itself must enter the erase/paint cycle: the class-brush
            // erase fills the reallocated surface before the guest paints.
            // Mirrors the SetWindowPlacement show path (both flags, cb38299) —
            // a bare WM_PAINT leaves the unpainted area black.
            window.invalidated = true;
            window.flags.insert(WindowFlags::ERASE_BACKGROUND);
        }
        // Invalidate the whole descendant subtree (children, grandchildren —
        // e.g. a dialog's buttons).  A WS_CLIPCHILDREN parent's own repaint is
        // deliberately clipped around its children, so their regions in the
        // reallocated ancestor surface keep stale/zero-padded pixels unless
        // they repaint themselves — in real Windows those pixels persist in
        // the framebuffer; here the surface is rebuilt on resize, so the
        // children must be explicitly repainted via WM_PAINT synthesis.
        let mut descendants: Vec<wie_winapi::handles::Hwnd> = Vec::new();
        let mut frontier = vec![hwnd];
        while let Some(parent) = frontier.pop() {
            for w in &ws.windows {
                if w.parent_handle == parent {
                    descendants.push(w.handle);
                    frontier.push(w.handle);
                }
            }
        }
        for w in ws.windows.iter_mut() {
            if descendants.contains(&w.handle) {
                w.invalidated = true;
            }
        }
        // This mutates geometry OUTSIDE any handler's `finish()` (it runs on
        // the presenter thread), so the mirror would go stale with no rev
        // bump to catch it — push the fresh projection now. Rare (per host
        // resize), so the full sync cost is fine.
        state.sync_window_mirror();
    }

    /// Consume a pending guest-requested host-window geometry change, if any.
    ///
    /// `SetWindowPlacement` (guest thread) records `(x, y, width, height)` in
    /// screen coordinates here when the applied rcNormalPosition differs from
    /// the current rect, along with the hwnd it was applied to — so with
    /// multiple top-levels the host applies the move to the matching winit
    /// window, not always the primary one. The host presenter applies it and
    /// calls this to clear the slot; `None` when no move is pending. Mirrors
    /// the [`Self::resize_window`] seam — geometry flows guest → host through
    /// the shared `WinApiState`, applied on the event-loop thread.
    ///
    /// Wave 2 Step 2 note: legitimately still takes the big `WinApiState`
    /// lock — this CONSUMES a slot (take), which a read-only mirror can't
    /// express, and it fires once per guest-requested move, not per frame.
    #[must_use]
    pub fn take_host_geometry_request(&self) -> Option<(u64, i32, i32, i32, i32)> {
        let mut state = self.lock_state()?;
        let ws = state.window_state();
        let rect = ws.host_geometry_request.take()?;
        let hwnd = ws.host_geometry_hwnd.take()?;
        Some((hwnd, rect.0, rect.1, rect.2, rect.3))
    }
}

#[cfg(test)]
mod tests;
