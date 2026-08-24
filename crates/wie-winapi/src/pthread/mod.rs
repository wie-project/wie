//! `libwinpthread-1.dll` — native host implementation of the mingw-w64
//! winpthreads API.
//!
//! mingw's `-lpthread` links against this DLL by default, so most POSIX-threaded
//! programs built for Windows reach WIE through here rather than through
//! `CreateThread`. Every export is implemented on the host and mapped onto WIE's
//! existing multithread runtime: `pthread_create` goes through the same
//! `PendingSpawn` path as `CreateThread`, so a pthread is a real 1:1 host thread
//! with its own CPU engine.

#![allow(unreachable_pub)]
//!
//! # Guest ABI
//!
//! From `pthread.h` (x86-64 mingw-w64), every synchronisation type is one
//! pointer-sized word and every attribute type is a small scalar:
//!
//! | guest type | width | contents |
//! |---|---|---|
//! | `pthread_t`, `pthread_mutex_t`, `pthread_cond_t`, `pthread_rwlock_t`, `pthread_spinlock_t`, `pthread_barrier_t`, `sem_t` | 8 | tagged id ([`PT_TAG`]) |
//! | `pthread_key_t`, `pthread_mutexattr_t` | 4 | key index / attribute bits |
//! | `pthread_once_t`, `pthread_condattr_t`, `pthread_rwlockattr_t` | 4 | control word / attribute bits |
//! | `pthread_attr_t` | 40 | `{ unsigned p_state; void *stack; size_t s_size; struct sched_param param; }` |
//!
//! Storing an id (not a host pointer) in the guest word means a guest that
//! copies, compares, or zeroes a handle behaves exactly as it would on Windows.
//! The static initialisers `PTHREAD_MUTEX_INITIALIZER` (−1), `…ERRORCHECK…`
//! (−2) and `…RECURSIVE…` (−3) are recognised and lazily promoted to a real
//! object on first use, as winpthreads does.
//!
//! # Blocking
//!
//! A handler runs with the process-wide WinAPI mutex held, so it can never
//! block inline — the thread that would unblock it needs that same mutex.
//! Blocking operations instead record a [`PtPark`] and return
//! [`crate::WinApiControlSignal::HostPark`]; the runtime drops all process
//! locks, parks on the object's [`WakeQueue`], and then re-enters the *same*
//! handler with RIP unchanged. Handlers are therefore written to be re-entrant
//! and idempotent: they re-check the condition on every entry.
//!
//! Operations that cannot be expressed as a single retry (`pthread_cond_wait`
//! must release a mutex, wait, then re-acquire it) carry a [`PtPending`] record
//! keyed by guest TID that drives a small state machine across re-entries.
//!
//! # Calling guest code
//!
//! `pthread_once` and `pthread_key_create` destructors have to run guest
//! functions from inside a handler. Rather than add a new runtime facility,
//! these push the *current export's own fake VA* as the callee's return address
//! (see [`call_guest`]): the guest function returns straight back into this
//! dispatcher, where the [`PtPending`] record says what to do next.

mod locks;
mod objects;
mod threads;

use ahash::HashMap;
use ahash::HashMapExt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use wie_cpu::CpuEngine;

use crate::{HandlerContext, WinApiHandlerResult, WinApiState};

pub use objects::{
    CondWaiter, MUTEX_ERRORCHECK, MUTEX_NORMAL, MUTEX_RECURSIVE, PtBarrier, PtCond, PtMutex,
    PtOnce, PtRwLock, PtSem, PtSpin, PtThread, WakeQueue,
};

// ── errno values (mingw-w64 <errno.h> / <pthread_compat.h>) ────────────

/// `EPERM` — operation not permitted (not the lock owner).
pub const EPERM: i32 = 1;
/// `ESRCH` — no such thread.
pub const ESRCH: i32 = 3;
/// `EINTR` — interrupted.
pub const EINTR: i32 = 4;
/// `EAGAIN` — resource temporarily unavailable / `sem_trywait` would block.
pub const EAGAIN: i32 = 11;
/// `EOVERFLOW` — value too large for the target type.
pub const EOVERFLOW: i32 = 75;
/// `EBUSY` — object is in use.
pub const EBUSY: i32 = 16;
/// `EEXIST` — named semaphore already exists.
pub const EEXIST: i32 = 17;
/// `EINVAL` — invalid argument.
pub const EINVAL: i32 = 22;
/// `EDEADLK` — error-checking mutex relocked by its owner.
pub const EDEADLK: i32 = 36;
/// `ENOSYS` — not implemented.
pub const ENOSYS: i32 = 40;
/// `ENOTSUP` — unsupported (mingw value, not the Linux one).
pub const ENOTSUP: i32 = 129;
/// `ETIMEDOUT` — timed wait expired (mingw value, not the Linux one).
pub const ETIMEDOUT: i32 = 138;
/// `ENOENT` — no such named semaphore.
pub const ENOENT: i32 = 2;

// ── pthread.h constants ────────────────────────────────────────────────

/// `PTHREAD_CANCEL_ENABLE`.
pub const CANCEL_ENABLE: i32 = 0x01;
/// `PTHREAD_CREATE_DETACHED`.
pub const CREATE_DETACHED: u32 = 0x04;
/// `PTHREAD_INHERIT_SCHED`.
pub const INHERIT_SCHED: u32 = 0x08;
/// `PTHREAD_SCOPE_SYSTEM`.
pub const SCOPE_SYSTEM: u32 = 0x10;
/// `PTHREAD_CANCEL_ASYNCHRONOUS`.
pub const CANCEL_ASYNCHRONOUS: i32 = 0x02;
/// `PTHREAD_BARRIER_SERIAL_THREAD`.
pub const BARRIER_SERIAL_THREAD: i32 = 1;
/// `PTHREAD_CANCELED` — the `void *` a cancelled thread joins with.
pub const CANCELED: u64 = 0xDEAD_BEEF;
/// `PTHREAD_PROCESS_SHARED`.
pub const PROCESS_SHARED: i32 = 1;

/// Guest VA of the process-wide `errno` slot, shared with the UCRT `_errno`
/// handler so `sem_*` / `clock_*` failures are visible to guest code.
const ERRNO_VA: u64 = 0x7EFD_0070;

/// Tag in the high 16 bits of every guest-visible pthread object id.
///
/// Chosen so the value is a positive `intptr_t` (never mistaken for one of the
/// negative static initialisers) and never a plausible guest VA.
pub const PT_TAG: u64 = 0x5054_0000_0000_0000;
const PT_TAG_MASK: u64 = 0xFFFF_0000_0000_0000;

/// Longest a parked thread sleeps before the runtime re-checks liveness.
///
/// Waits are not woken by `process_dying`, so an unbounded park would keep a
/// worker alive after `ExitProcess`. 50 ms matches the slice the `WaitForSingle
/// Object` park already uses.
const PARK_SLICE: Duration = Duration::from_millis(50);

// ── Park + pending state ───────────────────────────────────────────────

/// A queued host park: what to sleep on and for how long.
#[derive(Debug, Clone)]
pub struct PtPark {
    /// Queue to park on.
    pub queue: std::sync::Arc<WakeQueue>,
    /// Wake sequence sampled under the WinAPI mutex.
    pub observed: u64,
    /// Upper bound on this park (always finite; handlers re-check on re-entry).
    pub slice: Duration,
}

/// Multi-step operation in flight on one guest thread.
#[derive(Debug, Clone)]
pub enum PtPending {
    /// `pthread_cond_wait`: mutex released, waiting to be signaled.
    CondWait {
        /// Condition variable id.
        cond: u64,
        /// Mutex id to re-acquire.
        mutex: u64,
        /// Recursion depth to restore on re-acquire.
        depth: u32,
        /// Absolute deadline for `pthread_cond_timedwait`.
        deadline: Option<Instant>,
    },
    /// `pthread_cond_wait`: signaled (or expired), re-acquiring the mutex.
    CondReacquire {
        /// Mutex id to re-acquire.
        mutex: u64,
        /// Recursion depth to restore.
        depth: u32,
        /// Result to return once the mutex is held (`0` or `ETIMEDOUT`).
        result: i32,
    },
    /// `pthread_barrier_wait`: parked until the generation advances.
    Barrier {
        /// Barrier id.
        barrier: u64,
        /// Generation observed on arrival.
        generation: u64,
    },
    /// `pthread_once`: the init routine is running on this thread.
    Once {
        /// Guest VA of the `pthread_once_t` control word.
        once_va: u64,
        /// Where `pthread_once` must return once the routine finishes.
        return_va: u64,
    },
    /// Key destructors are running before the thread terminates.
    Destructors {
        /// Remaining `(destructor VA, value)` pairs for this pass.
        remaining: Vec<(u64, u64)>,
        /// Destructor passes already completed.
        pass: u32,
        /// Guest RSP where the destructor call frame was set up.
        frame_rsp: u64,
        /// `void *` this thread will exit with.
        exit_value: u64,
    },
}

// ── Process-wide pthread state ─────────────────────────────────────────

/// Everything `libwinpthread-1.dll` owns, hung off [`crate::WinApiState`].
#[derive(Debug, Clone)]
pub struct PthreadState {
    next_id: u64,
    next_key: u32,
    /// Live mutexes by id.
    pub mutexes: HashMap<u64, PtMutex>,
    /// Live condition variables by id.
    pub conds: HashMap<u64, PtCond>,
    /// Live rwlocks by id.
    pub rwlocks: HashMap<u64, PtRwLock>,
    /// Live spinlocks by id.
    pub spins: HashMap<u64, PtSpin>,
    /// Live barriers by id.
    pub barriers: HashMap<u64, PtBarrier>,
    /// Live semaphores by id.
    pub sems: HashMap<u64, PtSem>,
    /// `sem_open` name → semaphore id.
    pub named_sems: HashMap<String, u64>,
    /// `pthread_once_t` guest VA → control state.
    pub onces: HashMap<u64, PtOnce>,
    /// Live threads by `pthread_t`.
    pub threads: HashMap<u64, PtThread>,
    /// Guest TID → `pthread_t`.
    pub by_tid: HashMap<u32, u64>,
    /// `pthread_key_t` → destructor VA (`0` when there is none).
    pub keys: HashMap<u32, u64>,
    /// Pending host park, keyed by guest TID.
    pub parks: HashMap<u32, PtPark>,
    /// In-flight multi-step operation, keyed by guest TID.
    pub pending: HashMap<u32, PtPending>,
    /// `pthread_setconcurrency` level.
    pub concurrency: i32,
    /// `pthread_set_num_processors_np` override.
    pub num_processors: i32,
    /// Guest scratch page for `pthread_getclean` heads, or 0 if not allocated.
    pub clean_page: u64,
    /// Next free slot in [`Self::clean_page`].
    pub clean_next: u64,
    /// Process start instant for `CLOCK_MONOTONIC`.
    started: Instant,
}

impl Default for PthreadState {
    fn default() -> Self {
        Self::new()
    }
}

impl PthreadState {
    /// Empty state (no threads registered until the first pthread call).
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_id: 0,
            next_key: 1,
            mutexes: HashMap::new(),
            conds: HashMap::new(),
            rwlocks: HashMap::new(),
            spins: HashMap::new(),
            barriers: HashMap::new(),
            sems: HashMap::new(),
            named_sems: HashMap::new(),
            onces: HashMap::new(),
            threads: HashMap::new(),
            by_tid: HashMap::new(),
            keys: HashMap::new(),
            parks: HashMap::new(),
            pending: HashMap::new(),
            concurrency: 0,
            num_processors: 0,
            clean_page: 0,
            clean_next: 0,
            started: Instant::now(),
        }
    }

    /// Allocate a fresh tagged object id.
    pub fn alloc_id(&mut self) -> u64 {
        self.next_id = self.next_id.saturating_add(1);
        PT_TAG | (self.next_id & !PT_TAG_MASK)
    }

    /// Milliseconds since process start (`CLOCK_MONOTONIC`).
    #[must_use]
    pub fn monotonic(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Whether `value` is one of our tagged object ids.
#[must_use]
pub fn is_pt_id(value: u64) -> bool {
    value & PT_TAG_MASK == PT_TAG && value & !PT_TAG_MASK != 0
}

/// Take the park queued for `tid`, if any. Called by the runtime park handler.
pub fn take_park(state: &mut WinApiState, tid: u32) -> Option<PtPark> {
    state.pthread().parks.remove(&tid)
}

// ── Small guest-memory helpers ─────────────────────────────────────────

/// Read a 64-bit word from `va` (0 when `va` is null or unreadable).
fn read_u64(engine: &mut dyn CpuEngine, va: u64) -> u64 {
    if va == 0 {
        return 0;
    }
    let mut b = [0_u8; 8];
    if engine.mem_read(va, &mut b).is_err() {
        return 0;
    }
    u64::from_le_bytes(b)
}

/// Read a 32-bit word from `va` (0 when `va` is null or unreadable).
fn read_u32(engine: &mut dyn CpuEngine, va: u64) -> u32 {
    if va == 0 {
        return 0;
    }
    let mut b = [0_u8; 4];
    if engine.mem_read(va, &mut b).is_err() {
        return 0;
    }
    u32::from_le_bytes(b)
}

/// Store a 64-bit word at `va` (no-op when `va` is null).
fn write_u64(engine: &mut dyn CpuEngine, va: u64, value: u64) {
    if va != 0 {
        drop(engine.mem_write(va, &value.to_le_bytes()));
    }
}

/// Store a 32-bit word at `va` (no-op when `va` is null).
fn write_u32(engine: &mut dyn CpuEngine, va: u64, value: u32) {
    if va != 0 {
        drop(engine.mem_write(va, &value.to_le_bytes()));
    }
}

/// Store a 32-bit signed word at `va`.
fn write_i32(engine: &mut dyn CpuEngine, va: u64, value: i32) {
    write_u32(engine, va, value.cast_unsigned());
}

/// Read a NUL-terminated ASCII string of at most 512 bytes.
fn read_cstr(engine: &mut dyn CpuEngine, va: u64) -> String {
    if va == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut i = 0_u64;
    while i < 512 {
        let mut b = [0_u8; 1];
        if engine.mem_read(va.saturating_add(i), &mut b).is_err() {
            break;
        }
        let Some(&c) = b.first() else { break };
        if c == 0 {
            break;
        }
        out.push(char::from(c));
        i = i.saturating_add(1);
    }
    out
}

/// Publish `code` to the guest `errno` slot shared with the UCRT `_errno`.
fn set_errno(engine: &mut dyn CpuEngine, code: i32) {
    write_i32(engine, ERRNO_VA, code);
}

// ── Return helpers ─────────────────────────────────────────────────────

/// Return an `int` from a pthread export (errno-style: 0 on success).
fn ret_int(engine: &mut dyn CpuEngine, value: i32) -> Result<WinApiHandlerResult> {
    ret_u64(engine, u64::from(value.cast_unsigned()))
}

/// Return a pointer-sized value.
fn ret_u64(engine: &mut dyn CpuEngine, value: u64) -> Result<WinApiHandlerResult> {
    let return_address = engine
        .return_from_win64_api(value)
        .context("failed to return from pthread export")?;
    Ok(WinApiHandlerResult {
        return_address,
        return_value: value,
    })
}

/// Return `-1` and set `errno` (the `sem_*` / `clock_*` convention).
fn ret_errno(engine: &mut dyn CpuEngine, code: i32) -> Result<WinApiHandlerResult> {
    set_errno(engine, code);
    ret_int(engine, -1)
}

/// Queue a host park on `queue` and hand control back to the runtime.
///
/// The runtime drops every process lock, sleeps, and re-enters this handler
/// with RIP unchanged, so the caller must be safe to run again from the top.
fn park_on(
    state: &mut WinApiState,
    queue: &std::sync::Arc<WakeQueue>,
    slice: Duration,
) -> anyhow::Error {
    let tid = state.kernel.threads.current_tid();
    let observed = queue.observe();
    state.pthread().parks.insert(
        tid,
        PtPark {
            queue: std::sync::Arc::clone(queue),
            observed,
            slice,
        },
    );
    crate::WinApiControlSignal::HostPark {
        reason: crate::HostParkReason::PthreadWait,
    }
    .into()
}

/// How long to park given an optional absolute deadline.
fn slice_until(deadline: Option<Instant>) -> Duration {
    match deadline {
        None => PARK_SLICE,
        Some(dl) => dl
            .saturating_duration_since(Instant::now())
            .min(PARK_SLICE)
            .max(Duration::from_millis(1)),
    }
}

// ── Time helpers ───────────────────────────────────────────────────────

/// Read a `struct _timespec32` / `_timespec64` as `(seconds, nanoseconds)`.
///
/// `_timespec32` is `{ __time32_t tv_sec; long tv_nsec; }` (8 bytes);
/// `_timespec64` is `{ __time64_t tv_sec; long tv_nsec; }` (16 bytes with the
/// trailing padding x86-64 inserts after the 4-byte `long`).
fn read_timespec(engine: &mut dyn CpuEngine, va: u64, bits64: bool) -> Option<(i64, i64)> {
    if va == 0 {
        return None;
    }
    if bits64 {
        let secs = read_u64(engine, va).cast_signed();
        let nsecs = i64::from(read_u32(engine, va.saturating_add(8)).cast_signed());
        Some((secs, nsecs))
    } else {
        let secs = i64::from(read_u32(engine, va).cast_signed());
        let nsecs = i64::from(read_u32(engine, va.saturating_add(4)).cast_signed());
        Some((secs, nsecs))
    }
}

/// Convert `(seconds, nanoseconds)` to a `Duration`, clamping negatives to zero.
fn timespec_to_duration(secs: i64, nsecs: i64) -> Duration {
    if secs < 0 {
        return Duration::ZERO;
    }
    let s = u64::try_from(secs).unwrap_or(0);
    let n = u32::try_from(nsecs.clamp(0, 999_999_999)).unwrap_or(0);
    Duration::new(s, n)
}

/// Turn an **absolute** `CLOCK_REALTIME` timespec into a deadline on the host
/// monotonic clock. `None` when the pointer is null (wait forever).
fn absolute_deadline(engine: &mut dyn CpuEngine, va: u64, bits64: bool) -> Option<Instant> {
    let (secs, nsecs) = read_timespec(engine, va, bits64)?;
    let target = timespec_to_duration(secs, nsecs);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let remaining = target.saturating_sub(now);
    Some(
        Instant::now()
            .checked_add(remaining)
            .unwrap_or_else(Instant::now),
    )
}

/// Turn a **relative** timespec into a deadline. `None` when null.
fn relative_deadline(engine: &mut dyn CpuEngine, va: u64, bits64: bool) -> Option<Instant> {
    let (secs, nsecs) = read_timespec(engine, va, bits64)?;
    Some(
        Instant::now()
            .checked_add(timespec_to_duration(secs, nsecs))
            .unwrap_or_else(Instant::now),
    )
}

/// Write `(seconds, nanoseconds)` into a guest `struct timespec`.
fn write_timespec(engine: &mut dyn CpuEngine, va: u64, d: Duration, bits64: bool) {
    if va == 0 {
        return;
    }
    let secs = d.as_secs();
    let nsecs = d.subsec_nanos();
    if bits64 {
        write_u64(engine, va, secs);
        write_u32(engine, va.saturating_add(8), nsecs);
    } else {
        write_u32(engine, va, u32::try_from(secs).unwrap_or(u32::MAX));
        write_u32(engine, va.saturating_add(4), nsecs);
    }
}

// ── Calling guest code from a handler ──────────────────────────────────

/// Arrange for the guest to call `func(arg)` and return **into this export**.
///
/// At handler entry `RSP` points at the caller's return address and RIP is this
/// export's fake VA. Overwriting that one stack slot with the fake VA turns the
/// guest's `ret` into a re-entry: the callee keeps the shadow space the guest
/// already reserved for our call, `RSP % 16 == 8` stays correct, and the real
/// return address is stashed in the [`PtPending`] record. On re-entry the
/// handler restores the slot and returns for real.
fn call_guest(engine: &mut dyn CpuEngine, func: u64, arg: u64) -> Result<u64> {
    let rsp = engine.read_rsp().context("read RSP for guest call")?;
    let self_va = engine.read_rip().context("read RIP for guest call")?;
    let return_va = read_u64(engine, rsp);
    engine
        .mem_write(rsp, &self_va.to_le_bytes())
        .context("plant pthread re-entry return address")?;
    engine.write_rcx(arg).context("set guest call argument")?;
    engine.write_rip(func).context("enter guest call")?;
    Ok(return_va)
}

/// Finish a [`call_guest`] sequence: return `value` to the original caller.
fn finish_guest_call(
    engine: &mut dyn CpuEngine,
    return_va: u64,
    value: u64,
) -> Result<WinApiHandlerResult> {
    let rsp = engine.read_rsp().context("read RSP after guest call")?;
    let slot = rsp.wrapping_sub(8);
    engine
        .mem_write(slot, &return_va.to_le_bytes())
        .context("restore pthread return address")?;
    engine.write_rsp(slot).context("restore RSP after call")?;
    ret_u64(engine, value)
}

// ── Dispatch ───────────────────────────────────────────────────────────

/// Dispatch one `libwinpthread-1.dll` export.
///
/// Unknown names return `ENOSYS` rather than aborting the process: winpthreads
/// carries internal helpers that no ordinary program imports, and a stray one
/// should not be fatal.
pub fn dispatch(ctx: &mut HandlerContext<'_>, name: &str) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    // winpthreads exports several helpers with a leading underscore
    // (`_pthread_tryjoin`, `__pthread_shallcancel`); match on the bare name.
    let bare = name.trim_start_matches('_');

    if let Some(r) = threads::dispatch(engine, state, bare)? {
        return Ok(r);
    }
    if let Some(r) = locks::dispatch(engine, state, bare)? {
        return Ok(r);
    }
    if let Some(r) = dispatch_misc(engine, state, bare)? {
        return Ok(r);
    }

    tracing::debug!(export = name, "unimplemented libwinpthread export");
    ret_int(engine, ENOSYS)
}

/// Scheduling, clocks, and the `_np` extensions.
fn dispatch_misc(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    name: &str,
) -> Result<Option<WinApiHandlerResult>> {
    let r = match name {
        // ── sched.h ────────────────────────────────────────────────────
        "sched_yield" => {
            std::thread::yield_now();
            ret_int(engine, 0)?
        }
        "sched_get_priority_min" => ret_int(engine, -15)?,
        "sched_get_priority_max" => ret_int(engine, 15)?,
        "sched_getscheduler" => {
            // Windows has no POSIX scheduling policies; SCHED_OTHER (0) is the
            // only honest answer.
            ret_int(engine, 0)?
        }
        "sched_setscheduler" => ret_int(engine, 0)?,

        // ── concurrency / processor count ──────────────────────────────
        "pthread_num_processors_np" => {
            let n = if state.pthread().num_processors > 0 {
                state.pthread().num_processors
            } else {
                i32::try_from(
                    std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
                )
                .unwrap_or(1)
            };
            ret_int(engine, n)?
        }
        "pthread_set_num_processors_np" => {
            let n = trunc_i32(engine.read_rcx()?);
            state.pthread().num_processors = n.max(0);
            ret_int(engine, 0)?
        }
        "pthread_getconcurrency" => ret_int(engine, state.pthread().concurrency)?,
        "pthread_setconcurrency" => {
            let level = trunc_i32(engine.read_rcx()?);
            if level < 0 {
                ret_int(engine, EINVAL)?
            } else {
                state.pthread().concurrency = level;
                ret_int(engine, 0)?
            }
        }
        "pthread_get_concurrency" => {
            let out = engine.read_rcx()?;
            write_i32(engine, out, state.pthread().concurrency);
            ret_int(engine, 0)?
        }
        "pthread_set_concurrency" => {
            state.pthread().concurrency = trunc_i32(engine.read_rcx()?);
            ret_int(engine, 0)?
        }

        // ── clocks ─────────────────────────────────────────────────────
        "clock_gettime" | "clock_gettime64" => clock_gettime(engine, state, true)?,
        "clock_gettime32" => clock_gettime(engine, state, false)?,
        "clock_getres" | "clock_getres64" => {
            let out = engine.read_rdx()?;
            // Host clocks are nanosecond-resolution; report 1 ns.
            write_timespec(engine, out, Duration::from_nanos(1), true);
            ret_int(engine, 0)?
        }
        "clock_getres32" => {
            let out = engine.read_rdx()?;
            write_timespec(engine, out, Duration::from_nanos(1), false);
            ret_int(engine, 0)?
        }
        "clock_settime" | "clock_settime32" | "clock_settime64" => {
            // The guest cannot move the host wall clock.
            ret_errno(engine, EPERM)?
        }
        "nanosleep" | "nanosleep64" => nanosleep(engine, 0, true)?,
        "nanosleep32" => nanosleep(engine, 0, false)?,
        "clock_nanosleep" | "clock_nanosleep64" => clock_nanosleep(engine, true)?,
        "clock_nanosleep32" => clock_nanosleep(engine, false)?,
        "pthread_delay_np" | "pthread_delay64_np" => {
            let ts = engine.read_rcx()?;
            let d = read_timespec(engine, ts, true)
                .map_or(Duration::ZERO, |(s, n)| timespec_to_duration(s, n));
            std::thread::sleep(d.min(Duration::from_secs(1)));
            ret_int(engine, 0)?
        }
        "pthread_delay32_np" => {
            let ts = engine.read_rcx()?;
            let d = read_timespec(engine, ts, false)
                .map_or(Duration::ZERO, |(s, n)| timespec_to_duration(s, n));
            std::thread::sleep(d.min(Duration::from_secs(1)));
            ret_int(engine, 0)?
        }
        "pthread_delay_np_ms" => {
            let ms = engine.read_rcx()? & 0xffff_ffff;
            std::thread::sleep(Duration::from_millis(ms.min(1000)));
            ret_int(engine, 0)?
        }
        "pthread_time_in_ms" => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            ret_u64(engine, u64::try_from(now.as_millis()).unwrap_or(u64::MAX))?
        }
        "pthread_time_in_ms_from_timespec" => {
            let ts = engine.read_rcx()?;
            let ms = read_timespec(engine, ts, true)
                .map_or(0, |(s, n)| timespec_to_duration(s, n).as_millis());
            ret_u64(engine, u64::try_from(ms).unwrap_or(u64::MAX))?
        }
        "pthread_rel_time_in_ms" => {
            let ts = engine.read_rcx()?;
            let target = read_timespec(engine, ts, true)
                .map_or(Duration::ZERO, |(s, n)| timespec_to_duration(s, n));
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let rel = target.saturating_sub(now);
            ret_u64(engine, u64::try_from(rel.as_millis()).unwrap_or(u64::MAX))?
        }
        // Windows fires this from a WM_TIMECHANGE handler; nothing to re-arm.
        "pthread_timechange_handler_np" => ret_u64(engine, 0)?,

        _ => return Ok(None),
    };
    Ok(Some(r))
}

/// `clock_gettime(clockid, struct timespec *)`.
fn clock_gettime(
    engine: &mut dyn CpuEngine,
    state: &mut WinApiState,
    bits64: bool,
) -> Result<WinApiHandlerResult> {
    let clock_id = trunc_i32(engine.read_rcx()?);
    let out = engine.read_rdx()?;
    if out == 0 {
        return ret_errno(engine, EINVAL);
    }
    // CLOCK_REALTIME (0) is wall time; every other clock mingw defines is a
    // monotonic count, which we serve from process start.
    let d = if clock_id == 0 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
    } else {
        state.pthread().monotonic()
    };
    write_timespec(engine, out, d, bits64);
    ret_int(engine, 0)
}

/// `nanosleep(const struct timespec *req, struct timespec *rem)`.
fn nanosleep(
    engine: &mut dyn CpuEngine,
    arg_shift: u64,
    bits64: bool,
) -> Result<WinApiHandlerResult> {
    let _ = arg_shift;
    let req = engine.read_rcx()?;
    let rem = engine.read_rdx()?;
    let Some((secs, nsecs)) = read_timespec(engine, req, bits64) else {
        return ret_errno(engine, EINVAL);
    };
    if !(0..=999_999_999).contains(&nsecs) || secs < 0 {
        return ret_errno(engine, EINVAL);
    }
    std::thread::sleep(timespec_to_duration(secs, nsecs));
    if rem != 0 {
        write_timespec(engine, rem, Duration::ZERO, bits64);
    }
    ret_int(engine, 0)
}

/// `clock_nanosleep(clockid, flags, const struct timespec *, struct timespec *)`.
fn clock_nanosleep(engine: &mut dyn CpuEngine, bits64: bool) -> Result<WinApiHandlerResult> {
    const TIMER_ABSTIME: i32 = 1;
    let _clock_id = trunc_i32(engine.read_rcx()?);
    let flags = trunc_i32(engine.read_rdx()?);
    let req = engine.read_r8()?;
    let rem = engine.read_r9()?;
    let Some((secs, nsecs)) = read_timespec(engine, req, bits64) else {
        return ret_int(engine, EINVAL);
    };
    let target = timespec_to_duration(secs, nsecs);
    let d = if flags & TIMER_ABSTIME == 0 {
        target
    } else {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        target.saturating_sub(now)
    };
    std::thread::sleep(d);
    if rem != 0 {
        write_timespec(engine, rem, Duration::ZERO, bits64);
    }
    // clock_nanosleep returns the error number directly, unlike nanosleep.
    ret_int(engine, 0)
}

/// Low 32 bits of a register as a signed `int`.
fn trunc_i32(reg: u64) -> i32 {
    u32::try_from(reg & 0xffff_ffff).unwrap_or(0).cast_signed()
}
