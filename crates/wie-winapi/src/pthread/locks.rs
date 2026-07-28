//! Mutexes, condition variables, rwlocks, spinlocks, barriers, and semaphores.
//!
//! Every blocking entry point is written as a *retry loop across re-entries*:
//! it re-reads the object, and either completes or queues a park and returns
//! [`crate::WinApiControlSignal::HostPark`]. The runtime sleeps with no process
//! locks held and then re-enters the handler at the same RIP.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use wie_cpu::CpuEngine;

use super::objects::{CondWaiter, PtBarrier, PtCond, PtMutex, PtRwLock, PtSem, PtSpin};
use super::{
    BARRIER_SERIAL_THREAD, EAGAIN, EBUSY, EEXIST, EINVAL, ENOENT, EPERM, ETIMEDOUT, MUTEX_ERRORCHECK,
    MUTEX_NORMAL, MUTEX_RECURSIVE, PROCESS_SHARED, PtPending, absolute_deadline, is_pt_id,
    park_on, read_cstr, read_u32, read_u64, relative_deadline, ret_errno, ret_int, ret_u64,
    slice_until, stack_arg, trunc_i32, write_i32, write_u32, write_u64,
};
use super::threads::{cancellation_point, self_pt};
use crate::{WinApiHandlerResult, WinApiState};

/// Dispatch the synchronisation exports. `None` if unrecognised.
#[expect(clippy::too_many_lines, reason = "flat export table; one arm per name")]
pub(super) fn dispatch(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let r = match name {
        // ── mutex ──────────────────────────────────────────────────────
        "pthread_mutex_init" => mutex_init(engine, state)?,
        "pthread_mutex_destroy" => mutex_destroy(engine, state)?,
        "pthread_mutex_lock" => mutex_lock(engine, state, None, false)?,
        "pthread_mutex_trylock" => mutex_lock(engine, state, None, true)?,
        "pthread_mutex_timedlock" | "pthread_mutex_timedlock64" => {
            let ts = engine.read_rdx()?;
            let deadline = absolute_deadline(engine, ts, true);
            mutex_lock(engine, state, deadline, false)?
        }
        "pthread_mutex_timedlock32" => {
            let ts = engine.read_rdx()?;
            let deadline = absolute_deadline(engine, ts, false);
            mutex_lock(engine, state, deadline, false)?
        }
        "pthread_mutex_unlock" => mutex_unlock(engine, state)?,

        // ── mutex attributes (a 4-byte bitfield in the guest) ──────────
        "pthread_mutexattr_init" => {
            let a = engine.read_rcx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_u32(engine, a, 0);
                ret_int(engine, 0)?
            }
        }
        "pthread_mutexattr_destroy" => {
            let a = engine.read_rcx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_u32(engine, a, 0);
                ret_int(engine, 0)?
            }
        }
        "pthread_mutexattr_settype" => {
            let a = engine.read_rcx()?;
            let kind = trunc_i32(engine.read_rdx()?);
            if a == 0 || !matches!(kind, MUTEX_NORMAL | MUTEX_ERRORCHECK | MUTEX_RECURSIVE) {
                ret_int(engine, EINVAL)?
            } else {
                let cur = read_u32(engine, a);
                write_u32(engine, a, (cur & !0x3) | kind.cast_unsigned());
                ret_int(engine, 0)?
            }
        }
        "pthread_mutexattr_gettype" => {
            let a = engine.read_rcx()?;
            let out = engine.read_rdx()?;
            if a == 0 || out == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let val = read_u32(engine, a) & 0x3;
                write_u32(engine, out, val);
                ret_int(engine, 0)?
            }
        }
        "pthread_mutexattr_setpshared" => {
            let a = engine.read_rcx()?;
            let shared = trunc_i32(engine.read_rdx()?);
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let cur = read_u32(engine, a);
                let bit = if shared == PROCESS_SHARED { 0x4 } else { 0 };
                write_u32(engine, a, (cur & !0x4) | bit);
                ret_int(engine, 0)?
            }
        }
        "pthread_mutexattr_getpshared" => {
            let a = engine.read_rcx()?;
            let out = engine.read_rdx()?;
            if a == 0 || out == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let shared = i32::from(read_u32(engine, a) & 0x4 != 0);
                write_i32(engine, out, shared);
                ret_int(engine, 0)?
            }
        }
        // Priority protocols need a real-time scheduler Windows does not have.
        "pthread_mutexattr_setprotocol" => {
            let proto = trunc_i32(engine.read_rdx()?);
            ret_int(engine, if proto == 0 { 0 } else { super::ENOTSUP })?
        }
        "pthread_mutexattr_getprotocol" => {
            let rdx = engine.read_rdx()?;
            write_i32(engine, rdx, 0);
            ret_int(engine, 0)?
        }
        "pthread_mutexattr_setprioceiling" | "pthread_mutexattr_getprioceiling" => {
            ret_int(engine, super::ENOTSUP)?
        }

        // ── condition variable ─────────────────────────────────────────
        "pthread_cond_init" => {
            let cv = engine.read_rcx()?;
            if cv == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let id = state.pthread.alloc_id();
                state.pthread.conds.insert(id, PtCond::new());
                write_u64(engine, cv, id);
                ret_int(engine, 0)?
            }
        }
        "pthread_cond_destroy" => {
            let cv = engine.read_rcx()?;
            match cond_id(engine, state, cv) {
                None => ret_int(engine, EINVAL)?,
                Some(id) => {
                    if state
                        .pthread
                        .conds
                        .get(&id)
                        .is_some_and(|c| !c.waiters.is_empty())
                    {
                        ret_int(engine, EBUSY)?
                    } else {
                        state.pthread.conds.remove(&id);
                        write_u64(engine, cv, 0);
                        ret_int(engine, 0)?
                    }
                }
            }
        }
        "pthread_cond_signal" => cond_wake(engine, state, false)?,
        "pthread_cond_broadcast" => cond_wake(engine, state, true)?,
        "pthread_cond_wait" => cond_wait(engine, state, DeadlineSource::None)?,
        "pthread_cond_timedwait" | "pthread_cond_timedwait64" => {
            cond_wait(engine, state, DeadlineSource::Absolute { bits64: true })?
        }
        "pthread_cond_timedwait32" => {
            cond_wait(engine, state, DeadlineSource::Absolute { bits64: false })?
        }
        "pthread_cond_timedwait_relative_np" | "pthread_cond_timedwait64_relative_np" => {
            cond_wait(engine, state, DeadlineSource::Relative { bits64: true })?
        }
        "pthread_cond_timedwait32_relative_np" => {
            cond_wait(engine, state, DeadlineSource::Relative { bits64: false })?
        }

        // ── condition variable attributes ──────────────────────────────
        "pthread_condattr_init" | "pthread_condattr_destroy" => {
            let a = engine.read_rcx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_u32(engine, a, 0);
                ret_int(engine, 0)?
            }
        }
        "pthread_condattr_setpshared" => {
            let a = engine.read_rcx()?;
            let shared = trunc_i32(engine.read_rdx()?);
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_i32(engine, a, shared);
                ret_int(engine, 0)?
            }
        }
        "pthread_condattr_getpshared" => {
            let a = engine.read_rcx()?;
            let out = engine.read_rdx()?;
            if a == 0 || out == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let v = read_u32(engine, a);
                write_u32(engine, out, v);
                ret_int(engine, 0)?
            }
        }
        // Both clocks are served from the same host time source.
        "pthread_condattr_setclock" => ret_int(engine, 0)?,
        "pthread_condattr_getclock" => {
            let rdx = engine.read_rdx()?;
            write_i32(engine, rdx, 0);
            ret_int(engine, 0)?
        }

        // ── rwlock ─────────────────────────────────────────────────────
        "pthread_rwlock_init" => {
            let l = engine.read_rcx()?;
            if l == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let id = state.pthread.alloc_id();
                state.pthread.rwlocks.insert(id, PtRwLock::new());
                write_u64(engine, l, id);
                ret_int(engine, 0)?
            }
        }
        "pthread_rwlock_destroy" => {
            let l = engine.read_rcx()?;
            match rwlock_id(engine, state, l) {
                None => ret_int(engine, EINVAL)?,
                Some(id) => {
                    if state.pthread.rwlocks.get(&id).is_some_and(PtRwLock::is_held) {
                        ret_int(engine, EBUSY)?
                    } else {
                        state.pthread.rwlocks.remove(&id);
                        write_u64(engine, l, 0);
                        ret_int(engine, 0)?
                    }
                }
            }
        }
        "pthread_rwlock_rdlock" => rwlock_acquire(engine, state, false, None, false)?,
        "pthread_rwlock_tryrdlock" => rwlock_acquire(engine, state, false, None, true)?,
        "pthread_rwlock_wrlock" => rwlock_acquire(engine, state, true, None, false)?,
        "pthread_rwlock_trywrlock" => rwlock_acquire(engine, state, true, None, true)?,
        "pthread_rwlock_timedrdlock" | "pthread_rwlock_timedrdlock64" => {
            let ts = engine.read_rdx()?;
            let d = absolute_deadline(engine, ts, true);
            rwlock_acquire(engine, state, false, d, false)?
        }
        "pthread_rwlock_timedrdlock32" => {
            let ts = engine.read_rdx()?;
            let d = absolute_deadline(engine, ts, false);
            rwlock_acquire(engine, state, false, d, false)?
        }
        "pthread_rwlock_timedwrlock" | "pthread_rwlock_timedwrlock64" => {
            let ts = engine.read_rdx()?;
            let d = absolute_deadline(engine, ts, true);
            rwlock_acquire(engine, state, true, d, false)?
        }
        "pthread_rwlock_timedwrlock32" => {
            let ts = engine.read_rdx()?;
            let d = absolute_deadline(engine, ts, false);
            rwlock_acquire(engine, state, true, d, false)?
        }
        "pthread_rwlock_unlock" => rwlock_unlock(engine, state)?,

        "pthread_rwlockattr_init" | "pthread_rwlockattr_destroy" => {
            let a = engine.read_rcx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_u32(engine, a, 0);
                ret_int(engine, 0)?
            }
        }
        "pthread_rwlockattr_setpshared" => {
            let a = engine.read_rcx()?;
            let shared = trunc_i32(engine.read_rdx()?);
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_i32(engine, a, shared);
                ret_int(engine, 0)?
            }
        }
        "pthread_rwlockattr_getpshared" => {
            let a = engine.read_rcx()?;
            let out = engine.read_rdx()?;
            if a == 0 || out == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let v = read_u32(engine, a);
                write_u32(engine, out, v);
                ret_int(engine, 0)?
            }
        }

        // ── spinlock ───────────────────────────────────────────────────
        "pthread_spin_init" => {
            let l = engine.read_rcx()?;
            if l == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let id = state.pthread.alloc_id();
                state.pthread.spins.insert(id, PtSpin::new());
                write_u64(engine, l, id);
                ret_int(engine, 0)?
            }
        }
        "pthread_spin_destroy" => {
            let l = engine.read_rcx()?;
            match spin_id(engine, state, l) {
                None => ret_int(engine, EINVAL)?,
                Some(id) => {
                    state.pthread.spins.remove(&id);
                    write_u64(engine, l, 0);
                    ret_int(engine, 0)?
                }
            }
        }
        "pthread_spin_lock" => spin_lock(engine, state, false)?,
        "pthread_spin_trylock" => spin_lock(engine, state, true)?,
        "pthread_spin_unlock" => spin_unlock(engine, state)?,

        // ── barrier ────────────────────────────────────────────────────
        "pthread_barrier_init" => {
            let b = engine.read_rcx()?;
            let count = u32::try_from(engine.read_r8()? & 0xffff_ffff).unwrap_or(0);
            if b == 0 || count == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let id = state.pthread.alloc_id();
                state.pthread.barriers.insert(id, PtBarrier::new(count));
                write_u64(engine, b, id);
                ret_int(engine, 0)?
            }
        }
        "pthread_barrier_destroy" => {
            let b = engine.read_rcx()?;
            let word = read_u64(engine, b);
            match state.pthread.barriers.get(&word) {
                None => ret_int(engine, EINVAL)?,
                Some(bar) if bar.arrived > 0 => ret_int(engine, EBUSY)?,
                Some(_) => {
                    state.pthread.barriers.remove(&word);
                    write_u64(engine, b, 0);
                    ret_int(engine, 0)?
                }
            }
        }
        "pthread_barrier_wait" => barrier_wait(engine, state)?,
        "pthread_barrierattr_init" | "pthread_barrierattr_destroy" => {
            let a = engine.read_rcx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_u64(engine, a, 0);
                ret_int(engine, 0)?
            }
        }
        "pthread_barrierattr_setpshared" => {
            let a = engine.read_rcx()?;
            let shared = engine.read_rdx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_u64(engine, a, shared);
                ret_int(engine, 0)?
            }
        }
        "pthread_barrierattr_getpshared" => {
            let a = engine.read_rcx()?;
            let out = engine.read_rdx()?;
            if a == 0 || out == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let v = u32::try_from(read_u64(engine, a) & 0xffff_ffff).unwrap_or(0);
                write_u32(engine, out, v);
                ret_int(engine, 0)?
            }
        }

        // ── semaphores ─────────────────────────────────────────────────
        "sem_init" => sem_init(engine, state)?,
        "sem_destroy" => sem_destroy(engine, state)?,
        "sem_wait" => sem_wait(engine, state, None, false)?,
        "sem_trywait" => sem_wait(engine, state, None, true)?,
        "sem_timedwait" | "sem_timedwait64" => {
            let ts = engine.read_rdx()?;
            let d = absolute_deadline(engine, ts, true);
            sem_wait(engine, state, d, false)?
        }
        "sem_timedwait32" => {
            let ts = engine.read_rdx()?;
            let d = absolute_deadline(engine, ts, false);
            sem_wait(engine, state, d, false)?
        }
        "sem_post" => sem_post(engine, state, 1)?,
        "sem_post_multiple" => {
            let n = trunc_i32(engine.read_rdx()?);
            sem_post(engine, state, n)?
        }
        "sem_getvalue" => {
            let s = engine.read_rcx()?;
            let out = engine.read_rdx()?;
            match sem_lookup(engine, state, s) {
                None => ret_errno(engine, EINVAL)?,
                Some(id) => {
                    let v = state.pthread.sems.get(&id).map_or(0, |x| x.count);
                    write_i32(engine, out, v);
                    ret_int(engine, 0)?
                }
            }
        }
        "sem_open" => sem_open(engine, state)?,
        "sem_close" => sem_close(engine, state)?,
        "sem_unlink" => sem_unlink(engine, state)?,

        _ => return Ok(None),
    };
    Ok(Some(r))
}

// ── Handle resolution ──────────────────────────────────────────────────

/// Resolve a `pthread_mutex_t *`, promoting a static initialiser on first use.
///
/// `PTHREAD_MUTEX_INITIALIZER` and friends are the negative sentinels −1/−2/−3
/// from `pthread.h`; a zeroed word is also accepted as a default mutex, which
/// is what memset-initialised structs give and what glibc allows.
fn mutex_id(engine: &mut dyn CpuEngine, state: &mut WinApiState, va: u64) -> Option<u64> {
    if va == 0 {
        return None;
    }
    let word = read_u64(engine, va);
    if is_pt_id(word) && state.pthread.mutexes.contains_key(&word) {
        return Some(word);
    }
    let kind = match word.cast_signed() {
        0 | -1 => MUTEX_NORMAL,
        -2 => MUTEX_ERRORCHECK,
        -3 => MUTEX_RECURSIVE,
        _ => return None,
    };
    let id = state.pthread.alloc_id();
    state.pthread.mutexes.insert(id, PtMutex::new(kind));
    write_u64(engine, va, id);
    Some(id)
}

/// Resolve a `pthread_cond_t *`, promoting `PTHREAD_COND_INITIALIZER`.
fn cond_id(engine: &mut dyn CpuEngine, state: &mut WinApiState, va: u64) -> Option<u64> {
    if va == 0 {
        return None;
    }
    let word = read_u64(engine, va);
    if is_pt_id(word) && state.pthread.conds.contains_key(&word) {
        return Some(word);
    }
    if !matches!(word.cast_signed(), 0 | -1) {
        return None;
    }
    let id = state.pthread.alloc_id();
    state.pthread.conds.insert(id, PtCond::new());
    write_u64(engine, va, id);
    Some(id)
}

/// Resolve a `pthread_rwlock_t *`, promoting `PTHREAD_RWLOCK_INITIALIZER`.
fn rwlock_id(engine: &mut dyn CpuEngine, state: &mut WinApiState, va: u64) -> Option<u64> {
    if va == 0 {
        return None;
    }
    let word = read_u64(engine, va);
    if is_pt_id(word) && state.pthread.rwlocks.contains_key(&word) {
        return Some(word);
    }
    if !matches!(word.cast_signed(), 0 | -1) {
        return None;
    }
    let id = state.pthread.alloc_id();
    state.pthread.rwlocks.insert(id, PtRwLock::new());
    write_u64(engine, va, id);
    Some(id)
}

/// Resolve a `pthread_spinlock_t *`, promoting `PTHREAD_SPINLOCK_INITIALIZER`.
fn spin_id(engine: &mut dyn CpuEngine, state: &mut WinApiState, va: u64) -> Option<u64> {
    if va == 0 {
        return None;
    }
    let word = read_u64(engine, va);
    if is_pt_id(word) && state.pthread.spins.contains_key(&word) {
        return Some(word);
    }
    if !matches!(word.cast_signed(), 0 | -1) {
        return None;
    }
    let id = state.pthread.alloc_id();
    state.pthread.spins.insert(id, PtSpin::new());
    write_u64(engine, va, id);
    Some(id)
}

/// Resolve a `sem_t *`. Semaphores have no static initialiser.
fn sem_lookup(engine: &mut dyn CpuEngine, state: &WinApiState, va: u64) -> Option<u64> {
    if va == 0 {
        return None;
    }
    let word = read_u64(engine, va);
    if state.pthread.sems.contains_key(&word) {
        Some(word)
    } else {
        None
    }
}

// ── Mutex ──────────────────────────────────────────────────────────────

/// `pthread_mutex_init(m, attr)`.
fn mutex_init(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let m = engine.read_rcx()?;
    let attr = engine.read_rdx()?;
    if m == 0 {
        return ret_int(engine, EINVAL);
    }
    let kind = if attr == 0 {
        MUTEX_NORMAL
    } else {
        (read_u32(engine, attr) & 0x3).cast_signed()
    };
    let id = state.pthread.alloc_id();
    state.pthread.mutexes.insert(id, PtMutex::new(kind));
    write_u64(engine, m, id);
    ret_int(engine, 0)
}

/// `pthread_mutex_destroy(m)`.
fn mutex_destroy(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let m = engine.read_rcx()?;
    if m == 0 {
        return ret_int(engine, EINVAL);
    }
    let word = read_u64(engine, m);
    // Destroying a never-locked static initialiser is a no-op, not an error.
    if !is_pt_id(word) {
        return ret_int(engine, if matches!(word.cast_signed(), 0 | -1 | -2 | -3) { 0 } else { EINVAL });
    }
    match state.pthread.mutexes.get(&word) {
        None => ret_int(engine, EINVAL),
        Some(mx) if mx.is_held() => ret_int(engine, EBUSY),
        Some(_) => {
            state.pthread.mutexes.remove(&word);
            write_u64(engine, m, 0);
            ret_int(engine, 0)
        }
    }
}

/// `pthread_mutex_lock` / `_trylock` / `_timedlock`.
fn mutex_lock(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    deadline: Option<Instant>,
    try_only: bool,
) -> Result<WinApiHandlerResult> {
    let m = engine.read_rcx()?;
    let Some(id) = mutex_id(engine, state, m) else {
        return ret_int(engine, EINVAL);
    };
    let me = self_pt(engine, state);
    let Some(mx) = state.pthread.mutexes.get_mut(&id) else {
        return ret_int(engine, EINVAL);
    };
    match mx.try_acquire(me) {
        Err(code) => ret_int(engine, code),
        Ok(true) => ret_int(engine, 0),
        Ok(false) => {
            if try_only {
                return ret_int(engine, EBUSY);
            }
            if let Some(dl) = deadline
                && Instant::now() >= dl
            {
                return ret_int(engine, ETIMEDOUT);
            }
            let queue = Arc::clone(&mx.queue);
            Err(park_on(state, &queue, slice_until(deadline)))
        }
    }
}

/// `pthread_mutex_unlock(m)`.
fn mutex_unlock(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let m = engine.read_rcx()?;
    let Some(id) = mutex_id(engine, state, m) else {
        return ret_int(engine, EINVAL);
    };
    let me = self_pt(engine, state);
    let Some(mx) = state.pthread.mutexes.get_mut(&id) else {
        return ret_int(engine, EINVAL);
    };
    match mx.release(me) {
        Err(code) => ret_int(engine, code),
        Ok(freed) => {
            if freed {
                mx.queue.wake();
            }
            ret_int(engine, 0)
        }
    }
}

// ── Condition variable ─────────────────────────────────────────────────

/// Where a `pthread_cond_*wait` variant reads its deadline from.
#[derive(Debug, Clone, Copy)]
enum DeadlineSource {
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
fn cond_wake(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    all: bool,
) -> Result<WinApiHandlerResult> {
    let cv = engine.read_rcx()?;
    let Some(id) = cond_id(engine, state, cv) else {
        return ret_int(engine, EINVAL);
    };
    if let Some(c) = state.pthread.conds.get_mut(&id) {
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
fn cond_wait(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    source: DeadlineSource,
) -> Result<WinApiHandlerResult> {
    if let Some(r) = cancellation_point(engine, state)? {
        return Ok(r);
    }
    let tid = state.kernel.threads.current_tid();
    let me = self_pt(engine, state);

    match state.pthread.pending.get(&tid).cloned() {
        // Phase 2: parked, waiting for a signal or the deadline.
        Some(PtPending::CondWait {
            cond,
            mutex,
            depth,
            deadline,
        }) => {
            let signaled = state
                .pthread
                .conds
                .get(&cond)
                .is_some_and(|c| c.is_signaled(me));
            let expired = deadline.is_some_and(|dl| Instant::now() >= dl);
            if !signaled && !expired {
                let Some(c) = state.pthread.conds.get(&cond) else {
                    state.pthread.pending.remove(&tid);
                    return ret_int(engine, EINVAL);
                };
                let queue = Arc::clone(&c.queue);
                return Err(park_on(state, &queue, slice_until(deadline)));
            }
            if let Some(c) = state.pthread.conds.get_mut(&cond) {
                c.remove(me);
            }
            let result = if signaled { 0 } else { ETIMEDOUT };
            state.pthread.pending.insert(
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
            let (Some(cond), Some(mutex)) = (
                cond_id(engine, state, cv),
                mutex_id(engine, state, m),
            ) else {
                return ret_int(engine, EINVAL);
            };
            let Some(mx) = state.pthread.mutexes.get_mut(&mutex) else {
                return ret_int(engine, EINVAL);
            };
            let depth = match mx.release_all(me) {
                Ok(d) => d,
                Err(code) => return ret_int(engine, code),
            };
            mx.queue.wake();

            let Some(c) = state.pthread.conds.get_mut(&cond) else {
                return ret_int(engine, EINVAL);
            };
            c.waiters.push(CondWaiter {
                pt: me,
                signaled: false,
            });
            let queue = Arc::clone(&c.queue);
            state.pthread.pending.insert(
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
    let Some(mx) = state.pthread.mutexes.get_mut(&mutex) else {
        state.pthread.pending.remove(&tid);
        return ret_int(engine, EINVAL);
    };
    if mx.owner.is_some() && mx.owner != Some(me) {
        let queue = Arc::clone(&mx.queue);
        return Err(park_on(state, &queue, slice_until(None)));
    }
    mx.owner = Some(me);
    mx.depth = depth.max(1);
    state.pthread.pending.remove(&tid);
    ret_int(engine, result)
}

// ── Read/write lock ────────────────────────────────────────────────────

/// `pthread_rwlock_{rd,wr,tryrd,trywr,timedrd,timedwr}lock`.
fn rwlock_acquire(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    write: bool,
    deadline: Option<Instant>,
    try_only: bool,
) -> Result<WinApiHandlerResult> {
    let l = engine.read_rcx()?;
    let Some(id) = rwlock_id(engine, state, l) else {
        return ret_int(engine, EINVAL);
    };
    let me = self_pt(engine, state);
    let Some(rw) = state.pthread.rwlocks.get_mut(&id) else {
        return ret_int(engine, EINVAL);
    };
    let got = if write { rw.try_write(me) } else { rw.try_read() };
    if got {
        return ret_int(engine, 0);
    }
    if try_only {
        return ret_int(engine, EBUSY);
    }
    if let Some(dl) = deadline
        && Instant::now() >= dl
    {
        return ret_int(engine, ETIMEDOUT);
    }
    let queue = Arc::clone(&rw.queue);
    Err(park_on(state, &queue, slice_until(deadline)))
}

/// `pthread_rwlock_unlock(l)`.
fn rwlock_unlock(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let l = engine.read_rcx()?;
    let Some(id) = rwlock_id(engine, state, l) else {
        return ret_int(engine, EINVAL);
    };
    let me = self_pt(engine, state);
    let Some(rw) = state.pthread.rwlocks.get_mut(&id) else {
        return ret_int(engine, EINVAL);
    };
    match rw.unlock(me) {
        Err(code) => ret_int(engine, code),
        Ok(()) => {
            rw.queue.wake();
            ret_int(engine, 0)
        }
    }
}

// ── Spinlock ───────────────────────────────────────────────────────────

/// `pthread_spin_lock` / `pthread_spin_trylock`.
fn spin_lock(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    try_only: bool,
) -> Result<WinApiHandlerResult> {
    let l = engine.read_rcx()?;
    let Some(id) = spin_id(engine, state, l) else {
        return ret_int(engine, EINVAL);
    };
    let me = self_pt(engine, state);
    let Some(sp) = state.pthread.spins.get_mut(&id) else {
        return ret_int(engine, EINVAL);
    };
    if sp.owner.is_none() {
        sp.owner = Some(me);
        return ret_int(engine, 0);
    }
    if try_only {
        return ret_int(engine, EBUSY);
    }
    let queue = Arc::clone(&sp.queue);
    Err(park_on(state, &queue, slice_until(None)))
}

/// `pthread_spin_unlock(l)`.
fn spin_unlock(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let l = engine.read_rcx()?;
    let Some(id) = spin_id(engine, state, l) else {
        return ret_int(engine, EINVAL);
    };
    let me = self_pt(engine, state);
    let Some(sp) = state.pthread.spins.get_mut(&id) else {
        return ret_int(engine, EINVAL);
    };
    if sp.owner != Some(me) {
        return ret_int(engine, EPERM);
    }
    sp.owner = None;
    sp.queue.wake();
    ret_int(engine, 0)
}

// ── Barrier ────────────────────────────────────────────────────────────

/// `pthread_barrier_wait(b)`.
///
/// Returns [`BARRIER_SERIAL_THREAD`] to exactly one thread per generation and
/// `0` to the rest, as POSIX requires.
fn barrier_wait(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    if let Some(r) = cancellation_point(engine, state)? {
        return Ok(r);
    }
    let tid = state.kernel.threads.current_tid();

    // Re-entry: did our generation trip while we were parked?
    if let Some(PtPending::Barrier {
        barrier,
        generation,
    }) = state.pthread.pending.get(&tid).cloned()
    {
        let Some(bar) = state.pthread.barriers.get(&barrier) else {
            state.pthread.pending.remove(&tid);
            return ret_int(engine, EINVAL);
        };
        if bar.generation != generation {
            state.pthread.pending.remove(&tid);
            return ret_int(engine, 0);
        }
        let queue = Arc::clone(&bar.queue);
        return Err(park_on(state, &queue, slice_until(None)));
    }

    let b = engine.read_rcx()?;
    let word = read_u64(engine, b);
    let Some(bar) = state.pthread.barriers.get_mut(&word) else {
        return ret_int(engine, EINVAL);
    };
    bar.arrived = bar.arrived.saturating_add(1);
    if bar.arrived >= bar.count {
        bar.arrived = 0;
        bar.generation = bar.generation.wrapping_add(1);
        bar.queue.wake();
        return ret_int(engine, BARRIER_SERIAL_THREAD);
    }
    let generation = bar.generation;
    let queue = Arc::clone(&bar.queue);
    state.pthread.pending.insert(
        tid,
        PtPending::Barrier {
            barrier: word,
            generation,
        },
    );
    Err(park_on(state, &queue, slice_until(None)))
}

// ── Semaphores ─────────────────────────────────────────────────────────

/// `sem_init(sem, pshared, value)`.
fn sem_init(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let s = engine.read_rcx()?;
    let value = u32::try_from(engine.read_r8()? & 0xffff_ffff).unwrap_or(0);
    let Ok(count) = i32::try_from(value) else {
        return ret_errno(engine, EINVAL);
    };
    if s == 0 {
        return ret_errno(engine, EINVAL);
    }
    let id = state.pthread.alloc_id();
    state.pthread.sems.insert(id, PtSem::new(count, None));
    write_u64(engine, s, id);
    ret_int(engine, 0)
}

/// `sem_destroy(sem)`.
fn sem_destroy(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let s = engine.read_rcx()?;
    let Some(id) = sem_lookup(engine, state, s) else {
        return ret_errno(engine, EINVAL);
    };
    state.pthread.sems.remove(&id);
    write_u64(engine, s, 0);
    ret_int(engine, 0)
}

/// `sem_wait` / `sem_trywait` / `sem_timedwait`.
fn sem_wait(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    deadline: Option<Instant>,
    try_only: bool,
) -> Result<WinApiHandlerResult> {
    if let Some(r) = cancellation_point(engine, state)? {
        return Ok(r);
    }
    let s = engine.read_rcx()?;
    let Some(id) = sem_lookup(engine, state, s) else {
        return ret_errno(engine, EINVAL);
    };
    let Some(sem) = state.pthread.sems.get_mut(&id) else {
        return ret_errno(engine, EINVAL);
    };
    if sem.count > 0 {
        sem.count = sem.count.saturating_sub(1);
        return ret_int(engine, 0);
    }
    if try_only {
        return ret_errno(engine, EAGAIN);
    }
    if let Some(dl) = deadline
        && Instant::now() >= dl
    {
        return ret_errno(engine, ETIMEDOUT);
    }
    let queue = Arc::clone(&sem.queue);
    Err(park_on(state, &queue, slice_until(deadline)))
}

/// `sem_post(sem)` / `sem_post_multiple(sem, count)`.
fn sem_post(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    count: i32,
) -> Result<WinApiHandlerResult> {
    if count <= 0 {
        return ret_errno(engine, EINVAL);
    }
    let s = engine.read_rcx()?;
    let Some(id) = sem_lookup(engine, state, s) else {
        return ret_errno(engine, EINVAL);
    };
    let Some(sem) = state.pthread.sems.get_mut(&id) else {
        return ret_errno(engine, EINVAL);
    };
    let Some(next) = sem.count.checked_add(count) else {
        return ret_errno(engine, super::EOVERFLOW);
    };
    sem.count = next;
    sem.queue.wake();
    ret_int(engine, 0)
}

/// `sem_open(name, oflag, mode, value)`.
fn sem_open(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    const O_CREAT: i32 = 0x0100;
    const O_EXCL: i32 = 0x0400;
    let name_va = engine.read_rcx()?;
    let oflag = trunc_i32(engine.read_rdx()?);
    let value = u32::try_from(engine.read_r9()? & 0xffff_ffff).unwrap_or(0);
    let name = read_cstr(engine, name_va);
    if name.is_empty() {
        set_errno_failed(engine, EINVAL);
        return ret_u64(engine, 0);
    }

    if let Some(&id) = state.pthread.named_sems.get(&name) {
        if oflag & O_CREAT != 0 && oflag & O_EXCL != 0 {
            set_errno_failed(engine, EEXIST);
            return ret_u64(engine, 0);
        }
        if let Some(sem) = state.pthread.sems.get_mut(&id) {
            sem.refs = sem.refs.saturating_add(1);
        }
        return ret_u64(engine, id);
    }

    if oflag & O_CREAT == 0 {
        set_errno_failed(engine, ENOENT);
        return ret_u64(engine, 0);
    }
    let Ok(count) = i32::try_from(value) else {
        set_errno_failed(engine, EINVAL);
        return ret_u64(engine, 0);
    };
    let id = state.pthread.alloc_id();
    state
        .pthread
        .sems
        .insert(id, PtSem::new(count, Some(name.clone())));
    state.pthread.named_sems.insert(name, id);
    ret_u64(engine, id)
}

/// `sem_close(sem)`.
fn sem_close(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let id = engine.read_rcx()?;
    let Some(sem) = state.pthread.sems.get_mut(&id) else {
        return ret_errno(engine, EINVAL);
    };
    sem.refs = sem.refs.saturating_sub(1);
    if sem.refs == 0 && sem.unlinked {
        state.pthread.sems.remove(&id);
    }
    ret_int(engine, 0)
}

/// `sem_unlink(name)`.
fn sem_unlink(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let rcx = engine.read_rcx()?;
    let name = read_cstr(engine, rcx);
    let Some(id) = state.pthread.named_sems.remove(&name) else {
        return ret_errno(engine, ENOENT);
    };
    let drop_now = match state.pthread.sems.get_mut(&id) {
        Some(sem) => {
            sem.unlinked = true;
            sem.refs == 0
        }
        None => false,
    };
    if drop_now {
        state.pthread.sems.remove(&id);
    }
    ret_int(engine, 0)
}

/// `sem_open` reports failure as `SEM_FAILED` (null) plus `errno`.
fn set_errno_failed(engine: &mut dyn CpuEngine, code: i32) {
    super::set_errno(engine, code);
}

/// Keep the shared stack-argument helper reachable from this module.
const _: fn(&mut dyn CpuEngine, u64) -> u64 = stack_arg;
