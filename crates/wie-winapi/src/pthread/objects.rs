//! Host-side objects that back the guest's opaque pthread handles.
//!
//! The guest sees only an `intptr_t` / `void *` (see `pthread.h`): every real
//! object lives here and the guest stores a tagged id. That mirrors what
//! mingw-w64 winpthreads itself does — its `pthread_mutex_t` is a pointer to a
//! heap `mutex_t` — so programs that copy, compare, or zero the handle keep
//! working.
//!
//! **Locking model.** Every field below is mutated only while the caller holds
//! the process-wide WinAPI mutex, so the structs need no interior locking. The
//! one exception is [`WakeQueue`], which is the host primitive a guest thread
//! parks on *after* dropping that mutex.

use ahash::HashMap;
use ahash::HashMapExt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Park queue with a monotonic wake sequence.
///
/// A parking thread samples [`WakeQueue::observe`] while it still holds the
/// WinAPI mutex, drops that mutex, then calls [`WakeQueue::park`] with the
/// sampled value. [`WakeQueue::wake`] bumps the sequence under the queue's own
/// mutex, so a wake published in that window is seen by the sequence compare
/// instead of being lost — the classic lost-wakeup race that a bare condvar
/// plus "notify without holding the waiter's lock" would hit.
///
/// Inbox-parked waiters (Painpoint 1) register through
/// [`WakeQueue::enter_wait`] / [`WakeQueue::exit_wait`]; `wake` also delivers
/// a token to each registered inbox so an inbox park wakes without polling.
#[derive(Debug, Default)]
pub struct WakeQueue {
    seq: Mutex<u64>,
    cv: Condvar,
    waiters: crate::wake::WaiterRegistry,
}

impl WakeQueue {
    /// Fresh queue behind an `Arc` (objects hand clones to parkers).
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Sample the wake sequence. Call while holding the WinAPI mutex.
    #[must_use]
    pub fn observe(&self) -> u64 {
        self.seq.lock().map_or(0, |g| *g)
    }

    /// Register an inbox-parked waiter (wait-enter; before the sequence check).
    pub fn enter_wait(&self, inbox: &crate::wake::ThreadInbox) {
        self.waiters.enter(inbox);
    }

    /// Unregister an inbox-parked waiter (wait-exit).
    pub fn exit_wait(&self, inbox: &crate::wake::ThreadInbox) {
        self.waiters.exit(inbox);
    }

    /// Publish a wake to every parker that sampled an older sequence.
    pub fn wake(&self) {
        if let Ok(mut g) = self.seq.lock() {
            *g = g.wrapping_add(1);
            self.cv.notify_all();
        }
        // Tokens are hints: an inbox-parked waiter re-checks its condition on
        // re-entry regardless of which token it saw.
        self.waiters.wake_all(crate::wake::Wake::Shutdown);
    }

    /// Block until the sequence moves past `observed`, or `timeout` elapses.
    ///
    /// Callers **must** hold no process locks: the thread that will wake us
    /// needs the WinAPI mutex to do it.
    pub fn park(&self, observed: u64, timeout: Duration) {
        let Ok(guard) = self.seq.lock() else {
            return;
        };
        if *guard != observed {
            // A wake landed between our sample and this lock — do not sleep.
            return;
        }
        drop(self.cv.wait_timeout(guard, timeout));
    }
}

// ── Mutex ──────────────────────────────────────────────────────────────

/// `PTHREAD_MUTEX_NORMAL`.
pub const MUTEX_NORMAL: i32 = 0;
/// `PTHREAD_MUTEX_ERRORCHECK`.
pub const MUTEX_ERRORCHECK: i32 = 1;
/// `PTHREAD_MUTEX_RECURSIVE`.
pub const MUTEX_RECURSIVE: i32 = 2;

/// A `pthread_mutex_t`.
#[derive(Debug, Clone)]
pub struct PtMutex {
    /// One of [`MUTEX_NORMAL`] / [`MUTEX_ERRORCHECK`] / [`MUTEX_RECURSIVE`].
    pub kind: i32,
    /// Current owner (`pthread_t`), or `None` when unlocked.
    pub owner: Option<u64>,
    /// Recursion depth for [`MUTEX_RECURSIVE`]; 1 for other kinds while held.
    pub depth: u32,
    /// Threads blocked in `pthread_mutex_lock`.
    pub queue: Arc<WakeQueue>,
}

impl PtMutex {
    /// New unlocked mutex of `kind`.
    #[must_use]
    pub fn new(kind: i32) -> Self {
        Self {
            kind,
            owner: None,
            depth: 0,
            queue: WakeQueue::new(),
        }
    }

    /// Whether any thread holds this mutex.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.owner.is_some()
    }

    /// Take the mutex for `pt` if free (or re-enter it when recursive).
    ///
    /// `Ok(true)` = acquired, `Ok(false)` = busy (caller parks or reports
    /// `EBUSY`), `Err(errno)` = a definite error the caller returns as-is.
    pub fn try_acquire(&mut self, pt: u64) -> Result<bool, i32> {
        match self.owner {
            None => {
                self.owner = Some(pt);
                self.depth = 1;
                Ok(true)
            }
            Some(cur) if cur == pt => {
                if self.kind == MUTEX_RECURSIVE {
                    self.depth = self.depth.saturating_add(1);
                    Ok(true)
                } else if self.kind == MUTEX_ERRORCHECK {
                    Err(super::EDEADLK)
                } else {
                    // NORMAL self-lock is undefined behaviour in POSIX and a
                    // real deadlock on Linux; block, matching the hardware.
                    Ok(false)
                }
            }
            Some(_) => Ok(false),
        }
    }

    /// Drop one level of ownership held by `pt`.
    ///
    /// Returns `Ok(true)` when the mutex became free (caller must wake the
    /// queue), `Ok(false)` when recursion remains.
    pub fn release(&mut self, pt: u64) -> Result<bool, i32> {
        match self.owner {
            Some(cur) if cur == pt => {
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.owner = None;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Err(super::EPERM),
        }
    }

    /// Release every recursion level (for `pthread_cond_wait`), returning the
    /// depth so the wait can restore it on re-acquire.
    pub fn release_all(&mut self, pt: u64) -> Result<u32, i32> {
        match self.owner {
            Some(cur) if cur == pt => {
                let depth = self.depth.max(1);
                self.owner = None;
                self.depth = 0;
                Ok(depth)
            }
            _ => Err(super::EPERM),
        }
    }
}

// ── Condition variable ─────────────────────────────────────────────────

/// One thread blocked in `pthread_cond_wait` / `pthread_cond_timedwait`.
#[derive(Debug, Clone)]
pub struct CondWaiter {
    /// Waiting thread.
    pub pt: u64,
    /// Set by `pthread_cond_signal` / `pthread_cond_broadcast`.
    pub signaled: bool,
}

/// A `pthread_cond_t`.
#[derive(Debug, Clone)]
pub struct PtCond {
    /// FIFO of blocked threads — `pthread_cond_signal` wakes the oldest.
    pub waiters: Vec<CondWaiter>,
    /// Host queue the blocked threads park on.
    pub queue: Arc<WakeQueue>,
}

impl PtCond {
    /// New condition variable with no waiters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            waiters: Vec::new(),
            queue: WakeQueue::new(),
        }
    }

    /// Mark the oldest unsignaled waiter as woken. True if one was found.
    pub fn signal_one(&mut self) -> bool {
        for w in &mut self.waiters {
            if !w.signaled {
                w.signaled = true;
                return true;
            }
        }
        false
    }

    /// Mark every waiter as woken.
    pub fn signal_all(&mut self) {
        for w in &mut self.waiters {
            w.signaled = true;
        }
    }

    /// Whether `pt`'s waiter has been signaled.
    #[must_use]
    pub fn is_signaled(&self, pt: u64) -> bool {
        self.waiters.iter().any(|w| w.pt == pt && w.signaled)
    }

    /// Remove `pt` from the waiter list.
    pub fn remove(&mut self, pt: u64) {
        self.waiters.retain(|w| w.pt != pt);
    }
}

impl Default for PtCond {
    fn default() -> Self {
        Self::new()
    }
}

// ── Read/write lock ────────────────────────────────────────────────────

/// A `pthread_rwlock_t`.
///
/// Reader-preferring, like the glibc default: a read lock succeeds whenever no
/// writer holds the lock, even with writers queued. Writer-preference would
/// deadlock the (POSIX-legal) recursive read-lock pattern.
#[derive(Debug, Clone)]
pub struct PtRwLock {
    /// Number of read locks currently held.
    pub readers: u32,
    /// Writer (`pthread_t`) holding the lock, if any.
    pub writer: Option<u64>,
    /// Blocked readers and writers.
    pub queue: Arc<WakeQueue>,
}

impl PtRwLock {
    /// New unlocked rwlock.
    #[must_use]
    pub fn new() -> Self {
        Self {
            readers: 0,
            writer: None,
            queue: WakeQueue::new(),
        }
    }

    /// Take a read lock if no writer holds it.
    pub fn try_read(&mut self) -> bool {
        if self.writer.is_some() {
            return false;
        }
        self.readers = self.readers.saturating_add(1);
        true
    }

    /// Take the write lock if completely free.
    pub fn try_write(&mut self, pt: u64) -> bool {
        if self.writer.is_some() || self.readers > 0 {
            return false;
        }
        self.writer = Some(pt);
        true
    }

    /// Release whichever lock `pt` holds. `Err` when it holds none.
    pub fn unlock(&mut self, pt: u64) -> Result<(), i32> {
        if self.writer == Some(pt) {
            self.writer = None;
            return Ok(());
        }
        if self.readers > 0 {
            self.readers = self.readers.saturating_sub(1);
            return Ok(());
        }
        Err(super::EPERM)
    }

    /// Whether any thread holds this lock.
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.writer.is_some() || self.readers > 0
    }
}

impl Default for PtRwLock {
    fn default() -> Self {
        Self::new()
    }
}

// ── Spinlock ───────────────────────────────────────────────────────────

/// A `pthread_spinlock_t`.
///
/// WIE cannot let a guest spin on a host thread — the lock holder needs the
/// WinAPI mutex to release it — so this parks like a mutex. Behaviour matches
/// the POSIX contract; only the "never sleeps" performance property differs.
#[derive(Debug, Clone)]
pub struct PtSpin {
    /// Owner (`pthread_t`) while locked.
    pub owner: Option<u64>,
    /// Blocked threads.
    pub queue: Arc<WakeQueue>,
}

impl PtSpin {
    /// New unlocked spinlock.
    #[must_use]
    pub fn new() -> Self {
        Self {
            owner: None,
            queue: WakeQueue::new(),
        }
    }
}

impl Default for PtSpin {
    fn default() -> Self {
        Self::new()
    }
}

// ── Barrier ────────────────────────────────────────────────────────────

/// A `pthread_barrier_t`.
#[derive(Debug, Clone)]
pub struct PtBarrier {
    /// Threads required to trip the barrier.
    pub count: u32,
    /// Threads that have arrived in the current generation.
    pub arrived: u32,
    /// Bumped every time the barrier trips; parked threads compare against it.
    pub generation: u64,
    /// Blocked threads.
    pub queue: Arc<WakeQueue>,
}

impl PtBarrier {
    /// New barrier that trips once `count` threads arrive.
    #[must_use]
    pub fn new(count: u32) -> Self {
        Self {
            count,
            arrived: 0,
            generation: 0,
            queue: WakeQueue::new(),
        }
    }
}

// ── Semaphore ──────────────────────────────────────────────────────────

/// A POSIX `sem_t`.
#[derive(Debug, Clone)]
pub struct PtSem {
    /// Current value.
    pub count: i32,
    /// Name for `sem_open` / `sem_unlink`; `None` for unnamed semaphores.
    pub name: Option<String>,
    /// Open references (named semaphores are shared by name).
    pub refs: u32,
    /// Whether `sem_unlink` has removed the name.
    pub unlinked: bool,
    /// Blocked threads.
    pub queue: Arc<WakeQueue>,
}

impl PtSem {
    /// New semaphore with `count` units.
    #[must_use]
    pub fn new(count: i32, name: Option<String>) -> Self {
        Self {
            count,
            name,
            refs: 1,
            unlinked: false,
            queue: WakeQueue::new(),
        }
    }
}

// ── One-time initialisation ────────────────────────────────────────────

/// State of a `pthread_once_t` control word, keyed by its guest VA.
#[derive(Debug, Clone)]
pub struct PtOnce {
    /// Thread currently running the init routine, if any.
    pub running: Option<u64>,
    /// Set once the init routine has returned.
    pub done: bool,
    /// Threads waiting for the init routine to finish.
    pub queue: Arc<WakeQueue>,
}

impl PtOnce {
    /// New, not-yet-run control.
    #[must_use]
    pub fn new() -> Self {
        Self {
            running: None,
            done: false,
            queue: WakeQueue::new(),
        }
    }
}

impl Default for PtOnce {
    fn default() -> Self {
        Self::new()
    }
}

// ── Thread ─────────────────────────────────────────────────────────────

/// A `pthread_t` and everything POSIX asks us to remember about it.
#[derive(Debug, Clone)]
pub struct PtThread {
    /// Guest-visible `pthread_t`.
    pub pt: u64,
    /// WIE guest thread id (`GetCurrentThreadId`).
    pub tid: u32,
    /// Guest RSP captured before the start routine runs, for setting up the
    /// return trampoline.
    pub entry_rsp: u64,
    /// Kernel thread handle from `create_guest_thread` (`pthread_gethandle`).
    pub win_handle: u64,
    /// Start routine VA (for diagnostics).
    pub start: u64,
    /// True after `pthread_detach` or `PTHREAD_CREATE_DETACHED`.
    pub detached: bool,
    /// True once a joiner has claimed the result.
    pub joined: bool,
    /// `void *` result from the start routine or `pthread_exit`.
    pub exit_value: u64,
    /// True once the start routine returned or `pthread_exit` ran.
    pub finished: bool,
    /// `pthread_cancel` has been requested.
    pub cancel_requested: bool,
    /// `PTHREAD_CANCEL_ENABLE` (default) vs `PTHREAD_CANCEL_DISABLE`.
    pub cancel_enabled: bool,
    /// `PTHREAD_CANCEL_ASYNCHRONOUS` vs `PTHREAD_CANCEL_DEFERRED` (default).
    pub cancel_async: bool,
    /// `pthread_setspecific` values, keyed by `pthread_key_t`.
    pub tls: HashMap<u32, u64>,
    /// `pthread_setname_np`.
    pub name: String,
    /// Guest VA of the `_pthread_cleanup` list head (`pthread_getclean`).
    pub clean_head_va: u64,
    /// Scheduling policy (`sched_setscheduler`; advisory here).
    pub policy: i32,
    /// Scheduling priority (advisory here).
    pub priority: i32,
    /// Threads blocked in `pthread_join` on this thread.
    pub queue: Arc<WakeQueue>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::{
        CondWaiter, MUTEX_ERRORCHECK, MUTEX_NORMAL, MUTEX_RECURSIVE, PtCond, PtMutex, PtOnce,
        PtRwLock, PtSem, PtSpin, PtThread, WakeQueue,
    };
    use crate::pthread::{EDEADLK, EPERM, is_pt_id};
    use std::time::Duration;

    // ── WakeQueue ───────────────────────────────────────────────────────

    #[test]
    fn wake_queue_observe_returns_current_seq() {
        let q = WakeQueue::new();
        let s = q.observe();
        q.wake();
        assert_eq!(q.observe(), s.wrapping_add(1));
    }

    #[test]
    fn wake_queue_park_returns_immediately_when_seq_changed() {
        let q = WakeQueue::new();
        let s = q.observe();
        q.wake(); // seq advances before park
        // park(observed = s) should see seq != observed and return
        q.park(s, Duration::from_secs(10));
        // If we got here without blocking for 10s, the no-wait worked.
    }

    #[test]
    fn wake_queue_multiple_wakes_increment_seq() {
        let q = WakeQueue::new();
        let s = q.observe();
        for _ in 0..5 {
            q.wake();
        }
        assert_eq!(q.observe(), s.wrapping_add(5));
    }

    // ── PtMutex ─────────────────────────────────────────────────────────

    #[test]
    fn mutex_new_is_unlocked() {
        let m = PtMutex::new(MUTEX_NORMAL);
        assert!(!m.is_held());
        assert_eq!(m.depth, 0);
    }

    #[test]
    fn mutex_try_acquire_succeeds_on_free() {
        let mut m = PtMutex::new(MUTEX_NORMAL);
        assert_eq!(m.try_acquire(42), Ok(true));
        assert!(m.is_held());
        assert_eq!(m.depth, 1);
    }

    #[test]
    fn mutex_release_frees_the_mutex() {
        let mut m = PtMutex::new(MUTEX_NORMAL);
        m.try_acquire(42).unwrap();
        assert_eq!(m.release(42), Ok(true));
        assert!(!m.is_held());
    }

    #[test]
    fn mutex_release_by_non_owner_returns_eperm() {
        let mut m = PtMutex::new(MUTEX_NORMAL);
        m.try_acquire(42).unwrap();
        assert_eq!(m.release(99), Err(EPERM));
    }

    #[test]
    fn mutex_try_acquire_busy_returns_false() {
        let mut m = PtMutex::new(MUTEX_NORMAL);
        m.try_acquire(42).unwrap();
        assert_eq!(m.try_acquire(99), Ok(false));
    }

    #[test]
    fn mutex_normal_self_lock_returns_false() {
        let mut m = PtMutex::new(MUTEX_NORMAL);
        m.try_acquire(42).unwrap();
        assert_eq!(m.try_acquire(42), Ok(false));
    }

    #[test]
    fn mutex_errorcheck_self_lock_returns_edeadlk() {
        let mut m = PtMutex::new(MUTEX_ERRORCHECK);
        m.try_acquire(42).unwrap();
        assert_eq!(m.try_acquire(42), Err(EDEADLK));
    }

    #[test]
    fn mutex_recursive_allows_reentry() {
        let mut m = PtMutex::new(MUTEX_RECURSIVE);
        m.try_acquire(42).unwrap();
        assert_eq!(m.try_acquire(42), Ok(true));
        assert_eq!(m.depth, 2);
        // One release drops depth to 1
        m.release(42).unwrap();
        assert!(m.is_held());
        // Final release frees
        m.release(42).unwrap();
        assert!(!m.is_held());
    }

    #[test]
    fn mutex_release_all_returns_depth() {
        let mut m = PtMutex::new(MUTEX_RECURSIVE);
        m.try_acquire(42).unwrap(); // depth = 1
        m.try_acquire(42).unwrap(); // depth = 2
        assert_eq!(m.release_all(42), Ok(2));
        assert!(!m.is_held());
    }

    #[test]
    fn mutex_release_all_by_non_owner_returns_eperm() {
        let mut m = PtMutex::new(MUTEX_NORMAL);
        m.try_acquire(42).unwrap();
        assert_eq!(m.release_all(99), Err(EPERM));
    }

    #[test]
    fn mutex_static_initializers_are_not_tagged_ids() {
        // winpthreads uses -1, -2, -3 as static initializers
        assert!(!is_pt_id(u64::from(u32::MAX))); // PTHREAD_MUTEX_INITIALIZER
    }

    // ── PtCond ──────────────────────────────────────────────────────────

    #[test]
    fn cond_new_has_no_waiters() {
        let c = PtCond::new();
        assert!(c.waiters.is_empty());
    }

    #[test]
    fn cond_signal_one_wakes_oldest_waiter() {
        let mut c = PtCond::new();
        c.waiters.push(CondWaiter {
            pt: 1,
            signaled: false,
        });
        c.waiters.push(CondWaiter {
            pt: 2,
            signaled: false,
        });
        assert!(c.signal_one());
        assert!(c.is_signaled(1));
        // second still not signaled
        assert!(!c.is_signaled(2));
    }

    #[test]
    fn cond_signal_all_wakes_everyone() {
        let mut c = PtCond::new();
        c.waiters.push(CondWaiter {
            pt: 10,
            signaled: false,
        });
        c.waiters.push(CondWaiter {
            pt: 20,
            signaled: false,
        });
        c.signal_all();
        assert!(c.is_signaled(10));
        assert!(c.is_signaled(20));
    }

    #[test]
    fn cond_remove_cleans_up_a_waiter() {
        let mut c = PtCond::new();
        c.waiters.push(CondWaiter {
            pt: 7,
            signaled: false,
        });
        c.remove(7);
        assert!(c.waiters.is_empty());
    }

    // ── PtRwLock ────────────────────────────────────────────────────────

    #[test]
    fn rwlock_new_is_free() {
        let l = PtRwLock::new();
        assert!(!l.is_held());
        assert_eq!(l.readers, 0);
        assert!(l.writer.is_none());
    }

    #[test]
    fn rwlock_read_lock_succeeds_when_free() {
        let mut l = PtRwLock::new();
        assert!(l.try_read());
        assert_eq!(l.readers, 1);
    }

    #[test]
    fn rwlock_multiple_readers_succeed() {
        let mut l = PtRwLock::new();
        l.try_read();
        assert!(l.try_read());
        assert_eq!(l.readers, 2);
    }

    #[test]
    fn rwlock_write_lock_succeeds_when_free() {
        let mut l = PtRwLock::new();
        assert!(l.try_write(1));
        assert_eq!(l.writer, Some(1));
    }

    #[test]
    fn rwlock_write_lock_fails_with_active_reader() {
        let mut l = PtRwLock::new();
        l.try_read();
        assert!(!l.try_write(2));
    }

    #[test]
    fn rwlock_write_lock_fails_with_active_writer() {
        let mut l = PtRwLock::new();
        l.try_write(1);
        assert!(!l.try_write(2));
    }

    #[test]
    fn rwlock_unlock_reader_releases_one_read() {
        let mut l = PtRwLock::new();
        l.try_read();
        l.try_read();
        assert!(l.unlock(1).is_ok());
        assert_eq!(l.readers, 1);
        assert!(l.unlock(1).is_ok());
        assert_eq!(l.readers, 0);
    }

    #[test]
    fn rwlock_unlock_writer_releases() {
        let mut l = PtRwLock::new();
        l.try_write(1);
        assert!(l.unlock(1).is_ok());
        assert_eq!(l.writer, None);
    }

    #[test]
    fn rwlock_unlock_unowned_returns_eperm() {
        let mut l = PtRwLock::new();
        assert_eq!(l.unlock(1), Err(EPERM));
    }

    // ── PtSpin ──────────────────────────────────────────────────────────

    #[test]
    fn spin_new_is_unlocked() {
        let s = PtSpin::new();
        assert!(s.owner.is_none());
    }

    // ── PtOnce ──────────────────────────────────────────────────────────

    #[test]
    fn once_new_is_not_done() {
        let o = PtOnce::new();
        assert!(!o.done);
        assert!(o.running.is_none());
    }

    // ── PtSem ───────────────────────────────────────────────────────────

    #[test]
    fn sem_new_has_specified_count() {
        let s = PtSem::new(3, None);
        assert_eq!(s.count, 3);
        assert!(s.name.is_none());
    }

    #[test]
    fn sem_new_with_name_is_unnlinked() {
        let s = PtSem::new(0, Some("test".into()));
        assert_eq!(s.name.as_deref(), Some("test"));
        assert!(!s.unlinked);
    }

    // ── PtThread ────────────────────────────────────────────────────────

    #[test]
    fn thread_new_is_not_finished() {
        let t = PtThread::new(0x5054_0000_0000_0001, 100, 0x6000_0001, 0x1400_1000, false);
        assert!(!t.finished);
        assert!(!t.detached);
        assert!(t.cancel_enabled);
        assert_eq!(t.tid, 100);
    }

    #[test]
    fn thread_detached_flag_is_stored() {
        let t = PtThread::new(0x5054_0000_0000_0002, 101, 0x6000_0001, 0x1400_1000, true);
        assert!(t.detached);
    }
}

impl PtThread {
    /// New joinable, cancellable thread record.
    #[must_use]
    pub fn new(pt: u64, tid: u32, win_handle: u64, start: u64, detached: bool) -> Self {
        Self {
            pt,
            tid,
            entry_rsp: 0,
            win_handle,
            start,
            detached,
            joined: false,
            exit_value: 0,
            finished: false,
            cancel_requested: false,
            cancel_enabled: true,
            cancel_async: false,
            tls: HashMap::new(),
            name: String::new(),
            clean_head_va: 0,
            policy: 0,
            priority: 0,
            queue: WakeQueue::new(),
        }
    }
}
