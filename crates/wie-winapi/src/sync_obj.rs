//! Kernel waitable objects for MT.2 / MT.3 (threads, events, CS wait queues).
//!
//! Host threads park on [`std::sync::Condvar`] while another guest thread holds
//! the shared CPU engine. Guest data races are still the application's problem;
//! engine metadata is serialized by the runtime process lock.

use ahash::HashMap;
use ahash::HashMapExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use wie_cpu::ThreadContext;

use crate::state::handle_newtype;
use crate::wake::{ThreadInbox, WaiterRegistry, Wake};

/// `STILL_ACTIVE` — thread has not terminated (`GetExitCodeThread`).
pub const STILL_ACTIVE: u32 = 259;

/// First guest pid handed to a child spawned by `CreateProcessW/A`.
///
/// The parent's own `GetCurrentProcessId` returns the fixed
/// `FAKE_CURRENT_PROCESS_ID` (0x1234, kernel32/mod.rs); children get
/// monotonic ids strictly after it so `OpenProcess` lookups never collide.
const FIRST_CHILD_PID: u32 = 0x1235;

// ── Directory change notifications (FindFirstChangeNotification* / ReadDirectoryChangesW) ──

/// `FILE_NOTIFY_CHANGE_FILE_NAME` — a file was created/removed/renamed.
pub(crate) const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x0000_0001;
/// `FILE_NOTIFY_CHANGE_DIR_NAME` — a directory was created/removed/renamed.
pub(crate) const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x0000_0002;
/// `FILE_NOTIFY_CHANGE_ATTRIBUTES` — attributes changed.
pub(crate) const FILE_NOTIFY_CHANGE_ATTRIBUTES: u32 = 0x0000_0004;
/// `FILE_NOTIFY_CHANGE_SIZE` — file size changed.
pub(crate) const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x0000_0008;
/// `FILE_NOTIFY_CHANGE_LAST_WRITE` — last write time changed.
pub(crate) const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x0000_0010;
/// `FILE_NOTIFY_CHANGE_LAST_ACCESS` — last access time changed.
pub(crate) const FILE_NOTIFY_CHANGE_LAST_ACCESS: u32 = 0x0000_0020;

/// `FILE_ACTION_ADDED` — the file was added to the directory.
pub(crate) const FILE_ACTION_ADDED: u32 = 1;
/// `FILE_ACTION_REMOVED` — the file was removed from the directory.
pub(crate) const FILE_ACTION_REMOVED: u32 = 2;
/// `FILE_ACTION_MODIFIED` — the file was modified.
pub(crate) const FILE_ACTION_MODIFIED: u32 = 3;
/// `FILE_ACTION_RENAMED_OLD_NAME` — old name of a renamed file.
pub(crate) const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
/// `FILE_ACTION_RENAMED_NEW_NAME` — new name of a renamed file.
pub(crate) const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;

// A kernel-object handle (thread / event / semaphore) in the [`SyncState`]
// table. Typed so a `KernelHandle` cannot be mixed with file, window, or other
// handle namespaces (ADR-003). Handlers keep raw `u64` registers; lookups
// convert at the table boundary.
handle_newtype! {
    /// A kernel-object handle (thread / event / semaphore).
    KernelHandle
}

/// `WAIT_OBJECT_0` success from `WaitForSingleObject`.
pub const WAIT_OBJECT_0: u32 = 0;
/// `WAIT_TIMEOUT`.
pub const WAIT_TIMEOUT: u32 = 0x0000_0102;
/// `WAIT_FAILED`.
pub const WAIT_FAILED: u32 = 0xffff_ffff;
/// `INFINITE` timeout.
pub const INFINITE: u32 = 0xffff_ffff;

/// Handle table + wait infrastructure owned by [`crate::WinApiState`].
#[derive(Debug, Clone, Default)]
pub struct SyncState {
    /// Next kernel handle value (never zero / `INVALID_HANDLE_VALUE`).
    ///
    /// `pub(crate)`: only the allocators here and the kernel32 handle writers
    /// touch the counter; external crates receive handles as raw `u64`.
    pub(crate) next_handle: KernelHandle,
    /// Live kernel objects keyed by handle.
    pub objects: HashMap<KernelHandle, KernelObject>,
    /// Guest TID → saved CPU context while not running on the shared engine.
    pub thread_cpu: HashMap<u32, ThreadContext>,
    /// Critical-section wait queues keyed by guest CS VA.
    pub cs_waiters: HashMap<u64, Arc<CsWaitQueue>>,
    /// Monotonic stack slot for worker stacks.
    pub(crate) next_stack_slot: u32,
    /// Threads waiting to be spawned by the session after `CreateThread`.
    pub pending_spawns: Vec<PendingSpawn>,
    /// `CREATE_SUSPENDED` threads awaiting `ResumeThread` (keyed by handle).
    pub(crate) suspended_spawns: HashMap<u64, PendingSpawn>,
    /// Pending `WaitForMultipleObjects` args, keyed by guest TID of the waiter.
    pub multi_wait: HashMap<u32, MultiWaitRequest>,
    /// `ReadDirectoryChangesW` anchors keyed by the guest file handle that
    /// `CreateFileW(FILE_LIST_DIRECTORY)` returned for a directory.
    ///
    /// The anchor object is created at open time but its notify watcher is
    /// started lazily by the first `ReadDirectoryChangesW` on the handle, so
    /// opening a directory never spawns a background thread. `CloseHandle`
    /// removes the entry, which drops the last strong ref and stops the
    /// watcher (the watcher callback holds only a weak ref).
    pub watch_handles: HashMap<u64, Arc<DirectoryWatchObject>>,
    /// Process is dying (`ExitProcess`); workers should stop.
    pub process_dying: bool,
    /// Per-module function tables (image_base → sorted Vec of RuntimeFunction).
    /// Used by `RtlLookupFunctionEntry` to find unwind info for a given RIP.
    /// Seeded by the runtime at session init (`parse_pdata`).
    pub function_tables: HashMap<u64, Vec<crate::exception::RuntimeFunction>>,
    /// Guest pid → kernel handle of its `KernelObject::Process` (children
    /// spawned via `CreateProcessW/A`; the parent's own fixed pid is not
    /// registered). `OpenProcess` resolves through this.
    pub process_by_pid: HashMap<u32, u64>,
    /// Next guest pid for a spawned child (see [`FIRST_CHILD_PID`]).
    pub(crate) next_child_pid: u32,
    /// Per-thread wake inboxes (Painpoint 1): guest TID → parked thread's
    /// channel. Producers (`PostMessage`, `SetTimer`, teardown) send
    /// [`Wake`] tokens; park sites block on their inbox instead of sleeping
    /// through poll quanta. Cloned out as an [`Arc`] so producers never need
    /// the big WinAPI mutex.
    pub wake_hub: crate::wake::WakeHub,
}

/// Detached args for one `WaitForMultipleObjects` host park.
#[derive(Debug, Clone)]
pub struct MultiWaitRequest {
    /// Kernel handles to wait on.
    pub handles: Vec<u64>,
    /// Wait for all (`true`) or any (`false`).
    pub wait_all: bool,
    /// Timeout in ms (`INFINITE` = forever).
    pub timeout_ms: u32,
}

impl SyncState {
    /// Bootstrap empty sync state (primary thread is not a kernel object until needed).
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_handle: KernelHandle::from(0x0000_0000_8000_0001),
            objects: HashMap::new(),
            thread_cpu: HashMap::new(),
            cs_waiters: HashMap::new(),
            next_stack_slot: 1,
            pending_spawns: Vec::new(),
            suspended_spawns: HashMap::new(),
            multi_wait: HashMap::new(),
            watch_handles: HashMap::new(),
            process_dying: false,
            function_tables: HashMap::new(),
            process_by_pid: HashMap::new(),
            next_child_pid: FIRST_CHILD_PID,
            wake_hub: crate::wake::WakeHub::default(),
        }
    }

    fn alloc_handle(&mut self) -> KernelHandle {
        let h = self.next_handle;
        let next = h.as_u64().saturating_add(1);
        self.next_handle = if next == 0 || next == u64::MAX {
            KernelHandle::from(0x0000_0000_8000_0001)
        } else {
            KernelHandle::from(next)
        };
        h
    }

    /// Register a new thread object; returns (handle, Arc body).
    pub fn register_thread(&mut self, tid: u32, ctx: ThreadContext) -> (u64, Arc<ThreadObject>) {
        let handle = self.alloc_handle();
        let handle_u64 = handle.as_u64();
        let obj = Arc::new(ThreadObject {
            tid,
            handle: handle_u64,
            exit_code: std::sync::atomic::AtomicU32::new(STILL_ACTIVE),
            finished: Mutex::new(false),
            finished_cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        self.thread_cpu.insert(tid, ctx);
        self.objects
            .insert(handle, KernelObject::Thread(Arc::clone(&obj)));
        (handle_u64, obj)
    }

    /// Register a Win32 event object.
    pub fn register_event(&mut self, manual_reset: bool, initial: bool) -> (u64, Arc<EventObject>) {
        let handle = self.alloc_handle();
        let handle_u64 = handle.as_u64();
        let obj = Arc::new(EventObject {
            handle: handle_u64,
            manual_reset,
            state: Mutex::new(EventInner { signaled: initial }),
            cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        self.objects
            .insert(handle, KernelObject::Event(Arc::clone(&obj)));
        (handle_u64, obj)
    }

    /// Register a Win32 semaphore (`CreateSemaphore*`).
    pub fn register_semaphore(
        &mut self,
        initial_count: i32,
        maximum_count: i32,
    ) -> (u64, Arc<SemaphoreObject>) {
        let handle = self.alloc_handle();
        let handle_u64 = handle.as_u64();
        let initial = initial_count.clamp(0, maximum_count.max(0));
        let maximum = maximum_count.max(1);
        let obj = Arc::new(SemaphoreObject {
            handle: handle_u64,
            maximum,
            state: Mutex::new(SemaphoreInner { count: initial }),
            cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        self.objects
            .insert(handle, KernelObject::Semaphore(Arc::clone(&obj)));
        (handle_u64, obj)
    }

    /// Register a file mapping (`CreateFileMappingW`).
    ///
    /// The mapping captures the mapped file's guest path and size; the actual
    /// byte copy happens at `MapViewOfFile` time so the file can be re-read
    /// after the source handle closes.
    pub fn register_file_mapping(
        &mut self,
        guest_path: String,
        size: u64,
    ) -> (u64, Arc<FileMappingObject>) {
        let handle = self.alloc_handle();
        let handle_u64 = handle.as_u64();
        let obj = Arc::new(FileMappingObject {
            handle: handle_u64,
            guest_path,
            size,
        });
        self.objects
            .insert(handle, KernelObject::FileMapping(Arc::clone(&obj)));
        (handle_u64, obj)
    }

    /// Register a directory change notification object
    /// (`FindFirstChangeNotification*`).
    ///
    /// The object is registered in the kernel table and therefore waitable;
    /// the notify watcher itself is started by the caller
    /// ([`crate::kernel32::file_io::watch`]) so a failed `watch()` can clean
    /// up before the handle is returned.
    pub fn register_directory_watch(
        &mut self,
        guest_path: &str,
        host_path: PathBuf,
        mask: u32,
    ) -> (u64, Arc<DirectoryWatchObject>) {
        let handle = self.alloc_handle();
        let handle_u64 = handle.as_u64();
        let obj = Arc::new(DirectoryWatchObject::new(guest_path, host_path));
        obj.mask.store(mask, Ordering::Release);
        self.objects
            .insert(handle, KernelObject::DirectoryWatch(Arc::clone(&obj)));
        (handle_u64, obj)
    }

    /// Register a child-process object for guest pid; returns (handle, Arc
    /// body). The object starts with `STILL_ACTIVE`; the child-session
    /// wrapper calls [`ProcessObject::finish`] when the child terminates.
    pub fn register_process(&mut self, pid: u32) -> (u64, Arc<ProcessObject>) {
        let handle = self.alloc_handle();
        let handle_u64 = handle.as_u64();
        let obj = Arc::new(ProcessObject {
            handle: handle_u64,
            pid,
            exit_code: std::sync::atomic::AtomicU32::new(STILL_ACTIVE),
            finished: Mutex::new(false),
            finished_cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        self.process_by_pid.insert(pid, handle_u64);
        self.objects
            .insert(handle, KernelObject::Process(Arc::clone(&obj)));
        (handle_u64, obj)
    }

    /// Register a detached thread object with NO guest CPU-context slot.
    ///
    /// Used for a spawned child's primary-thread handle: the child runs in
    /// its OWN session, so the parent-side object is only a closeable /
    /// waitable handle — inserting a `thread_cpu` entry under the real
    /// `PRIMARY_THREAD_ID` would clobber the parent's own primary context.
    /// `tid` is purely informational (callers pass the child's pid, which
    /// never collides with a live guest tid).
    pub fn register_detached_thread(&mut self, tid: u32) -> (u64, Arc<ThreadObject>) {
        let handle = self.alloc_handle();
        let handle_u64 = handle.as_u64();
        let obj = Arc::new(ThreadObject {
            tid,
            handle: handle_u64,
            exit_code: std::sync::atomic::AtomicU32::new(STILL_ACTIVE),
            finished: Mutex::new(false),
            finished_cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        self.objects
            .insert(handle, KernelObject::Thread(Arc::clone(&obj)));
        (handle_u64, obj)
    }

    /// Look up a thread object by handle.
    pub fn thread_by_handle(&self, handle: u64) -> Option<Arc<ThreadObject>> {
        match self.objects.get(&KernelHandle::from(handle))? {
            KernelObject::Thread(t) => Some(Arc::clone(t)),
            KernelObject::Event(_)
            | KernelObject::Semaphore(_)
            | KernelObject::FileMapping(_)
            | KernelObject::DirectoryWatch(_)
            | KernelObject::Process(_) => None,
        }
    }

    /// Look up a child-process object by handle.
    pub fn process_by_handle(&self, handle: u64) -> Option<Arc<ProcessObject>> {
        match self.objects.get(&KernelHandle::from(handle))? {
            KernelObject::Process(p) => Some(Arc::clone(p)),
            KernelObject::Thread(_)
            | KernelObject::Event(_)
            | KernelObject::Semaphore(_)
            | KernelObject::FileMapping(_)
            | KernelObject::DirectoryWatch(_) => None,
        }
    }

    /// Kernel handle registered for guest pid (child processes only).
    pub fn pid_to_handle(&self, pid: u32) -> Option<u64> {
        self.process_by_pid.get(&pid).copied()
    }

    /// Next guest pid for a spawned child (monotonic, wraps past `u32::MAX`
    /// to the first child pid again — an emulator-only simplification).
    pub fn alloc_child_pid(&mut self) -> u32 {
        let pid = self.next_child_pid;
        self.next_child_pid = if self.next_child_pid == u32::MAX {
            FIRST_CHILD_PID
        } else {
            self.next_child_pid.saturating_add(1)
        };
        pid
    }

    /// Look up any waitable object.
    pub fn object(&self, handle: u64) -> Option<&KernelObject> {
        self.objects.get(&KernelHandle::from(handle))
    }
    /// CS wait queue for guest VA (created on demand).
    pub fn cs_queue(&mut self, cs_va: u64) -> Arc<CsWaitQueue> {
        self.cs_waiters
            .entry(cs_va)
            .or_insert_with(|| {
                Arc::new(CsWaitQueue {
                    lock: Mutex::new(()),
                    cv: Condvar::new(),
                })
            })
            .clone()
    }
}

/// One pending `CreateThread` for the session to spawn as a host OS thread.
#[derive(Debug, Clone)]
pub struct PendingSpawn {
    /// Guest TID.
    pub tid: u32,
    /// Thread handle returned to the creator.
    pub handle: u64,
    /// Start routine guest VA.
    pub start_address: u64,
    /// Parameter in RCX.
    pub parameter: u64,
    /// Guest stack base (mapped).
    pub stack_base: u64,
    /// Guest stack size in bytes.
    pub stack_size: usize,
}

/// Kernel object stored in the handle table.
#[derive(Debug, Clone)]
pub enum KernelObject {
    /// Guest thread (1:1 host thread after spawn).
    Thread(Arc<ThreadObject>),
    /// Auto/manual-reset event.
    Event(Arc<EventObject>),
    /// Counting semaphore.
    Semaphore(Arc<SemaphoreObject>),
    /// A file mapping (`CreateFileMappingW`) — a snapshot of an open file's
    /// bytes that `MapViewOfFile` copies into guest memory.
    FileMapping(Arc<FileMappingObject>),
    /// A directory change notification (`FindFirstChangeNotification*`).
    ///
    /// Kept at the end of the enum so a concurrently-landed kernel-object
    /// variant (e.g. Process) does not shift variant ordering.
    DirectoryWatch(Arc<DirectoryWatchObject>),
    /// A child process spawned via `CreateProcessW/A` (waitable on exit).
    ///
    /// Registered in the PARENT's handle table; the child's own session is a
    /// separate `RuntimeSession` whose termination notifies this object (see
    /// `wie-runtime`'s `ChildProcessSpawnRequested` pump arm).
    Process(Arc<ProcessObject>),
}

/// Guest thread waitable + exit state.
#[derive(Debug)]
pub struct ThreadObject {
    /// Guest TID (`GetCurrentThreadId` for that thread).
    pub tid: u32,
    /// Kernel handle value.
    pub handle: u64,
    /// Exit code or [`STILL_ACTIVE`].
    pub exit_code: std::sync::atomic::AtomicU32,
    /// True after `ExitThread` / natural end.
    pub finished: Mutex<bool>,
    /// Notified when the thread finishes.
    pub finished_cv: Condvar,
    /// Inbox-parked joiners (Painpoint 1): `finish` wakes ALL of them.
    pub waiters: WaiterRegistry,
}

impl ThreadObject {
    /// Mark finished with `code` and wake joiners.
    pub fn finish(&self, code: u32) {
        self.exit_code
            .store(code, std::sync::atomic::Ordering::Release);
        if let Ok(mut g) = self.finished.lock() {
            *g = true;
            self.finished_cv.notify_all();
        }
        // Wake inbox-parked joiners (Painpoint 1). Tokens are hints — a
        // joiner that also waits on something else re-checks and re-parks.
        self.waiters.wake_all(Wake::Shutdown);
    }

    /// Whether the thread has terminated.
    pub fn is_finished(&self) -> bool {
        self.finished.lock().map_or(true, |g| *g)
    }

    /// Block until finished or timeout. Returns true if finished.
    pub fn wait_until_finished(&self, timeout_ms: u32) -> bool {
        let Ok(guard) = self.finished.lock() else {
            return true;
        };
        if *guard {
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        if timeout_ms == INFINITE {
            let mut g = guard;
            while !*g {
                g = match self.finished_cv.wait(g) {
                    Ok(x) => x,
                    Err(p) => p.into_inner(),
                };
            }
            return true;
        }
        let remain = Duration::from_millis(u64::from(timeout_ms));
        let (next, timeout_result) = match self.finished_cv.wait_timeout(guard, remain) {
            Ok(x) => x,
            Err(p) => {
                let (inner, _) = p.into_inner();
                return *inner;
            }
        };
        if *next {
            return true;
        }
        // One-shot wait; for short timeouts this is enough for micros.
        // Extended waits loop with fixed slices.
        if timeout_result.timed_out() {
            return false;
        }
        true
    }
}

/// Guest child-process object (`CreateProcessW/A`), waitable on termination.
///
/// Mirrors [`ThreadObject`]: the exit code starts at [`STILL_ACTIVE`] and the
/// child-session wrapper stores the real code via [`ProcessObject::finish`].
/// A process handle never resets — once finished it stays signaled.
#[derive(Debug)]
pub struct ProcessObject {
    /// Kernel handle value (in the parent's table).
    pub handle: u64,
    /// Guest-visible process id (`PROCESS_INFORMATION.dwProcessId`,
    /// `OpenProcess`).
    pub pid: u32,
    /// Exit code or [`STILL_ACTIVE`].
    pub exit_code: std::sync::atomic::AtomicU32,
    /// True after the child session terminated.
    pub finished: Mutex<bool>,
    /// Notified when the child terminates.
    pub finished_cv: Condvar,
    /// Inbox-parked waiters (Painpoint 1): `finish` wakes ALL of them.
    pub waiters: WaiterRegistry,
}

impl ProcessObject {
    /// Mark finished with `code` and wake waiters.
    pub fn finish(&self, code: u32) {
        self.exit_code
            .store(code, std::sync::atomic::Ordering::Release);
        if let Ok(mut g) = self.finished.lock() {
            *g = true;
            self.finished_cv.notify_all();
        }
        // Wake inbox-parked waiters (Painpoint 1).
        self.waiters.wake_all(Wake::Shutdown);
    }

    /// Whether the child has terminated.
    pub fn is_finished(&self) -> bool {
        self.finished.lock().map_or(true, |g| *g)
    }

    /// Block until finished or timeout. Returns true if finished.
    pub fn wait_until_finished(&self, timeout_ms: u32) -> bool {
        let Ok(guard) = self.finished.lock() else {
            return true;
        };
        if *guard {
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        if timeout_ms == INFINITE {
            let mut g = guard;
            while !*g {
                g = match self.finished_cv.wait(g) {
                    Ok(x) => x,
                    Err(p) => p.into_inner(),
                };
            }
            return true;
        }
        let remain = Duration::from_millis(u64::from(timeout_ms));
        let (next, timeout_result) = match self.finished_cv.wait_timeout(guard, remain) {
            Ok(x) => x,
            Err(p) => {
                let (inner, _) = p.into_inner();
                return *inner;
            }
        };
        if *next {
            return true;
        }
        // Spurious wake without finish: report retry so poll-loop callers
        // re-check rather than returning success on nothing.
        let _ = timeout_result;
        false
    }
}

/// Win32 event object.
#[derive(Debug)]
pub struct EventObject {
    /// Kernel handle.
    pub handle: u64,
    /// Manual-reset vs auto-reset.
    pub manual_reset: bool,
    /// Signaled flag.
    pub state: Mutex<EventInner>,
    /// Waiters.
    pub cv: Condvar,
    /// Inbox-parked waiters (Painpoint 1): manual-reset `set` wakes ALL,
    /// auto-reset `set` wakes exactly ONE (Win32 acquire semantics).
    pub waiters: WaiterRegistry,
}

/// Interior of an event (under mutex).
#[derive(Debug)]
pub struct EventInner {
    /// Whether the event is signaled.
    pub signaled: bool,
}

impl EventObject {
    /// `SetEvent` — signal; wake all (manual) or one (auto).
    pub fn set(&self) {
        if let Ok(mut g) = self.state.lock() {
            g.signaled = true;
            if self.manual_reset {
                self.cv.notify_all();
                // Manual-reset: every waiter may pass — wake all.
                drop(g);
                self.waiters.wake_all(Wake::Shutdown);
            } else {
                self.cv.notify_one();
                // Auto-reset: exactly one acquirer wins the state; one token.
                drop(g);
                self.waiters.wake_one(Wake::Shutdown);
            }
        }
    }

    /// `ResetEvent`.
    pub fn reset(&self) {
        if let Ok(mut g) = self.state.lock() {
            g.signaled = false;
        }
    }

    /// Wait until signaled (auto-reset consumes). Returns false on timeout.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        let Ok(mut guard) = self.state.lock() else {
            return true;
        };
        if guard.signaled {
            if !self.manual_reset {
                guard.signaled = false;
            }
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        if timeout_ms == INFINITE {
            while !guard.signaled {
                guard = match self.cv.wait(guard) {
                    Ok(x) => x,
                    Err(p) => p.into_inner(),
                };
            }
            if !self.manual_reset {
                guard.signaled = false;
            }
            return true;
        }
        let remain = Duration::from_millis(u64::from(timeout_ms));
        let (next, timeout_result) = match self.cv.wait_timeout(guard, remain) {
            Ok(x) => x,
            Err(p) => {
                let (inner, _) = p.into_inner();
                return inner.signaled;
            }
        };
        guard = next;
        if guard.signaled {
            if !self.manual_reset {
                guard.signaled = false;
            }
            return true;
        }
        if timeout_result.timed_out() {
            return false;
        }
        true
    }
}

/// Win32 counting semaphore.
#[derive(Debug)]
pub struct SemaphoreObject {
    /// Kernel handle.
    pub handle: u64,
    /// Maximum count (`lMaximumCount`).
    pub maximum: i32,
    /// Current count under mutex.
    pub state: Mutex<SemaphoreInner>,
    /// Waiters for count &gt; 0.
    pub cv: Condvar,
    /// Inbox-parked waiters (Painpoint 1): `ReleaseSemaphore(n)` wakes ALL
    /// registered waiters (tokens are hints; losers re-check the count).
    pub waiters: WaiterRegistry,
}

/// Interior of a semaphore (under mutex).
#[derive(Debug)]
pub struct SemaphoreInner {
    /// Current count (`0..=maximum`).
    pub count: i32,
}

impl SemaphoreObject {
    /// Non-blocking acquire: true if a unit was taken.
    pub fn try_acquire(&self) -> bool {
        let Ok(mut g) = self.state.lock() else {
            return false;
        };
        if g.count > 0 {
            g.count = g.count.saturating_sub(1);
            true
        } else {
            false
        }
    }

    /// Wait until a unit is available (decrements). Returns false on timeout.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        let Ok(mut guard) = self.state.lock() else {
            return true;
        };
        if guard.count > 0 {
            guard.count = guard.count.saturating_sub(1);
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        if timeout_ms == INFINITE {
            while guard.count <= 0 {
                guard = match self.cv.wait(guard) {
                    Ok(x) => x,
                    Err(p) => p.into_inner(),
                };
            }
            guard.count = guard.count.saturating_sub(1);
            return true;
        }
        let remain = Duration::from_millis(u64::from(timeout_ms));
        let (next, timeout_result) = match self.cv.wait_timeout(guard, remain) {
            Ok(x) => x,
            Err(p) => {
                let (inner, _) = p.into_inner();
                return inner.count > 0;
            }
        };
        guard = next;
        if guard.count > 0 {
            guard.count = guard.count.saturating_sub(1);
            return true;
        }
        if timeout_result.timed_out() {
            return false;
        }
        // Spurious wake: treat as timeout for short waits (caller may retry).
        false
    }

    /// `ReleaseSemaphore` — add `release_count` units. Returns previous count, or `None` if invalid.
    pub fn release(&self, release_count: i32) -> Option<i32> {
        if release_count <= 0 {
            return None;
        }
        let Ok(mut g) = self.state.lock() else {
            return None;
        };
        let prev = g.count;
        let new = prev.checked_add(release_count)?;
        if new > self.maximum {
            return None;
        }
        g.count = new;
        // Wake waiters proportional to units released (notify_all is safe).
        if prev == 0 {
            self.cv.notify_all();
        } else {
            for _ in 0..release_count.min(16) {
                self.cv.notify_one();
            }
        }
        drop(g);
        // ReleaseSemaphore(n): token to ALL registered waiters — losers
        // re-check the count and re-park (hints-never-data).
        self.waiters.wake_all(Wake::Shutdown);
        Some(prev)
    }

    /// Wake all waiters during process teardown (does not change count semantics for dying).
    pub fn notify_all(&self) {
        self.cv.notify_all();
        self.waiters.wake_all(Wake::Shutdown);
    }
}

/// A file-mapping object (`CreateFileMappingW`).
///
/// Carries the mapped file's guest path and size so `MapViewOfFile` can copy
/// its bytes into guest memory. Not waitable (Windows maps never are).
#[derive(Debug)]
pub struct FileMappingObject {
    /// Kernel handle.
    pub handle: u64,
    /// The mapped file's guest path (`C:\...` / `Z:\pick{N}\...`).
    pub guest_path: String,
    /// Mapped size in bytes (the file size at creation).
    pub size: u64,
}

/// One queued change notification for a [`DirectoryWatchObject`].
#[derive(Debug, Clone)]
pub struct FileNotifyRecord {
    /// `FILE_ACTION_*` code (ADDED / REMOVED / MODIFIED / RENAMED_*).
    pub action: u32,
    /// File name relative to the watched directory (guest `WCHAR` units,
    /// `\`-separated for subdirectories under recursive watches).
    pub file_name: String,
}

/// Directory change notification object backing `FindFirstChangeNotification*`
/// and `ReadDirectoryChangesW`.
///
/// The notify-crate watcher lives inside the object behind
/// `Arc<Mutex<Option<_>>>` so the object stays `Clone` (the kernel table
/// derives `Clone`). The watcher's callback holds a **weak** reference, so
/// dropping the last strong ref (`FindCloseChangeNotification`, `CloseHandle`
/// of a `ReadDirectoryChangesW` anchor) drops the watcher and stops event
/// delivery without a reference cycle.
///
/// Signaled state: `pending` is non-empty, or the watch was closed
/// (`active == false`). Unlike events, waiting does **not** consume records —
/// `ReadDirectoryChangesW` drains them; `FindNextChangeNotification` resets.
pub struct DirectoryWatchObject {
    /// Kernel handle when registered in the objects table (0 for
    /// `ReadDirectoryChangesW`-only anchors held in [`SyncState::watch_handles`]).
    pub handle: u64,
    /// Guest Windows path of the watched directory (`C:\...`).
    pub guest_path: Arc<str>,
    /// Host path the notify watcher monitors.
    pub host_path: Arc<Path>,
    /// `FILE_NOTIFY_CHANGE_*` filter mask; events not matching are dropped.
    pub mask: AtomicU32,
    /// Pending change records (consumed by `ReadDirectoryChangesW`).
    pub pending: Mutex<Vec<FileNotifyRecord>>,
    /// Waiter notification: a pushed record wakes parked host waits.
    pub cv: Condvar,
    /// Inbox-parked waiters (Painpoint 1): push / deactivate wake ALL.
    pub waiters: WaiterRegistry,
    /// False after close/teardown; wakes waiters so they stop blocking.
    pub active: AtomicBool,
    /// The notify watcher, started lazily. `pub(crate)` so the watch module
    /// (`kernel32/file_io/watch.rs`) can create it via
    /// [`DirectoryWatchObject::start_watching`].
    pub(crate) watcher: Arc<Mutex<Option<notify::RecommendedWatcher>>>,
}

impl std::fmt::Debug for DirectoryWatchObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryWatchObject")
            .field("handle", &self.handle)
            .field("guest_path", &self.guest_path)
            .field("host_path", &self.host_path)
            .field("mask", &self.mask.load(Ordering::Relaxed))
            .field("pending_len", &self.pending.lock().map_or(0, |g| g.len()))
            .field("active", &self.active.load(Ordering::Relaxed))
            .finish()
    }
}

impl DirectoryWatchObject {
    /// Build an inactive watch (no watcher started). `mask` starts at 0, so
    /// no events match until a handler stores the real filter.
    #[must_use]
    pub fn new(guest_path: &str, host_path: PathBuf) -> Self {
        Self {
            handle: 0,
            guest_path: Arc::from(guest_path),
            host_path: Arc::from(host_path.as_path()),
            mask: AtomicU32::new(0),
            pending: Mutex::new(Vec::new()),
            cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
            active: AtomicBool::new(true),
            watcher: Arc::new(Mutex::new(None)),
        }
    }

    /// The waitable "signaled" state: a change is pending, or the watch
    /// closed. A poisoned pending mutex counts as signaled (fail open), like
    /// [`EventObject::wait`].
    pub fn is_signaled(&self) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return true;
        }
        self.pending.lock().map_or(true, |guard| !guard.is_empty())
    }

    /// Non-blocking readiness check (does not consume records).
    pub fn try_wait(&self) -> bool {
        self.is_signaled()
    }

    /// Block until a change is pending or the watch closes. Does **not**
    /// consume records. Returns false only on timeout (or a spurious wake
    /// with no pending records, which the poll-loop callers treat as retry).
    pub fn wait(&self, timeout_ms: u32) -> bool {
        let Ok(mut guard) = self.pending.lock() else {
            return true;
        };
        if !guard.is_empty() || !self.active.load(Ordering::Acquire) {
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        if timeout_ms == INFINITE {
            while guard.is_empty() && self.active.load(Ordering::Acquire) {
                guard = match self.cv.wait(guard) {
                    Ok(next) => next,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
            return !guard.is_empty() || !self.active.load(Ordering::Acquire);
        }
        let remain = Duration::from_millis(u64::from(timeout_ms));
        let (next, timeout_result) = match self.cv.wait_timeout(guard, remain) {
            Ok(pair) => pair,
            Err(poisoned) => {
                let (inner, _) = poisoned.into_inner();
                return !inner.is_empty() || !self.active.load(Ordering::Acquire);
            }
        };
        guard = next;
        if !guard.is_empty() || !self.active.load(Ordering::Acquire) {
            return true;
        }
        // Spurious wake without records: report retry so the 50 ms poll-loop
        // callers re-check rather than returning success on nothing.
        let _ = timeout_result;
        false
    }
    /// `FindNextChangeNotification`: clear pending records so the next wait
    /// re-blocks until NEW changes arrive.
    pub fn reset(&self) {
        if let Ok(mut guard) = self.pending.lock() {
            guard.clear();
        }
    }

    /// `FindCloseChangeNotification` / teardown: stop delivering and wake any
    /// parked waiter (a wait on a closed watch returns satisfied).
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        self.cv.notify_all();
        self.waiters.wake_all(Wake::Shutdown);
    }

    /// Queue one change record and wake parked waiters.
    pub fn push(&self, record: FileNotifyRecord) {
        if let Ok(mut guard) = self.pending.lock() {
            guard.push(record);
            self.cv.notify_all();
            drop(guard);
            self.waiters.wake_all(Wake::Shutdown);
        }
    }
}

/// Wait queue for one guest critical section VA.
#[derive(Debug)]
pub struct CsWaitQueue {
    /// Mutex for condvar (no extra state).
    pub lock: Mutex<()>,
    /// Signaled on Leave when unlocked.
    pub cv: Condvar,
}

impl CsWaitQueue {
    /// Park until Leave notifies, or a short timeout (lost-wakeup safe).
    ///
    /// Callers **must** retry `EnterCriticalSection` after this returns. Never
    /// wait forever without a timeout: Leave may notify before we reach
    /// `wait_timeout` (classic lost wakeup under process-lock serialization).
    pub fn wait_ms(&self, timeout_ms: u64) {
        let Ok(guard) = self.lock.lock() else {
            return;
        };
        drop(
            self.cv
                .wait_timeout(guard, Duration::from_millis(timeout_ms.max(1))),
        );
    }

    /// Preferred park for contended CS: short exponential backoff yielding,
    /// then a brief condvar wait.
    ///
    /// Was a fixed 16× `yield_now` spin before the condvar wait, which burns
    /// CPU under contention with many host threads. Modern schedulers punish
    /// long yield spins; a bounded backoff (2, 4, 8 yields) covers the "peer
    /// unlocks immediately" case without the same cost, then falls through
    /// to `wait_ms(1)` for genuine contention.
    pub fn park_brief(&self) {
        for iters in [2_u32, 4, 8] {
            for _ in 0..iters {
                std::thread::yield_now();
            }
            // No cheap ownership signal to break early; the yields simply give
            // the current CS owner a scheduling slot to Leave. If they haven't
            // released after 14 yields (2+4+8), fall through to condvar wait.
        }
        self.wait_ms(1);
    }

    /// Wake one waiter after Leave unlocks the CS.
    pub fn notify_one(&self) {
        self.cv.notify_one();
    }

    /// Wake all waiters (process dying / teardown).
    pub fn notify_all(&self) {
        self.cv.notify_all();
    }
}

/// Detached wait target so the host can park **without** holding process locks.
///
/// Holding `engine`/`winapi` mutexes while waiting deadlocks workers that need
/// those locks to `ExitThread` / `LeaveCriticalSection` / `SetEvent`.
#[derive(Debug, Clone)]
pub enum WaitTarget {
    /// Thread object (join).
    Thread(Arc<ThreadObject>),
    /// Event object.
    Event(Arc<EventObject>),
    /// Semaphore object.
    Semaphore(Arc<SemaphoreObject>),
    /// Directory change notification object.
    DirectoryWatch(Arc<DirectoryWatchObject>),
    /// Child-process object (signaled when the child session terminated).
    Process(Arc<ProcessObject>),
}

impl WaitTarget {
    /// Non-blocking check / consume. True if the wait would succeed immediately.
    pub fn try_wait(&self) -> bool {
        match self {
            Self::Thread(t) => t.is_finished(),
            Self::Event(e) => e.wait(0),
            Self::Semaphore(s) => s.try_acquire(),
            Self::DirectoryWatch(d) => d.try_wait(),
            Self::Process(p) => p.is_finished(),
        }
    }

    /// Register an inbox-parked waiter on this object (wait-enter).
    ///
    /// Call BEFORE the first readiness check: a signal landing between the
    /// check and a later park then always delivers a token (no lost wakeup).
    pub fn enter_wait(&self, inbox: &ThreadInbox) {
        match self {
            Self::Thread(t) => t.waiters.enter(inbox),
            Self::Event(e) => e.waiters.enter(inbox),
            Self::Semaphore(s) => s.waiters.enter(inbox),
            Self::DirectoryWatch(d) => d.waiters.enter(inbox),
            Self::Process(p) => p.waiters.enter(inbox),
        }
    }

    /// Unregister a parked waiter on this object (wait-exit).
    pub fn exit_wait(&self, inbox: &ThreadInbox) {
        match self {
            Self::Thread(t) => t.waiters.exit(inbox),
            Self::Event(e) => e.waiters.exit(inbox),
            Self::Semaphore(s) => s.waiters.exit(inbox),
            Self::DirectoryWatch(d) => d.waiters.exit(inbox),
            Self::Process(p) => p.waiters.exit(inbox),
        }
    }

    /// Block until signaled / finished. Returns `WAIT_*` codes.
    pub fn wait(&self, timeout_ms: u32) -> u32 {
        match self {
            Self::Thread(t) => {
                if t.wait_until_finished(timeout_ms) {
                    WAIT_OBJECT_0
                } else {
                    WAIT_TIMEOUT
                }
            }
            Self::Event(e) => {
                if e.wait(timeout_ms) {
                    WAIT_OBJECT_0
                } else {
                    WAIT_TIMEOUT
                }
            }
            Self::Semaphore(s) => {
                if s.wait(timeout_ms) {
                    WAIT_OBJECT_0
                } else {
                    WAIT_TIMEOUT
                }
            }
            Self::DirectoryWatch(d) => {
                if d.wait(timeout_ms) {
                    WAIT_OBJECT_0
                } else {
                    WAIT_TIMEOUT
                }
            }
            Self::Process(p) => {
                if p.wait_until_finished(timeout_ms) {
                    WAIT_OBJECT_0
                } else {
                    WAIT_TIMEOUT
                }
            }
        }
    }
}

/// Maximum handles for `WaitForMultipleObjects` (Windows `MAXIMUM_WAIT_OBJECTS`).
pub const MAXIMUM_WAIT_OBJECTS: usize = 64;

/// Wait on multiple detached targets (any or all). Parks event-driven on the
/// waiter registries: every target's signal delivers a token that wakes the
/// recheck loop, so there are no fixed poll slices. The process lock is never
/// held while sleeping.
///
/// Returns `WAIT_OBJECT_0 + index` for wait-any, `WAIT_OBJECT_0` for wait-all,
/// or `WAIT_TIMEOUT` / `WAIT_FAILED`.
pub fn wait_multiple(targets: &[WaitTarget], wait_all: bool, timeout_ms: u32) -> u32 {
    if targets.is_empty() || targets.len() > MAXIMUM_WAIT_OBJECTS {
        return WAIT_FAILED;
    }

    if let Some(code) = multi_try_once(targets, wait_all) {
        return code;
    }
    if timeout_ms == 0 {
        return WAIT_TIMEOUT;
    }

    let infinite = timeout_ms == INFINITE;
    let deadline = (!infinite).then(|| {
        std::time::Instant::now()
            .checked_add(Duration::from_millis(u64::from(timeout_ms)))
            .unwrap_or_else(std::time::Instant::now)
    });

    // Register BEFORE the first re-check (enter → check → park ordering): a
    // signal landing after registration always delivers a token, and a signal
    // landing before it is covered by the check itself.
    let inbox = crate::wake::ThreadInbox::new();
    for target in targets {
        target.enter_wait(&inbox);
    }
    let result = loop {
        if let Some(code) = multi_try_once(targets, wait_all) {
            break code;
        }
        match deadline {
            Some(dl) if std::time::Instant::now() >= dl => break WAIT_TIMEOUT,
            _ => {}
        }
        // Bounded tick: tokens wake early; the cap only bounds shutdown
        // latency (this function has no dying-observation hook of its own —
        // callers driving INFINITE waits layer that outside).
        inbox.wait_bounded(deadline, Duration::from_millis(50));
    };
    for target in targets {
        target.exit_wait(&inbox);
    }
    result
}

/// One non-blocking multi-wait attempt. `Some` if satisfied.
pub fn wait_multiple_step(targets: &[WaitTarget], wait_all: bool) -> Option<u32> {
    multi_try_once(targets, wait_all)
}

/// One non-blocking multi-wait attempt (internal). `Some` if satisfied.
fn multi_try_once(targets: &[WaitTarget], wait_all: bool) -> Option<u32> {
    if wait_all {
        // Threads: readiness is non-destructive. Events/semaphores consume on
        // acquire — only acquire after every thread is finished; if a later
        // acquire fails, earlier units stay taken (emulator limitation).
        for t in targets {
            if let WaitTarget::Thread(th) = t
                && !th.is_finished()
            {
                return None;
            }
        }
        for t in targets {
            if !t.try_wait() {
                return None;
            }
        }
        Some(WAIT_OBJECT_0)
    } else {
        for (i, t) in targets.iter().enumerate() {
            if t.try_wait() {
                return Some(WAIT_OBJECT_0.saturating_add(u32::try_from(i).unwrap_or(0)));
            }
        }
        None
    }
}

impl SyncState {
    /// Clone a waitable handle into a [`WaitTarget`] (or `None` if invalid).
    pub fn wait_target(&self, handle: u64) -> Option<WaitTarget> {
        match self.objects.get(&KernelHandle::from(handle))? {
            KernelObject::Thread(t) => Some(WaitTarget::Thread(Arc::clone(t))),
            KernelObject::Event(e) => Some(WaitTarget::Event(Arc::clone(e))),
            KernelObject::Semaphore(s) => Some(WaitTarget::Semaphore(Arc::clone(s))),
            // File mappings are not waitable.
            KernelObject::FileMapping(_) => None,
            KernelObject::DirectoryWatch(d) => Some(WaitTarget::DirectoryWatch(Arc::clone(d))),
            KernelObject::Process(p) => Some(WaitTarget::Process(Arc::clone(p))),
        }
    }

    /// Build wait targets for a handle list; `None` if any handle is invalid.
    pub fn wait_targets(&self, handles: &[u64]) -> Option<Vec<WaitTarget>> {
        let mut out = Vec::with_capacity(handles.len());
        for &h in handles {
            out.push(self.wait_target(h)?);
        }
        Some(out)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod wait_registry_tests {
    use super::*;
    use crate::wake::ThreadInbox;
    use std::time::{Duration, Instant};

    /// A manual-reset `SetEvent` delivers a token to every inbox-parked
    /// waiter; an auto-reset set delivers exactly one.
    #[test]
    fn event_set_wakes_registered_inboxes_manual_all_auto_one() {
        let manual = Arc::new(EventObject {
            handle: 0xA001,
            manual_reset: true,
            state: Mutex::new(EventInner { signaled: false }),
            cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        let a = ThreadInbox::new();
        let b = ThreadInbox::new();
        manual.waiters.enter(&a);
        manual.waiters.enter(&b);
        manual.set();
        assert_eq!(a.drain(), 1, "manual reset wakes A");
        assert_eq!(b.drain(), 1, "manual reset wakes B");

        let auto = Arc::new(EventObject {
            handle: 0xA002,
            manual_reset: false,
            state: Mutex::new(EventInner { signaled: false }),
            cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        auto.waiters.enter(&a);
        auto.waiters.enter(&b);
        auto.set();
        let woke_a = a.drain();
        let woke_b = b.drain();
        assert_eq!(
            woke_a + woke_b,
            1,
            "auto reset selects exactly one acquirer"
        );
    }

    /// `ReleaseSemaphore` and thread finish deliver tokens to ALL registered
    /// waiters; `exit_wait` stops delivery.
    #[test]
    fn semaphore_release_and_thread_finish_wake_all_registered() {
        let sem = Arc::new(SemaphoreObject {
            handle: 0xB001,
            maximum: 4,
            state: Mutex::new(SemaphoreInner { count: 0 }),
            cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        let a = ThreadInbox::new();
        let b = ThreadInbox::new();
        sem.waiters.enter(&a);
        sem.waiters.enter(&b);
        sem.release(1).expect("release");
        assert_eq!(a.drain(), 1, "semaphore release wakes A");
        assert_eq!(b.drain(), 1, "semaphore release wakes B");

        let (handle, thread) = SyncState::new().register_thread(7, ThreadContext::default());
        let target = crate::WaitTarget::Thread(Arc::clone(&thread));
        let joiner_a = ThreadInbox::new();
        let joiner_b = ThreadInbox::new();
        target.enter_wait(&joiner_a);
        target.enter_wait(&joiner_b);
        thread.finish(0);
        assert_eq!(joiner_a.drain(), 1, "finish wakes parked joiner A");
        assert_eq!(joiner_b.drain(), 1, "finish wakes parked joiner B");
        assert!(handle > 0, "registered thread returned a handle");

        // exit removes the entry: later signals deliver nothing.
        target.exit_wait(&joiner_a);
        thread.finish(1); // idempotent finish still broadcasts — but A exited
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(joiner_a.drain(), 0, "exited waiter receives nothing");
    }

    /// The rewritten `wait_multiple` returns promptly when one of its targets
    /// is signaled from another host thread (event-driven, no poll slices).
    #[test]
    fn wait_multiple_wakes_on_signal_within_bound() {
        let mut sync = SyncState::new();
        let (_h_event, event) = sync.register_event(true, false);
        let (_h_thread, thread) = sync.register_thread(9, ThreadContext::default());
        let targets = vec![
            crate::WaitTarget::Event(Arc::clone(&event)),
            crate::WaitTarget::Thread(Arc::clone(&thread)),
        ];
        let signaler = {
            let event = Arc::clone(&event);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                event.set();
            })
        };
        let t0 = Instant::now();
        let result = wait_multiple(&targets, false, 5_000);
        assert_eq!(result, WAIT_OBJECT_0, "the event index satisfied the wait");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "signal-driven wake must be prompt, took {:?}",
            t0.elapsed()
        );
        signaler.join().expect("signaler");
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod directory_watch_tests {
    use super::*;

    fn watch_obj() -> DirectoryWatchObject {
        DirectoryWatchObject::new("C:\\watch", PathBuf::from("/tmp/watch"))
    }

    #[test]
    fn directory_watch_wait_reset_semantics() {
        let obj = Arc::new(watch_obj());
        assert!(!obj.try_wait(), "fresh watch is not signaled");
        assert!(!obj.wait(0), "zero-timeout wait on a fresh watch times out");

        obj.push(FileNotifyRecord {
            action: FILE_ACTION_ADDED,
            file_name: "x.txt".into(),
        });
        assert!(obj.try_wait(), "pushed record signals the watch");
        // Waiting must NOT consume: FindNextChangeNotification owns the reset.
        assert!(obj.wait(0), "wait is satisfied while records are pending");
        assert!(obj.try_wait(), "records survive the wait");

        obj.reset();
        assert!(!obj.try_wait(), "reset clears the signaled state");

        obj.push(FileNotifyRecord {
            action: FILE_ACTION_REMOVED,
            file_name: "x.txt".into(),
        });
        let waiter = {
            let obj = Arc::clone(&obj);
            std::thread::spawn(move || obj.wait(5_000))
        };
        assert!(
            waiter.join().expect("waiter thread"),
            "push wakes a parked waiter"
        );
        obj.deactivate();
        assert!(obj.try_wait(), "a closed watch reads as signaled");
        assert!(obj.wait(0), "zero-timeout wait on a closed watch succeeds");
    }

    #[test]
    fn directory_watch_kernel_object_wait_target() {
        let mut sync = SyncState::new();
        let (handle, obj) =
            sync.register_directory_watch("C:\\watch", PathBuf::from("/tmp/watch"), 0);
        assert!(
            sync.wait_target(handle).is_some(),
            "watch handles resolve to a wait target"
        );
        obj.push(FileNotifyRecord {
            action: FILE_ACTION_MODIFIED,
            file_name: "y.txt".into(),
        });
        let target = sync.wait_target(handle).expect("wait target");
        assert!(target.try_wait(), "signaled watch is immediately waitable");
        let result = target.wait(0);
        assert_eq!(
            result, WAIT_OBJECT_0,
            "wait on a signaled watch returns WAIT_OBJECT_0"
        );
    }

    #[test]
    fn process_object_wait_semantics() {
        let obj = Arc::new(ProcessObject {
            handle: 0x1234,
            pid: FIRST_CHILD_PID,
            exit_code: std::sync::atomic::AtomicU32::new(STILL_ACTIVE),
            finished: Mutex::new(false),
            finished_cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        assert!(!obj.is_finished(), "a fresh child is running");
        assert_eq!(
            obj.exit_code.load(std::sync::atomic::Ordering::Acquire),
            STILL_ACTIVE,
            "STILL_ACTIVE until the child terminates"
        );
        assert!(!obj.wait_until_finished(0), "zero-timeout wait times out");
        assert!(!obj.wait_until_finished(1), "1 ms wait times out");

        obj.finish(42);
        assert!(obj.is_finished(), "finish marks the child terminated");
        assert_eq!(
            obj.exit_code.load(std::sync::atomic::Ordering::Acquire),
            42,
            "finish stores the exit code"
        );
        // A process handle never resets: it stays signaled forever.
        assert!(
            obj.wait_until_finished(0),
            "finished is immediately waitable"
        );
        assert!(
            obj.wait_until_finished(5_000),
            "a long wait on a finished process returns immediately"
        );

        // Parked waiter is woken by finish from another thread.
        let waiter_obj = Arc::new(ProcessObject {
            handle: 0x1235,
            pid: FIRST_CHILD_PID + 1,
            exit_code: std::sync::atomic::AtomicU32::new(STILL_ACTIVE),
            finished: Mutex::new(false),
            finished_cv: Condvar::new(),
            waiters: WaiterRegistry::default(),
        });
        let waiter_clone = Arc::clone(&waiter_obj);
        let waiter = std::thread::spawn(move || waiter_clone.wait_until_finished(5_000));
        std::thread::sleep(Duration::from_millis(20));
        waiter_obj.finish(7);
        assert!(
            waiter.join().expect("waiter thread"),
            "finish wakes a parked waiter"
        );
    }

    #[test]
    fn process_pid_map_round_trip() {
        let mut sync = SyncState::new();
        let first_pid = sync.alloc_child_pid();
        assert!(
            first_pid >= FIRST_CHILD_PID,
            "child pids start after the parent's fixed pid"
        );
        let second_pid = sync.alloc_child_pid();
        assert_ne!(first_pid, second_pid, "pids are monotonic");

        let (handle, obj) = sync.register_process(first_pid);
        assert_eq!(
            sync.pid_to_handle(first_pid),
            Some(handle),
            "pid maps to the registered handle"
        );
        assert_eq!(
            sync.process_by_handle(handle).map(|p| p.pid),
            Some(first_pid),
            "handle maps back to the pid"
        );
        assert_eq!(
            obj.exit_code.load(std::sync::atomic::Ordering::Acquire),
            STILL_ACTIVE,
            "a fresh process reports STILL_ACTIVE"
        );
        assert_eq!(
            sync.pid_to_handle(second_pid),
            None,
            "unregistered pids do not resolve"
        );
        assert!(
            sync.process_by_handle(0).is_none(),
            "handle 0 is never a process"
        );
        // Process objects participate in the wait-target table.
        assert!(
            sync.wait_target(handle).is_some(),
            "process handles resolve to a wait target"
        );
        obj.finish(3);
        let target = sync.wait_target(handle).expect("process wait target");
        assert!(
            target.try_wait(),
            "a finished process is immediately waitable"
        );
        assert_eq!(target.wait(0), WAIT_OBJECT_0);
    }
}
