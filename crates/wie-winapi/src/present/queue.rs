//! The guest message queues: one FIFO of posted messages PER GUEST THREAD
//! behind a single mutex, kept separate from `WinApiState` so the host (winit
//! thread) can post input without locking the big state mutex (split from
//! `mod.rs`).

use ahash::HashMap;
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

/// The per-thread sub-queues of the guest message pump.
///
/// Windows gives every thread its own message queue; a `PostMessage` lands in
/// the queue of the thread that owns the window and a `PostThreadMessage`
/// lands in the named thread's queue, and a `GetMessage` only ever drains the
/// calling thread's queue. This is the inner container of [`MessageQueue`],
/// which owns the mutex.
#[derive(Debug, Default)]
pub struct MessageQueues {
    /// Queued messages per guest TID, FIFO within each sub-queue.
    by_tid: HashMap<u32, Vec<crate::QueuedWindowMessage>>,
    /// Routing mirror of `WindowRecord::owner_tid`, keyed by raw HWND.
    ///
    /// The host (winit thread) posts input while holding ONLY the message
    /// queue mutex — never the big `WinApiState` mutex the guest thread holds
    /// during API-handler execution — so it cannot read the window records to
    /// resolve the owning thread. `create_window_record` mirrors the owner tid
    /// here at window creation, keeping the host's "locks only the queue"
    /// discipline intact.
    ///
    /// Append-only on purpose: window records are dropped on `DestroyWindow`
    /// and dialog teardown, and a stale entry is harmless because handles come
    /// from a monotonic counter (`WindowState::next_window_handle`) and are
    /// never re-used. Removing entries would mean mirroring two removal paths
    /// for zero behavioural gain.
    window_owner_tids: HashMap<u64, u32>,
}

impl MessageQueues {
    /// The sub-queue of `tid`, or an empty slice when the thread has never
    /// received a message.
    #[must_use]
    pub fn messages(&self, tid: u32) -> &[crate::QueuedWindowMessage] {
        self.by_tid.get(&tid).map_or(&[], Vec::as_slice)
    }

    /// Mutable sub-queue of `tid`, creating it (with the burst capacity the
    /// single-queue model used to reserve up front) on first use.
    pub fn queue_for(&mut self, tid: u32) -> &mut Vec<crate::QueuedWindowMessage> {
        self.by_tid.entry(tid).or_insert_with(|| {
            // Reserve the common burst up-front so PostMessage/SendMessage
            // pushes do not reallocate from an empty Vec on every burst.
            Vec::with_capacity(64)
        })
    }

    /// Pop the most recently pushed message of `tid` (`None` when empty).
    pub fn pop_back(&mut self, tid: u32) -> Option<crate::QueuedWindowMessage> {
        self.by_tid.get_mut(&tid).and_then(Vec::pop)
    }

    /// Whether ANY sub-queue holds a message — process-wide emptiness.
    ///
    /// The pump itself never asks this (it scans one thread's sub-queue); it
    /// is for "did anything at all get posted" checks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_tid.values().all(Vec::is_empty)
    }

    /// Record that `hwnd` belongs to guest thread `owner_tid` (see the
    /// `window_owner_tids` field docs).
    pub fn register_window_owner(&mut self, hwnd: u64, owner_tid: u32) {
        self.window_owner_tids.insert(hwnd, owner_tid);
    }

    /// The mirrored owning thread of `hwnd`, or `None` when the window was
    /// never mirrored (or has been destroyed).
    #[must_use]
    pub fn window_owner_tid(&self, hwnd: u64) -> Option<u32> {
        self.window_owner_tids.get(&hwnd).copied()
    }

    /// The thread a message for `window_handle` belongs to.
    ///
    /// `fallback` is the calling guest thread — used for thread messages
    /// (`NULL` HWND) and for windows with no recorded owner.
    #[must_use]
    pub fn resolve_tid(&self, window_handle: crate::handles::Hwnd, fallback: u32) -> u32 {
        self.window_owner_tid(window_handle.as_u64())
            .unwrap_or(fallback)
    }

    /// Push one fully-formed message onto `tid`'s sub-queue.
    ///
    /// The caller owns the full record shape (timestamps, cursor point). NO
    /// wake token is broadcast here — [`MessageQueue::push_to`] and
    /// [`MessageQueue::push_host`] do that, and a direct caller (e.g.
    /// `RuntimeSession::post_message`) must broadcast explicitly.
    pub fn push_message(&mut self, tid: u32, message: crate::QueuedWindowMessage) {
        self.queue_for(tid).push(message);
    }
}

/// Guest message queues (one per guest thread), behind their own mutex.
///
/// Kept separate from `WinApiState` so the host (winit thread) can post
/// input messages without ever locking the big `WinApiState` mutex that the
/// guest thread holds during API-handler execution.  Input events therefore
/// never block on guest work.
#[derive(Debug)]
pub struct MessageQueue {
    /// Per-thread sub-queues (the pump's actual storage).
    pub queues: MessageQueues,
    /// Deterministic fake message timestamp source.
    ///
    /// Deliberately PROCESS-GLOBAL, not per thread: the values are only ever
    /// compared for relative order (`GetMessageTime`), and a single counter
    /// keeps cross-thread posted messages totally ordered the way Windows'
    /// `GetMessage` "0x0000FFFF since boot" stamps are.
    pub next_message_time: u32,
    /// TID a HOST-side post falls back to when its window has no recorded
    /// owner (host posts know no "current thread"; the primary thread owns
    /// the windowing loop, so its queue is the correct catch-all).
    pub primary_tid: u32,
    /// Cross-thread signal: a message was posted.
    pub signal: Arc<MessageSignal>,
    /// Wake hub for parked guest threads (Painpoint 1). Wired at session
    /// init to the SAME hub as [`crate::sync_obj::SyncState::wake_hub`]; a
    /// default (unwired) queue broadcasts into an empty hub — a no-op.
    pub wake: crate::wake::WakeHub,
    /// Number of modal dialogs currently open in the process.
    ///
    /// Deliberately PROCESS-GLOBAL, not per thread: a modal dialog blocks the
    /// whole process's input loop from reaching its owner, and every
    /// WIE-supported guest is single-UI-thread (see `docs/architecture/`).
    /// Making it per-thread would let a second thread's empty `GetMessage`
    /// synthesize the regression-mode `WM_QUIT` while a dialog is up.
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
            queues: MessageQueues::default(),
            next_message_time: 0,
            primary_tid: crate::PRIMARY_THREAD_ID,
            signal: Arc::new(MessageSignal::new()),
            wake: crate::wake::WakeHub::default(),
            dialog_depth: 0,
        }
    }
}

impl MessageQueue {
    /// Push one message into `tid`'s sub-queue with a fresh timestamp and a
    /// zero cursor point.
    ///
    /// Bumps `next_message_time` (overflow is an error) and appends the
    /// `PostMessage`-style payload: word/long parameters as given, point
    /// `(0, 0)`. The single overflow message covers every posting site.
    ///
    /// `tid` names the destination thread explicitly — `PostThreadMessageW`
    /// and the host post path use this directly.
    pub fn push_to(
        &mut self,
        tid: u32,
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
        self.queues.push_message(
            tid,
            crate::QueuedWindowMessage {
                window_handle,
                message,
                word_parameter,
                long_parameter,
                time,
                point_x: 0,
                point_y: 0,
            },
        );
        // A queued message may unblock a parked GetMessage — send one token
        // per push. Tokens are hints: a woken pump re-checks ITS OWN queue and
        // re-parks when nothing matches its filter.
        //
        // `broadcast`, not `send_to`: the hub cannot be told which inbox to
        // wake without threading the tid through, and over-waking is safe
        // (a spuriously woken pump re-parks) while under-waking hangs.
        self.wake.broadcast(crate::wake::Wake::MessagePosted);
        Ok(())
    }

    /// Host-side (winit thread) post with a cursor position.
    ///
    /// Routes to the queue of the thread that owns `window_handle`, falling
    /// back to [`Self::primary_tid`] — the host knows no "current thread".
    /// Locks only this queue's mutex: the whole point of the split from
    /// `WinApiState`.
    ///
    /// Note the guest-side counterpart is
    /// `WinApiState::post_message`, which resolves the owner from the window
    /// RECORDS (authoritative) rather than from this routing mirror. There is
    /// deliberately no third `push(current_tid, …)` on the queue: a guest-side
    /// caller that reached for the mirror would route from a copy, not from
    /// the truth.
    pub fn push_host(
        &mut self,
        window_handle: crate::handles::Hwnd,
        message: u32,
        word_parameter: u64,
        long_parameter: u64,
        point_x: i32,
        point_y: i32,
    ) {
        let tid = self.queues.resolve_tid(window_handle, self.primary_tid);
        let time = self.next_message_time;
        self.next_message_time = time.wrapping_add(1);
        self.queues.push_message(
            tid,
            crate::QueuedWindowMessage {
                window_handle,
                message,
                word_parameter,
                long_parameter,
                time,
                point_x,
                point_y,
            },
        );
        self.wake.broadcast(crate::wake::Wake::MessagePosted);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::MessageQueue;
    use crate::handles::Hwnd;

    const WORKER_TID: u32 = crate::FIRST_WORKER_TID;

    /// A host post to a window owned by another thread lands in THAT thread's
    /// sub-queue and still broadcasts the wake token — the host holds only the
    /// queue mutex, so routing must come from the mirrored owner index.
    #[test]
    fn host_post_routes_to_the_windows_owning_thread_and_wakes_it() {
        let mut queue = MessageQueue::default();
        let hwnd = Hwnd::from(0x6610_00B1);
        queue
            .queues
            .register_window_owner(hwnd.as_u64(), WORKER_TID);
        let worker_inbox = queue.wake.inbox_for(WORKER_TID);
        let primary_inbox = queue.wake.inbox_for(crate::PRIMARY_THREAD_ID);
        worker_inbox.drain();
        primary_inbox.drain();

        queue.push_host(hwnd, 0x0100, 0xAB, 0, 7, 9);

        let queued = queue.queues.messages(WORKER_TID);
        assert_eq!(queued.len(), 1, "landed in the owner's queue");
        assert_eq!(queued.first().map(|m| m.message), Some(0x0100));
        assert_eq!(queued.first().map(|m| m.point_x), Some(7));
        assert!(
            queue.queues.messages(crate::PRIMARY_THREAD_ID).is_empty(),
            "the primary queue is untouched"
        );
        // Deliberate scope decision: the push BROADCASTS (over-waking is safe,
        // under-waking hangs), so BOTH inboxes see the token; only the
        // routing is per-thread.
        assert_eq!(primary_inbox.drain(), 1, "broadcast wakes every inbox");
        assert_eq!(worker_inbox.drain(), 1, "the owner is woken too");
    }

    /// A host post for a window with no mirrored owner falls back to the
    /// primary thread's queue, so it can never become invisible (a dropped
    /// post would hang the pump that should have seen it).
    #[test]
    fn host_post_to_an_unknown_window_falls_back_to_the_primary_queue() {
        let mut queue = MessageQueue::default();
        assert_eq!(queue.primary_tid, crate::PRIMARY_THREAD_ID);
        queue.push_host(Hwnd::from(0xDEAD), 0x000F, 0, 0, 0, 0);
        assert_eq!(queue.queues.messages(queue.primary_tid).len(), 1);
        assert!(queue.queues.messages(WORKER_TID).is_empty());
    }

    /// `push_to` targets a queue by tid with no owner lookup at all, and each
    /// push stamps a strictly increasing (process-global) timestamp.
    #[test]
    fn push_to_targets_the_named_thread_with_monotonic_timestamps() {
        let mut queue = MessageQueue::default();
        queue
            .push_to(WORKER_TID, Hwnd::NULL, 0x0111, 0, 0)
            .expect("push");
        queue
            .push_to(crate::PRIMARY_THREAD_ID, Hwnd::NULL, 0x0111, 0, 0)
            .expect("push");
        let times: Vec<u32> = [WORKER_TID, crate::PRIMARY_THREAD_ID]
            .iter()
            .flat_map(|tid| {
                queue
                    .queues
                    .messages(*tid)
                    .iter()
                    .map(|m| m.time)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(times, vec![0, 1], "one global clock, per-thread order kept");
    }

    /// A wake token is delivered to a thread parked on ITS OWN inbox, which is
    /// the whole point of the per-thread queue split.
    #[test]
    fn foreign_tid_post_reaches_the_target_threads_parked_inbox() {
        let mut queue = MessageQueue::default();
        let worker_inbox = queue.wake.inbox_for(WORKER_TID);
        worker_inbox.drain();
        queue
            .push_to(WORKER_TID, Hwnd::NULL, 0x8000, 0xDEAD, 0xBEEF)
            .expect("post");
        assert_eq!(worker_inbox.drain(), 1);
        assert_eq!(queue.queues.messages(WORKER_TID).len(), 1);
    }
}
