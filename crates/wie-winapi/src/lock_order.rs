//! Debug-only assertion of the `WinApiState → MessageQueue → PresentChannel`
//! lock order.
//!
//! That order is prose. It is stated once, on [`present::PresentChannel`]:
//! guest threads take big-`WinApiState` → channel (inside paint handlers and
//! `publish`), the presenter takes channel only, so nobody takes channel →
//! big. The two other legs are prose in passing (`MessageQueue` lives behind
//! its own mutex precisely so the host can post without the big lock, which
//! makes queue-under-state the only way it is ever taken).
//!
//! Prose is not enforceable, and a violation here does not fail — it **hangs
//! the macOS UI thread**. A hang is invisible in CI, unreproducible on a
//! developer's machine, and looks like a beachball rather than a bug. This
//! module turns it into a panic at the acquisition that creates the cycle.
//!
//! ## What counts as a violation
//!
//! * **Inversion.** Taking a lock while holding one of *higher* rank. The
//!   cycle is `channel → state`; `queue → state` and `channel → queue` are the
//!   same bug one rank down.
//! * **Double lock.** Taking a lock this thread already holds. A `std` mutex
//!   is not re-entrant, so this is always a hang, and it is what a
//!   drop/re-lock boundary in a long dispatch arm gets wrong when it forgets
//!   to drop before re-locking.
//!
//! The rank order is the declaration order of [`Lock`], so the invariant is
//! written once, in the enum, instead of in each doc comment.
//!
//! ## Cost
//!
//! One `u8` per thread: a thread-local bitmask, no allocation, no atomics, no
//! lock of its own. Everything that touches it is `#[cfg(debug_assertions)]`.
//! In release [`Scope`] is a zero-sized type with an empty `Drop`, so
//! [`Ordered`] is a newtype over the guard it wraps and compiles to the code
//! the mutex produced before this module existed.
//!
//! ## Using it
//!
//! [`enter`] *before* blocking on the mutex (that is the point: the panic must
//! land before the thread parks), then hand the [`Scope`] to [`Ordered::wrap`]
//! with the guard, so the record lives exactly as long as the lock is held:
//!
//! ```
//! # use wie_winapi::lock_order::{Lock, Ordered, enter};
//! # fn f<T>(m: &std::sync::Mutex<T>) {
//! let scope = enter(Lock::Queue);
//! let guard = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
//! let guard = Ordered::wrap(scope, guard);
//! # let _ = guard;
//! # }
//! ```
//!
//! [`present::PresentChannel`]: crate::present::PresentChannel

use std::ops::{Deref, DerefMut};
use std::sync::MutexGuard;

use crate::state::WinApiState;

#[cfg(debug_assertions)]
use std::cell::Cell;

/// One of the three locks whose order is documented, in the order they may be
/// taken. The rank is the declaration order — do not reorder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lock {
    /// The process-wide `Arc<Mutex<WinApiState>>`. Outermost.
    State,
    /// [`present::MessageQueue`](crate::present::MessageQueue), always taken
    /// with `State` already held (its accessor is `&WinApiState`).
    Queue,
    /// [`present::PresentChannel`](crate::present::PresentChannel)'s frame
    /// slot. Innermost: the presenter takes it alone, the guest takes it under
    /// `State`.
    Channel,
}

impl Lock {
    /// Every lock, outermost first.
    ///
    /// Debug-only with the rest of the bookkeeping, so a release build does not
    /// carry a table it can never consult (and does not warn that it is unused).
    #[cfg(debug_assertions)]
    const ALL: [Self; 3] = [Self::State, Self::Queue, Self::Channel];

    /// The bit this lock occupies in a thread's held-set.
    #[cfg(debug_assertions)]
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }

    #[cfg(debug_assertions)]
    const fn name(self) -> &'static str {
        match self {
            Self::State => "WinApiState",
            Self::Queue => "MessageQueue",
            Self::Channel => "PresentChannel",
        }
    }
}

// The locks one thread currently holds, as a bitmask of `Lock::bit`.
#[cfg(debug_assertions)]
thread_local! {
    static HELD: Cell<u8> = const { Cell::new(0) };
}

/// Proof that this thread now holds `lock`; cleared when dropped.
///
/// Zero-sized in release.
#[cfg(debug_assertions)]
#[derive(Debug)]
#[must_use = "dropping the scope immediately records nothing; keep it alive as long as the lock is held"]
pub struct Scope {
    bit: u8,
}

/// Proof that this thread now holds `lock`; cleared when dropped.
///
/// Zero-sized in release.
#[cfg(not(debug_assertions))]
#[derive(Debug)]
pub struct Scope;

/// Record that this thread is about to take `lock`, panicking if that closes a
/// cycle.
///
/// Must be called **before** blocking on the mutex: a panic that arrives after
/// the thread has parked is a hang, not a diagnostic.
///
/// # Panics
///
/// In debug builds, when `lock` outranks a lock already held (an inversion) or
/// is already held by this thread (a double lock).
#[cfg(debug_assertions)]
#[must_use = "dropping the scope immediately records nothing; keep it alive as long as the lock is held"]
pub fn enter(lock: Lock) -> Scope {
    let held = HELD.with(Cell::get);
    if held & lock.bit() != 0 {
        panic!(
            "lock order: {} taken twice on one thread — `std` mutexes are not \
             re-entrant, so this self-deadlocks (a dispatch arm that re-locks \
             without dropping first?)",
            lock.name(),
        );
    }
    if let Some(outer) = Lock::ALL
        .into_iter()
        .find(|outer| *outer > lock && held & outer.bit() != 0)
    {
        panic!(
            "lock order: {} acquired while {} is held. The documented order is \
             WinApiState → MessageQueue → PresentChannel (see the \
             `PresentChannel` doc comment); taking {} → {} lets a guest thread \
             holding {} wait on {} while this thread holds {} and waits on {} — \
             which deadlocks the macOS UI thread.",
            lock.name(),
            outer.name(),
            outer.name(),
            lock.name(),
            outer.name(),
            lock.name(),
            outer.name(),
            lock.name(),
        );
    }
    HELD.with(|cell| cell.set(held | lock.bit()));
    Scope { bit: lock.bit() }
}

/// Record that this thread is about to take `lock`.
///
/// Release builds do no bookkeeping: there is no order to violate that a
/// release build could report.
#[cfg(not(debug_assertions))]
#[inline]
#[must_use = "dropping the scope immediately records nothing; keep it alive as long as the lock is held"]
pub fn enter(_lock: Lock) -> Scope {
    Scope
}

#[cfg(debug_assertions)]
impl Drop for Scope {
    fn drop(&mut self) {
        HELD.with(|cell| cell.set(cell.get() & !self.bit));
    }
}

#[cfg(not(debug_assertions))]
impl Drop for Scope {
    #[inline]
    fn drop(&mut self) {}
}

/// A mutex guard that keeps the lock-order record alive for as long as it is
/// held.
///
/// Derefs to the guarded value, so a call site that took a `MutexGuard` works
/// unchanged.
#[derive(Debug)]
pub struct Ordered<'a, T> {
    // Declared before `scope` so the guard (and therefore the lock) is released
    // first and the record is cleared second: clearing it while the lock was
    // still held would let this thread take an out-of-order lock unnoticed.
    guard: MutexGuard<'a, T>,
    _scope: Scope,
}

impl<'a, T> Ordered<'a, T> {
    /// Pair a [`Scope`] from [`enter`] with the guard it was taken for.
    ///
    /// The two must belong together — that is why this is a constructor rather
    /// than letting callers keep them in separate bindings, where one can be
    /// dropped early.
    pub fn wrap(scope: Scope, guard: MutexGuard<'a, T>) -> Self {
        Self {
            guard,
            _scope: scope,
        }
    }
}

impl<T> Deref for Ordered<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for Ordered<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

/// The big `WinApiState` guard, carrying its lock-order record.
///
/// The runtime's hook protocol passes the guard by value across several
/// functions, so the record has to travel with it — this alias is what those
/// signatures name.
pub type OrderedState<'a> = Ordered<'a, WinApiState>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::PoisonError;

    /// The documented order must not trip the assertion, or every test in the
    /// crate would.
    #[test]
    fn documented_order_does_not_panic() {
        let _state = enter(Lock::State);
        let _queue = enter(Lock::Queue);
        let _channel = enter(Lock::Channel);
    }

    /// **The assertion is falsifiable.** `channel → state` is the exact cycle
    /// the `PresentChannel` doc comment says is impossible; if taking it does
    /// not panic, this module is a no-op and every other test here is
    /// decoration.
    #[test]
    fn channel_then_state_panics() {
        let panicked = std::panic::catch_unwind(|| {
            let _channel = enter(Lock::Channel);
            let _state = enter(Lock::State);
        });
        let payload = panicked.expect_err("channel → state must panic, not hang");
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.contains("PresentChannel") && message.contains("WinApiState"),
            "the panic must name both locks, got: {message}"
        );
    }

    /// Same cycle, one rank down: the queue accessor is `&WinApiState`, so
    /// taking the queue under the channel would come straight back for the
    /// state lock.
    #[test]
    fn channel_then_queue_panics() {
        let panicked = std::panic::catch_unwind(|| {
            let _channel = enter(Lock::Channel);
            let _queue = enter(Lock::Queue);
        });
        assert!(panicked.is_err(), "channel → queue must panic");
    }

    /// A dispatch arm that re-locks without dropping first: not an ordering
    /// bug but just as fatal, and just as invisible without this assertion.
    #[test]
    fn double_lock_panics() {
        let panicked = std::panic::catch_unwind(|| {
            let _first = enter(Lock::State);
            let _second = enter(Lock::State);
        });
        assert!(panicked.is_err(), "double-locking must panic");
    }

    /// Dropping the guard must release the slot, or the first visitor to any
    /// lock would poison every later acquisition on that thread.
    #[test]
    fn dropping_a_scope_releases_its_slot() {
        {
            let _channel = enter(Lock::Channel);
        }
        // Would panic if the channel bit were still set.
        let _state = enter(Lock::State);
    }

    /// The record has to be attached to the real guard, not to a scope the
    /// caller can drop early — otherwise the assertion silently stops watching
    /// the lock while it is still held.
    #[test]
    fn ordered_holds_the_record_for_the_lifespan_of_the_guard() {
        let mutex = Mutex::new(0_u8);
        let scope = enter(Lock::Channel);
        let mut guard = Ordered::wrap(scope, mutex.lock().unwrap_or_else(PoisonError::into_inner));
        *guard += 1;
        assert_eq!(*guard, 1, "Ordered derefs to the guarded value");

        // Still held → still recorded: this is the inversion the whole module
        // exists to catch, reached through the wrapper rather than by hand.
        let inverted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = enter(Lock::State);
        }));
        assert!(inverted.is_err(), "state under a live Ordered must panic");

        drop(guard);
        // Released → the slot is free again.
        let _state = enter(Lock::State);
    }

    /// The two hooks in this crate must actually fire: a real
    /// `lock_message_queue` under a held channel is the deadlock, and it has to
    /// panic through the real accessor, not only through [`enter`] by hand.
    #[test]
    fn holding_the_channel_makes_the_real_queue_accessor_panic() {
        let state = crate::state::tests::winapi_state_default();
        // `WinApiState::lock_message_queue` is `&self`, so the big state is
        // already held by definition; simulate the presenter side instead —
        // a thread holding channel with no state.
        let _channel = enter(Lock::Channel);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _queue = state.lock_message_queue();
        }));
        assert!(
            panicked.is_err(),
            "the queue accessor must panic while channel is held"
        );
    }
}
