//! Host terminal control: raw mode, size query, resize notification, raw reads.
//!
//! Every Windows console API above this layer is expressed in terms of these
//! primitives. The guest never sees a host fd — it sees the fake console
//! handles from [`crate::kernel32`], and this module is the only place that
//! touches `termios`, `ioctl`, or `poll`.
//!
//! State here is process-global rather than part of `ConsoleState`, because a
//! terminal is a process-wide resource: `WinApiState` is cloned per guest
//! thread, and cloning a saved `termios` per thread would let two threads race
//! to restore different snapshots.

#[cfg(unix)]
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the `SIGWINCH` handler; drained by [`resize_pending`].
static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

/// Set by the `SIGINT` handler; drained by [`drain_ctrlc`].
static CTRLC_PENDING: AtomicBool = AtomicBool::new(false);

/// True while the host terminal is in cbreak mode and needs restoring.
static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Guards one-time `atexit` / signal-handler installation.
static HOOKS_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Cached-once presence of `WIE_RUNTIME_PROFILE` in the environment: when set,
/// Ctrl+C stops the whole emulated session (profile report + exit 130) instead
/// of being delivered to the guest as a Ctrl+C key event.
///
/// Cached in a [`OnceLock`] because [`take_ctrlc_for_profile_stop`] runs on
/// every quantum boundary of the runtime pump — a per-quantum env lookup would
/// be measurable, and the variable is never changed after startup.
static PROFILE_SIGINT_GATE: OnceLock<bool> = OnceLock::new();

/// Test-only override for [`PROFILE_SIGINT_GATE`]: mutating the real process
/// environment from tests is racy under a threaded harness (`set_var` is
/// `unsafe` and observable from other threads), so tests flip this instead.
/// `Some(true)`/`Some(false)` forces the gate; `None` falls back to the env.
#[cfg(test)]
static GATE_OVERRIDE: Mutex<Option<bool>> = Mutex::new(None);

/// Serializes the gate-override tests so they cannot interleave when run
/// under a threaded harness (nextest's per-process isolation already makes
/// this redundant there, but plain `cargo test` shares one process).
#[cfg(test)]
static GATE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Windows default console dimensions, used when the host is not a terminal
/// (piped stdout in the test suite) so behaviour stays deterministic.
pub(crate) const FALLBACK_COLUMNS: u16 = 80;
pub(crate) const FALLBACK_ROWS: u16 = 25;

#[cfg(unix)]
mod imp {
    use super::{
        CTRLC_PENDING, FALLBACK_COLUMNS, FALLBACK_ROWS, HOOKS_INSTALLED, Mutex, Ordering,
        RAW_ACTIVE, RESIZE_PENDING,
    };

    /// Terminal settings captured before the first switch into cbreak mode.
    ///
    /// `libc::termios` is plain data, so a `Mutex` is enough to make it shared
    /// state; there is no handle ownership to track.
    static SAVED_TERMIOS: Mutex<Option<libc::termios>> = Mutex::new(None);

    /// `SIGWINCH` handler: async-signal-safe (a single relaxed atomic store).
    extern "C" fn on_sigwinch(_signal: libc::c_int) {
        RESIZE_PENDING.store(true, Ordering::Relaxed);
    }

    /// `SIGINT` handler: set a flag so the console pump can deliver Ctrl+C
    /// as an `INPUT_RECORD` rather than killing the emulator.
    ///
    /// When `ENABLE_PROCESSED_INPUT` is off the byte `0x03` arrives through
    /// the normal `read` path anyway, so this handler is the fallback that
    /// catches the signal when `ISIG` is on.
    ///
    /// This handler must stay async-signal-safe, so it only does an atomic
    /// store. In particular it must not call [`restore_now`]: that function
    /// locks `SAVED_TERMIOS`, and taking a `std::sync::Mutex` inside a signal
    /// handler can deadlock if the handler interrupts the thread holding the
    /// lock. Restoring the terminal is deferred to [`drain_ctrlc`], which runs
    /// in normal context on the guest thread.
    extern "C" fn on_sigint(_signal: libc::c_int) {
        CTRLC_PENDING.store(true, Ordering::Release);
    }

    /// `SIGTERM` handler: restore the terminal, then die by the default action.
    extern "C" fn on_fatal_signal(signal: libc::c_int) {
        restore_now();
        // SAFETY: reinstalling the default disposition and re-raising is the
        // documented way to exit with the signal's own status. Both calls take
        // integers only — no pointers, no allocation.
        #[expect(unsafe_code)]
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    /// `atexit` hook so a normal return from `main` also restores the terminal.
    extern "C" fn on_exit() {
        restore_now();
    }

    /// True when stdin is a terminal. Console APIs degrade to line-buffered
    /// behaviour when it is not (piped input under the micro-suite).
    pub(crate) fn is_tty() -> bool {
        // SAFETY: `isatty` reads only the fd number and never touches memory.
        #[expect(unsafe_code)]
        let rc = unsafe { libc::isatty(libc::STDIN_FILENO) };
        rc == 1
    }

    /// Current terminal size as `(columns, rows)`.
    ///
    /// Falls back to Windows' 80x25 default when stdout is not a terminal, so a
    /// guest sizing itself to the console still gets a sane, fixed answer under
    /// a pipe.
    pub(crate) fn window_size() -> (u16, u16) {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `TIOCGWINSZ` writes exactly one `winsize` through the pointer,
        // which points at a live local for the duration of the call.
        #[expect(unsafe_code)]
        let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &raw mut size) };
        if rc != 0 || size.ws_col == 0 || size.ws_row == 0 {
            return (FALLBACK_COLUMNS, FALLBACK_ROWS);
        }
        (size.ws_col, size.ws_row)
    }

    /// `signal(2)` takes the handler as an integer-typed `sighandler_t`, so the
    /// function pointer has to be laundered through a data pointer first.
    fn sighandler_addr(handler: extern "C" fn(libc::c_int)) -> libc::sighandler_t {
        handler as *const () as libc::sighandler_t
    }

    /// Install the `atexit` and signal hooks exactly once.
    pub(crate) fn install_hooks() {
        if HOOKS_INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        // SAFETY: all three take a plain `extern "C"` function pointer with the
        // signature the C library expects. `atexit` returns non-zero on failure,
        // which we tolerate — the explicit restore paths still run.
        #[expect(unsafe_code)]
        unsafe {
            libc::atexit(on_exit);
            libc::signal(libc::SIGWINCH, sighandler_addr(on_sigwinch));
            libc::signal(libc::SIGINT, sighandler_addr(on_sigint));
            libc::signal(libc::SIGTERM, sighandler_addr(on_fatal_signal));
        }
    }

    /// Read the current terminal attributes for stdin.
    fn current_termios() -> Option<libc::termios> {
        let mut raw = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `tcgetattr` fully initialises the struct on success; we only
        // call `assume_init` after checking the return code.
        #[expect(unsafe_code)]
        let rc = unsafe { libc::tcgetattr(libc::STDIN_FILENO, raw.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }
        // SAFETY: `tcgetattr` returned success, so the struct is initialised.
        #[expect(unsafe_code)]
        let value = unsafe { raw.assume_init() };
        Some(value)
    }

    /// Apply terminal attributes to stdin, discarding pending output changes.
    fn apply_termios(settings: &libc::termios) -> bool {
        // SAFETY: `settings` points at a live, fully initialised `termios`;
        // `tcsetattr` copies from it and does not retain the pointer.
        #[expect(unsafe_code)]
        let rc = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, settings) };
        rc == 0
    }

    /// Switch the host terminal into cbreak mode.
    ///
    /// This is *not* full raw mode: `c_oflag` keeps `OPOST`/`ONLCR` so a guest
    /// `"\n"` still lands at column 0 of the next line, matching what
    /// `ENABLE_PROCESSED_OUTPUT` does on a Windows console.
    ///
    /// `processed_input` mirrors `ENABLE_PROCESSED_INPUT`: when set, `ISIG`
    /// stays on so Ctrl+C raises `SIGINT` (routed to the guest's console
    /// control handler); when clear, Ctrl+C arrives as an ordinary key event.
    pub(crate) fn enter_raw(processed_input: bool) -> bool {
        if !is_tty() {
            return false;
        }
        install_hooks();

        let Some(original) = current_termios() else {
            return false;
        };
        if let Ok(mut saved) = SAVED_TERMIOS.lock()
            && saved.is_none()
        {
            *saved = Some(original);
        }

        let mut settings = original;
        settings.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL | libc::IEXTEN);
        if processed_input {
            settings.c_lflag |= libc::ISIG;
        } else {
            settings.c_lflag &= !libc::ISIG;
        }
        // ICRNL would turn the Enter key's CR into LF; the input decoder wants
        // the unmodified CR so it can map it to VK_RETURN unambiguously.
        settings.c_iflag &= !(libc::ICRNL | libc::IXON);
        // VMIN=0 / VTIME=0: `read` returns immediately with whatever is
        // buffered. Blocking is expressed with `poll` instead, so a guest
        // waiting on input can still observe SIGWINCH.
        if let Some(slot) = settings.c_cc.get_mut(libc::VMIN) {
            *slot = 0;
        }
        if let Some(slot) = settings.c_cc.get_mut(libc::VTIME) {
            *slot = 0;
        }

        if !apply_termios(&settings) {
            return false;
        }
        RAW_ACTIVE.store(true, Ordering::SeqCst);
        true
    }

    /// Restore the saved terminal attributes if cbreak mode is active.
    ///
    /// Idempotent: safe to call from the exit hook after an explicit restore.
    pub(crate) fn restore_now() {
        if !RAW_ACTIVE.swap(false, Ordering::SeqCst) {
            return;
        }
        // Clear the screen and show the cursor so no gameboard content
        // remains visible after the emulated process terminates.
        crate::console::host_term::write_stdout(b"\x1b[2J\x1b[H");
        crate::console::screen::set_cursor_visible(true);
        let saved = match SAVED_TERMIOS.lock() {
            Ok(guard) => *guard,
            // A poisoned lock means a thread panicked mid-update; the snapshot
            // itself is plain data and still valid, so recover it rather than
            // leaving the terminal broken.
            Err(poisoned) => *poisoned.into_inner(),
        };
        if let Some(settings) = saved {
            // Nothing useful to do if the restore fails — the process is on its
            // way out and the fd may already be closed.
            let _restored = apply_termios(&settings);
        }
    }

    /// Block until stdin has bytes ready or `timeout_ms` elapses.
    ///
    /// A negative timeout waits indefinitely. Returns `true` when a subsequent
    /// [`read_stdin`] is expected to produce bytes.
    pub(crate) fn poll_stdin_ready(timeout_ms: i32) -> bool {
        let mut fds = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one-element array described by the count argument; `poll`
        // writes back only `revents` on the live local.
        #[expect(unsafe_code)]
        let rc = unsafe { libc::poll(&raw mut fds, 1, timeout_ms) };
        rc > 0 && (fds.revents & libc::POLLIN) != 0
    }

    /// Read whatever stdin has buffered, returning the byte count.
    ///
    /// Never blocks once [`enter_raw`] has set `VMIN`/`VTIME` to zero.
    pub(crate) fn read_stdin(buffer: &mut [u8]) -> usize {
        if buffer.is_empty() {
            return 0;
        }
        // SAFETY: `read` writes at most `buffer.len()` bytes into the slice's
        // own allocation and does not retain the pointer.
        #[expect(unsafe_code)]
        let got = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
            )
        };
        usize::try_from(got).unwrap_or(0)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{FALLBACK_COLUMNS, FALLBACK_ROWS};

    pub(crate) fn is_tty() -> bool {
        false
    }
    pub(crate) fn window_size() -> (u16, u16) {
        (FALLBACK_COLUMNS, FALLBACK_ROWS)
    }
    pub(crate) fn enter_raw(_processed_input: bool) -> bool {
        false
    }
    pub(crate) fn restore_now() {}
    pub(crate) fn poll_stdin_ready(_timeout_ms: i32) -> bool {
        false
    }
    pub(crate) fn read_stdin(_buffer: &mut [u8]) -> usize {
        0
    }
}

// `poll_stdin_ready` / `read_stdin` are consumed by the Tier 2 input decoder.
#[allow(unused_imports)]
pub(crate) use imp::{enter_raw, is_tty, poll_stdin_ready, read_stdin, restore_now, window_size};

/// Take the pending-resize flag, clearing it.
///
/// The `SIGWINCH` handler only sets a flag; the `WINDOW_BUFFER_SIZE_EVENT`
/// record is synthesised later on the guest thread, where allocation is legal.
pub(crate) fn resize_pending() -> bool {
    RESIZE_PENDING.swap(false, Ordering::Relaxed)
}

/// Drain the `SIGINT` → Ctrl+C flag, returning `true` if a Ctrl+C key is
/// pending.
///
/// Called from the input pump before reading terminal bytes, so even when a
/// signal arrives between reads it is not lost.
///
/// While [`profile_sigint_armed`] is true this deliberately returns `false`
/// WITHOUT swapping: under `WIE_RUNTIME_PROFILE` the Ctrl+C belongs to the
/// profiling-stop path ([`take_ctrlc_for_profile_stop`]), and a guest key
/// delivery here would race it — whoever swapped first would silently eat the
/// interrupt. One consumer per mode keeps the ownership total.
///
/// The terminal is restored here, in normal context, the moment a Ctrl+C is
/// recognized — before the guest ever sees the event. The `SIGINT` handler
/// cannot do this (see [`on_sigint`] for the async-signal-safety argument),
/// and leaving the host in cbreak mode after the guest exits would break the
/// user's shell: echo disabled, no CR conversion. [`restore_now`] is
/// idempotent and no-ops when raw mode was never entered, so this is safe on
/// every drain.
pub(crate) fn drain_ctrlc() -> bool {
    if profile_sigint_armed() {
        return false;
    }
    let pending = CTRLC_PENDING.swap(false, Ordering::Acquire);
    if pending {
        restore_now();
    }
    pending
}

/// Take the pending `SIGINT` flag for the profiling-stop path, returning
/// `true` when a Ctrl+C should end the whole emulated session.
///
/// The single consumer of the flag while `WIE_RUNTIME_PROFILE` is armed; the
/// runtime pump calls it once per quantum boundary, so responsiveness is
/// bounded by the next API-stop boundary.
///
/// When the gate is off this returns `false` WITHOUT touching
/// `CTRLC_PENDING`: the flag then belongs to [`drain_ctrlc`] (guest Ctrl+C key
/// delivery), and even reading-and-restoring it here could swallow an
/// interrupt that arrived between the pump's drains. The disabled-gate cost
/// is one cached-bool load ([`PROFILE_SIGINT_GATE`]) — no env access in the
/// hot loop.
///
/// When the gate is on and a signal is pending, the terminal is restored
/// first, in normal context, for the same reason as in [`drain_ctrlc`]: the
/// handler must stay async-signal-safe (store only), and [`restore_now`] is
/// idempotent so repeated takes stay safe.
pub(crate) fn take_ctrlc_for_profile_stop() -> bool {
    if !profile_sigint_armed() {
        return false;
    }
    let pending = CTRLC_PENDING.swap(false, Ordering::Acquire);
    if pending {
        restore_now();
    }
    pending
}

/// The cached-once diagnostics-SIGINT gate.
///
/// Armed by `WIE_RUNTIME_PROFILE` (full report) or `WIE_JIT_OPCODE_HISTO=1`
/// (histogram-only): either knob makes Ctrl+C stop the session cleanly so the
/// opt-in diag sections dump instead of the process dying silently.
///
/// Test runs force the value through [`GATE_OVERRIDE`] instead of mutating
/// the process environment (racy under threads); production reads the env
/// exactly once and caches it.
fn profile_sigint_gate() -> bool {
    #[cfg(test)]
    if let Ok(over) = GATE_OVERRIDE.lock()
        && let Some(forced) = *over
    {
        return forced;
    }
    *PROFILE_SIGINT_GATE.get_or_init(|| {
        std::env::var_os("WIE_RUNTIME_PROFILE").is_some()
            || std::env::var_os("WIE_JIT_OPCODE_HISTO").is_some()
    })
}

/// True when profiling is armed (`WIE_RUNTIME_PROFILE` or
/// `WIE_JIT_OPCODE_HISTO` set): Ctrl+C stops the session instead of reaching
/// the guest.
pub(crate) fn profile_sigint_armed() -> bool {
    profile_sigint_gate()
}

/// Install the `atexit` / signal hooks eagerly, before any console session
/// exists.
///
/// [`install_hooks`] normally installs lazily from [`enter_raw`], i.e. only
/// when a console session enters cbreak mode — a micro or `--gui` run never
/// does, and the default `SIGINT` disposition would kill the process with no
/// chance to dump the profile report. The CLI calls this once at startup when
/// [`profile_sigint_armed`] is true; [`HOOKS_INSTALLED`] makes repeats no-ops.
pub(crate) fn ensure_hooks_installed() {
    #[cfg(unix)]
    imp::install_hooks();
}

/// True while the terminal is in cbreak mode.
pub(crate) fn raw_active() -> bool {
    RAW_ACTIVE.load(Ordering::SeqCst)
}

/// Write bytes to host stdout, bypassing `std::io`'s buffer and lock.
///
/// Shares [`crate::ucrt::write_all_fd`] so console output keeps a single write
/// path: buffering here would reorder guest `printf` against cell rendering,
/// and Rust's buffers are not flushed when the guest terminates the process.
pub(crate) fn write_stdout(bytes: &[u8]) {
    #[cfg(unix)]
    crate::ucrt::write_all_fd(libc::STDOUT_FILENO, bytes);
    #[cfg(not(unix))]
    {
        use std::io::Write;
        drop(std::io::stdout().write_all(bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CTRLC_PENDING, GATE_OVERRIDE, GATE_TEST_LOCK, Ordering, take_ctrlc_for_profile_stop,
    };

    /// Force the gate for one test body, restoring the env fallback after.
    /// The lock serializes gate state against the other tests under a
    /// threaded harness; nextest's per-process isolation is the second belt.
    fn with_gate(forced: bool, body: impl FnOnce()) {
        let _serial = GATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *GATE_OVERRIDE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(forced);
        body();
        *GATE_OVERRIDE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// Gate off: the take must report "nothing" AND leave a pending Ctrl+C
    /// intact — the flag belongs to the guest key-delivery drain then, and
    /// stealing it here would lose the guest's Ctrl+C event.
    #[test]
    fn take_when_gate_off_leaves_pending_flag_intact() {
        with_gate(false, || {
            CTRLC_PENDING.store(true, Ordering::Relaxed);
            assert!(!take_ctrlc_for_profile_stop());
            assert!(
                CTRLC_PENDING.load(Ordering::Relaxed),
                "disabled gate must not consume the guest's pending Ctrl+C"
            );
            CTRLC_PENDING.store(false, Ordering::Relaxed);
        });
    }

    /// Gate on: the take consumes a pending Ctrl+C exactly once (the swap is
    /// the whole hand-off) and reports nothing afterwards.
    #[test]
    fn take_when_gate_on_consumes_exactly_once() {
        with_gate(true, || {
            CTRLC_PENDING.store(true, Ordering::Relaxed);
            assert!(take_ctrlc_for_profile_stop());
            assert!(!CTRLC_PENDING.load(Ordering::Relaxed));
            assert!(!take_ctrlc_for_profile_stop(), "second take sees no signal");
        });
    }

    /// The GUEST delivery drain defers to the profiling stop while the gate
    /// is armed (a key-event delivery here would race the take and could eat
    /// the interrupt), and behaves byte-identically to before when it is off.
    #[test]
    fn guest_drain_defers_to_the_profiling_stop() {
        use super::drain_ctrlc;
        with_gate(true, || {
            CTRLC_PENDING.store(true, Ordering::Relaxed);
            assert!(
                !drain_ctrlc(),
                "armed gate must not deliver Ctrl+C to the guest"
            );
            assert!(
                CTRLC_PENDING.load(Ordering::Relaxed),
                "the flag stays pending for the profiling-stop take"
            );
            CTRLC_PENDING.store(false, Ordering::Relaxed);
        });
        with_gate(false, || {
            CTRLC_PENDING.store(true, Ordering::Relaxed);
            assert!(drain_ctrlc(), "gate off keeps legacy guest delivery");
            assert!(!CTRLC_PENDING.load(Ordering::Relaxed));
        });
    }
}
