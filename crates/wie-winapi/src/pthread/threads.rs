//! Thread lifecycle, attributes, thread-specific data, once-control, and
//! cancellation.

#![allow(clippy::match_same_arms, clippy::single_match_else)]

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;

use super::objects::PtThread;
use super::{
    BARRIER_SERIAL_THREAD, CANCEL_ASYNCHRONOUS, CANCEL_ENABLE, CANCELED, CREATE_DETACHED, EAGAIN,
    EDEADLK, EINVAL, ENOTSUP, EPERM, ESRCH, INHERIT_SCHED, PtPending, SCOPE_SYSTEM, call_guest,
    finish_guest_call, park_on, read_cstr, read_u32, read_u64, ret_int, ret_u64, slice_until,
    trunc_i32, write_i32, write_u32, write_u64,
};
use crate::{WinApiHandlerResult, WinApiState};

/// `struct pthread_attr_t` field offsets (`pthread.h`).
mod attr {
    /// `unsigned p_state` — detach / inherit-sched / scope bits.
    pub const P_STATE: u64 = 0;
    /// `void *stack`.
    pub const STACK: u64 = 8;
    /// `size_t s_size`.
    pub const S_SIZE: u64 = 16;
    /// `struct sched_param param` (`int sched_priority`).
    pub const PARAM: u64 = 24;
    /// Total size, including tail padding.
    pub const SIZE: usize = 32;
}

/// Maximum thread-specific-data destructor passes.
///
/// POSIX requires at least `_POSIX_THREAD_DESTRUCTOR_ITERATIONS` (4); mingw
/// advertises 256, which would let a misbehaving destructor spin the shutdown
/// path. 8 keeps the common "destructor re-sets its own key once" idiom working
/// without an unbounded loop.
const DESTRUCTOR_PASSES: u32 = 8;

/// Scratch carved below the current frame for guest calls made from a handler.
const CALL_FRAME_GAP: u64 = 0x200;

/// Dispatch the thread / TSD / once / cancel exports. `None` if unrecognised.
#[expect(clippy::too_many_lines, reason = "flat export table; one arm per name")]
pub(super) fn dispatch(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let r = match name {
        // ── lifecycle ──────────────────────────────────────────────────
        "pthread_create" => create(engine, state)?,
        "pthread_join" => join(engine, state, true)?,
        "pthread_tryjoin" => join(engine, state, false)?,
        "pthread_detach" => detach(engine, state)?,
        "pthread_exit" => exit(engine, state)?,
        "pthread_self" => {
            let pt = self_pt(engine, state);
            ret_u64(engine, pt)?
        }
        "pthread_equal" => {
            let a = engine.read_rcx()?;
            let b = engine.read_rdx()?;
            ret_int(engine, i32::from(a != 0 && a == b))?
        }
        "pthread_gethandle" => {
            let pt = engine.read_rcx()?;
            let h = state.pthread().threads.get(&pt).map_or(0, |t| t.win_handle);
            ret_u64(engine, h)?
        }
        // winpthreads hands out a per-thread auto-reset event for cancellation.
        // WIE cancels through its own park queues, so there is no such object.
        "pthread_getevent" => ret_u64(engine, 0)?,
        "pthread_setname_np" => {
            let pt = engine.read_rcx()?;
            let name_va = engine.read_rdx()?;
            let value = read_cstr(engine, name_va);
            match state.pthread().threads.get_mut(&pt) {
                Some(t) => {
                    t.name = value;
                    ret_int(engine, 0)?
                }
                None => ret_int(engine, ESRCH)?,
            }
        }
        "pthread_getname_np" => getname(engine, state)?,
        "pthread_create_wrapper" => {
            // The real DLL's `CreateThread` shim. WIE spawns the start routine
            // directly, so nothing ever calls this; answer harmlessly.
            ret_u64(engine, 0)?
        }

        // ── attributes ─────────────────────────────────────────────────
        "pthread_attr_init" => {
            let a = engine.read_rcx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                drop(engine.mem_write(a, &[0_u8; attr::SIZE]));
                // PTHREAD_DEFAULT_ATTR == PTHREAD_CANCEL_ENABLE.
                write_u32(engine, a.wrapping_add(attr::P_STATE), 0x01);
                ret_int(engine, 0)?
            }
        }
        "pthread_attr_destroy" => {
            let a = engine.read_rcx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                drop(engine.mem_write(a, &[0_u8; attr::SIZE]));
                ret_int(engine, 0)?
            }
        }
        "pthread_attr_setdetachstate" => attr_set_bits(engine, CREATE_DETACHED)?,
        "pthread_attr_getdetachstate" => attr_get_bits(engine, CREATE_DETACHED)?,
        "pthread_attr_setinheritsched" => attr_set_bits(engine, INHERIT_SCHED)?,
        "pthread_attr_getinheritsched" => attr_get_bits(engine, INHERIT_SCHED)?,
        "pthread_attr_setscope" => attr_set_bits(engine, SCOPE_SYSTEM)?,
        "pthread_attr_getscope" => attr_get_bits(engine, SCOPE_SYSTEM)?,
        "pthread_attr_setstacksize" => {
            let a = engine.read_rcx()?;
            let size = engine.read_rdx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_u64(engine, a.wrapping_add(attr::S_SIZE), size);
                ret_int(engine, 0)?
            }
        }
        "pthread_attr_getstacksize" => {
            let a = engine.read_rcx()?;
            let out = engine.read_rdx()?;
            if a == 0 || out == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let size = read_u64(engine, a.wrapping_add(attr::S_SIZE));
                write_u64(engine, out, size);
                ret_int(engine, 0)?
            }
        }
        "pthread_attr_setstackaddr" => {
            let a = engine.read_rcx()?;
            let stack = engine.read_rdx()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_u64(engine, a.wrapping_add(attr::STACK), stack);
                ret_int(engine, 0)?
            }
        }
        "pthread_attr_getstackaddr" => {
            let a = engine.read_rcx()?;
            let out = engine.read_rdx()?;
            if a == 0 || out == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let stack = read_u64(engine, a.wrapping_add(attr::STACK));
                write_u64(engine, out, stack);
                ret_int(engine, 0)?
            }
        }
        "pthread_attr_setstack" => {
            let a = engine.read_rcx()?;
            let stack = engine.read_rdx()?;
            let size = engine.read_r8()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                write_u64(engine, a.wrapping_add(attr::STACK), stack);
                write_u64(engine, a.wrapping_add(attr::S_SIZE), size);
                ret_int(engine, 0)?
            }
        }
        "pthread_attr_getstack" => {
            let a = engine.read_rcx()?;
            let stack_out = engine.read_rdx()?;
            let size_out = engine.read_r8()?;
            if a == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let stack = read_u64(engine, a.wrapping_add(attr::STACK));
                let ssize = read_u64(engine, a.wrapping_add(attr::S_SIZE));
                write_u64(engine, stack_out, stack);
                write_u64(engine, size_out, ssize);
                ret_int(engine, 0)?
            }
        }
        "pthread_attr_setschedparam" => {
            let a = engine.read_rcx()?;
            let param = engine.read_rdx()?;
            if a == 0 || param == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let pval = read_u32(engine, param);
                write_u32(engine, a.wrapping_add(attr::PARAM), pval);
                ret_int(engine, 0)?
            }
        }
        "pthread_attr_getschedparam" => {
            let a = engine.read_rcx()?;
            let param = engine.read_rdx()?;
            if a == 0 || param == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let detach = read_u32(engine, a.wrapping_add(attr::PARAM));
                write_u32(engine, param, detach);
                ret_int(engine, 0)?
            }
        }
        // Windows exposes no POSIX scheduling policies; SCHED_OTHER only.
        "pthread_attr_setschedpolicy" => {
            let policy = trunc_i32(engine.read_rdx()?);
            ret_int(engine, if policy == 0 { 0 } else { ENOTSUP })?
        }
        "pthread_attr_getschedpolicy" => {
            let rdx = engine.read_rdx()?;
            write_i32(engine, rdx, 0);
            ret_int(engine, 0)?
        }
        "pthread_getschedparam" => {
            let pt = engine.read_rcx()?;
            let pol_out = engine.read_rdx()?;
            let param_out = engine.read_r8()?;
            match state.pthread().threads.get(&pt) {
                Some(t) => {
                    write_i32(engine, pol_out, t.policy);
                    write_i32(engine, param_out, t.priority);
                    ret_int(engine, 0)?
                }
                None => ret_int(engine, ESRCH)?,
            }
        }
        "pthread_setschedparam" => {
            let pt = engine.read_rcx()?;
            let policy = trunc_i32(engine.read_rdx()?);
            let param = engine.read_r8()?;
            let priority = trunc_i32(u64::from(read_u32(engine, param)));
            match state.pthread().threads.get_mut(&pt) {
                Some(t) => {
                    t.policy = policy;
                    t.priority = priority;
                    ret_int(engine, 0)?
                }
                None => ret_int(engine, ESRCH)?,
            }
        }
        "pthread_setschedprio" => {
            let pt = engine.read_rcx()?;
            let prio = trunc_i32(engine.read_rdx()?);
            match state.pthread().threads.get_mut(&pt) {
                Some(t) => {
                    t.priority = prio;
                    ret_int(engine, 0)?
                }
                None => ret_int(engine, ESRCH)?,
            }
        }
        "pthread_get_state" => {
            let a = engine.read_rcx()?;
            let flag = engine.read_rdx()?;
            let bits = u64::from(read_u32(engine, a.wrapping_add(attr::P_STATE)));
            ret_u64(engine, bits & flag)?
        }
        "pthread_set_state" => {
            let a = engine.read_rcx()?;
            let flag = u32::try_from(engine.read_rdx()? & 0xffff_ffff).unwrap_or(0);
            let val = u32::try_from(engine.read_r8()? & 0xffff_ffff).unwrap_or(0);
            let cur = read_u32(engine, a.wrapping_add(attr::P_STATE));
            write_u32(engine, a.wrapping_add(attr::P_STATE), (cur & !flag) | val);
            ret_int(engine, 0)?
        }

        // ── thread-specific data ───────────────────────────────────────
        "pthread_key_create" => {
            let key_out = engine.read_rcx()?;
            let dtor = engine.read_rdx()?;
            if key_out == 0 {
                ret_int(engine, EINVAL)?
            } else {
                let key = state.pthread().next_key;
                state.pthread().next_key = state.pthread().next_key.saturating_add(1);
                state.pthread().keys.insert(key, dtor);
                write_u32(engine, key_out, key);
                ret_int(engine, 0)?
            }
        }
        "pthread_key_delete" => {
            let key = u32::try_from(engine.read_rcx()? & 0xffff_ffff).unwrap_or(0);
            if state.pthread().keys.remove(&key).is_none() {
                ret_int(engine, EINVAL)?
            } else {
                for t in state.pthread().threads.values_mut() {
                    t.tls.remove(&key);
                }
                ret_int(engine, 0)?
            }
        }
        "pthread_getspecific" => {
            let key = u32::try_from(engine.read_rcx()? & 0xffff_ffff).unwrap_or(0);
            let pt = self_pt(engine, state);
            let v = state
                .pthread()
                .threads
                .get(&pt)
                .and_then(|t| t.tls.get(&key).copied())
                .unwrap_or(0);
            ret_u64(engine, v)?
        }
        "pthread_setspecific" => {
            let key = u32::try_from(engine.read_rcx()? & 0xffff_ffff).unwrap_or(0);
            let value = engine.read_rdx()?;
            let missing = !state.pthread().keys.contains_key(&key);
            if missing {
                ret_int(engine, EINVAL)?
            } else {
                let pt = self_pt(engine, state);
                if let Some(t) = state.pthread().threads.get_mut(&pt) {
                    t.tls.insert(key, value);
                }
                ret_int(engine, 0)?
            }
        }
        "pthread_tls_init" => ret_int(engine, 0)?,

        // ── cleanup chain (pthread_cleanup_push / _pop) ────────────────
        "pthread_getclean" => getclean(engine, state)?,
        "pthread_cleanup_dest" => {
            // `_pthread_cleanup_dest(t)` runs the chain for a thread that is
            // *not* exiting; WIE runs the chain from the termination path, so
            // there is nothing left to do here.
            ret_int(engine, 0)?
        }

        // ── once ───────────────────────────────────────────────────────
        "pthread_once" => once(engine, state)?,

        // ── cancellation ───────────────────────────────────────────────
        "pthread_setcancelstate" => {
            let new = trunc_i32(engine.read_rcx()?);
            let out = engine.read_rdx()?;
            let pt = self_pt(engine, state);
            if let Some(t) = state.pthread().threads.get_mut(&pt) {
                write_i32(engine, out, i32::from(t.cancel_enabled) * CANCEL_ENABLE);
                t.cancel_enabled = new & CANCEL_ENABLE != 0;
            }
            ret_int(engine, 0)?
        }
        "pthread_setcanceltype" => {
            let new = trunc_i32(engine.read_rcx()?);
            let out = engine.read_rdx()?;
            let pt = self_pt(engine, state);
            if let Some(t) = state.pthread().threads.get_mut(&pt) {
                write_i32(engine, out, i32::from(t.cancel_async) * CANCEL_ASYNCHRONOUS);
                t.cancel_async = new & CANCEL_ASYNCHRONOUS != 0;
            }
            ret_int(engine, 0)?
        }
        "pthread_cancel" => {
            let pt = engine.read_rcx()?;
            match state.pthread().threads.get_mut(&pt) {
                Some(t) if !t.finished => {
                    t.cancel_requested = true;
                    // Nudge every queue the target could be parked on so it
                    // reaches its next cancellation point promptly.
                    t.queue.wake();
                    wake_all_queues(state);
                    ret_int(engine, 0)?
                }
                Some(_) => ret_int(engine, 0)?,
                None => ret_int(engine, ESRCH)?,
            }
        }
        "pthread_testcancel" => {
            let pt = self_pt(engine, state);
            if cancel_pending(state, pt) {
                return Ok(Some(begin_termination(engine, state, CANCELED)?));
            }
            ret_u64(engine, 0)?
        }
        "pthread_shallcancel" => {
            let pt = self_pt(engine, state);
            let pending = cancel_pending(state, pt);
            ret_int(engine, i32::from(pending))?
        }
        "pthread_invoke_cancel" => begin_termination(engine, state, CANCELED)?,
        // No POSIX signals on Windows; `sig == 0` is the documented "does this
        // thread exist?" probe and is the only form winpthreads answers.
        "pthread_kill" => {
            let pt = engine.read_rcx()?;
            let sig = trunc_i32(engine.read_rdx()?);
            let alive = state
                .pthread()
                .threads
                .get(&pt)
                .is_some_and(|t| !t.finished);
            if !alive {
                ret_int(engine, ESRCH)?
            } else if sig == 0 {
                ret_int(engine, 0)?
            } else {
                ret_int(engine, ENOTSUP)?
            }
        }
        "pthread_setnobreak" => ret_int(engine, 0)?,

        _ => return Ok(None),
    };
    Ok(Some(r))
}

// ── pthread_self / registration ────────────────────────────────────────

/// `pthread_t` of the running thread, registering it on first use.
///
/// The primary thread and any thread created through `CreateThread` reach
/// pthread APIs without ever passing through `pthread_create`; POSIX still
/// expects `pthread_self` to work there.
pub(super) fn self_pt(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> u64 {
    let tid = state.kernel.threads.current_tid();
    if let Some(&pt) = state.pthread().by_tid.get(&tid) {
        return pt;
    }
    let _ = engine;
    let pt = state.pthread().alloc_id();
    let mut t = PtThread::new(pt, tid, 0, 0, false);
    // A thread WIE did not start has no entry frame to unwind to.
    t.entry_rsp = 0;
    state.pthread().threads.insert(pt, t);
    state.pthread().by_tid.insert(tid, pt);
    pt
}

/// Whether a cancel has been requested and enabled for `pt`.
fn cancel_pending(state: &mut WinApiState, pt: u64) -> bool {
    state
        .pthread()
        .threads
        .get(&pt)
        .is_some_and(|t| t.cancel_requested && t.cancel_enabled)
}

/// Wake every pthread park queue (used when a cancel is posted).
fn wake_all_queues(state: &mut WinApiState) {
    for m in state.pthread().mutexes.values() {
        m.queue.wake();
    }
    for c in state.pthread().conds.values() {
        c.queue.wake();
    }
    for r in state.pthread().rwlocks.values() {
        r.queue.wake();
    }
    for b in state.pthread().barriers.values() {
        b.queue.wake();
    }
    for s in state.pthread().sems.values() {
        s.queue.wake();
    }
    for t in state.pthread().threads.values() {
        t.queue.wake();
    }
}

/// Abandon a blocking wait when a cancel is pending. `Ok(Some(_))` terminates.
pub(super) fn cancellation_point(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
) -> Result<Option<WinApiHandlerResult>> {
    let pt = self_pt(engine, state);
    if !cancel_pending(state, pt) {
        return Ok(None);
    }
    // Drop any half-finished wait bookkeeping before unwinding.
    let tid = state.kernel.threads.current_tid();
    state.pthread().pending.remove(&tid);
    for c in state.pthread().conds.values_mut() {
        c.remove(pt);
    }
    Ok(Some(begin_termination(engine, state, CANCELED)?))
}

// ── pthread_create ─────────────────────────────────────────────────────

/// `pthread_create(pthread_t *th, const pthread_attr_t *attr, start, arg)`.
fn create(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let th_out = engine.read_rcx()?;
    let attr_va = engine.read_rdx()?;
    let start = engine.read_r8()?;
    let arg = engine.read_r9()?;

    if start == 0 {
        return ret_int(engine, EINVAL);
    }

    let (detached, stack_size) = if attr_va == 0 {
        (false, 0)
    } else {
        let p_state = read_u32(engine, attr_va.wrapping_add(attr::P_STATE));
        (
            p_state & CREATE_DETACHED != 0,
            read_u64(engine, attr_va.wrapping_add(attr::S_SIZE)),
        )
    };

    // Reuse the CreateThread spawn path: stack allocation, TID, kernel thread
    // object, and the PendingSpawn the runtime turns into a host thread.
    let handle = crate::kernel32::create_guest_thread(engine, state, stack_size, start, arg, 0, 0)
        .context("pthread_create: guest thread spawn failed")?;
    if handle == 0 {
        // create_guest_thread already set last_error; POSIX wants EAGAIN.
        return ret_int(engine, EAGAIN);
    }

    let Some(thread_obj) = state.kernel.sync.thread_by_handle(handle) else {
        return ret_int(engine, EAGAIN);
    };
    let tid = thread_obj.tid;

    // Redirect the new thread's return address from the runtime's "RIP 0 means
    // exit" sentinel to our trampoline, so we capture the `void *` result and
    // get to run TSD destructors before the thread dies.
    let entry_rsp = state
        .kernel
        .sync
        .thread_cpu
        .get(&tid)
        .and_then(|ctx| ctx.gpr.get(4).copied())
        .unwrap_or(0);
    if entry_rsp != 0 {
        drop(engine.mem_write(
            entry_rsp,
            &crate::pthread_return_trampoline_va().to_le_bytes(),
        ));
    }

    let pt = state.pthread().alloc_id();
    let mut t = PtThread::new(pt, tid, handle, start, detached);
    t.entry_rsp = entry_rsp;
    state.pthread().threads.insert(pt, t);
    state.pthread().by_tid.insert(tid, pt);

    write_u64(engine, th_out, pt);
    ret_int(engine, 0)
}

// ── pthread_join / detach ──────────────────────────────────────────────

/// `pthread_join(t, void **res)` and `_pthread_tryjoin(t, void **res)`.
fn join(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    blocking: bool,
) -> Result<WinApiHandlerResult> {
    if let Some(r) = cancellation_point(engine, state)? {
        return Ok(r);
    }
    let target = engine.read_rcx()?;
    let res_out = engine.read_rdx()?;
    let me = self_pt(engine, state);

    if target == me {
        return ret_int(engine, EDEADLK);
    }
    let Some(t) = state.pthread().threads.get(&target).cloned() else {
        return ret_int(engine, ESRCH);
    };
    if t.detached || t.joined {
        return ret_int(engine, EINVAL);
    }

    // A thread that died without reaching the trampoline (a fault, or
    // ExitProcess teardown) still marks its kernel object finished; treat that
    // as termination so a joiner can never hang forever.
    let dead = t.finished
        || state
            .kernel
            .sync
            .thread_by_handle(t.win_handle)
            .is_some_and(|k| k.is_finished());

    if !dead {
        if !blocking {
            return ret_int(engine, EBUSY_TRYJOIN);
        }
        let queue = std::sync::Arc::clone(&t.queue);
        return Err(park_on(state, &queue, slice_until(None)));
    }

    let value = t.exit_value;
    if let Some(t) = state.pthread().threads.get_mut(&target) {
        t.joined = true;
        t.finished = true;
    }
    write_u64(engine, res_out, value);
    reap(state, target);
    ret_int(engine, 0)
}

/// `EBUSY` as returned by `_pthread_tryjoin` for a still-running thread.
const EBUSY_TRYJOIN: i32 = super::EBUSY;

/// `pthread_detach(t)`.
fn detach(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let target = engine.read_rcx()?;
    let Some(t) = state.pthread().threads.get_mut(&target) else {
        return ret_int(engine, ESRCH);
    };
    if t.detached {
        return ret_int(engine, EINVAL);
    }
    t.detached = true;
    let finished = t.finished;
    if finished {
        reap(state, target);
    }
    ret_int(engine, 0)
}

/// Drop a terminated thread's bookkeeping once nobody can observe it again.
fn reap(state: &mut WinApiState, pt: u64) {
    let Some(t) = state.pthread().threads.get(&pt) else {
        return;
    };
    if !t.finished || (!t.detached && !t.joined) {
        return;
    }
    let tid = t.tid;
    state.pthread().threads.remove(&pt);
    if state.pthread().by_tid.get(&tid) == Some(&pt) {
        state.pthread().by_tid.remove(&tid);
    }
}

// ── Termination: pthread_exit, cancel, start-routine return ────────────

/// `pthread_exit(void *res)`.
fn exit(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let tid = state.kernel.threads.current_tid();
    if state.pthread().pending.contains_key(&tid) {
        // Re-entered because a cleanup handler or destructor just returned.
        return continue_termination(engine, state);
    }
    let value = engine.read_rcx()?;
    begin_termination(engine, state, value)
}

/// The start routine returned: called by the runtime at the pthread trampoline.
///
/// `RAX` holds the routine's `void *` result and `RSP` sits one slot above the
/// entry frame the spawn path built.
///
/// # Errors
/// Propagates guest memory / register access failures, and returns the
/// `ExitThread` control signal once the thread is ready to die.
pub fn handle_thread_return(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let tid = state.kernel.threads.current_tid();
    if state.pthread().pending.contains_key(&tid) {
        return continue_termination(engine, state);
    }
    let value = engine.read_rax()?;
    begin_termination(engine, state, value)
}

/// Start unwinding this thread: cleanup handlers, then TSD destructors.
fn begin_termination(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    exit_value: u64,
) -> Result<WinApiHandlerResult> {
    let pt = self_pt(engine, state);
    let tid = state.kernel.threads.current_tid();

    // Collect the `pthread_cleanup_push` chain (LIFO — the head is newest).
    let head_slot = state
        .pthread()
        .threads
        .get(&pt)
        .map_or(0, |t| t.clean_head_va);
    let mut remaining = Vec::new();
    let mut node = read_u64(engine, head_slot);
    let mut guard = 0_u32;
    while node != 0 && guard < 256 {
        let func = read_u64(engine, node);
        let arg = read_u64(engine, node.wrapping_add(8));
        if func != 0 {
            remaining.push((func, arg));
        }
        node = read_u64(engine, node.wrapping_add(16));
        guard = guard.saturating_add(1);
    }
    // The chain is consumed; a re-entrant handler must not see it again.
    write_u64(engine, head_slot, 0);

    let frame_rsp = call_frame(engine)?;
    state.pthread().pending.insert(
        tid,
        PtPending::Destructors {
            remaining,
            pass: 0,
            exit_value,
            frame_rsp,
        },
    );
    continue_termination(engine, state)
}

/// Run the next cleanup handler / destructor, or finish the thread.
fn continue_termination(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
) -> Result<WinApiHandlerResult> {
    let tid = state.kernel.threads.current_tid();
    let Some(PtPending::Destructors {
        mut remaining,
        mut pass,
        exit_value,
        frame_rsp,
    }) = state.pthread().pending.remove(&tid)
    else {
        // Nothing in flight: finish immediately.
        return finish_thread(engine, state, 0);
    };

    let pt = self_pt(engine, state);

    loop {
        if let Some((func, arg)) = remaining.pop() {
            state.pthread().pending.insert(
                tid,
                PtPending::Destructors {
                    remaining,
                    pass,
                    exit_value,
                    frame_rsp,
                },
            );
            return enter_guest_call(engine, frame_rsp, func, arg);
        }
        // Cleanup chain done (pass 0) or a destructor pass finished: rescan the
        // keys, because a destructor may legitimately set its key again.
        pass = pass.saturating_add(1);
        if pass > DESTRUCTOR_PASSES {
            break;
        }
        remaining = take_destructor_values(state, pt);
        if remaining.is_empty() {
            break;
        }
    }

    finish_thread(engine, state, exit_value)
}

/// Clear every non-null TSD value that has a destructor, returning the pairs.
fn take_destructor_values(state: &mut WinApiState, pt: u64) -> Vec<(u64, u64)> {
    let keys = state.pthread().keys.clone();
    let mut out = Vec::new();
    let Some(t) = state.pthread().threads.get_mut(&pt) else {
        return out;
    };
    for (key, dtor) in &keys {
        if *dtor == 0 {
            continue;
        }
        if let Some(value) = t.tls.get(key).copied()
            && value != 0
        {
            // POSIX: clear the value *before* running its destructor.
            t.tls.insert(*key, 0);
            out.push((*dtor, value));
        }
    }
    out
}

/// Mark the thread dead, wake joiners, and hand `ExitThread` to the runtime.
fn finish_thread(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    exit_value: u64,
) -> Result<WinApiHandlerResult> {
    let _ = engine;
    let pt = state.kernel.threads.current_tid();
    let pt = state.pthread().by_tid.get(&pt).copied().unwrap_or(0);
    let mut detached = false;
    if let Some(t) = state.pthread().threads.get_mut(&pt) {
        t.finished = true;
        t.exit_value = exit_value;
        detached = t.detached;
        t.queue.wake();
    }
    if detached {
        reap(state, pt);
    }
    let code = u32::try_from(exit_value & 0xffff_ffff).unwrap_or(0);
    Err(crate::WinApiControlSignal::ExitThread { code }.into())
}

/// Fresh scratch frame below the current one for a handler-issued guest call.
///
/// Reusing the caller's return-address slot would let successive calls walk
/// `RSP` up through the caller's live frame; carving a gap keeps every call at
/// the same, private depth.
fn call_frame(engine: &mut dyn CpuEngine) -> Result<u64> {
    let rsp = engine
        .read_rsp()
        .context("read RSP for pthread call frame")?;
    // 16-byte align, then bias by 8 so the callee sees the post-CALL alignment
    // the Win64 ABI guarantees.
    Ok(((rsp.saturating_sub(CALL_FRAME_GAP)) & !0xF_u64).wrapping_sub(8))
}

/// Enter `func(arg)` on a private frame, returning into this same export.
fn enter_guest_call(
    engine: &mut dyn CpuEngine,
    frame_rsp: u64,
    func: u64,
    arg: u64,
) -> Result<WinApiHandlerResult> {
    let self_va = engine.read_rip().context("read RIP for pthread call")?;
    engine
        .write_rsp(frame_rsp)
        .context("set pthread call frame")?;
    engine
        .mem_write(frame_rsp, &self_va.to_le_bytes())
        .context("plant pthread call return address")?;
    engine.write_rcx(arg).context("set pthread call argument")?;
    engine.write_rip(func).context("enter pthread call")?;
    Ok(WinApiHandlerResult {
        return_address: func,
        return_value: 0,
    })
}

// ── pthread_once ───────────────────────────────────────────────────────

/// `pthread_once(pthread_once_t *o, void (*func)(void))`.
fn once(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let tid = state.kernel.threads.current_tid();

    // Re-entry: the init routine just returned.
    if let Some(PtPending::Once { once_va, return_va }) = state.pthread().pending.get(&tid).cloned()
    {
        state.pthread().pending.remove(&tid);
        write_u32(engine, once_va, 1);
        if let Some(o) = state.pthread().onces.get_mut(&once_va) {
            o.done = true;
            o.running = None;
            o.queue.wake();
        }
        return finish_guest_call(engine, return_va, 0);
    }

    let once_va = engine.read_rcx()?;
    let func = engine.read_rdx()?;
    if once_va == 0 || func == 0 {
        return ret_int(engine, EINVAL);
    }
    // The guest control word is authoritative for "already done": it survives
    // even if our side table was cleared.
    if read_u32(engine, once_va) != 0 {
        return ret_int(engine, 0);
    }

    let me = self_pt(engine, state);
    let entry = state.pthread().onces.entry(once_va).or_default();
    if entry.done {
        return ret_int(engine, 0);
    }
    if let Some(runner) = entry.running {
        if runner == me {
            // Recursive pthread_once on the same control word deadlocks in
            // POSIX; report it instead of hanging.
            return ret_int(engine, EINVAL);
        }
        let queue = std::sync::Arc::clone(&entry.queue);
        return Err(park_on(state, &queue, slice_until(None)));
    }
    entry.running = Some(me);

    let return_va = call_guest(engine, func, 0)?;
    state
        .pthread()
        .pending
        .insert(tid, PtPending::Once { once_va, return_va });
    Ok(WinApiHandlerResult {
        return_address: func,
        return_value: 0,
    })
}

// ── pthread_getclean ───────────────────────────────────────────────────

/// `pthread_getclean()` — pointer to this thread's `_pthread_cleanup *` head.
///
/// The `pthread_cleanup_push` macro dereferences and assigns through it, so it
/// must be a real, per-thread, writable guest word. One is carved from the
/// guest heap on first use.
fn getclean(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let pt = self_pt(engine, state);
    let existing = state
        .pthread()
        .threads
        .get(&pt)
        .map_or(0, |t| t.clean_head_va);
    if existing != 0 {
        return ret_u64(engine, existing);
    }
    let va = state.heap_state.heap.alloc_coherent(engine, 8);
    if va == 0 {
        return ret_u64(engine, 0);
    }
    write_u64(engine, va, 0);
    if let Some(t) = state.pthread().threads.get_mut(&pt) {
        t.clean_head_va = va;
    }
    ret_u64(engine, va)
}

// ── attribute bit helpers ──────────────────────────────────────────────

/// `pthread_attr_set{detachstate,inheritsched,scope}` — set or clear one bit.
fn attr_set_bits(engine: &mut dyn CpuEngine, mask: u32) -> Result<WinApiHandlerResult> {
    let a = engine.read_rcx()?;
    let flag = u32::try_from(engine.read_rdx()? & 0xffff_ffff).unwrap_or(0);
    if a == 0 || (flag & !mask) != 0 {
        return ret_int(engine, EINVAL);
    }
    let cur = read_u32(engine, a.wrapping_add(attr::P_STATE));
    write_u32(engine, a.wrapping_add(attr::P_STATE), (cur & !mask) | flag);
    ret_int(engine, 0)
}

/// `pthread_attr_get{detachstate,inheritsched,scope}` — read one bit.
fn attr_get_bits(engine: &mut dyn CpuEngine, mask: u32) -> Result<WinApiHandlerResult> {
    let a = engine.read_rcx()?;
    let out = engine.read_rdx()?;
    if a == 0 || out == 0 {
        return ret_int(engine, EINVAL);
    }
    let cur = read_u32(engine, a.wrapping_add(attr::P_STATE));
    write_u32(engine, out, cur & mask);
    ret_int(engine, 0)
}

/// `pthread_getname_np(t, char *buf, size_t len)`.
fn getname(engine: &mut dyn CpuEngine, state: &mut WinApiState) -> Result<WinApiHandlerResult> {
    let pt = engine.read_rcx()?;
    let buf = engine.read_rdx()?;
    let len = engine.read_r8()?;
    let Some(t) = state.pthread().threads.get(&pt) else {
        return ret_int(engine, ESRCH);
    };
    if buf == 0 || len == 0 {
        return ret_int(engine, EINVAL);
    }
    let bytes = t.name.as_bytes();
    let copy = usize::try_from(len.saturating_sub(1))
        .unwrap_or(0)
        .min(bytes.len());
    let Some(slice) = bytes.get(..copy) else {
        return ret_int(engine, EINVAL);
    };
    drop(engine.mem_write(buf, slice));
    drop(engine.mem_write(
        buf.saturating_add(u64::try_from(copy).unwrap_or(0)),
        &[0_u8],
    ));
    ret_int(engine, 0)
}

/// Silence the unused-import warning for constants used only in docs.
const _: (i32, i32, u64) = (BARRIER_SERIAL_THREAD, EPERM, stack_arg_unused());

/// Placeholder so the shared `stack_arg` helper stays reachable from this file.
const fn stack_arg_unused() -> u64 {
    0
}
