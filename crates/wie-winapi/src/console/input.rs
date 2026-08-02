//! Terminal byte stream → Windows `INPUT_RECORD` decoding.
//!
//! # The impedance mismatch
//!
//! A Windows console reports *key transitions*: a press and a release, each

#![expect(dead_code)]
#![allow(unreachable_pub)]
//! carrying a virtual-key code, a scan code, the translated character, and a
//! modifier mask. A Unix terminal reports *characters*, with special keys
//! encoded as multi-byte escape sequences and no release information at all.
//!
//! Two consequences, both unavoidable and both documented here rather than
//! discovered later:
//!
//! - **Key-up is synthesised.** Each decoded key produces a down record
//!   immediately followed by an up record. A guest that tracks "is this key
//!   currently held" — common for movement in a game — sees every key as
//!   instantaneously released. There is no fix at this layer; the terminal
//!   never sent the information. (The `kitty` keyboard protocol does report
//!   releases, but requires terminal opt-in and is not yet negotiated here.)
//! - **Escape is ambiguous.** A lone `ESC` byte and the start of a function-key
//!   sequence are the same byte. [`decode`] resolves this by only treating
//!   `ESC` as a complete key press when no continuation bytes follow in the
//!   same read; a partial sequence is left in the buffer for the next call.

use super::{Coord, InputRecord, KeyEvent, MouseEvent};

/// `RIGHT_ALT_PRESSED`.
pub const RIGHT_ALT_PRESSED: u32 = 0x0001;
/// `LEFT_ALT_PRESSED`.
pub const LEFT_ALT_PRESSED: u32 = 0x0002;
/// `RIGHT_CTRL_PRESSED`.
pub const RIGHT_CTRL_PRESSED: u32 = 0x0004;
/// `LEFT_CTRL_PRESSED`.
pub const LEFT_CTRL_PRESSED: u32 = 0x0008;
/// `SHIFT_PRESSED`.
pub const SHIFT_PRESSED: u32 = 0x0010;

/// `FROM_LEFT_1ST_BUTTON_PRESSED`.
pub const FROM_LEFT_1ST_BUTTON_PRESSED: u32 = 0x0001;
/// `RIGHTMOST_BUTTON_PRESSED`.
pub const RIGHTMOST_BUTTON_PRESSED: u32 = 0x0002;
/// `FROM_LEFT_2ND_BUTTON_PRESSED`.
pub const FROM_LEFT_2ND_BUTTON_PRESSED: u32 = 0x0004;

/// `MOUSE_MOVED`.
pub const MOUSE_MOVED: u32 = 0x0001;
/// `DOUBLE_CLICK`.
pub const DOUBLE_CLICK: u32 = 0x0002;
/// `MOUSE_WHEELED`.
pub const MOUSE_WHEELED: u32 = 0x0004;

// Virtual-key codes (winuser.h). Only the ones a terminal can actually produce.
const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_SPACE: u16 = 0x20;
const VK_PRIOR: u16 = 0x21;
const VK_NEXT: u16 = 0x22;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_INSERT: u16 = 0x2D;
const VK_DELETE: u16 = 0x2E;
const VK_F1: u16 = 0x70;

/// Outcome of trying to decode one key from the head of the buffer.
enum Step {
    /// Consumed `used` bytes and produced records.
    Emit {
        used: usize,
        records: Vec<InputRecord>,
    },
    /// A valid prefix, but the rest has not arrived yet.
    Incomplete,
}

/// Decode as many complete events as `bytes` contains.
///
/// Returns the records plus the number of bytes consumed; any trailing partial
/// escape sequence is left for the caller to re-present with more data.
///
/// `more_coming` tells the decoder whether the caller has reason to believe
/// further bytes are imminent. When false, a trailing lone `ESC` is resolved as
/// the Escape key rather than being held indefinitely — otherwise pressing Esc
/// in a TUI would appear to do nothing until the next keystroke.
#[must_use]
pub fn decode(bytes: &[u8], more_coming: bool) -> (Vec<InputRecord>, usize) {
    let mut out = Vec::new();
    let mut offset = 0_usize;

    while offset < bytes.len() {
        let rest = bytes.get(offset..).unwrap_or(&[]);
        match decode_one(rest, more_coming) {
            Step::Emit { used, records } => {
                if used == 0 {
                    // Defensive: a zero-width step would spin forever.
                    break;
                }
                out.extend(records);
                offset = offset.saturating_add(used);
            }
            Step::Incomplete => break,
        }
    }
    (out, offset)
}

/// Decode the single event at the head of `bytes`.
fn decode_one(bytes: &[u8], more_coming: bool) -> Step {
    let Some(&first) = bytes.first() else {
        return Step::Incomplete;
    };

    if first == 0x1B {
        return decode_escape(bytes, more_coming);
    }
    decode_plain(bytes)
}

/// Decode a non-escape byte (or UTF-8 sequence) as a character key.
fn decode_plain(bytes: &[u8]) -> Step {
    let Some(&first) = bytes.first() else {
        return Step::Incomplete;
    };

    // Control characters map to their unmodified key plus a Ctrl modifier.
    match first {
        b'\r' | b'\n' => {
            return Step::Emit {
                used: 1,
                records: key_pair(VK_RETURN, u16::from(b'\r'), 0),
            };
        }
        0x7F | 0x08 => {
            return Step::Emit {
                used: 1,
                records: key_pair(VK_BACK, u16::from(b'\x08'), 0),
            };
        }
        b'\t' => {
            return Step::Emit {
                used: 1,
                records: key_pair(VK_TAB, u16::from(b'\t'), 0),
            };
        }
        b' ' => {
            return Step::Emit {
                used: 1,
                records: key_pair(VK_SPACE, u16::from(b' '), 0),
            };
        }
        // Ctrl+A..Ctrl+Z arrive as 0x01..0x1A.
        0x01..=0x1A => {
            let letter = first.saturating_add(b'A').saturating_sub(1);
            return Step::Emit {
                used: 1,
                records: key_pair(u16::from(letter), u16::from(first), LEFT_CTRL_PRESSED),
            };
        }
        _ => {}
    }

    // Everything else: decode one UTF-8 scalar so non-ASCII keys survive.
    let width = utf8_width(first);
    if bytes.len() < width {
        return Step::Incomplete;
    }
    let slice = bytes.get(..width).unwrap_or(&[]);
    let text = String::from_utf8_lossy(slice);
    let Some(ch) = text.chars().next() else {
        return Step::Emit {
            used: width.max(1),
            records: Vec::new(),
        };
    };

    let mut units = [0_u16; 2];
    let encoded = ch.encode_utf16(&mut units);
    let unit = encoded.first().copied().unwrap_or(0);
    // Windows reports the virtual key of the *unshifted* key, which for the
    // Latin alphabet is the uppercase letter.
    let virtual_key = if ch.to_ascii_uppercase().is_ascii() {
        u16::from(u8::try_from(u32::from(ch.to_ascii_uppercase())).unwrap_or(0))
    } else {
        0
    };
    let modifiers = if ch.is_ascii_uppercase() {
        SHIFT_PRESSED
    } else {
        0
    };
    Step::Emit {
        used: width,
        records: key_pair(virtual_key, unit, modifiers),
    }
}

/// Byte length of the UTF-8 sequence introduced by `first`.
fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        // Continuation or invalid lead: consume one byte so we make progress.
        _ => 1,
    }
}

/// Decode a sequence introduced by `ESC`.
fn decode_escape(bytes: &[u8], more_coming: bool) -> Step {
    match bytes.get(1) {
        None => {
            if more_coming {
                // Could be the start of a longer sequence still in flight.
                return Step::Incomplete;
            }
            Step::Emit {
                used: 1,
                records: key_pair(VK_ESCAPE, u16::from(b'\x1b'), 0),
            }
        }
        Some(b'[') => decode_csi(bytes),
        Some(b'O') => decode_ss3(bytes),
        // ESC followed by anything else is Alt+that key.
        Some(_) => {
            let tail = bytes.get(1..).unwrap_or(&[]);
            match decode_plain(tail) {
                Step::Emit { used, records } => Step::Emit {
                    used: used.saturating_add(1),
                    records: records
                        .into_iter()
                        .map(|record| add_modifier(record, LEFT_ALT_PRESSED))
                        .collect(),
                },
                Step::Incomplete => Step::Incomplete,
            }
        }
    }
}

/// Decode `ESC O <letter>` — the SS3 form some terminals use for F1..F4.
fn decode_ss3(bytes: &[u8]) -> Step {
    let Some(&final_byte) = bytes.get(2) else {
        return Step::Incomplete;
    };
    let virtual_key = match final_byte {
        b'P' => VK_F1,
        b'Q' => VK_F1.saturating_add(1),
        b'R' => VK_F1.saturating_add(2),
        b'S' => VK_F1.saturating_add(3),
        b'H' => VK_HOME,
        b'F' => VK_END,
        // Application-cursor-key mode sends arrows as SS3 too.
        b'A' => VK_UP,
        b'B' => VK_DOWN,
        b'C' => VK_RIGHT,
        b'D' => VK_LEFT,
        _ => {
            return Step::Emit {
                used: 3,
                records: Vec::new(),
            };
        }
    };
    Step::Emit {
        used: 3,
        records: key_pair(virtual_key, 0, 0),
    }
}

/// Decode a CSI sequence: `ESC [ <params> <final>`.
fn decode_csi(bytes: &[u8]) -> Step {
    // SGR mouse reporting has its own grammar: `ESC [ < b ; x ; y (M|m)`.
    if bytes.get(2) == Some(&b'<') {
        return decode_sgr_mouse(bytes);
    }

    // Scan to the final byte, which is the first in 0x40..=0x7E after the
    // parameter and intermediate bytes.
    let mut index = 2_usize;
    loop {
        let Some(&byte) = bytes.get(index) else {
            return Step::Incomplete;
        };
        if (0x40..=0x7E).contains(&byte) {
            break;
        }
        index = index.saturating_add(1);
        if index > 32 {
            // Runaway sequence: drop the ESC and resynchronise.
            return Step::Emit {
                used: 1,
                records: Vec::new(),
            };
        }
    }
    let final_byte = bytes.get(index).copied().unwrap_or(0);
    let params_slice = bytes.get(2..index).unwrap_or(&[]);
    let params = parse_params(params_slice);
    let used = index.saturating_add(1);

    // xterm encodes modifiers as a second parameter: 1 + bitmask.
    let modifiers = params
        .get(1)
        .copied()
        .flatten()
        .map_or(0, |value| xterm_modifier_mask(value));

    let virtual_key = match final_byte {
        b'A' => VK_UP,
        b'B' => VK_DOWN,
        b'C' => VK_RIGHT,
        b'D' => VK_LEFT,
        b'H' => VK_HOME,
        b'F' => VK_END,
        b'Z' => {
            // Shift+Tab.
            return Step::Emit {
                used,
                records: key_pair(VK_TAB, u16::from(b'\t'), SHIFT_PRESSED),
            };
        }
        b'~' => {
            let Some(code) = params.first().copied().flatten() else {
                return Step::Emit {
                    used,
                    records: Vec::new(),
                };
            };
            match tilde_virtual_key(code) {
                Some(key) => key,
                None => {
                    return Step::Emit {
                        used,
                        records: Vec::new(),
                    };
                }
            }
        }
        _ => {
            return Step::Emit {
                used,
                records: Vec::new(),
            };
        }
    };
    Step::Emit {
        used,
        records: key_pair(virtual_key, 0, modifiers),
    }
}

/// Map the numeric code in a `ESC [ <n> ~` sequence to a virtual key.
fn tilde_virtual_key(code: u32) -> Option<u16> {
    let key = match code {
        1 | 7 => VK_HOME,
        2 => VK_INSERT,
        3 => VK_DELETE,
        4 | 8 => VK_END,
        5 => VK_PRIOR,
        6 => VK_NEXT,
        // F1..F5
        11 => VK_F1,
        12 => VK_F1.saturating_add(1),
        13 => VK_F1.saturating_add(2),
        14 => VK_F1.saturating_add(3),
        15 => VK_F1.saturating_add(4),
        // 16 is unused by xterm; F6..F12 resume at 17.
        17 => VK_F1.saturating_add(5),
        18 => VK_F1.saturating_add(6),
        19 => VK_F1.saturating_add(7),
        20 => VK_F1.saturating_add(8),
        21 => VK_F1.saturating_add(9),
        23 => VK_F1.saturating_add(10),
        24 => VK_F1.saturating_add(11),
        _ => return None,
    };
    Some(key)
}

/// Translate xterm's `1 + bitmask` modifier parameter into a control-key state.
fn xterm_modifier_mask(value: u32) -> u32 {
    let bits = value.saturating_sub(1);
    let mut mask = 0;
    if bits & 0x01 != 0 {
        mask |= SHIFT_PRESSED;
    }
    if bits & 0x02 != 0 {
        mask |= LEFT_ALT_PRESSED;
    }
    if bits & 0x04 != 0 {
        mask |= LEFT_CTRL_PRESSED;
    }
    mask
}

/// Decode `ESC [ < button ; column ; row (M|m)` — xterm SGR mouse reporting.
///
/// SGR (mode 1006) is the form worth supporting: the older X10 encoding caps
/// coordinates at 223 because it packs them into single bytes, which breaks on
/// any terminal wider than that.
fn decode_sgr_mouse(bytes: &[u8]) -> Step {
    let mut index = 3_usize;
    loop {
        let Some(&byte) = bytes.get(index) else {
            return Step::Incomplete;
        };
        if byte == b'M' || byte == b'm' {
            break;
        }
        index = index.saturating_add(1);
        if index > 32 {
            return Step::Emit {
                used: 1,
                records: Vec::new(),
            };
        }
    }
    let final_byte = bytes.get(index).copied().unwrap_or(b'M');
    let params = parse_params(bytes.get(3..index).unwrap_or(&[]));
    let used = index.saturating_add(1);

    let button_code = params.first().copied().flatten().unwrap_or(0);
    let column = params.get(1).copied().flatten().unwrap_or(1);
    let row = params.get(2).copied().flatten().unwrap_or(1);

    // The terminal reports 1-based coordinates; console records are 0-based.
    let position = Coord::new(
        i16::try_from(column.saturating_sub(1)).unwrap_or(0),
        i16::try_from(row.saturating_sub(1)).unwrap_or(0),
    );

    let mut control_key_state = 0;
    if button_code & 0x04 != 0 {
        control_key_state |= SHIFT_PRESSED;
    }
    if button_code & 0x08 != 0 {
        control_key_state |= LEFT_ALT_PRESSED;
    }
    if button_code & 0x10 != 0 {
        control_key_state |= LEFT_CTRL_PRESSED;
    }

    // Bit 6 marks a wheel event; bit 5 marks motion.
    let is_wheel = button_code & 0x40 != 0;
    let is_motion = button_code & 0x20 != 0;
    let released = final_byte == b'm';

    let (button_state, event_flags) = if is_wheel {
        // Wheel delta lives in the high word, signed: +120 up, -120 down.
        let delta: i32 = if button_code & 0x01 == 0 { 120 } else { -120 };
        let packed = u32::from_ne_bytes(delta.to_ne_bytes()) << 16;
        (packed, MOUSE_WHEELED)
    } else if released {
        (0, 0)
    } else {
        let button = match button_code & 0x03 {
            0 => FROM_LEFT_1ST_BUTTON_PRESSED,
            1 => FROM_LEFT_2ND_BUTTON_PRESSED,
            2 => RIGHTMOST_BUTTON_PRESSED,
            _ => 0,
        };
        (button, if is_motion { MOUSE_MOVED } else { 0 })
    };

    Step::Emit {
        used,
        records: vec![InputRecord::Mouse(MouseEvent {
            position,
            button_state,
            control_key_state,
            event_flags,
        })],
    }
}

/// Parse `;`-separated numeric parameters; empty fields decode to `None`.
fn parse_params(bytes: &[u8]) -> Vec<Option<u32>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|&byte| byte == b';')
        .map(|field| {
            if field.is_empty() {
                return None;
            }
            let mut value: u32 = 0;
            for &byte in field {
                if !byte.is_ascii_digit() {
                    return None;
                }
                value = value
                    .saturating_mul(10)
                    .saturating_add(u32::from(byte.saturating_sub(b'0')));
            }
            Some(value)
        })
        .collect()
}

/// Build the down/up record pair for one key press.
///
/// See the module header: the terminal never reports releases, so the up record
/// is synthesised immediately after the down.
fn key_pair(virtual_key: u16, unit: u16, control_key_state: u32) -> Vec<InputRecord> {
    let event = KeyEvent {
        key_down: true,
        repeat_count: 1,
        virtual_key_code: virtual_key,
        // Scan codes would need a keyboard-layout table to be meaningful, and
        // programs that read them at all accept 0 from a redirected console.
        virtual_scan_code: 0,
        unit,
        control_key_state,
    };
    vec![
        InputRecord::Key(event),
        InputRecord::Key(KeyEvent {
            key_down: false,
            ..event
        }),
    ]
}

/// Add a modifier bit to an already-built record.
fn add_modifier(record: InputRecord, modifier: u32) -> InputRecord {
    match record {
        InputRecord::Key(event) => InputRecord::Key(KeyEvent {
            control_key_state: event.control_key_state | modifier,
            ..event
        }),
        other => other,
    }
}

#[cfg(test)]
// Tests assert on decoded shapes; a wrong variant should fail loudly rather
// than be threaded through an Option the assertions then ignore.
#[allow(clippy::panic)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<KeyEvent> {
        let (records, used) = decode(bytes, false);
        assert_eq!(used, bytes.len(), "decoder left bytes unconsumed");
        records
            .into_iter()
            .filter_map(|record| match record {
                InputRecord::Key(event) if event.key_down => Some(event),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_letter_reports_uppercase_virtual_key() {
        let decoded = keys(b"a");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded.first().map(|k| k.virtual_key_code), Some(0x41));
        assert_eq!(decoded.first().map(|k| k.unit), Some(u16::from(b'a')));
    }

    #[test]
    fn shift_is_reported_for_capitals() {
        let decoded = keys(b"A");
        assert_eq!(
            decoded.first().map(|k| k.control_key_state),
            Some(SHIFT_PRESSED)
        );
    }

    #[test]
    fn arrow_keys_decode_from_csi() {
        for (bytes, expected) in [
            (&b"\x1b[A"[..], VK_UP),
            (&b"\x1b[B"[..], VK_DOWN),
            (&b"\x1b[C"[..], VK_RIGHT),
            (&b"\x1b[D"[..], VK_LEFT),
        ] {
            let decoded = keys(bytes);
            assert_eq!(decoded.first().map(|k| k.virtual_key_code), Some(expected));
        }
    }

    #[test]
    fn function_and_navigation_keys_decode() {
        assert_eq!(
            keys(b"\x1b[11~").first().map(|k| k.virtual_key_code),
            Some(VK_F1)
        );
        assert_eq!(
            keys(b"\x1b[3~").first().map(|k| k.virtual_key_code),
            Some(VK_DELETE)
        );
        assert_eq!(
            keys(b"\x1b[5~").first().map(|k| k.virtual_key_code),
            Some(VK_PRIOR)
        );
    }

    #[test]
    fn ctrl_letter_sets_ctrl_modifier() {
        let decoded = keys(b"\x03"); // Ctrl+C
        assert_eq!(decoded.first().map(|k| k.virtual_key_code), Some(0x43));
        assert_eq!(
            decoded.first().map(|k| k.control_key_state),
            Some(LEFT_CTRL_PRESSED)
        );
    }

    #[test]
    fn modified_arrow_reports_the_modifier() {
        // ESC [ 1 ; 5 A — Ctrl+Up.
        let decoded = keys(b"\x1b[1;5A");
        assert_eq!(decoded.first().map(|k| k.virtual_key_code), Some(VK_UP));
        assert_eq!(
            decoded.first().map(|k| k.control_key_state),
            Some(LEFT_CTRL_PRESSED)
        );
    }

    #[test]
    fn every_key_emits_a_synthesised_release() {
        let (records, _) = decode(b"a", false);
        assert_eq!(records.len(), 2);
        assert!(matches!(
            records.first(),
            Some(InputRecord::Key(KeyEvent { key_down: true, .. }))
        ));
        assert!(matches!(
            records.get(1),
            Some(InputRecord::Key(KeyEvent {
                key_down: false,
                ..
            }))
        ));
    }

    #[test]
    fn lone_escape_waits_when_more_bytes_may_follow() {
        let (records, used) = decode(b"\x1b", true);
        assert!(records.is_empty());
        assert_eq!(used, 0, "partial sequence must stay buffered");

        let (records, used) = decode(b"\x1b", false);
        assert_eq!(used, 1);
        assert_eq!(
            records.first().and_then(|r| match r {
                InputRecord::Key(k) => Some(k.virtual_key_code),
                _ => None,
            }),
            Some(VK_ESCAPE)
        );
    }

    #[test]
    fn partial_csi_is_left_buffered() {
        let (records, used) = decode(b"\x1b[", true);
        assert!(records.is_empty());
        assert_eq!(used, 0);
    }

    #[test]
    fn alt_prefixed_key_sets_alt_modifier() {
        let decoded = keys(b"\x1bx");
        assert_eq!(decoded.first().map(|k| k.virtual_key_code), Some(0x58));
        assert_eq!(
            decoded.first().map(|k| k.control_key_state),
            Some(LEFT_ALT_PRESSED)
        );
    }

    #[test]
    fn sgr_mouse_press_decodes_to_zero_based_coordinates() {
        // Left button press at column 10, row 5 (1-based on the wire).
        let (records, used) = decode(b"\x1b[<0;10;5M", false);
        assert_eq!(used, 10);
        let Some(InputRecord::Mouse(event)) = records.first() else {
            panic!("expected a mouse record");
        };
        assert_eq!(event.position, Coord::new(9, 4));
        assert_eq!(event.button_state, FROM_LEFT_1ST_BUTTON_PRESSED);
    }

    #[test]
    fn sgr_mouse_release_clears_the_button_state() {
        let (records, _) = decode(b"\x1b[<0;10;5m", false);
        let Some(InputRecord::Mouse(event)) = records.first() else {
            panic!("expected a mouse record");
        };
        assert_eq!(event.button_state, 0);
    }

    #[test]
    fn sgr_wheel_up_sets_positive_delta_in_the_high_word() {
        let (records, _) = decode(b"\x1b[<64;1;1M", false);
        let Some(InputRecord::Mouse(event)) = records.first() else {
            panic!("expected a mouse record");
        };
        assert_eq!(event.event_flags, MOUSE_WHEELED);
        let delta = i32::from_ne_bytes(event.button_state.to_ne_bytes()) >> 16;
        assert_eq!(delta, 120);
    }

    #[test]
    fn a_batch_of_keystrokes_decodes_in_order() {
        let decoded = keys(b"hi\x1b[A");
        let codes: Vec<u16> = decoded.iter().map(|k| k.virtual_key_code).collect();
        assert_eq!(codes, vec![0x48, 0x49, VK_UP]);
    }

    #[test]
    fn utf8_scalar_survives_as_one_key() {
        let decoded = keys("é".as_bytes());
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded.first().map(|k| k.unit), Some(0x00E9));
    }
}
