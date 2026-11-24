//! The console stdin buffer's host-side refill.
//!
//! The buffer itself is [`FileIoState::stdin_bytes`](crate::state::FileIoState)
//! — a file-I/O field — but every *reader* of it is a console call: this is the
//! console's input contract, and ADR-0005 stage 1 put the module that owns
//! `DllId::Console` in charge of it.
//!
//! `ReadFile` on a console handle reaches the same buffer through
//! [`refill_stdin_from_host`], which is why the two operations take
//! `&mut FileIoState` rather than a whole
//! [`WinApiState`](crate::state::WinApiState): the refill never touches another
//! sub-state.
//!
//! [`read_char`] is the third reader: `ucrt`'s `getchar` wanted the buffer's
//! byte-per-line semantics, so the semantics live here (behind
//! [`ConsoleAccess::stdin_read_char`](crate::capability::ConsoleAccess::stdin_read_char))
//! instead of in a handler that could reach every other sub-state.

use crate::GuestStdinMode;
use crate::console::host_term;
use crate::state::FileIoState;

/// Cap for a single host console line fill (safety against huge pastes).
pub(crate) const MAX_HOST_STDIN_LINE: usize = 64 * 1024;

/// Read one line from host stdin (through `\n` or EOF), capped at `max_line`.
///
/// The cap is the caller's policy, not this module's: `ReadConsole`/`ReadFile`
/// buffer up to [`MAX_HOST_STDIN_LINE`], while the `ucrt` character readers
/// have always truncated at 4 KiB (see [`MAX_CHAR_STDIN_LINE`]). Naming it at
/// the call site keeps both policies visible instead of silently picking one.
///
/// Models Microsoft Learn default console line input (`ENABLE_LINE_INPUT`):
/// `ReadFile` on a console handle does not complete until a carriage return is
/// entered. On Unix hosts we treat `\n` as the line terminator.
///
/// Returns `Ok(None)` on host EOF with no bytes, `Err(())` on a host I/O error.
pub(crate) fn read_host_console_stdin_line(max_line: usize) -> Result<Option<Vec<u8>>, ()> {
    use std::io::Read;

    let mut out = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    let mut stdin = std::io::stdin().lock();
    loop {
        if out.len() >= max_line {
            break;
        }
        match stdin.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return Err(()),
        }
        out.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

/// Block on host stdin for one line and store it as the console's input.
///
/// Returns `Ok(true)` if bytes were stored, `Ok(false)` on host EOF, `Err(())`
/// on host I/O failure (the caller sets `ERROR_READ_FAULT`).
pub(crate) fn refill_stdin_from_host(
    file_io: &mut FileIoState,
    max_line: usize,
) -> Result<bool, ()> {
    match read_host_console_stdin_line(max_line) {
        Ok(None) => Ok(false),
        Ok(Some(line)) => {
            file_io.stdin_bytes = line;
            file_io.stdin_cursor = 0;
            Ok(true)
        }
        Err(()) => Err(()),
    }
}

/// Cap for the piped-input line fill of [`read_char`].
///
/// Deliberately NOT [`MAX_HOST_STDIN_LINE`]: the `ucrt` character readers have
/// always truncated a host line at 4 KiB, and moving them behind
/// [`ConsoleAccess`](crate::capability::ConsoleAccess) must not change that.
const MAX_CHAR_STDIN_LINE: usize = 4096;

/// One character with `getchar` semantics: consume the buffered byte, else
/// block for host input, else EOF (`None`).
///
/// Moved here verbatim from `ucrt::stdio::handle_getchar` when that handler was
/// narrowed onto the console capability (ADR-0005 stage 2). Every branch — the
/// buffered byte, the "skip to the end of the line" rule, the `LiveHost` TTY
/// raw read, the piped line fill, and the `InjectOnly` EOF — is this module's
/// input contract, so it belongs next to the buffer it drives rather than in a
/// `ucrt` handler that can reach the whole state.
pub(crate) fn read_char(file_io: &mut FileIoState) -> Option<u8> {
    if file_io.stdin_cursor < file_io.stdin_bytes.len() {
        // Data already in the guest-side buffer.
        let idx = file_io.stdin_cursor;
        let ch = file_io.stdin_bytes[idx];
        if ch == b'\n' {
            file_io.stdin_cursor = idx.wrapping_add(1);
            return Some(b'\n');
        }
        // Not '\n' — skip ahead if there's a newline later in the buffer.
        if let Some(nl_pos) = file_io.stdin_bytes[idx..].iter().position(|&b| b == b'\n') {
            file_io.stdin_cursor = idx.wrapping_add(nl_pos).wrapping_add(1);
            return Some(b'\n');
        }
        file_io.stdin_cursor = idx.wrapping_add(1);
        return Some(ch);
    }
    if file_io.stdin_mode != GuestStdinMode::LiveHost {
        return None; // EOF (InjectOnly)
    }
    // LiveHost mode with a TTY: read raw bytes from stdin.
    // We buffer up to 64 bytes so arrow-key escape sequences
    // (3 bytes each) don't need three separate poll/read rounds.
    if host_term::is_tty() {
        const BUF_CAP: usize = 64;
        let mut buf = [0_u8; BUF_CAP];
        // Only poll when the buffer is empty (subsequent calls after an arrow
        // key will find buffered bytes).
        if file_io.stdin_cursor >= file_io.stdin_bytes.len() {
            file_io.stdin_bytes.clear();
            if host_term::poll_stdin_ready(-1) {
                let n = host_term::read_stdin(&mut buf);
                if n > 0 {
                    file_io.stdin_bytes = buf[..n].to_vec();
                    file_io.stdin_cursor = 0;
                }
            }
        }
        let idx = file_io.stdin_cursor;
        if idx < file_io.stdin_bytes.len() {
            let ch = file_io.stdin_bytes[idx];
            file_io.stdin_cursor = idx.wrapping_add(1);
            return Some(ch);
        }
    } else {
        // Piped input: read a line (like fgets) so shared state works.
        let line = read_char_stdin_line();
        if let Some(&first) = line.first() {
            file_io.stdin_bytes = line;
            file_io.stdin_cursor = 1_usize;
            return Some(first);
        }
    }
    None // EOF
}

/// Read one capped line from host stdin for [`read_char`]'s piped-input branch.
fn read_char_stdin_line() -> Vec<u8> {
    use std::io::Read;

    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    let mut host_stdin = std::io::stdin().lock();
    loop {
        if line.len() >= MAX_CHAR_STDIN_LINE || host_stdin.read(&mut byte).unwrap_or(0) == 0 {
            break;
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    line
}
