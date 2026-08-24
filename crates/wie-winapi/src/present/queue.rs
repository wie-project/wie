//! The guest message queue: a FIFO of posted messages behind its own mutex,
//! kept separate from `WinApiState` so the host (winit thread) can post
//! input without locking the big state mutex (split from `mod.rs`).

use anyhow::Context;
use std::sync::{Arc, Condvar, Mutex};

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
    /// Wake hub for parked guest threads (Painpoint 1). Wired at session
    /// init to the SAME hub as [`crate::sync_obj::SyncState::wake_hub`]; a
    /// default (unwired) queue broadcasts into an empty hub — a no-op.
    pub wake: crate::wake::WakeHub,
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
            wake: crate::wake::WakeHub::default(),
            dialog_depth: 0,
        }
    }
}

impl MessageQueue {
    /// Push one message with a fresh timestamp and a zero cursor point.
    ///
    /// Bumps `next_message_time` (overflow is an error) and appends the
    /// `PostMessage`-style payload: word/long parameters as given, point
    /// `(0, 0)`. The single overflow message covers every posting site.
    pub fn push(
        &mut self,
        window_handle: crate::handles::Hwnd,
        message: u32,
        word_parameter: u64,
        long_parameter: u64,
    ) -> anyhow::Result<()> {
        let time = self.next_message_time;
        self.next_message_time = self
            .next_message_time
            .checked_add(1)
            .context("message timestamp overflow")?;
        self.messages.push(crate::QueuedWindowMessage {
            window_handle,
            message,
            word_parameter,
            long_parameter,
            time,
            point_x: 0,
            point_y: 0,
        });
        // A queued message may unblock a parked GetMessage — send one token
        // per push. Tokens are hints: a woken pump re-checks the queue and
        // re-parks when nothing matches its filter.
        self.wake.broadcast(crate::wake::Wake::MessagePosted);
        Ok(())
    }
}
