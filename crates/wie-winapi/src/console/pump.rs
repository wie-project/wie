//! Host input pump and `INPUT_RECORD` marshalling.
//!
//! Sits between [`super::input`]'s pure byte decoder and the console handlers:
//! it owns the reads from the terminal, the resize bookkeeping, and the layout
//! of the records written back into guest memory.

#![expect(dead_code)]
#![allow(unreachable_pub)]

use super::{Coord, InputRecord, KeyEvent, host_term, input};
use crate::WinApiState;

/// `INPUT_RECORD.EventType` values (wincon.h).
const KEY_EVENT: u16 = 0x0001;
const MOUSE_EVENT: u16 = 0x0002;
const WINDOW_BUFFER_SIZE_EVENT: u16 = 0x0004;

/// Size of one `INPUT_RECORD` in the 64-bit ABI.
///
/// `EventType` (2 bytes) is followed by 2 bytes of padding to align the union,
/// and the largest member — `KEY_EVENT_RECORD` — is 16 bytes 4-byte aligned.
/// Total 20. Getting this wrong shifts every record after the first, so it is
/// asserted against the field offsets below rather than left as a bare number.
pub const INPUT_RECORD_SIZE: usize = 20;

/// Chunk size for one terminal read.
const READ_CHUNK: usize = 1024;

/// Cap on buffered undecoded bytes, so a terminal spewing garbage cannot grow
/// the buffer without bound.
const MAX_PENDING_BYTES: usize = 64 * 1024;

/// Pull whatever the terminal has ready and turn it into queued records.
///
/// `timeout_ms` is passed to `poll`: `0` polls without blocking, a negative
/// value blocks indefinitely. Returns the number of records added.
///
/// Resize detection runs on every call regardless of key input, because
/// `SIGWINCH` is the one event with no bytes attached.
pub fn pump(state: &mut WinApiState, timeout_ms: i32) -> usize {
    let before = state.console().pending_input.len();

    // A resize is reported by signal, so check it even when no bytes arrive.
    if host_term::resize_pending()
        && let Some(size) = state.console().sync_window_size()
        && state.console().input_mode & super::ENABLE_WINDOW_INPUT != 0
    {
        state
            .console()
            .pending_input
            .push_back(InputRecord::WindowBufferSize(size));
    }

    // SIGINT → Ctrl+C key event, delivered even when no bytes are pending.
    if host_term::drain_ctrlc() {
        state.console().pending_input.push_back(InputRecord::Key(KeyEvent {
            key_down: true,
            repeat_count: 1,
            virtual_key_code: 0x43, // 'C'
            virtual_scan_code: 0,
            unit: 3, // Ctrl+C
            control_key_state: super::input::LEFT_CTRL_PRESSED,
        }));
    }

    if host_term::is_tty() && host_term::poll_stdin_ready(timeout_ms) {
        let mut chunk = [0_u8; READ_CHUNK];
        let got = host_term::read_stdin(&mut chunk);
        if got > 0
            && let Some(bytes) = chunk.get(..got)
            && state.console().input_bytes.len() < MAX_PENDING_BYTES
        {
            state.console().input_bytes.extend_from_slice(bytes);
        }
    }

    if !state.console().input_bytes.is_empty() {
        // A full read means more bytes are probably queued behind it, so a
        // trailing ESC should wait rather than resolve as the Escape key.
        let more_coming = state.console().input_bytes.len() >= READ_CHUNK;
        let (records, used) = input::decode(&state.console().input_bytes, more_coming);
        if used > 0 {
            state.console().input_bytes.drain(..used);
        }
        let mouse_enabled = state.console().input_mode & super::ENABLE_MOUSE_INPUT != 0;
        for record in records {
            // Windows filters by mode at the queue, not at ReadConsoleInput.
            let keep = match record {
                InputRecord::Mouse(_) => mouse_enabled,
                InputRecord::WindowBufferSize(_) => {
                    state.console().input_mode & super::ENABLE_WINDOW_INPUT != 0
                }
                InputRecord::Key(_) => true,
            };
            if keep {
                state.console().pending_input.push_back(record);
            }
        }
    }

    state.console().pending_input.len().saturating_sub(before)
}

/// Enable or disable xterm SGR mouse reporting on the host terminal.
///
/// Called when the guest changes `ENABLE_MOUSE_INPUT`, because a terminal sends
/// no mouse bytes at all until asked. Mode 1000 reports button events, 1002
/// adds drag motion, and 1006 switches to the SGR encoding that survives
/// terminals wider than 223 columns.
pub fn set_mouse_reporting(enabled: bool) {
    if !host_term::is_tty() {
        return;
    }
    if enabled {
        host_term::write_stdout(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h");
    } else {
        host_term::write_stdout(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l");
    }
}

/// Serialise one record into the 20-byte `INPUT_RECORD` layout.
///
/// Field offsets follow wincon.h:
/// - `EventType` at 0 (WORD), 2 bytes padding
/// - `KEY_EVENT_RECORD`: `bKeyDown` BOOL at 4, `wRepeatCount` at 8,
///   `wVirtualKeyCode` at 10, `wVirtualScanCode` at 12, `UnicodeChar` at 14,
///   `dwControlKeyState` at 16
/// - `MOUSE_EVENT_RECORD`: `dwMousePosition` COORD at 4, `dwButtonState` at 8,
///   `dwControlKeyState` at 12, `dwEventFlags` at 16
/// - `WINDOW_BUFFER_SIZE_RECORD`: `dwSize` COORD at 4
#[must_use]
pub fn encode_record(record: InputRecord) -> [u8; INPUT_RECORD_SIZE] {
    let mut out = [0_u8; INPUT_RECORD_SIZE];
    match record {
        InputRecord::Key(event) => {
            put_u16(&mut out, 0, KEY_EVENT);
            put_u32(&mut out, 4, u32::from(event.key_down));
            put_u16(&mut out, 8, event.repeat_count);
            put_u16(&mut out, 10, event.virtual_key_code);
            put_u16(&mut out, 12, event.virtual_scan_code);
            put_u16(&mut out, 14, event.unit);
            put_u32(&mut out, 16, event.control_key_state);
        }
        InputRecord::Mouse(event) => {
            put_u16(&mut out, 0, MOUSE_EVENT);
            put_u32(&mut out, 4, event.position.to_packed());
            put_u32(&mut out, 8, event.button_state);
            put_u32(&mut out, 12, event.control_key_state);
            put_u32(&mut out, 16, event.event_flags);
        }
        InputRecord::WindowBufferSize(size) => {
            put_u16(&mut out, 0, WINDOW_BUFFER_SIZE_EVENT);
            put_u32(&mut out, 4, size.to_packed());
        }
    }
    out
}

fn put_u16(buffer: &mut [u8; INPUT_RECORD_SIZE], offset: usize, value: u16) {
    if let Some(slot) = buffer.get_mut(offset..offset.saturating_add(2)) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

fn put_u32(buffer: &mut [u8; INPUT_RECORD_SIZE], offset: usize, value: u32) {
    if let Some(slot) = buffer.get_mut(offset..offset.saturating_add(4)) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

/// Check if a key press is available without consuming it.
/// Used by `_kbhit`, which must not remove the key from the queue.
pub fn peek_key_press(state: &mut WinApiState) -> bool {
    // Pump the terminal first so keys that arrived between frames are seen.
    pump(state, 0);
    state.console().pending_input.iter().any(|record| {
        matches!(record, InputRecord::Key(event) if event.key_down)
    })
}

/// Take the next queued key press, pumping the terminal if the queue is empty.
///
/// Skips releases and non-key events, which is what the `conio` functions
/// (`_getch`, `_kbhit`) expect to see.
pub fn next_key_press(state: &mut WinApiState, block: bool) -> Option<super::KeyEvent> {
    loop {
        while let Some(record) = state.console().pending_input.pop_front() {
            if let InputRecord::Key(event) = record
                && event.key_down
            {
                return Some(event);
            }
        }
        let added = pump(state, if block { -1 } else { 0 });
        if added == 0 && !block {
            return None;
        }
        if added == 0 && block && !host_term::is_tty() {
            // No terminal to block on; refusing to spin is better than hanging.
            return None;
        }
    }
}

/// Ensure the terminal is in the mode the guest's console state implies.
///
/// Idempotent, and cheap enough to call at the top of any input handler: a
/// program that calls `ReadConsoleInput` without first clearing
/// `ENABLE_LINE_INPUT` still needs cbreak mode to get per-key delivery.
pub fn ensure_input_ready(state: &mut WinApiState) {
    if host_term::raw_active() {
        return;
    }
    let processed = state.console().input_mode & super::ENABLE_PROCESSED_INPUT != 0;
    host_term::enter_raw(processed);
}

/// Current terminal size as a `COORD`.
#[must_use]
pub fn window_size_coord() -> Coord {
    let (columns, rows) = host_term::window_size();
    Coord::new(
        i16::try_from(columns).unwrap_or(i16::MAX),
        i16::try_from(rows).unwrap_or(i16::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::{KeyEvent, MouseEvent};

    #[test]
    fn key_record_lands_at_the_documented_offsets() {
        let encoded = encode_record(InputRecord::Key(KeyEvent {
            key_down: true,
            repeat_count: 1,
            virtual_key_code: 0x26,
            virtual_scan_code: 0x48,
            unit: 0,
            control_key_state: input::LEFT_CTRL_PRESSED,
        }));
        assert_eq!(encoded.get(0..2), Some(&1_u16.to_le_bytes()[..]));
        assert_eq!(encoded.get(4..8), Some(&1_u32.to_le_bytes()[..]));
        assert_eq!(encoded.get(8..10), Some(&1_u16.to_le_bytes()[..]));
        assert_eq!(encoded.get(10..12), Some(&0x26_u16.to_le_bytes()[..]));
        assert_eq!(encoded.get(12..14), Some(&0x48_u16.to_le_bytes()[..]));
        assert_eq!(
            encoded.get(16..20),
            Some(&input::LEFT_CTRL_PRESSED.to_le_bytes()[..])
        );
    }

    #[test]
    fn mouse_record_packs_position_into_one_dword() {
        let encoded = encode_record(InputRecord::Mouse(MouseEvent {
            position: Coord::new(9, 4),
            button_state: input::FROM_LEFT_1ST_BUTTON_PRESSED,
            control_key_state: 0,
            event_flags: 0,
        }));
        assert_eq!(encoded.get(0..2), Some(&2_u16.to_le_bytes()[..]));
        // COORD { X = 9, Y = 4 } packs to 0x0004_0009.
        assert_eq!(encoded.get(4..8), Some(&0x0004_0009_u32.to_le_bytes()[..]));
    }

    #[test]
    fn window_size_record_uses_event_type_four() {
        let encoded = encode_record(InputRecord::WindowBufferSize(Coord::new(120, 40)));
        assert_eq!(encoded.get(0..2), Some(&4_u16.to_le_bytes()[..]));
        assert_eq!(encoded.get(4..8), Some(&0x0028_0078_u32.to_le_bytes()[..]));
    }

    #[test]
    fn negative_coordinates_round_trip_through_the_packed_form() {
        let coord = Coord::new(-3, -1);
        assert_eq!(Coord::from_packed(u64::from(coord.to_packed())), coord);
    }
}
