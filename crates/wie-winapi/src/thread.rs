//! Guest thread identity and thread-local storage (MT.0–MT.3).
//!
//! WIE models a Windows process with one or more guest threads. Primary runs on
//! the session host thread; workers (MT.2+) each get a host `std::thread` and
//! serialize guest execution on the shared CPU engine.
//!
//! Design notes (Apple Silicon / soft-translate):
//! - Guest TID is independent of host pthread id.
//! - TLS **indices** are process-wide (`TlsAlloc`); **values** live on the
//!   active [`GuestThread`].
//! - Last-error is per-guest-thread on the host side ([`GuestThread::last_error`],
//!   exactly like real Windows' per-TEB storage) and in guest memory: the
//!   primary engine's GS base is the fixed [`crate::GS_BASE`] page and each
//!   worker engine is bound to its own TEB page (`CpuEngine::set_gs_base`),
//!   so GS-relative last-error accesses resolve per thread, never to one
//!   shared mirror. The `WinApiState` absorb/publish helpers keep the ACTIVE
//!   engine's TEB slot and the host slot coherent across API dispatches.

use std::cell::Cell;

use ahash::HashMap;
use ahash::HashMapExt;

/// Documented primary (bootstrap) guest thread id.
///
/// Matches the historical constant used by `GetCurrentThreadId` stubs and
/// micro-tests that only require a non-zero TID.
pub const PRIMARY_THREAD_ID: u32 = 0x5678;

/// First guest TID allocated for workers (`CreateThread`).
pub const FIRST_WORKER_TID: u32 = 0x5679;

thread_local! {
    /// Guest TID of the thread that *this host thread* is currently running.
    ///
    /// `ThreadState` lives inside the process-wide `Arc<Mutex<WinApiState>>`,
    /// so `active` is a single slot written by every guest thread's scheduler
    /// loop and read by every handler. WIE releases that mutex before running
    /// guest code (`QuantumCore::step` locks only around activation /
    /// dispatch), so two guest threads overlap: whichever loop called
    /// `activate()` last owns `active`, and a handler running on the *other*
    /// host thread would read that foreign TID. Identity must not be inferred
    /// from shared mutable state.
    ///
    /// WIE's threading is 1:1 — one host thread runs exactly one guest thread
    /// at a time — so a thread-local is the correct owner: the binding is
    /// written only by the loop that owns the guest thread and read only by
    /// that same host thread, which makes it race-free by construction with no
    /// lock on the hottest path (`GetCurrentThreadId` and friends).
    ///
    /// `UNBOUND` means "no guest thread bound on this host thread": the
    /// presenter and other non-guest host threads reach handlers that consult
    /// `current_tid()` without ever having run a quantum. Those fall back to
    /// `active`, preserving the previous behaviour.
    static BOUND_TID: Cell<u32> = const { Cell::new(UNBOUND_TID) };
}

/// Sentinel for "this host thread has no guest thread bound" (see [`BOUND_TID`]).
///
/// `0` is never a valid guest TID: [`PRIMARY_THREAD_ID`] is `0x5678` and
/// workers are allocated from [`FIRST_WORKER_TID`], so it cannot collide.
const UNBOUND_TID: u32 = 0;

/// Bind the guest TID that the calling host thread is running.
///
/// Called from the scheduler loops (`QuantumCore::step` and the primary pump)
/// immediately before each guest quantum, on the host thread that owns that
/// guest thread. This is the only writer, which is what makes [`ThreadState::
/// current_tid`] safe to read from a handler.
pub fn bind_current_tid(tid: u32) {
    BOUND_TID.with(|slot| slot.set(tid));
}

/// Clear the calling host thread's guest-TID binding.
pub fn unbind_current_tid() {
    BOUND_TID.with(|slot| slot.set(UNBOUND_TID));
}

/// Guest TID bound to the calling host thread, or [`UNBOUND_TID`] if none.
#[must_use]
pub fn bound_current_tid() -> u32 {
    BOUND_TID.with(Cell::get)
}

/// Process-wide TLS index bookkeeping + the currently scheduled guest thread.
///
/// Embedded in [`crate::WinApiState`]. Session / worker loops switch
/// [`Self::active`] when dispatching host API stops for that guest thread.
#[derive(Debug, Clone)]
pub struct ThreadState {
    /// Number of TLS indices allocated process-wide (`TlsAlloc` count).
    pub tls_index_count: u32,
    /// Thread that is currently running guest code / handling a WinAPI stop.
    pub active: GuestThread,
    /// All known guest threads (primary + workers), keyed by TID.
    pub by_tid: HashMap<u32, GuestThread>,
    /// Next TID for `CreateThread` (monotonic).
    pub next_tid: u32,
}

impl Default for ThreadState {
    fn default() -> Self {
        Self::primary()
    }
}

impl ThreadState {
    /// Primary thread only (session bootstrap).
    #[must_use]
    pub fn primary() -> Self {
        let primary = GuestThread::primary();
        let mut by_tid = HashMap::new();
        by_tid.insert(primary.tid, primary.clone());
        Self {
            tls_index_count: 0,
            active: primary,
            by_tid,
            next_tid: FIRST_WORKER_TID,
        }
    }

    /// Guest TID of the calling thread.
    ///
    /// Prefers the thread-local binding established by the scheduler loop that
    /// owns this host thread ([`bind_current_tid`]) and falls back to
    /// [`Self::active`] only on host threads that never ran a quantum (the
    /// presenter, unit tests driving a bare [`ThreadState`]).
    ///
    /// Reading `active` unconditionally is wrong in the MT runtime: `active`
    /// is one slot in state shared by all guest threads, and the WinAPI mutex
    /// is released while guest code runs, so a peer thread's `activate()` can
    /// land between this thread's activation and its handler dispatch. That
    /// made `GetCurrentThreadId` report a peer's TID, which winpthreads'
    /// `pthread_mutex_unlock` compares against the mutex owner and rejects,
    /// driving the guest into its never-release spin.
    #[must_use]
    pub fn current_tid(&self) -> u32 {
        let bound = bound_current_tid();
        if bound != UNBOUND_TID {
            return bound;
        }
        self.active.tid
    }

    /// Allocate a new worker TID and register an empty [`GuestThread`].
    pub fn alloc_worker(&mut self) -> u32 {
        let tid = self.next_tid;
        self.next_tid = self.next_tid.saturating_add(1);
        if self.next_tid == 0 {
            self.next_tid = FIRST_WORKER_TID;
        }
        let mut gt = GuestThread::with_tid(tid);
        let need = usize::try_from(self.tls_index_count).unwrap_or(0);
        if gt.tls_values.len() < need {
            gt.tls_values.resize(need, 0);
        }
        self.by_tid.insert(tid, gt);
        tid
    }

    /// Ensure `active.tls_values` can index `[0, tls_index_count)`.
    pub fn grow_active_tls_to_process_count(&mut self) {
        let need = usize::try_from(self.tls_index_count).unwrap_or(0);
        if self.active.tls_values.len() < need {
            self.active.tls_values.resize(need, 0);
        }
    }

    /// Persist `active` into `by_tid` (after host API that mutated TLS).
    pub fn save_active(&mut self) {
        let tid = self.active.tid;
        self.by_tid.insert(tid, self.active.clone());
    }

    /// Load `tid` into `active` (before running that guest thread).
    pub fn activate(&mut self, tid: u32) {
        self.save_active();
        if let Some(gt) = self.by_tid.get(&tid).cloned() {
            self.active = gt;
        } else {
            let mut gt = GuestThread::with_tid(tid);
            let need = usize::try_from(self.tls_index_count).unwrap_or(0);
            gt.tls_values.resize(need, 0);
            self.active = gt;
        }
        self.grow_active_tls_to_process_count();
    }
}

/// Per-guest-thread private state (registers live on the CPU engine / sync table).
#[derive(Debug, Clone)]
pub struct GuestThread {
    /// Guest `GetCurrentThreadId` value.
    pub tid: u32,
    /// Values for process TLS indices (`TlsGetValue` / `TlsSetValue`).
    pub tls_values: Vec<u64>,
    /// This thread's last-error value — the per-thread authoritative store.
    ///
    /// Windows keeps last-error in the per-thread TEB; `handle_get_last_error`
    /// / `handle_set_last_error` and every host handler that sets
    /// `ProcessState::last_error` operate on the ACTIVE thread through the
    /// `process.last_error` alias. The `WinApiState` absorb/publish helpers
    /// hydrate this slot from the ACTIVE engine's GS-relative TEB slot before
    /// a dispatch and publish it back (to that same per-engine slot) after —
    /// each thread's value lives in its own TEB page, never a shared mirror.
    pub last_error: u32,
}

impl GuestThread {
    /// Bootstrap primary thread.
    #[must_use]
    pub fn primary() -> Self {
        Self {
            tid: PRIMARY_THREAD_ID,
            tls_values: Vec::new(),
            last_error: 0,
        }
    }

    /// New worker thread with the given guest TID (MT.2+).
    #[must_use]
    pub fn with_tid(tid: u32) -> Self {
        Self {
            tid,
            tls_values: Vec::new(),
            last_error: 0,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::as_conversions)]
mod tests {
    use super::{PRIMARY_THREAD_ID, ThreadState, bind_current_tid, unbind_current_tid};

    /// The regression this guards: `WinApiState` (and therefore
    /// [`ThreadState`]) is shared by every guest thread's host thread, and the
    /// WinAPI mutex is released while guest code runs. So a peer's
    /// `activate()` landing between this thread's activation and its handler
    /// dispatch must not change what `current_tid()` reports here — that is
    /// what made winpthreads' `pthread_mutex_unlock` reject the owner and spin
    /// the guest forever (the `cpp_threads.exe` hang).
    ///
    /// Deterministic: each host thread binds its own TID and then the peer
    /// deliberately re-activates the shared slot, after which both must still
    /// observe their own identity. No timing, no sleeping, no load.
    #[test]
    fn current_tid_is_per_host_thread_not_the_shared_active_slot() {
        let state = std::sync::Arc::new(std::sync::Mutex::new(ThreadState::primary()));
        let worker_tid = {
            let mut guard = state.lock().expect("state lock");
            guard.alloc_worker()
        };

        let peer = state.clone();
        let observed = std::thread::spawn(move || {
            // This host thread owns `worker_tid`.
            bind_current_tid(worker_tid);

            // A peer host thread claims the shared `active` slot — exactly
            // what the runtime does when another guest thread takes a quantum.
            {
                let mut guard = peer.lock().expect("state lock");
                guard.activate(PRIMARY_THREAD_ID);
            }

            // Identity must still be this thread's, not the slot's new owner.
            let mine = guard_current_tid(&peer);
            unbind_current_tid();
            mine
        })
        .join()
        .expect("worker thread join");

        assert_eq!(
            observed, worker_tid,
            "a host thread bound to {worker_tid:#x} must keep reporting its own TID \
             after a peer re-activated the shared `active` slot"
        );
    }

    /// Read `current_tid()` through the shared state, as a handler would.
    fn guard_current_tid(state: &std::sync::Arc<std::sync::Mutex<ThreadState>>) -> u32 {
        let guard = state.lock().expect("state lock");
        guard.current_tid()
    }

    /// With nothing bound, `current_tid()` falls back to `active` so
    /// non-guest host threads (presenter) and bare-`ThreadState` unit tests
    /// keep their previous behaviour.
    #[test]
    fn current_tid_falls_back_to_active_when_unbound() {
        unbind_current_tid();
        let mut state = ThreadState::primary();
        assert_eq!(state.current_tid(), PRIMARY_THREAD_ID);
        let worker = state.alloc_worker();
        state.activate(worker);
        assert_eq!(state.current_tid(), worker);
        unbind_current_tid();
    }
}
