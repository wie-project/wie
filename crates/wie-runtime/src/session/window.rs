use super::menu::{MenuNode, MenuTreeCache, build_menu_tree};
use wie_winapi::handles::Hmenu;

/// Append one journal line when `WIE_API_JOURNAL` is set (backend A/B diffs).
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
    /// Cached menu-bar tree, keyed by the menu handle it was built for.
    ///
    /// `window_menu_items` rebuilds only when `WindowState.menu_dirty` flips
    /// or the first menu-bearing window's handle changes, so the host frame
    /// loop stops reconstructing the tree (and locking the big mutex) on
    /// every frame.
    pub(super) menu_tree_cache: MenuTreeCache,
}

impl GuestHandle {
    /// Take the latest published frame for `hwnd`, if any.
    #[must_use]
    pub fn take_frame(&self, hwnd: u64) -> Option<wie_winapi::present::SurfaceFrame> {
        let state = self.state.lock().ok()?;
        state
            .try_present()?
            .published
            .get(&wie_winapi::handles::Hwnd::from(hwnd))
            .cloned()
    }

    /// B9: whether frame timing instrumentation is active (lock-free gate).
    #[must_use]
    pub fn frame_timing_enabled(&self) -> bool {
        wie_winapi::present::frame_timing_enabled()
    }

    /// B2: the current present generation (bumped by every publish). Used by
    /// the host to skip redundant presents of an unchanged frame.
    #[must_use]
    pub fn present_generation(&self) -> u64 {
        let Ok(state) = self.state.lock() else {
            return 0;
        };
        state.try_present().map_or(0, |p| p.generation)
    }

    /// B9: record one host present (softbuffer copy + upload) wall time (ns).
    /// The internal gate makes this a no-op (no lock) when timing is disabled.
    pub fn record_present_time(&self, ns: u128) {
        if !wie_winapi::present::frame_timing_enabled() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.present().record_present(ns);
        }
    }

    /// B9: publish duration of the most recent frame (ns; 0 when timing disabled).
    #[must_use]
    pub fn present_publish_ns_last(&self) -> u128 {
        let Ok(state) = self.state.lock() else {
            return 0;
        };
        state.try_present().map_or(0, |p| p.publish_ns_last)
    }

    /// Return the first (and typically only) guest-created window handle.
    #[must_use]
    pub fn first_guest_window_handle(&self) -> Option<u64> {
        let state = self.state.lock().ok()?;
        state
            .try_window_state()?
            .windows
            .first()
            .map(|w| w.handle.as_u64())
    }

    /// Hit-test a point in the top-level window's client area.
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
    #[must_use]
    pub fn window_at(&self, x: i32, y: i32) -> Option<(u64, u32, u32)> {
        let state = self.state.lock().ok()?;
        let windows = &state.try_window_state()?.windows;
        let top = windows
            .iter()
            .find(|w| w.parent_handle == wie_winapi::handles::Hwnd::NULL)?
            .handle;
        // Descend z-order: at each level pick the topmost visible child that
        // contains the point, then recurse into it. Coordinates stay
        // child-relative at every step.
        let mut current = top;
        let mut rel_x = x;
        let mut rel_y = y;
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
        let state = self.state.lock().ok()?;
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

    /// The window currently holding the mouse capture, if any.
    #[must_use]
    pub fn capture_window(&self) -> Option<u64> {
        let state = self.state.lock().ok()?;
        let capture = state.try_window_state()?.capture_window_handle;
        (capture != wie_winapi::handles::Hwnd::NULL).then_some(capture.as_u64())
    }

    /// The window with keyboard focus (what `GetFocus` returns in-guest).
    #[must_use]
    pub fn focus_window(&self) -> Option<u64> {
        let state = self.state.lock().ok()?;
        let focus = state.try_window_state()?.focus_window_handle;
        (focus != wie_winapi::handles::Hwnd::NULL).then_some(focus.as_u64())
    }

    /// Whether `TrackMouseEvent` armed hover/leave tracking for `hwnd`.
    ///
    /// The host forwards `WM_MOUSEHOVER` / `WM_MOUSELEAVE` only for tracked
    /// windows — without a `TrackMouseEvent` call Windows sends neither.
    #[must_use]
    pub fn mouse_tracking(&self, hwnd: u64) -> bool {
        let Ok(state) = self.state.lock() else {
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
        let Ok(mut state) = self.state.lock() else {
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
        let state = self.state.lock().ok()?;
        let w = state.try_window_state()?.windows.first()?;
        Some((w.handle.as_u64(), w.title.clone(), w.width, w.height))
    }

    /// Snapshot of the first menu-bearing window's menu as a tree, for the
    /// host menu bar.
    ///
    /// Walks the native `MenuRecord` tree once and caches the result: while
    /// `WindowState.menu_dirty` is false the cache is returned without
    /// touching the menu records (the big-mutex lock is still taken, but the
    /// per-frame tree reconstruction is gone). Empty when no window has a
    /// menu yet.
    #[must_use]
    pub fn window_menu_items(&self) -> Vec<MenuNode> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        let Some(ws) = state.try_window_state() else {
            return Vec::new();
        };
        let Some(menu_handle) = ws
            .windows
            .iter()
            .find_map(|w| (w.menu_handle != 0).then_some(w.menu_handle))
        else {
            return Vec::new();
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
                return tree.clone();
            }
        }
        let tree = build_menu_tree(&ws.menus, menu_handle.as_u64());
        if let Ok(mut cache) = self.menu_tree_cache.write() {
            *cache = Some((menu_handle, tree.clone()));
        }
        tree
    }

    /// Set the wake callback — called when a new frame is published.
    pub fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
        if let Ok(mut state) = self.state.lock() {
            state.present().wake = Some(cb);
        }
    }

    /// Update the guest-visible window size (host window was resized).
    ///
    /// Updates the `WindowRecord` only — three `i32` writes, no allocation.
    /// Uses `try_lock` so the per-`Resized`-event hot path NEVER blocks on the
    /// guest thread mid-API-call: if the record can't be locked right now,
    /// the settled `WM_SIZE` carries the final size anyway (and the guest's
    /// own `recreate_dib` reads `lParam`, not the record).
    pub fn resize_window(&self, hwnd: u64, width: u32, height: u32) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        let ws = state.window_state();
        let hwnd = wie_winapi::handles::Hwnd::from(hwnd);
        if let Some(window) = ws.windows.iter_mut().find(|w| w.handle == hwnd) {
            window.width = i32::try_from(width).unwrap_or(0);
            window.height = i32::try_from(height).unwrap_or(0);
            window.client_rect = (0, 0, window.width, window.height);
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

    /// Post a message to the guest message queue.
    ///
    /// Locks ONLY the dedicated queue mutex and notifies the condvar, so a
    /// waiting guest loop wakes immediately (no 50 ms poll tick) and the host
    /// never blocks on guest API execution.
    pub fn post_message(&self, hwnd: u64, msg: u32, wparam: u64, lparam: u64) {
        self.post_message_at(hwnd, msg, wparam, lparam, 0, 0);
    }

    /// Post a message with a cursor position (fills `MSG.pt`).
    ///
    /// Mouse messages carry the cursor position at post time, matching the
    /// `MSG` layout Windows fills; keyboard/window messages use `(0, 0)`.
    pub fn post_message_at(
        &self,
        hwnd: u64,
        msg: u32,
        wparam: u64,
        lparam: u64,
        point_x: i32,
        point_y: i32,
    ) {
        if let Ok(mut queue) = self.queue.lock() {
            let time = queue.next_message_time;
            queue.next_message_time = time.wrapping_add(1);
            queue.messages.push(wie_winapi::QueuedWindowMessage {
                window_handle: wie_winapi::handles::Hwnd::from(hwnd),
                message: msg,
                word_parameter: wparam,
                long_parameter: lparam,
                time,
                point_x,
                point_y,
            });
            // Wake a guest blocked in run_windowed's condvar wait.
            {
                let mut triggered = queue
                    .signal
                    .triggered
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *triggered = true;
            }
            queue.signal.cvar.notify_one();
        }
    }

    /// Return the message signal, if the queue slot exists.
    ///
    /// The GUI loop waits on this condvar so posted messages (keyboard,
    /// mouse, close, WM_SIZE) wake the guest immediately instead of on a
    /// fixed poll interval.
    #[must_use]
    pub fn message_signal(&self) -> Option<std::sync::Arc<wie_winapi::present::MessageSignal>> {
        self.queue.lock().ok().map(|q| q.signal.clone())
    }
}

#[cfg(test)]
#[expect(clippy::expect_used)]
mod tests {
    use super::GuestHandle;
    use crate::memory::DEFAULT_LAYOUT;
    use std::sync::{Arc, Mutex, RwLock};
    use wie_winapi::user32::menu::{MenuEntry, MenuRecord};

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
                items: vec![MenuEntry::Item {
                    id: 100,
                    text: "Exit".to_owned(),
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
        let handle = GuestHandle {
            state: Arc::new(Mutex::new(winapi_state)),
            queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
            menu_tree_cache: Arc::new(RwLock::new(None)),
        };

        let first = handle.window_menu_items();
        assert_eq!(first.len(), 1, "File popup at the top level");
        assert_eq!(
            first.first().expect("popup").children.len(),
            1,
            "Exit inside File"
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
                id: 200,
                text: "About".to_owned(),
                enabled: true,
                checked: false,
            });
            ws.menu_dirty = true;
        }
        let second = handle.window_menu_items();
        assert_eq!(
            second.first().expect("popup").children.len(),
            2,
            "rebuild must reflect the appended item"
        );
    }
}
