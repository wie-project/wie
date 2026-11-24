//! Poison-tolerant lock acquisition — the crate's one convention for turning a
//! `PoisonError` into a usable guard.
//!
//! A `panic!` while a `Mutex`/`RwLock` is held poisons it, and every later
//! `lock()` then returns `Err`, so `lock().unwrap()` converts one panic into a
//! permanent cascade: every thread that touches the lock afterwards dies too.
//! This crate deliberately does not pay that price. The state behind these
//! locks is JIT bookkeeping (block cache, chain ids, code-page registrations,
//! pending code writes, the flush queue) and guest memory; the mutations are
//! insert-or-replace or append-to-list, and the expensive artifacts (compiled
//! blocks, persisted ledger entries) are re-derived or hash-validated before
//! use. So carrying on with possibly-stale data is the lesser failure: a JIT
//! that panics because a peer thread panicked once mid-cache-update is worse
//! than one that keeps rasterizing, interpreting and compiling while the flag
//! stays observable through `Mutex::is_poisoned` / `RwLock::is_poisoned` for
//! diagnostics.
//!
//! These three functions are the sanctioned acquisition path. A bare
//! `lock().unwrap()` in new code is exactly what a future `clippy::unwrap_used`
//! gate (maintainability T1.2) needs to catch, and unlike an `Option`/`Result`
//! unwrap it is nearly impossible to spot in review — which is why this lives
//! in one place instead of being spelled `unwrap_or_else(|e| e.into_inner())`
//! at ~90 sites.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock `mutex`, recovering the guard if a thread panicked while holding it.
pub(crate) fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Take a shared read guard on `lock`, recovering it if the lock is poisoned.
pub(crate) fn read_unpoisoned<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Take an exclusive write guard on `lock`, recovering it if it is poisoned.
pub(crate) fn write_unpoisoned<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}
