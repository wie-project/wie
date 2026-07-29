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
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the `SIGWINCH` handler; drained by [`resize_pending`].
static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

/// Set by the `SIGINT` handler; drained by [`drain_ctrlc`].
static CTRLC_PENDING: AtomicBool = AtomicBool::new(false);

/// True while the host terminal is in cbreak mode and needs restoring.
static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Guards one-time `atexit` / signal-handler installation.
static HOOKS_INSTALLED: AtomicBool = AtomicBool::new(false);

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
    #[expect(clippy::as_conversions)]
    fn sighandler_addr(handler: extern "C" fn(libc::c_int)) -> libc::sighandler_t {
        handler as *const () as libc::sighandler_t
    }

    /// Install the `atexit` and signal hooks exactly once.
    fn install_hooks() {
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
pub(crate) fn drain_ctrlc() -> bool {
    CTRLC_PENDING.swap(false, Ordering::Acquire)
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
