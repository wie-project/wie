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
fn hit_test_subtree(
    windows: &[wie_winapi::WindowRecord],
    mut current: wie_winapi::handles::Hwnd,
    mut rel_x: i32,
    mut rel_y: i32,
) -> Option<(u64, u32, u32)> {
    loop {
        let hit = windows.iter().rev().find(|w| {
            w.parent_handle == current
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
    #[must_use]
    pub fn edit_selection(&self, hwnd: u64) -> Option<(usize, usize, usize)> {
        let state = self.lock_state()?;
        state.try_window_state()?.edit_selection(hwnd)
    }

    /// The control-text buffer of `hwnd` (what `SetWindowText`/`WM_SETTEXT`
    /// maintain for built-in controls). Read by the GUI micro-tests to check
    /// a dialog field's contents.
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
    #[must_use]
    pub fn present_publish_ns_last(&self) -> u128 {
        let Some(state) = self.lock_state() else {
            return 0;
        };
        state.try_present().map_or(0, |p| p.publish_ns_last)
    }

    /// Return the first (and typically only) guest-created window handle.
    #[must_use]
    pub fn first_guest_window_handle(&self) -> Option<u64> {
        let state = self.lock_state()?;
        state
            .try_window_state()?
            .windows
            .first()
            .map(|w| w.handle.as_u64())
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
    #[must_use]
    pub fn window_at(&self, x: i32, y: i32) -> Option<(u64, u32, u32)> {
        let state = self.lock_state()?;
        let windows = &state.try_window_state()?.windows;
        let top = windows
            .iter()
            .find(|w| w.parent_handle == wie_winapi::handles::Hwnd::NULL)?
            .handle;
        hit_test_subtree(windows, top, x, y)
    }

    /// Hit-test a point in the subtree rooted at `hwnd` (a top-level window).
    ///
    /// Same descent as [`Self::window_at`] (topmost visible child per level,
    /// child-relative coordinates throughout) but rooted at the caller's
    /// window, so a mouse event delivered to a SECOND top-level (a dialog in
    /// its own winit window) resolves that window's own controls instead of
    /// the main window's. Returns `None` when `hwnd` is not a live guest
    /// window (a destroyed handle must not hit-test stale children).
    #[must_use]
    pub fn window_at_in(&self, hwnd: u64, x: i32, y: i32) -> Option<(u64, u32, u32)> {
        let state = self.lock_state()?;
        let windows = &state.try_window_state()?.windows;
        let root = wie_winapi::handles::Hwnd::from(hwnd);
        windows.iter().find(|w| w.handle == root)?;
        hit_test_subtree(windows, root, x, y)
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
    #[must_use]
    pub fn capture_target(&self, x: i32, y: i32) -> Option<(u64, u32, u32)> {
        let state = self.lock_state()?;
        let ws = state.try_window_state()?;
        let capture = ws.capture_window_handle;
        if capture == wie_winapi::handles::Hwnd::NULL {
            return None;
        }
        ws.windows.iter().find(|w| w.handle == capture)?;
        let (mut offset_x, mut offset_y) = (0_i32, 0_i32);
        let mut current = capture;
        while let Some(w) = ws.windows.iter().find(|w| w.handle == current) {
            if w.parent_handle == wie_winapi::handles::Hwnd::NULL {
                break;
            }
            offset_x = offset_x.saturating_add(w.x);
            offset_y = offset_y.saturating_add(w.y);
            current = w.parent_handle;
        }
        Some((
            capture.as_u64(),
            x.saturating_sub(offset_x).max(0) as u32,
            y.saturating_sub(offset_y).max(0) as u32,
        ))
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
    #[must_use]
    pub fn focused_top_level(&self) -> Option<u64> {
        let state = self.lock_state()?;
        let ws = state.try_window_state()?;
        let mut current = ws.focus_window_handle;
        if current == wie_winapi::handles::Hwnd::NULL {
            return None;
        }
        loop {
            let window = ws.windows.iter().find(|w| w.handle == current)?;
            if window.parent_handle == wie_winapi::handles::Hwnd::NULL {
                return Some(current.as_u64());
            }
            current = window.parent_handle;
        }
    }

    /// The window with keyboard focus (what `GetFocus` returns in-guest).
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
    #[must_use]
    pub fn mouse_tracking(&self, hwnd: u64) -> bool {
        let Some(state) = self.lock_state() else {
            return false;
        };
        state.try_window_state().is_some_and(|ws| {
            ws.windows
                .iter()
                .any(|w| w.handle == wie_winapi::handles::Hwnd::from(hwnd) && w.mouse_tracking)
        })
    }

    /// Update one virtual key's pressed state in the guest keyboard-state
    /// array (the 256-byte table `GetKeyState` / `GetAsyncKeyState` /
    /// `IsDialogMessage`'s Shift+Tab read).
    ///
    /// `pressed` sets or clears bit 0x80 (the "key is down" flag) for `vk`.
    pub fn set_key_state(&self, vk: u16, pressed: bool) {
        let Some(mut state) = self.lock_state() else {
            return;
        };
        let ws = state.window_state();
        if let Some(key) = ws.keyboard_state.get_mut(usize::from(vk)) {
            if pressed {
                *key |= 0x80;
            } else {
                *key &= !0x80;
            }
        }
    }

    /// Return info for the first guest window: (hwnd, title, width, height).
    #[must_use]
    pub fn first_guest_window_info(&self) -> Option<(u64, String, i32, i32)> {
        let state = self.lock_state()?;
        let w = state.try_window_state()?.windows.first()?;
        Some((w.handle.as_u64(), w.title.clone(), w.width, w.height))
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
    /// `WindowState.menu_dirty` is false AND the selected menu handle is
    /// unchanged the cache is returned without touching the menu records
    /// (the big-mutex lock is still taken, but the per-frame tree
    /// reconstruction is gone). A focus move to a different menu-bearing
    /// window changes the handle, so the cache rebuilds exactly then. Empty
    /// when no window has a menu.
    #[must_use]
    pub fn window_menu_items(&self) -> Arc<Vec<MenuNode>> {
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
#[allow(clippy::expect_used)]
mod tests {
    use super::GuestHandle;
    use crate::memory::DEFAULT_LAYOUT;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, RwLock};
    use wie_winapi::handles::Hwnd;
    use wie_winapi::user32::Dimension;
    use wie_winapi::user32::menu::{MenuEntry, MenuRecord};
    use wie_winapi::vfs::VolumeConfig;

    /// `window_menu_items` returns the cached tree while `menu_dirty` is
    /// false and rebuilds (seeing new items) once a mutation dirties it.
    #[test]
    fn window_menu_items_cache_invalidates_on_dirty() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "menu.exe".to_owned(),
            module_path: r"C:\App\menu.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "menu.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        let menu_handle = 0x0000_0000_6620_0000_u64;
        let submenu = 0x0000_0000_6620_0001_u64;
        {
            let ws = winapi_state.window_state();
            ws.menus.push(MenuRecord {
                handle: wie_winapi::handles::Hmenu::from(menu_handle),
                items: vec![MenuEntry::Popup {
                    text: "File".to_owned(),
                    submenu: wie_winapi::handles::Hmenu::from(submenu),
                }],
            });
            ws.menus.push(MenuRecord {
                handle: wie_winapi::handles::Hmenu::from(submenu),
                items: vec![
                    MenuEntry::Item {
                        id: 100,
                        text: "Exit".to_owned(),
                        enabled: true,
                        checked: false,
                    },
                    MenuEntry::Separator,
                    MenuEntry::Item {
                        id: 200,
                        text: "About".to_owned(),
                        enabled: true,
                        checked: false,
                    },
                ],
            });
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(0x100),
                menu_handle,
                ..Default::default()
            });
        }
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        let first = handle.window_menu_items();
        assert_eq!(first.len(), 1, "File popup at the top level");
        let file = first.first().expect("popup");
        // The RNotepad File shape: Exit / separator / About.
        assert_eq!(
            file.children.len(),
            3,
            "Exit / separator / About inside File"
        );
        assert!(
            file.children.get(1).expect("separator").separator,
            "the MF_SEPARATOR between Exit and About renders as a separator node"
        );
        // Same tree from the cache (dirty is false — no rebuild).
        assert_eq!(handle.window_menu_items(), first);

        // Mutate through the guest-facing state and dirty the tree.
        {
            let mut state = handle.state.lock().expect("lock state");
            let ws = state.window_state();
            let submenu_record = ws
                .menus
                .iter_mut()
                .find(|m| m.handle == wie_winapi::handles::Hmenu::from(submenu))
                .expect("submenu record");
            submenu_record.items.push(MenuEntry::Item {
                id: 300,
                text: "Open".to_owned(),
                enabled: true,
                checked: false,
            });
            ws.menu_dirty = true;
        }
        let second = handle.window_menu_items();
        assert_eq!(
            second.first().expect("popup").children.len(),
            4,
            "rebuild must reflect the appended item"
        );
    }

    /// The node-side half of the F4 menu-state round trip: a guest
    /// `EnableMenuItem` / `CheckMenuItem` mutation dirties the tree, and the
    /// rebuilt `MenuNode` carries the new enabled/checked flags — the state
    /// the host bar (`menu_item_states` in wie-cli) turns into muda
    /// `set_enabled` / `set_checked` calls.
    #[test]
    fn menu_node_state_reflects_guest_enable_check_mutations() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "state.exe".to_owned(),
            module_path: r"C:\App\state.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "state.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        let menu_handle = 0x0000_0000_6620_0000_u64;
        {
            let ws = winapi_state.window_state();
            ws.menus.push(MenuRecord {
                handle: wie_winapi::handles::Hmenu::from(menu_handle),
                items: vec![MenuEntry::Item {
                    id: 100,
                    text: "Paste".to_owned(),
                    enabled: true,
                    checked: false,
                }],
            });
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(0x100),
                menu_handle,
                ..Default::default()
            });
        }
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        let first = handle.window_menu_items();
        let paste = first.first().expect("item");
        assert!(paste.enabled, "fresh items start enabled");
        assert!(!paste.checked, "fresh items start unchecked");

        // EnableMenuItem(MF_GRAYED|MF_BYCOMMAND) — greys the item in the
        // native tree, exactly what the user32 handler's mutate_item does.
        {
            let mut state = handle.state.lock().expect("lock state");
            let ws = state.window_state();
            let record = ws
                .menus
                .iter_mut()
                .find(|m| m.handle == wie_winapi::handles::Hmenu::from(menu_handle))
                .expect("menu record");
            if let Some(MenuEntry::Item { enabled, .. }) = record.items.first_mut() {
                *enabled = false;
            }
            ws.menu_dirty = true;
        }
        let greyed = handle.window_menu_items();
        assert!(
            !greyed.first().expect("item").enabled,
            "rebuild must surface the greyed state"
        );

        // CheckMenuItem(MF_CHECKED|MF_BYCOMMAND) — checks the item.
        {
            let mut state = handle.state.lock().expect("lock state");
            let ws = state.window_state();
            let record = ws
                .menus
                .iter_mut()
                .find(|m| m.handle == wie_winapi::handles::Hmenu::from(menu_handle))
                .expect("menu record");
            if let Some(MenuEntry::Item {
                enabled, checked, ..
            }) = record.items.first_mut()
            {
                *enabled = false;
                *checked = true;
            }
            ws.menu_dirty = true;
        }
        let checked = handle.window_menu_items();
        let item = checked.first().expect("item");
        assert!(!item.enabled, "greyed state survives the check mutation");
        assert!(item.checked, "rebuild must surface the checked state");
    }

    /// The macOS dynamic-menu pattern: `window_menu_items` mirrors the
    /// FOCUSED guest window's menu. Focus can sit on a child (an EDIT inside
    /// the focused top-level), so the selection ascends to the top-level that
    /// carries the menu; a child's nonzero `menu_handle` slot (its child id)
    /// must never be mistaken for a menu. With no focus — or a focused window
    /// without a menu — the bar falls back to the first menu-bearing
    /// top-level (the pre-focus behavior).
    #[test]
    fn window_menu_items_prefers_the_focused_windows_menu() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "focus-menu.exe".to_owned(),
            module_path: r"C:\App\focus-menu.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "focus-menu.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        let menu_a = 0x0000_0000_6620_0001_u64;
        let menu_b = 0x0000_0000_6620_0002_u64;
        let a = 0x100_u64;
        let b = 0x200_u64;
        let b_edit = 0x201_u64;
        let c = 0x300_u64;
        {
            let ws = winapi_state.window_state();
            ws.menus.push(MenuRecord {
                handle: wie_winapi::handles::Hmenu::from(menu_a),
                items: vec![MenuEntry::Item {
                    id: 1,
                    text: "Exit".to_owned(),
                    enabled: true,
                    checked: false,
                }],
            });
            ws.menus.push(MenuRecord {
                handle: wie_winapi::handles::Hmenu::from(menu_b),
                items: vec![MenuEntry::Item {
                    id: 2,
                    text: "Paste".to_owned(),
                    enabled: true,
                    checked: false,
                }],
            });
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(a),
                menu_handle: menu_a,
                ..Default::default()
            });
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(b),
                menu_handle: menu_b,
                ..Default::default()
            });
            // B's EDIT: its menu_handle slot holds the CHILD ID (5), not a
            // menu — the selection must not mistake it for one.
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(b_edit),
                parent_handle: wie_winapi::handles::Hwnd::from(b),
                menu_handle: 5,
                ..Default::default()
            });
            // A top-level with no menu at all.
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(c),
                ..Default::default()
            });
        }
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        // Focus sits on B's child EDIT → the bar mirrors B's menu.
        {
            let mut state = handle.state.lock().expect("lock state");
            state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::from(b_edit);
        }
        assert_eq!(
            handle.window_menu_items().first().expect("item").title,
            "Paste",
            "a focused child must resolve to its top-level's menu"
        );

        // Focus on A → A's menu.
        {
            let mut state = handle.state.lock().expect("lock state");
            state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::from(a);
        }
        assert_eq!(
            handle.window_menu_items().first().expect("item").title,
            "Exit",
            "focus on A switches the bar to A's menu"
        );

        // Focus on C (no menu) → fall back to the first menu-bearing
        // top-level (A).
        {
            let mut state = handle.state.lock().expect("lock state");
            state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::from(c);
        }
        assert_eq!(
            handle.window_menu_items().first().expect("item").title,
            "Exit",
            "a focused window without a menu falls back to the first menu-bearing top-level"
        );

        // No focus at all → same fallback.
        {
            let mut state = handle.state.lock().expect("lock state");
            state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::NULL;
        }
        assert_eq!(
            handle.window_menu_items().first().expect("item").title,
            "Exit",
            "no focus → the first menu-bearing top-level (pre-focus behavior)"
        );
    }

    /// `take_host_geometry_request` reads the guest-set pending geometry —
    /// target hwnd + rect — and clears the slot (the SetWindowPlacement
    /// host-forwarding seam).
    #[test]
    fn take_host_geometry_request_reads_and_clears_the_pending_slot() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "placement.exe".to_owned(),
            module_path: r"C:\App\placement.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "placement.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        {
            let ws = winapi_state.window_state();
            ws.host_geometry_request = Some((20, 30, 200, 100));
            ws.host_geometry_hwnd = Some(0x200);
        }
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        assert_eq!(
            handle.take_host_geometry_request(),
            Some((0x200, 20, 30, 200, 100)),
            "the pending geometry must be handed to the host presenter, tagged \
             with the window it targets"
        );
        assert_eq!(
            handle.take_host_geometry_request(),
            None,
            "take clears the slot so a stale move is never re-applied"
        );
    }

    /// `window_at_in` roots the hit-test at the CALLER's top-level, so with
    /// two top-level windows each resolves its own controls: window B's child
    /// is found through window B, window A's child through window A, and the
    /// plain `window_at` keeps resolving the first top-level (the headless
    /// caller).
    #[test]
    fn window_at_in_hit_tests_the_named_top_level_subtree() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "two-win.exe".to_owned(),
            module_path: r"C:\App\two-win.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "two-win.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        let a = 0x100_u64;
        let a_button = 0x101_u64;
        let b = 0x200_u64;
        let b_edit = 0x201_u64;
        {
            let ws = winapi_state.window_state();
            // Top-level A (the main window) with a button at (10, 10, 120x40).
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(a),
                width: 360,
                height: 140,
                ..Default::default()
            });
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(a_button),
                parent_handle: wie_winapi::handles::Hwnd::from(a),
                x: 10,
                y: 10,
                width: 120,
                height: 40,
                visible: true,
                ..Default::default()
            });
            // Top-level B (a dialog) with an edit at (5, 5, 50x50).
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(b),
                width: 200,
                height: 200,
                ..Default::default()
            });
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(b_edit),
                parent_handle: wie_winapi::handles::Hwnd::from(b),
                x: 5,
                y: 5,
                width: 50,
                height: 50,
                visible: true,
                ..Default::default()
            });
        }
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        // A click at (20, 20) in window B's space hits B's edit, NOT A's
        // button — the pre-L2 window_at would have resolved A's button here.
        assert_eq!(
            handle.window_at_in(b, 20, 20),
            Some((b_edit, 15, 15)),
            "window_at_in(B) must descend B's subtree with child-relative coords"
        );
        // The same point through window A hits A's button.
        assert_eq!(
            handle.window_at_in(a, 20, 20),
            Some((a_button, 10, 10)),
            "window_at_in(A) must descend A's subtree"
        );
        // A point outside B's children resolves to B itself.
        assert_eq!(
            handle.window_at_in(b, 150, 150),
            Some((b, 150, 150)),
            "no child hit → the root window, client-relative"
        );
        // An unknown window yields nothing (a destroyed handle must not
        // hit-test stale children).
        assert_eq!(
            handle.window_at_in(0x999, 20, 20),
            None,
            "an unknown root is not a live guest window"
        );
        // The thin window_at wrapper still roots at the first top-level (A).
        assert_eq!(
            handle.window_at(20, 20),
            Some((a_button, 10, 10)),
            "window_at keeps resolving the first parentless record"
        );
    }

    /// `focus_window` surfaces the guest's `SetFocus` state — the keyboard
    /// routing target (`focus_window().unwrap_or(event window)`).
    #[test]
    fn focus_window_reflects_the_guest_focus_handle() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "focus.exe".to_owned(),
            module_path: r"C:\App\focus.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "focus.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        {
            let ws = winapi_state.window_state();
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(0x201),
                ..Default::default()
            });
            ws.focus_window_handle = wie_winapi::handles::Hwnd::from(0x201);
        }
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        assert_eq!(
            handle.focus_window(),
            Some(0x201),
            "the guest focus handle is the keyboard routing target"
        );
        {
            let mut state = handle.state.lock().expect("lock state");
            state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::NULL;
        }
        assert_eq!(
            handle.focus_window(),
            None,
            "no focus → the caller falls back to the event window"
        );
    }

    /// `focused_top_level` ascends a focused CHILD to its parentless
    /// top-level — the window a modal MessageBox should parent to (a dialog
    /// is a child of its owner here, so its focused controls resolve to the
    /// owner top-level). `None` when nothing is focused.
    #[test]
    fn focused_top_level_ascends_to_the_parentless_ancestor() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "focus-top.exe".to_owned(),
            module_path: r"C:\App\focus-top.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "focus-top.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        let main = 0x100_u64;
        let dialog = 0x200_u64;
        let dialog_button = 0x201_u64;
        {
            let ws = winapi_state.window_state();
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(main),
                ..Default::default()
            });
            // The modal dialog is a CHILD of the owner (it composites into
            // the owner's surface), and the button is a child of the dialog.
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(dialog),
                parent_handle: wie_winapi::handles::Hwnd::from(main),
                ..Default::default()
            });
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(dialog_button),
                parent_handle: wie_winapi::handles::Hwnd::from(dialog),
                ..Default::default()
            });
        }
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        // Focus on the dialog's button → the owner top-level (main).
        {
            let mut state = handle.state.lock().expect("lock state");
            state.window_state().focus_window_handle =
                wie_winapi::handles::Hwnd::from(dialog_button);
        }
        assert_eq!(
            handle.focused_top_level(),
            Some(main),
            "a focused dialog control ascends to the owner top-level"
        );

        // Focus on a top-level directly → itself.
        {
            let mut state = handle.state.lock().expect("lock state");
            state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::from(main);
        }
        assert_eq!(handle.focused_top_level(), Some(main));

        // No focus → None (the caller falls back to the primary window).
        {
            let mut state = handle.state.lock().expect("lock state");
            state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::NULL;
        }
        assert_eq!(
            handle.focused_top_level(),
            None,
            "no focus yields None so the bridge falls back to the primary window"
        );
    }

    /// `resize_window` must put the RESIZED window itself into the
    /// erase/paint cycle — the exact bug pattern behind the black-background
    /// regression (gui_edit, notepad): the guest DIB is reallocated ZEROED on
    /// resize, so any unpainted area uploads as black unless the class-brush
    /// erase fills it first. The erase runs only when `ERASE_BACKGROUND` is
    /// set; the SetWindowPlacement show path sets it (cb38299) but the resize
    /// path only marked the subtree `invalidated`. Descendants keep
    /// `invalidated` WITHOUT erase — the EDIT paints its own white background.
    #[test]
    fn resize_window_erases_background_of_the_resized_window() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "resize.exe".to_owned(),
            module_path: r"C:\App\resize.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "resize.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        let top = 0x100_u64;
        let child = 0x101_u64;
        {
            let ws = winapi_state.window_state();
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(top),
                width: 640,
                height: 420,
                ..Default::default()
            });
            ws.windows.push(wie_winapi::WindowRecord {
                handle: wie_winapi::handles::Hwnd::from(child),
                parent_handle: wie_winapi::handles::Hwnd::from(top),
                ..Default::default()
            });
        }
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        handle.resize_window(Hwnd::from(top), Dimension::new(800, 600));

        let mut state = handle.state.lock().expect("lock state");
        let ws = state.window_state();
        let top_record = ws
            .windows
            .iter()
            .find(|w| w.handle == wie_winapi::handles::Hwnd::from(top))
            .expect("top window record exists");
        assert_eq!(
            (top_record.width, top_record.height),
            (800, 600),
            "the resized window record must carry the new size"
        );
        assert!(
            top_record.invalidated,
            "the resized window itself must enter the repaint cycle"
        );
        assert!(
            top_record
                .flags
                .contains(wie_winapi::WindowFlags::ERASE_BACKGROUND),
            "the resized window must request a class-brush erase (the DIB was \
             reallocated zeroed — without the erase the background stays black)"
        );
        let child_record = ws
            .windows
            .iter()
            .find(|w| w.handle == wie_winapi::handles::Hwnd::from(child))
            .expect("child window record exists");
        assert!(
            child_record.invalidated,
            "a descendant must repaint itself after the ancestor surface grew"
        );
        assert!(
            !child_record
                .flags
                .contains(wie_winapi::WindowFlags::ERASE_BACKGROUND),
            "descendants paint their own background — no erase flag on the child"
        );
    }

    /// `set_drop_files` maps host drop paths into guest `C:\…` / `D:\…` paths
    /// through the session's volume config, skips unmapped files, stores the
    /// point, and returns the fake HDROP for the WM_DROPFILES wParam.
    #[test]
    fn set_drop_files_maps_host_paths_to_guest_paths() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "drop.exe".to_owned(),
            module_path: r"C:\App\drop.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "drop.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        winapi_state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: Some(PathBuf::from("/Users/me/data")),
        };
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        let hdrop = handle.set_drop_files(
            vec![
                PathBuf::from("/tmp/bottle/drive_c/App/out.txt"),
                PathBuf::from("/Users/me/data/archive/a.7z"),
                // Outside both volumes — the guest filesystem cannot see it.
                PathBuf::from("/etc/passwd"),
            ],
            (5, 6),
        );
        assert_eq!(
            hdrop,
            wie_winapi::user32::dragdrop::FAKE_HDROP,
            "set_drop_files returns the fake HDROP"
        );

        let mut state = handle.state.lock().expect("lock state");
        let drop = state.drag_drop();
        assert_eq!(
            drop.files(),
            &[r"C:\App\out.txt".to_owned(), r"D:\archive\a.7z".to_owned()],
            "unmapped host paths are skipped"
        );
        assert_eq!(drop.point(), (5, 6));
    }

    /// A drop in which NO host path maps into a guest volume must be skipped
    /// entirely: return 0 (the caller posts no WM_DROPFILES) rather than a
    /// fake HDROP over an empty list — the guest would otherwise open an
    /// empty path (notepad: CreateFileW("") → ERROR_PATH_NOT_FOUND → error
    /// dialog).
    #[test]
    fn set_drop_files_returns_zero_when_nothing_maps() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "drop.exe".to_owned(),
            module_path: r"C:\App\drop.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "drop.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        winapi_state.file_io.volumes = VolumeConfig {
            bottle_root: Some(PathBuf::from("/tmp/bottle")),
            drive_d_root: Some(PathBuf::from("/Users/me/data")),
        };
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        let hdrop = handle.set_drop_files(
            vec![
                // Outside both volumes — the guest filesystem cannot see it.
                PathBuf::from("/etc/passwd"),
                PathBuf::from("/tmp/elsewhere/file.txt"),
            ],
            (5, 6),
        );
        assert_eq!(hdrop, 0, "no mappable path → the drop is skipped");

        let mut state = handle.state.lock().expect("lock state");
        assert!(
            state.drag_drop().files().is_empty(),
            "the drop list must stay empty"
        );
    }

    /// `z_snapshot` reads the revision AND the ordered list under ONE lock:
    /// the Frame handler's reorder always applies the list the revision it
    /// compared actually describes. Separate locked reads could observe the
    /// list mid-mutation.
    #[test]
    fn z_snapshot_reads_rev_and_order_under_one_lock() {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "z.exe".to_owned(),
            module_path: r"C:\App\z.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "z.exe".to_owned(),
        };
        let mut winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        {
            let present = winapi_state.present();
            present.register_top_level(wie_winapi::handles::Hwnd::from(0x100));
            present.register_top_level(wie_winapi::handles::Hwnd::from(0x200));
            present.register_top_level(wie_winapi::handles::Hwnd::from(0x300));
            // HWND_TOP: 0x100 to the front → back-to-front [0x200, 0x300, 0x100].
            present.z_order_to_top(wie_winapi::handles::Hwnd::from(0x100));
        }
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
        };

        let (rev, order) = handle.z_snapshot();
        assert_eq!(rev, handle.z_rev(), "snapshot rev == the z_rev accessor");
        assert_eq!(order, vec![0x200, 0x300, 0x100], "back-to-front order");

        // A guest z-change bumps the revision; the NEXT snapshot reflects the
        // new order atomically (HWND_BOTTOM: 0x300 to the back).
        {
            let mut state = handle.state.lock().expect("lock state");
            state
                .present()
                .z_order_to_bottom(wie_winapi::handles::Hwnd::from(0x300));
        }
        let (rev2, order2) = handle.z_snapshot();
        assert!(rev2 > rev, "the z-change bumps the revision");
        assert_eq!(order2, vec![0x300, 0x200, 0x100], "reordered snapshot");
        assert_eq!(rev2, handle.z_rev(), "still consistent after the change");
    }

    // ── Task 7: presenter-side lock-wait timing ────────────────────────

    /// A presenter-side `GuestHandle` + shared stats fixture.
    fn handle_with_stats() -> (GuestHandle, Arc<crate::mt_runtime::LockWaitStats>) {
        let process = wie_pe::ProcessIdentity {
            module_file_name: "wait.exe".to_owned(),
            module_path: r"C:\App\wait.exe".to_owned(),
            current_directory: r"C:\App".to_owned(),
            command_line: "wait.exe".to_owned(),
        };
        let winapi_state =
            crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
                .expect("winapi state");
        let stats = Arc::new(crate::mt_runtime::LockWaitStats::new());
        let __state_arc = Arc::new(Mutex::new(winapi_state));
        let __present_channel = {
            let mut __guard = __state_arc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            __guard.present().channel_arc()
        };
        let handle = GuestHandle {
            state: Arc::clone(&__state_arc),
            present_channel: Arc::clone(&__present_channel),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
            lock_wait_stats: Arc::clone(&stats),
        };
        (handle, stats)
    }

    /// `take_frame` (the presenter's per-frame read path) reads the present
    /// channel WITHOUT the big WinApiState lock: even while the guest holds
    /// `shared_winapi` for 10 ms, the presenter's take returns immediately
    /// and never lands in the presenter-side lock stats (the Wave 1a
    /// contract — rendering no longer serializes behind guest WinAPI work).
    #[test]
    fn take_frame_times_presenter_wait_when_enabled() {
        let (handle, stats) = handle_with_stats();
        stats.set_enabled(true);
        let state = Arc::clone(&handle.state);
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = state.lock().ok();
            let _ = tx.send(());
            std::thread::sleep(std::time::Duration::from_millis(10));
        });
        let _ = rx.recv();
        let t0 = std::time::Instant::now();
        let frame = handle.take_frame(0x100);
        let elapsed = t0.elapsed();
        assert!(frame.is_none(), "no published frame yet");
        assert!(
            elapsed < std::time::Duration::from_millis(10),
            "take_frame must not wait out the guest-held big lock (took {elapsed:?})"
        );
        let _ = holder.join();
        let snap = stats.snapshot();
        assert_eq!(
            snap.presenter_total_ns, 0,
            "the channel read is lock-free — no presenter-side wait is recorded"
        );
        assert_eq!(snap.presenter_max_ns, 0);
        assert_eq!(
            snap.guest_total_ns, 0,
            "presenter reads never mix with guest"
        );
        assert_eq!(snap.guest_max_ns, 0);
    }

    /// Disabled mode: presenter-side reads take the lock without recording —
    /// the shared stats stay zero, so the per-frame path pays only the
    /// atomic gate load.
    #[test]
    fn take_frame_disabled_records_nothing() {
        let (handle, stats) = handle_with_stats();
        assert!(!stats.enabled(), "stats start disabled");
        let _ = handle.take_frame(0x100);
        let _ = handle.z_snapshot();
        let snap = stats.snapshot();
        assert_eq!(snap.presenter_total_ns, 0);
        assert_eq!(snap.presenter_max_ns, 0);
        assert_eq!(snap.guest_total_ns, 0);
        assert_eq!(snap.guest_max_ns, 0);
    }
}
