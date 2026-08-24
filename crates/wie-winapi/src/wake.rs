//! Per-thread wake channels (Painpoint 1 — event-driven idle parking).
//!
//! Every parked host thread owns one [`ThreadInbox`]: a `std::sync::mpsc`
//! channel carrying [`Wake`] tokens. Producers (`PostMessage`, `SetTimer`,
//! kernel-object signals, teardown) send tokens into the inbox of a thread
//! that might be parked; the parked thread blocks on `recv_timeout` instead of
//! sleeping through poll quanta.
//!
//! **Tokens are hints, never data.** Dropping, duplicating, or racing a token
//! only degrades to a spurious wakeup: every park site re-checks the real
//! state it waits on (queue contents, timer deadlines, object signaled-state)
//! after each wake and re-parks when nothing is ready. State therefore stays
//! in the structures that own it today; the channel carries notification only.
//!
//! The registry rule mirrors docs/architecture/runtime.md's deadlock rule:
//! [`WaiterRegistry`] is touched ONLY at wait-enter, wait-exit, and signal —
//! never held across a park.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A wake token sent to a parked thread.
///
/// Tokens are hints ONLY: every park site re-checks its real condition after
/// any wake, so [`Wake::Shutdown`] doubles as the generic "object state
/// changed — recheck now" token sent by kernel-object signals (event set,
/// semaphore release, thread/process finish). No park site acts on the token
/// kind itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// A message was posted to the shared queue (`PostMessage*` / host input).
    MessagePosted,
    /// A timer was armed (`SetTimer`) — the parked pump should recompute its
    /// deadline so the new timer is not slept past.
    TimerArmed,
    /// Explicit teardown: the process/session is ending; stop parking.
    Shutdown,
}

/// One guest thread's inbox: cloneable send-half + the receiving half.
///
/// The receiver lives behind the same `Arc` as the sender; all `mpsc`
/// receiver methods take `&self`, so clones share one queue without extra
/// locking on the send side.
#[derive(Debug, Clone)]
pub struct ThreadInbox {
    inner: Arc<InboxInner>,
}

#[derive(Debug)]
struct InboxInner {
    tx: std::sync::mpsc::Sender<Wake>,
    // Mutex-wrapped so `ThreadInbox` is Send + Sync: the watcher closures in
    // `kernel32::file_io::watch` capture weak references to objects that
    // (transitively) contain waiter registries, and those require the whole
    // object graph to be shareable. Only the owning thread ever receives.
    rx: std::sync::Mutex<std::sync::mpsc::Receiver<Wake>>,
}

impl Default for ThreadInbox {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadInbox {
    /// Create an empty inbox.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            inner: Arc::new(InboxInner {
                tx,
                rx: std::sync::Mutex::new(rx),
            }),
        }
    }

    /// Whether `self` and `other` share the same underlying channel.
    #[must_use]
    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Send one token (best-effort). Never blocks; a dead receiver drops the
    /// token silently — safe under the hints-never-data invariant.
    pub fn send(&self, wake: Wake) {
        let _ = self.inner.tx.send(wake);
    }

    /// Drain every queued token via `try_recv`. Returns how many were seen.
    pub fn drain(&self) -> usize {
        let rx = self.inner.rx.lock().unwrap_or_else(|p| p.into_inner());
        let mut seen: usize = 0;
        while let Ok(_wake) = rx.try_recv() {
            seen = seen.saturating_add(1);
        }
        seen
    }

    /// Block until a token arrives or the bounded wait expires.
    ///
    /// `deadline` (an absolute bound, e.g. the nearest due guest timer) and
    /// `cap` (a liveness ceiling for control-flag / signal responsiveness)
    /// combine into one timeout. Returns whether a token arrived before then.
    pub fn wait_bounded(&self, deadline: Option<Instant>, cap: Duration) -> bool {
        let mut timeout = cap;
        if let Some(deadline) = deadline {
            timeout = timeout.min(deadline.saturating_duration_since(Instant::now()));
        }
        let rx = self.inner.rx.lock().unwrap_or_else(|p| p.into_inner());
        matches!(
            rx.recv_timeout(timeout),
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        )
    }

    /// Block until a token arrives (no timeout). For genuinely INFINITE waits
    /// whose every wake source sends tokens.
    pub fn wait_forever(&self) -> Wake {
        let rx = self.inner.rx.lock().unwrap_or_else(|p| p.into_inner());
        match rx.recv() {
            Ok(wake) => wake,
            Err(std::sync::mpsc::RecvError) => Wake::Shutdown,
        }
    }
}

/// Guest TID → inbox registry.
///
/// One hub per session, stored in [`crate::state::KernelState`] (canonical)
/// and cloned onto the message queue + session so producers can send without
/// locking the big WinAPI mutex.
#[derive(Debug, Clone, Default)]
pub struct WakeHub {
    inner: Arc<Mutex<HashMap<u32, ThreadInbox>>>,
}

impl WakeHub {
    /// Get or create the inbox for `tid`.
    ///
    /// Idempotent: repeated registration hands back the same channel, so a
    /// park site can call this per park without leaking duplicates.
    #[must_use]
    pub fn inbox_for(&self, tid: u32) -> ThreadInbox {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(tid).or_default().clone()
    }

    /// Drop the inbox registered for `tid` (thread teardown).
    pub fn remove(&self, tid: u32) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.remove(&tid);
    }

    /// Send a token to `tid`'s inbox if one is registered.
    pub fn send_to(&self, tid: u32, wake: Wake) {
        let inbox = {
            let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            map.get(&tid).cloned()
        };
        // Outside the map lock: sending never blocks, but keep the critical
        // section minimal anyway.
        if let Some(inbox) = inbox {
            inbox.send(wake);
        }
    }

    /// Send a token to EVERY registered inbox.
    ///
    /// Broadcasts are always safe: non-target threads wake spuriously,
    /// re-check their own wait condition, and re-park.
    pub fn broadcast(&self, wake: Wake) {
        let targets: Vec<ThreadInbox> = {
            let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            map.values().cloned().collect()
        };
        for inbox in targets {
            inbox.send(wake);
        }
    }

    /// Number of registered inboxes (diagnostics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Whether no inbox is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Per-waitable-object waiter list: the inboxes of threads currently parked
/// on THIS object.
///
/// Touched only at wait-enter ([`Self::enter`]), wait-exit ([`Self::exit`]),
/// and signal ([`Self::wake_all`] / [`Self::wake_one`]) — never held across a
/// park, preserving the documented deadlock rule.
#[derive(Debug, Default)]
pub struct WaiterRegistry {
    inner: Mutex<Vec<ThreadInbox>>,
}

impl WaiterRegistry {
    /// Register `inbox` as parked on this object (wait-enter).
    ///
    /// Idempotent per channel: a re-entering waiter does not duplicate its
    /// entry, so `exit` removes exactly what `enter` added.
    pub fn enter(&self, inbox: &ThreadInbox) {
        let mut waiters = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if !waiters.iter().any(|w| w.same_channel(inbox)) {
            waiters.push(inbox.clone());
        }
    }

    /// Unregister `inbox` (wait-exit).
    pub fn exit(&self, inbox: &ThreadInbox) {
        let mut waiters = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        waiters.retain(|w| !w.same_channel(inbox));
    }

    /// Send a token to every registered waiter (manual-reset semantics,
    /// `ReleaseSemaphore`, thread/process finish, directory-watch push).
    ///
    /// Entries stay registered — removal belongs to the parking thread's
    /// wait-exit ([`Self::exit`]), because a woken waiter that finds its
    /// condition still unsatisfied re-parks WITHOUT re-entering.
    pub fn wake_all(&self, wake: Wake) {
        let targets: Vec<ThreadInbox> = {
            let waiters = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            waiters.iter().cloned().collect()
        };
        for inbox in targets {
            inbox.send(wake);
        }
    }

    /// Send exactly ONE token — deliberate selection matching Win32
    /// auto-reset / mutex-release semantics (a competing-consumer broadcast
    /// would let an arbitrary waiter steal the wake). The entry stays
    /// registered ([`Self::exit`] owns removal); the selected waiter consumes
    /// the object state its recheck observes, and losing waiters re-park on
    /// their next timeout tick.
    pub fn wake_one(&self, wake: Wake) {
        let target = {
            let waiters = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            waiters.first().cloned()
        };
        if let Some(inbox) = target {
            inbox.send(wake);
        }
    }

    /// Number of registered waiters (diagnostics / tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Whether no waiter is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A token sent from another thread reaches a parked waiter within a
    /// bounded time (the lost-wakeup contract).
    #[test]
    fn message_posted_token_wakes_parked_waiter_within_bound() {
        let inbox = ThreadInbox::new();
        let waiter = inbox.clone();
        let handle = std::thread::spawn(move || waiter.wait_bounded(None, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        let t0 = Instant::now();
        inbox.send(Wake::MessagePosted);
        assert!(handle.join().expect("waiter thread"), "token received");
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "wake must be prompt, took {:?}",
            t0.elapsed()
        );
    }

    /// A `Shutdown` token terminates a parked thread promptly even with no
    /// deadline armed.
    #[test]
    fn shutdown_token_terminates_parked_thread_promptly() {
        let inbox = ThreadInbox::new();
        let waiter = inbox.clone();
        let handle = std::thread::spawn(move || waiter.wait_bounded(None, Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(20));
        inbox.send(Wake::Shutdown);
        assert!(handle.join().expect("waiter thread"), "shutdown received");
    }

    /// Tokens are hints: several queued before a park drain as one spurious
    /// wakeup, and a later park with nothing pending times out instead of
    /// consuming phantom tokens.
    #[test]
    fn duplicate_tokens_drain_and_never_block_a_later_empty_park() {
        let inbox = ThreadInbox::new();
        inbox.send(Wake::MessagePosted);
        inbox.send(Wake::MessagePosted);
        inbox.send(Wake::TimerArmed);
        assert_eq!(inbox.drain(), 3, "all queued tokens drained");
        let t0 = Instant::now();
        assert!(!inbox.wait_bounded(None, Duration::from_millis(25)));
        assert!(t0.elapsed() >= Duration::from_millis(20), "timed out");
    }

    /// Concurrent producers against a draining waiter: no lost wakeups (the
    /// final count reaches the sent total), no panic under contention.
    #[test]
    fn concurrent_senders_stress_never_loses_wakeups() {
        const SENDERS: usize = 4;
        const PER_SENDER: usize = 250;
        let inbox = ThreadInbox::new();
        let received = Arc::new(AtomicUsize::new(0));
        let drainer_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let drainer_inbox = inbox.clone();
        let drainer_received = Arc::clone(&received);
        let drainer_stop_flag = Arc::clone(&drainer_stop);
        let drainer = std::thread::spawn(move || {
            while !drainer_stop_flag.load(Ordering::Relaxed) {
                let drained = drainer_inbox.drain();
                if drained > 0 {
                    drainer_received.fetch_add(drained, Ordering::Relaxed);
                }
                // A token consumed by the bounded wait counts too — every
                // delivered token must be accounted exactly once.
                if drainer_inbox.wait_bounded(None, Duration::from_millis(1)) {
                    drainer_received.fetch_add(1, Ordering::Relaxed);
                }
            }
            let drained = drainer_inbox.drain();
            drainer_received.fetch_add(drained, Ordering::Relaxed);
        });

        let mut producers = Vec::new();
        for _ in 0..SENDERS {
            let producer_inbox = inbox.clone();
            producers.push(std::thread::spawn(move || {
                for _ in 0..PER_SENDER {
                    producer_inbox.send(Wake::MessagePosted);
                }
            }));
        }
        for producer in producers {
            producer.join().expect("producer");
        }
        // Wait until every token was observed by the drainer (bounded).
        let total = SENDERS * PER_SENDER;
        let deadline = Instant::now() + Duration::from_secs(10);
        while received.load(Ordering::Relaxed) < total && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        drainer_stop.store(true, Ordering::Relaxed);
        inbox.send(Wake::Shutdown); // unblock the final 1 ms wait (counted too)
        drainer.join().expect("drainer");
        assert!(
            received.load(Ordering::Relaxed) >= total,
            "mpsc must deliver every token ({} observed)",
            received.load(Ordering::Relaxed)
        );
    }

    /// `WakeHub` get-or-create is idempotent and broadcasts reach everyone.
    #[test]
    fn hub_registration_is_idempotent_and_broadcast_reaches_all() {
        let hub = WakeHub::default();
        let primary = hub.inbox_for(0x5678);
        let again = hub.inbox_for(0x5678);
        assert!(primary.same_channel(&again), "re-registration is stable");
        let worker = hub.inbox_for(0x9001);
        assert_eq!(hub.len(), 2);

        hub.broadcast(Wake::Shutdown);
        assert_eq!(primary.drain(), 1);
        assert_eq!(worker.drain(), 1);

        hub.send_to(0x5678, Wake::MessagePosted);
        assert_eq!(primary.drain(), 1);
        hub.remove(0x5678);
        hub.send_to(0x5678, Wake::MessagePosted);
        assert_eq!(primary.drain(), 0, "removed inbox receives nothing");
    }

    /// Registry selection semantics: `wake_one` wakes exactly one waiter;
    /// `wake_all` wakes everyone; `exit` stops delivery.
    #[test]
    fn registry_selects_exactly_one_on_wake_one_and_all_on_wake_all() {
        let registry = WaiterRegistry::default();
        let a = ThreadInbox::new();
        let b = ThreadInbox::new();
        registry.enter(&a);
        registry.enter(&b);
        assert_eq!(registry.len(), 2);

        registry.wake_one(Wake::TimerArmed);
        let woke_a = a.drain();
        let woke_b = b.drain();
        assert_eq!(
            woke_a + woke_b,
            1,
            "exactly one waiter receives the single token"
        );
        assert_eq!(
            registry.len(),
            2,
            "selection does not unregister; exit owns removal"
        );

        registry.enter(&a);
        registry.enter(&b);
        registry.wake_all(Wake::Shutdown);
        assert_eq!(a.drain(), 1, "wake_all reaches A");
        assert_eq!(b.drain(), 1, "wake_all reaches B");

        registry.exit(&a);
        registry.wake_all(Wake::MessagePosted);
        assert_eq!(a.drain(), 0, "exited waiter gets nothing");
        assert_eq!(b.drain(), 1);
    }
}
