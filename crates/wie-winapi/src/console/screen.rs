//! Cell-grid rendering: `ScreenBuffer` → ANSI escape sequences.
//!
//! # Why diff
//!
//! A frame-by-frame console game calls `WriteConsoleOutput` with the whole
//! screen every frame. Repainting all of it naively costs, for a 200x50
//! terminal, 10 000 cells — roughly 100 KB of escape bytes per frame once
//! colour changes are included. At 60 fps that is 6 MB/s down a pty, which
//! stalls on the terminal's own parser long before the guest is the bottleneck.
//!
//! So [`flush`] compares the live grid against the last painted one and emits
//! only the runs that changed. A typical game frame touches a few hundred
//! cells, which is a ~50x reduction and turns rendering from the dominant cost
//! back into noise.
//!
//! # Colour mapping
//!
//! Windows attributes are 4-bit foreground and 4-bit background in
//! intensity-red-green-blue order. ANSI SGR uses a different bit order
//! (blue-green-red), so the two nibbles are permuted rather than passed
//! through — [`ansi_colour_index`] is where that happens.

#![expect(dead_code)]
#![allow(
    unreachable_pub,
    clippy::format_push_string,
    clippy::cast_possible_truncation,
    clippy::arithmetic_side_effects,
)]

use super::{CharInfo, ScreenBuffer, host_term};

/// `FOREGROUND_INTENSITY`.
const FOREGROUND_INTENSITY: u16 = 0x0008;
/// `BACKGROUND_INTENSITY`.
const BACKGROUND_INTENSITY: u16 = 0x0080;
/// `COMMON_LVB_REVERSE_VIDEO`.
const COMMON_LVB_REVERSE_VIDEO: u16 = 0x4000;
/// `COMMON_LVB_UNDERSCORE`.
const COMMON_LVB_UNDERSCORE: u16 = 0x8000;

/// Convert a Windows colour nibble to an ANSI colour index.
///
/// Windows packs the nibble as `I R G B`; ANSI expects `B G R`. Without the
/// swap, red text renders blue and vice versa — the single most visible way to
/// get console colour wrong.
#[must_use]
pub fn ansi_colour_index(nibble: u16) -> u8 {
    let red = u8::from(nibble & 0x4 != 0);
    let green = u8::from(nibble & 0x2 != 0);
    let blue = u8::from(nibble & 0x1 != 0);
    red | (green << 1) | (blue << 2)
}

/// Build the SGR sequence that switches from `previous` to `current`.
///
/// Returns an empty string when nothing changed, so a run of same-coloured
/// cells emits its escape once rather than per cell.
#[must_use]
pub fn sgr_for(previous: Option<u16>, current: u16) -> String {
    if previous == Some(current) {
        return String::new();
    }
    // DEFAULT_ATTRIBUTES (white on black) is the Windows console default.
    // On a Unix terminal the user's theme is the real default — don't force
    // 0;37;40 over it when the \033[0m reset at the start of the frame
    // already restores the terminal's native palette.
    if previous.is_none() && current == super::DEFAULT_ATTRIBUTES {
        return String::new();
    }
    let foreground = ansi_colour_index(current & 0x7);
    let background = ansi_colour_index((current >> 4) & 0x7);
    let bright_foreground = current & FOREGROUND_INTENSITY != 0;
    let bright_background = current & BACKGROUND_INTENSITY != 0;

    let mut sequence = String::from("\u{1b}[0");
    // 30..37 are the dim foregrounds, 90..97 the bright ones.
    let fg_base = if bright_foreground { 90 } else { 30 };
    sequence.push(';');
    sequence.push_str(&(fg_base + u16::from(foreground)).to_string());
    let bg_base = if bright_background { 100 } else { 40 };
    sequence.push(';');
    sequence.push_str(&(bg_base + u16::from(background)).to_string());
    if current & COMMON_LVB_REVERSE_VIDEO != 0 {
        sequence.push_str(";7");
    }
    if current & COMMON_LVB_UNDERSCORE != 0 {
        sequence.push_str(";4");
    }
    sequence.push('m');
    sequence
}

/// Repaint the terminal to match `buffer`.
///
/// `previous` is the grid last painted, or `None` to force a full repaint (on
/// first draw, after a resize, or after the guest emitted its own escapes).
/// Returns the bytes written, which the caller stores as the new `previous`.
pub fn flush(buffer: &ScreenBuffer, previous: Option<&ScreenBuffer>) -> String {
    let full_repaint = previous.is_none_or(|prior| {
        prior.width != buffer.width || prior.height != buffer.height
    });

    let mut out = String::new();
    let mut last_attributes: Option<u16> = None;
    // Where the terminal's cursor sits after everything emitted so far, so a
    // contiguous run does not re-emit a position for every cell.
    let mut cursor: Option<(u16, u16)> = None;

    if full_repaint {
        // Home the cursor so the first frame starts from (0,0).
        // No \033[2J here — the alt screen starts empty, and the
        // clear would cause a visible blank frame before the content.
        out.push_str("\u{1b}[H");
    }

    for row in 0..buffer.height {
        let mut column = 0_u16;
        while column < buffer.width {
            let index = usize::from(row)
                .saturating_mul(usize::from(buffer.width))
                .saturating_add(usize::from(column));
            let Some(cell) = buffer.cells.get(index).copied() else {
                break;
            };
            let unchanged = !full_repaint
                && previous
                    .and_then(|prior| prior.cells.get(index).copied())
                    .is_some_and(|prior_cell| prior_cell == cell);
            if unchanged {
                column = column.saturating_add(1);
                continue;
            }

            // Emit a maximal run of changed cells sharing one attribute.
            let run_attributes = cell.attributes;
            let mut text = String::new();
            let start_column = column;
            while column < buffer.width {
                let run_index = usize::from(row)
                    .saturating_mul(usize::from(buffer.width))
                    .saturating_add(usize::from(column));
                let Some(run_cell) = buffer.cells.get(run_index).copied() else {
                    break;
                };
                if run_cell.attributes != run_attributes {
                    break;
                }
                let run_unchanged = !full_repaint
                    && previous
                        .and_then(|prior| prior.cells.get(run_index).copied())
                        .is_some_and(|prior_cell| prior_cell == run_cell);
                if run_unchanged {
                    break;
                }
                text.push(char_of(run_cell));
                column = column.saturating_add(1);
            }
            if text.is_empty() {
                column = column.saturating_add(1);
                continue;
            }

            if cursor != Some((start_column, row)) {
                // ANSI cursor addressing is 1-based, row first.
                out.push_str(&format!(
                    "\u{1b}[{};{}H",
                    row.saturating_add(1),
                    start_column.saturating_add(1)
                ));
            }
            let sgr = sgr_for(last_attributes, run_attributes);
            if !sgr.is_empty() {
                out.push_str(&sgr);
                last_attributes = Some(run_attributes);
            }
            out.push_str(&text);
            cursor = Some((column, row));
        }
    }

    // Leave the terminal's cursor where the guest's console cursor is, so a
    // program that streams text after a cell repaint continues in the right
    // place, and so a visible caret sits where the user expects.
    let cursor_row = u16::try_from(buffer.cursor.y.max(0)).unwrap_or(0);
    let cursor_column = u16::try_from(buffer.cursor.x.max(0)).unwrap_or(0);
    out.push_str(&format!(
        "\u{1b}[{};{}H",
        cursor_row.saturating_add(1),
        cursor_column.saturating_add(1)
    ));
    out
}

/// The character a cell renders as.
///
/// NUL is the value a zeroed `CHAR_INFO` carries, and a guest that clears a
/// region by writing zeroed records means "blank", not "emit a NUL byte".
pub(crate) fn char_of(cell: CharInfo) -> char {
    if cell.unit == 0 {
        return ' ';
    }
    char::from_u32(u32::from(cell.unit)).unwrap_or(' ')
}

/// Show or hide the terminal's cursor.
pub fn set_cursor_visible(visible: bool) {
    if !host_term::is_tty() {
        return;
    }
    host_term::write_stdout(if visible {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    });
}

/// Move the terminal cursor directly, for Stream mode.
pub fn move_cursor(x: i16, y: i16) {
    if !host_term::is_tty() {
        return;
    }
    let row = u16::try_from(y.max(0)).unwrap_or(0).saturating_add(1);
    let column = u16::try_from(x.max(0)).unwrap_or(0).saturating_add(1);
    host_term::write_stdout(format!("\u{1b}[{row};{column}H").as_bytes());
}

/// Apply an attribute directly, for Stream mode.
pub fn apply_attributes(attributes: u16) {
    if !host_term::is_tty() {
        return;
    }
    let sequence = sgr_for(None, attributes);
    if !sequence.is_empty() {
        host_term::write_stdout(sequence.as_bytes());
    }
}

/// Switch the terminal to its alternate screen, preserving the user's scrollback.
///
/// A full-screen program should not leave its frames in the shell's history,
/// and the user's prompt should come back untouched when the guest exits.
pub fn enter_alternate_screen() {
    if !host_term::is_tty() {
        return;
    }
    host_term::write_stdout(b"\x1b[?1049h");
}

/// Return from the alternate screen.
pub fn leave_alternate_screen() {
    if !host_term::is_tty() {
        return;
    }
    host_term::write_stdout(b"\x1b[?1049l");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::DEFAULT_ATTRIBUTES;

    fn grid(width: u16, height: u16) -> ScreenBuffer {
        ScreenBuffer::new(width, height)
    }

    #[test]
    fn windows_colour_nibbles_map_to_the_right_ansi_index() {
        // FOREGROUND_RED (0x4) is ANSI colour 1, not 4.
        assert_eq!(ansi_colour_index(0x4), 1);
        // FOREGROUND_BLUE (0x1) is ANSI colour 4, not 1.
        assert_eq!(ansi_colour_index(0x1), 4);
        assert_eq!(ansi_colour_index(0x2), 2);
        // White (R|G|B) is 7 either way.
        assert_eq!(ansi_colour_index(0x7), 7);
    }

    #[test]
    fn default_attributes_render_as_grey_on_black() {
        let sgr = sgr_for(None, DEFAULT_ATTRIBUTES);
        // DEFAULT_ATTRIBUTES is the Windows console default and should not
        // emit any SGR — the \033[0m reset at frame start is enough.
        assert_eq!(sgr, "", "default attributes should not emit SGR");
        // But a non-default attribute should still emit SGR.
        let bright = sgr_for(None, DEFAULT_ATTRIBUTES | 0x0008);
        assert_eq!(bright, "\u{1b}[0;97;40m", "bright + default = bright white on black");
    }

    #[test]
    fn intensity_selects_the_bright_colour_range() {
        let bright = sgr_for(None, 0x0007 | FOREGROUND_INTENSITY);
        assert!(bright.contains(";97"), "expected bright white, got {bright}");
    }

    #[test]
    fn repeated_attributes_emit_no_escape() {
        assert_eq!(sgr_for(Some(DEFAULT_ATTRIBUTES), DEFAULT_ATTRIBUTES), "");
    }

    #[test]
    fn first_flush_clears_and_repaints() {
        let buffer = grid(4, 2);
        let out = flush(&buffer, None);
        assert!(out.contains("\u{1b}[H"), "expected cursor home on first paint");
        // A 4×2 grid of spaces: cursor home, row 0 (4 spaces), row 1 (4 spaces).
        assert!(!out.is_empty(), "expected repaint output");
    }

    #[test]
    fn an_unchanged_frame_emits_only_a_cursor_move() {
        let buffer = grid(20, 5);
        let painted = buffer.clone();
        let out = flush(&buffer, Some(&painted));
        // Only the trailing cursor reposition should survive the diff.
        assert!(!out.contains("\u{1b}[H\u{1b}[2J"));
        assert_eq!(out, "\u{1b}[1;1H");
    }

    #[test]
    fn only_the_changed_cell_is_repainted() {
        let painted = grid(10, 2);
        let mut buffer = painted.clone();
        if let Some(cell) = buffer.cells.get_mut(13) {
            *cell = CharInfo {
                unit: u16::from(b'X'),
                attributes: DEFAULT_ATTRIBUTES,
            };
        }
        let out = flush(&buffer, Some(&painted));
        assert!(out.contains('X'), "changed cell missing from output");
        // Cell 13 is row 1, column 3 → ANSI row 2, column 4.
        assert!(out.contains("\u{1b}[2;4H"), "wrong cursor address: {out}");
        // One character of payload, not the whole grid.
        assert_eq!(out.matches('X').count(), 1);
        assert!(!out.contains("          "));
    }

    #[test]
    fn a_resize_forces_a_full_repaint() {
        let painted = grid(10, 2);
        let buffer = grid(20, 2);
        let out = flush(&buffer, Some(&painted));
        assert!(out.contains("\u{1b}[H"), "resize must home cursor");
    }

    #[test]
    fn adjacent_changed_cells_share_one_cursor_move() {
        let painted = grid(10, 1);
        let mut buffer = painted.clone();
        for index in 2..6 {
            if let Some(cell) = buffer.cells.get_mut(index) {
                *cell = CharInfo {
                    unit: u16::from(b'#'),
                    attributes: DEFAULT_ATTRIBUTES,
                };
            }
        }
        let out = flush(&buffer, Some(&painted));
        assert_eq!(out.matches("\u{1b}[1;3H").count(), 1);
        assert!(out.contains("####"), "run should be emitted contiguously");
    }

    #[test]
    fn zeroed_cells_render_as_blanks_not_nul() {
        let painted = grid(4, 1);
        let mut buffer = painted.clone();
        if let Some(cell) = buffer.cells.get_mut(0) {
            *cell = CharInfo {
                unit: 0,
                attributes: DEFAULT_ATTRIBUTES,
            };
        }
        let out = flush(&buffer, Some(&painted));
        assert!(!out.contains('\0'), "NUL leaked into terminal output");
    }
}
