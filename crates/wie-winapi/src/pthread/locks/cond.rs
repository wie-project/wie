//! Condition-variable half of the pthread synchronisation primitives; see the
//! module docs in `locks/mod.rs` for the shared re-entry model.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use wie_cpu::CpuEngine;

use super::super::objects::{CondWaiter, PtCond};
use super::super::threads::{cancellation_point, self_pt};
use super::super::{
    EINVAL, ETIMEDOUT, PtPending, absolute_deadline, is_pt_id, park_on, read_u64,
    relative_deadline, ret_int, slice_until, write_u64,
};
use super::mutex_id;
use crate::{WinApiHandlerResult, WinApiState};

// ── Condition variable ─────────────────────────────────────────────────

/// Resolve a `pthread_cond_t *`, promoting `PTHREAD_COND_INITIALIZER`.
pub(super) fn cond_id(engine: &mut dyn CpuEngine, state: &mut WinApiState, va: u64) -> Option<u64> {
    if va == 0 {
        return None;
    }
    let word = read_u64(engine, va);
    if is_pt_id(word) && state.pthread().conds.contains_key(&word) {
        return Some(word);
    }
    if !matches!(word.cast_signed(), 0 | -1) {
        return None;
    }
    let id = state.pthread().alloc_id();
    state.pthread().conds.insert(id, PtCond::new());
    write_u64(engine, va, id);
    Some(id)
}

/// Where a `pthread_cond_*wait` variant reads its deadline from.
#[derive(Debug, Clone, Copy)]
pub(super) enum DeadlineSource {
    /// `pthread_cond_wait` — no timeout.
    None,
    /// Absolute `CLOCK_REALTIME` timespec in the third argument.
    Absolute {
        /// 64-bit `time_t` layout (the mingw default).
        bits64: bool,
    },
    /// Relative interval in the third argument (`*_relative_np`).
    Relative {
        /// 64-bit `time_t` layout.
        bits64: bool,
    },
}

/// `pthread_cond_signal` / `pthread_cond_broadcast`.
pub(super) fn cond_wake(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    all: bool,
) -> Result<WinApiHandlerResult> {
    let cv = engine.read_rcx()?;
    let Some(id) = cond_id(engine, state, cv) else {
        return ret_int(engine, EINVAL);
    };
    if let Some(c) = state.pthread().conds.get_mut(&id) {
        if all {
            c.signal_all();
        } else {
            c.signal_one();
        }
        c.queue.wake();
    }
    ret_int(engine, 0)
}

/// `pthread_cond_wait` and every timed variant.
///
/// Three-phase state machine across re-entries: release the mutex and park,
/// wait for a signal, then re-acquire the mutex before returning.
pub(super) fn cond_wait(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    source: DeadlineSource,
) -> Result<WinApiHandlerResult> {
    if let Some(r) = cancellation_point(engine, state)? {
        return Ok(r);
    }
    let tid = state.kernel.threads.current_tid();
    let me = self_pt(engine, state);

    match state.pthread().pending.get(&tid).cloned() {
        // Phase 2: parked, waiting for a signal or the deadline.
        Some(PtPending::CondWait {
            cond,
            mutex,
            depth,
            deadline,
        }) => {
            let signaled = state
                .pthread()
                .conds
                .get(&cond)
                .is_some_and(|c| c.is_signaled(me));
            let expired = deadline.is_some_and(|dl| Instant::now() >= dl);
            if !signaled && !expired {
                let Some(c) = state.pthread().conds.get(&cond) else {
                    state.pthread().pending.remove(&tid);
                    return ret_int(engine, EINVAL);
                };
                let queue = Arc::clone(&c.queue);
                return Err(park_on(state, &queue, slice_until(deadline)));
            }
            if let Some(c) = state.pthread().conds.get_mut(&cond) {
                c.remove(me);
            }
            let result = if signaled { 0 } else { ETIMEDOUT };
            state.pthread().pending.insert(
                tid,
                PtPending::CondReacquire {
                    mutex,
                    depth,
                    result,
                },
            );
            cond_reacquire(engine, state, mutex, depth, result)
        }

        // Phase 3: signaled; re-take the mutex before returning to the guest.
        Some(PtPending::CondReacquire {
            mutex,
            depth,
            result,
        }) => cond_reacquire(engine, state, mutex, depth, result),

        // Phase 1: fresh call — release the mutex and register as a waiter.
        _ => {
            let cv = engine.read_rcx()?;
            let m = engine.read_rdx()?;
            let ts = engine.read_r8()?;
            let deadline = match source {
                DeadlineSource::None => None,
                DeadlineSource::Absolute { bits64 } => absolute_deadline(engine, ts, bits64),
                DeadlineSource::Relative { bits64 } => relative_deadline(engine, ts, bits64),
            };
            let (Some(cond), Some(mutex)) =
                (cond_id(engine, state, cv), mutex_id(engine, state, m))
            else {
                return ret_int(engine, EINVAL);
            };
            let Some(mx) = state.pthread().mutexes.get_mut(&mutex) else {
                return ret_int(engine, EINVAL);
            };
            let depth = match mx.release_all(me) {
                Ok(d) => d,
                Err(code) => return ret_int(engine, code),
            };
            mx.queue.wake();

            let Some(c) = state.pthread().conds.get_mut(&cond) else {
                return ret_int(engine, EINVAL);
            };
            c.waiters.push(CondWaiter {
                pt: me,
                signaled: false,
            });
            let queue = Arc::clone(&c.queue);
            state.pthread().pending.insert(
                tid,
                PtPending::CondWait {
                    cond,
                    mutex,
                    depth,
                    deadline,
                },
            );
            Err(park_on(state, &queue, slice_until(deadline)))
        }
    }
}

/// Re-take the mutex a `pthread_cond_wait` released, then return `result`.
fn cond_reacquire(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    mutex: u64,
    depth: u32,
    result: i32,
) -> Result<WinApiHandlerResult> {
    let tid = state.kernel.threads.current_tid();
    let me = self_pt(engine, state);
    let Some(mx) = state.pthread().mutexes.get_mut(&mutex) else {
        state.pthread().pending.remove(&tid);
        return ret_int(engine, EINVAL);
    };
    if mx.owner.is_some() && mx.owner != Some(me) {
        let queue = Arc::clone(&mx.queue);
        return Err(park_on(state, &queue, slice_until(None)));
    }
    mx.owner = Some(me);
    mx.depth = depth.max(1);
    state.pthread().pending.remove(&tid);
    ret_int(engine, result)
}
