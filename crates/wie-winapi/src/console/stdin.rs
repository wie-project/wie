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

use crate::state::FileIoState;

/// Cap for a single host console line fill (safety against huge pastes).
pub(crate) const MAX_HOST_STDIN_LINE: usize = 64 * 1024;

/// Read one line from host stdin (through `\n` or EOF), capped at
/// [`MAX_HOST_STDIN_LINE`].
///
/// Models Microsoft Learn default console line input (`ENABLE_LINE_INPUT`):
/// `ReadFile` on a console handle does not complete until a carriage return is
/// entered. On Unix hosts we treat `\n` as the line terminator.
///
/// Returns `Ok(None)` on host EOF with no bytes, `Err(())` on a host I/O error.
pub(crate) fn read_host_console_stdin_line() -> Result<Option<Vec<u8>>, ()> {
    use std::io::Read;

    let mut out = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    let mut stdin = std::io::stdin().lock();
    loop {
        if out.len() >= MAX_HOST_STDIN_LINE {
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
pub(crate) fn refill_stdin_from_host(file_io: &mut FileIoState) -> Result<bool, ()> {
    match read_host_console_stdin_line() {
        Ok(None) => Ok(false),
        Ok(Some(line)) => {
            file_io.stdin_bytes = line;
            file_io.stdin_cursor = 0;
            Ok(true)
        }
        Err(()) => Err(()),
    }
}
