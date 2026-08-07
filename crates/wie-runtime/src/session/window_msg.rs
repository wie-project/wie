//! Host → guest message posting on [`GuestHandle`]: the queue writes and the
//! wake signal, moved out of `window.rs` (file-size cap).
//!
//! Posting locks ONLY the dedicated message-queue mutex — never the big
//! `WinApiState` mutex the guest thread holds during API-handler execution —
//! so input events never block on guest work. The guest loop parks on the
//! queue's condvar (`message_signal`), so a post wakes it immediately.

use super::window::GuestHandle;

impl GuestHandle {
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
