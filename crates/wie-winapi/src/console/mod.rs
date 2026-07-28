//! Windows console emulation on top of a Unix terminal.
//!
//! # Model
//!
//! A Windows console is a screen *buffer* (a grid of `CHAR_INFO` cells) plus a
//! cursor, a current attribute, and two independent mode words — one for the
//! input handle, one for the output handle. A Unix terminal is a byte stream.
//! Bridging the two is this module's whole job.
//!
//! Output takes one of two shapes, tracked by [`RenderMode`]:
//!
//! - **Stream** — the default. `WriteConsole`/`WriteFile` bytes go straight to
//!   host stdout, and cursor/attribute calls emit ANSI escapes immediately.
//!   This is what ordinary console programs and most TUIs do, and it keeps the
//!   output byte-for-byte faithful (including any VT sequences the guest emits
//!   itself once it sets `ENABLE_VIRTUAL_TERMINAL_PROCESSING`).
//! - **Cells** — entered the first time the guest calls a cell API
//!   (`WriteConsoleOutput*`, `FillConsoleOutput*`, `ScrollConsoleScreenBuffer`,
//!   `CreateConsoleScreenBuffer`). The grid becomes the source of truth and the
//!   terminal is repainted by diffing it against what was last drawn.
//!
//! Programs mixing the two are rare, and the mixed case is handled by folding
//! stream writes into the grid at the tracked cursor. That models plain text,
//! `\n`, `\r`, `\t`, and `\b`; it does *not* re-implement a VT parser, so a
//! guest that emits raw escapes while in Cells mode will desynchronise the
//! grid from the screen. [`ConsoleState::note_stream_escape`] detects that and
//! forces a full repaint on the next flush.
//!
//! # Threading
//!
//! `ConsoleState` lives in `WinApiState` behind the shared WinAPI mutex, so all
//! console calls are already serialised. Only genuinely process-wide terminal
//! state (the saved `termios`, the `SIGWINCH` flag) sits in [`host_term`]
//! statics.

// Most of these modules are consumed by the dispatch layer. Until every
// function is wired the `dead_code` and `unreachable_pub` lints are noise.
#![allow(
    clippy::as_conversions,
    clippy::integer_division,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::items_after_statements,
    clippy::format_push_string,
    clippy::cast_lossless,
)]

pub(crate) mod codepage;
pub(crate) mod host_term;
pub(crate) mod input;
pub(crate) mod pump;
pub(crate) mod screen;

use std::collections::VecDeque;

/// `ENABLE_PROCESSED_INPUT` (consoleapi.h).
pub const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
/// `ENABLE_LINE_INPUT`.
pub const ENABLE_LINE_INPUT: u32 = 0x0002;
/// `ENABLE_ECHO_INPUT`.
pub const ENABLE_ECHO_INPUT: u32 = 0x0004;
/// `ENABLE_WINDOW_INPUT` — deliver `WINDOW_BUFFER_SIZE_EVENT` records.
pub const ENABLE_WINDOW_INPUT: u32 = 0x0008;
/// `ENABLE_MOUSE_INPUT` — deliver `MOUSE_EVENT` records.
pub const ENABLE_MOUSE_INPUT: u32 = 0x0010;
/// `ENABLE_INSERT_MODE`.
pub const ENABLE_INSERT_MODE: u32 = 0x0020;
/// `ENABLE_QUICK_EDIT_MODE`.
pub const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
/// `ENABLE_EXTENDED_FLAGS`.
pub const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
/// `ENABLE_VIRTUAL_TERMINAL_INPUT` — report keys as VT sequences, not records.
pub const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

/// `ENABLE_PROCESSED_OUTPUT`.
pub const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
/// `ENABLE_WRAP_AT_EOL_OUTPUT`.
pub const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING` — guest emits its own ANSI escapes.
pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
/// `DISABLE_NEWLINE_AUTO_RETURN`.
pub const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;

/// Mode bits a guest may set on the input handle (others are rejected).
pub(crate) const VALID_INPUT_MODE: u32 = ENABLE_PROCESSED_INPUT
    | ENABLE_LINE_INPUT
    | ENABLE_ECHO_INPUT
    | ENABLE_WINDOW_INPUT
    | ENABLE_MOUSE_INPUT
    | ENABLE_INSERT_MODE
    | ENABLE_QUICK_EDIT_MODE
    | ENABLE_EXTENDED_FLAGS
    | ENABLE_VIRTUAL_TERMINAL_INPUT;

/// Mode bits a guest may set on an output handle.
pub(crate) const VALID_OUTPUT_MODE: u32 = ENABLE_PROCESSED_OUTPUT
    | ENABLE_WRAP_AT_EOL_OUTPUT
    | ENABLE_VIRTUAL_TERMINAL_PROCESSING
    | DISABLE_NEWLINE_AUTO_RETURN;

/// Windows' default input mode for a fresh console.
pub(crate) const DEFAULT_INPUT_MODE: u32 = ENABLE_PROCESSED_INPUT
    | ENABLE_LINE_INPUT
    | ENABLE_ECHO_INPUT
    | ENABLE_INSERT_MODE
    | ENABLE_QUICK_EDIT_MODE
    | ENABLE_EXTENDED_FLAGS;

/// Windows' default output mode for a fresh console.
pub(crate) const DEFAULT_OUTPUT_MODE: u32 = ENABLE_PROCESSED_OUTPUT | ENABLE_WRAP_AT_EOL_OUTPUT;

/// Default cell attribute: light gray on black (`FOREGROUND_R|G|B`).
pub const DEFAULT_ATTRIBUTES: u16 = 0x0007;

/// UTF-8 code page — what the host terminal actually speaks.
pub const CP_UTF8: u32 = 65001;
/// Windows-1252, the default ANSI code page WIE reports elsewhere.
pub const CP_WINDOWS_1252: u32 = 1252;
/// OEM code page 437, the console default on a US-English Windows.
pub const CP_OEM_437: u32 = 437;

/// Cap on a single `ReadConsole` line, mirroring the existing stdin cap.
#[expect(dead_code)]
pub(crate) const MAX_CONSOLE_LINE: usize = 64 * 1024;

/// How console output currently reaches the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    /// Bytes pass straight through; cursor moves emit ANSI immediately.
    #[default]
    Stream,
    /// The cell grid is authoritative and repaints happen by diffing it.
    Cells,
}

/// Console cursor / write position, in cells from the buffer origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Coord {
    pub x: i16,
    pub y: i16,
}

impl Coord {
    #[must_use]
    pub const fn new(x: i16, y: i16) -> Self {
        Self { x, y }
    }

    /// Decode the packed `COORD` the Win64 ABI passes by value in a register.
    #[must_use]
    pub const fn from_packed(packed: u64) -> Self {
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        // COORD is two i16 fields packed into the low 32 bits; the truncation
        // to u16 then reinterpretation as i16 is exactly the ABI's layout.
        let x = (packed & 0xffff) as u16;
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let y = ((packed >> 16) & 0xffff) as u16;
        Self {
            x: i16::from_ne_bytes(x.to_ne_bytes()),
            y: i16::from_ne_bytes(y.to_ne_bytes()),
        }
    }

    /// Re-pack into the 32-bit `COORD` representation.
    #[must_use]
    pub const fn to_packed(self) -> u32 {
        let x = u16::from_ne_bytes(self.x.to_ne_bytes());
        let y = u16::from_ne_bytes(self.y.to_ne_bytes());
        (x as u32) | ((y as u32) << 16)
    }
}

/// A `SMALL_RECT`: inclusive on every edge, as Windows defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SmallRect {
    pub left: i16,
    pub top: i16,
    pub right: i16,
    pub bottom: i16,
}

/// One console cell: a UTF-16 code unit plus its colour attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharInfo {
    pub unit: u16,
    pub attributes: u16,
}

impl Default for CharInfo {
    fn default() -> Self {
        Self {
            unit: u16::from(b' '),
            attributes: DEFAULT_ATTRIBUTES,
        }
    }
}

/// Which console event an `INPUT_RECORD` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRecord {
    Key(KeyEvent),
    Mouse(MouseEvent),
    WindowBufferSize(Coord),
}

/// `KEY_EVENT_RECORD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub key_down: bool,
    pub repeat_count: u16,
    pub virtual_key_code: u16,
    pub virtual_scan_code: u16,
    /// UTF-16 code unit, or 0 for a non-character key.
    pub unit: u16,
    pub control_key_state: u32,
}

/// `MOUSE_EVENT_RECORD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub position: Coord,
    pub button_state: u32,
    pub control_key_state: u32,
    pub event_flags: u32,
}

/// Per-screen-buffer state: the grid, cursor, and current attribute.
#[derive(Debug, Clone)]
pub struct ScreenBuffer {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<CharInfo>,
    pub cursor: Coord,
    pub attributes: u16,
    pub cursor_visible: bool,
    pub cursor_size: u32,
}

impl ScreenBuffer {
    #[must_use]
    pub fn new(width: u16, height: u16) -> Self {
        let count = usize::from(width).saturating_mul(usize::from(height));
        Self {
            width,
            height,
            cells: vec![CharInfo::default(); count],
            cursor: Coord::default(),
            attributes: DEFAULT_ATTRIBUTES,
            cursor_visible: true,
            cursor_size: 25,
        }
    }

    /// Linear index of `(x, y)`, or `None` when outside the buffer.
    #[must_use]
    pub fn index_of(&self, x: i16, y: i16) -> Option<usize> {
        if x < 0 || y < 0 {
            return None;
        }
        let x = usize::try_from(x).ok()?;
        let y = usize::try_from(y).ok()?;
        if x >= usize::from(self.width) || y >= usize::from(self.height) {
            return None;
        }
        y.checked_mul(usize::from(self.width))?.checked_add(x)
    }

    /// Resize the grid, preserving the top-left overlap.
    ///
    /// Windows truncates rather than reflows when a buffer shrinks, so the
    /// simple row-wise copy here matches the platform.
    pub fn resize(&mut self, width: u16, height: u16) {
        if width == self.width && height == self.height {
            return;
        }
        let count = usize::from(width).saturating_mul(usize::from(height));
        let mut next = vec![CharInfo::default(); count];
        let copy_rows = usize::from(height.min(self.height));
        let copy_cols = usize::from(width.min(self.width));
        for row in 0..copy_rows {
            for col in 0..copy_cols {
                let from = row
                    .saturating_mul(usize::from(self.width))
                    .saturating_add(col);
                let to = row.saturating_mul(usize::from(width)).saturating_add(col);
                if let (Some(cell), Some(slot)) = (self.cells.get(from).copied(), next.get_mut(to)) {
                    *slot = cell;
                }
            }
        }
        self.cells = next;
        self.width = width;
        self.height = height;
        self.cursor.x = self.cursor.x.min(i16::try_from(width).unwrap_or(i16::MAX));
        self.cursor.y = self.cursor.y.min(i16::try_from(height).unwrap_or(i16::MAX));
    }

    /// Scroll the whole buffer up one row, blanking the freed bottom row.
    ///
    /// Used when the cursor advances past the last row, which is what a
    /// Windows console does once the buffer is full.
    pub fn scroll_up(&mut self) {
        let stride = usize::from(self.width);
        if stride == 0 || self.height == 0 {
            return;
        }
        self.cells.rotate_left(stride);
        let start = self.cells.len().saturating_sub(stride);
        if let Some(tail) = self.cells.get_mut(start..) {
            for cell in tail {
                *cell = CharInfo {
                    unit: u16::from(b' '),
                    attributes: self.attributes,
                };
            }
        }
    }
}

/// All console state for the emulated process.
#[derive(Debug, Clone)]
pub struct ConsoleState {
    pub input_mode: u32,
    pub title: String,
    pub input_code_page: u32,
    pub output_code_page: u32,
    pub render_mode: RenderMode,
    /// Screen buffers keyed by their guest-visible handle.
    pub buffers: Vec<(u64, ScreenBuffer)>,
    /// Per-output-handle mode words, keyed the same way.
    pub output_modes: Vec<(u64, u32)>,
    /// Handle whose buffer is currently displayed.
    pub active_buffer: u64,
    /// Next handle value handed out by `CreateConsoleScreenBuffer`.
    pub next_buffer_handle: u64,
    /// Decoded records not yet consumed by `ReadConsoleInput`.
    pub pending_input: VecDeque<InputRecord>,
    /// Undecoded bytes from the host terminal.
    pub input_bytes: Vec<u8>,
    /// Last grid painted to the terminal, for diffing.
    pub rendered: Option<ScreenBuffer>,
    /// Set when a stream write may have moved the real cursor unpredictably.
    pub repaint_forced: bool,
    /// Accumulated frame data (system(cls) + fputs + ...) flushed on Sleep.
    pub stream_buf: Vec<u8>,
    /// Set when stream_buf has data pending flush.
    pub needs_flush: bool,
}

/// Handle of the screen buffer bound to `STD_OUTPUT_HANDLE` / `STD_ERROR_HANDLE`.
pub const PRIMARY_BUFFER_HANDLE: u64 = 0x0000_0000_6000_0010;

/// First handle handed out by `CreateConsoleScreenBuffer`.
const FIRST_ALT_BUFFER_HANDLE: u64 = 0x0000_0000_6000_0020;

impl Default for ConsoleState {
    fn default() -> Self {
        let (columns, rows) = host_term::window_size();
        Self {
            input_mode: DEFAULT_INPUT_MODE,
            title: String::new(),
            input_code_page: CP_OEM_437,
            output_code_page: CP_OEM_437,
            render_mode: RenderMode::Stream,
            buffers: vec![(PRIMARY_BUFFER_HANDLE, ScreenBuffer::new(columns, rows))],
            output_modes: vec![(PRIMARY_BUFFER_HANDLE, DEFAULT_OUTPUT_MODE)],
            active_buffer: PRIMARY_BUFFER_HANDLE,
            next_buffer_handle: FIRST_ALT_BUFFER_HANDLE,
            pending_input: VecDeque::new(),
            input_bytes: Vec::new(),
            rendered: None,
            repaint_forced: false,
            stream_buf: Vec::new(),
            needs_flush: false,
        }
    }
}

impl ConsoleState {
    /// Flush buffered output to the host terminal.
    /// Flush all pending output to the terminal.
    ///
    /// 1. Folds `stream_buf` into the grid via `fold_text_into_grid`.
    /// 2. Reconstructs the full grid as plain text with `\n` row separators.
    /// 3. Writes `\033[H` + SGR reset + reconstructed text.
    ///
    /// No diff renderer — the full grid is always written as text. This
    /// avoids ordering issues where the old character at a position is
    /// cleared before the new one is drawn (or vice versa), which looks
    /// like flickering or double-images.
    ///
    /// Called on natural frame boundaries: `Sleep`, `_getch`, `_kbhit`,
    /// `fflush`. Every CRT output path converges here.
    pub fn flush_stream_output(&mut self) {
        if !self.needs_flush {
            return;
        }
        self.needs_flush = false;

        // Fold any buffered CRT output into the grid first.
        if !self.stream_buf.is_empty() {
            let raw = std::mem::take(&mut self.stream_buf);
            if let Some(handle) = self.buffer_handle_for(PRIMARY_BUFFER_HANDLE) {
                let units: Vec<u16> = raw.iter().map(|&b| u16::from(b)).collect();
                crate::kernel32::console::fold_text_into_grid(self, handle, &units);
            }
        }

        // Reconstruct the full grid as text (every cell, every frame).
        // No diff — the terminal overwrites the previous frame in place.
        const SGR_RESET: &str = "\u{1b}[0m";
        if let Some(buffer) = self.buffer(PRIMARY_BUFFER_HANDLE) {
            let stride = usize::from(buffer.width);
            let rows = usize::from(buffer.height);
            let mut raw = Vec::with_capacity(stride * rows + rows + SGR_RESET.len() + 4);
            raw.extend_from_slice(b"\x1b[H");
            raw.extend_from_slice(SGR_RESET.as_bytes());
            for row in 0..rows {
                let base = row * stride;
                for col in 0..stride {
                    if let Some(cell) = buffer.cells.get(base + col) {
                        raw.push(crate::console::screen::char_of(*cell) as u8);
                    }
                }
                raw.push(b'\n');
            }
            host_term::write_stdout(&raw);
            self.rendered = Some(buffer.clone());
        }
    }

    /// Helper: get a handle to the primary screen buffer.
    fn buffer_handle_for(&self, handle: u64) -> Option<u64> {
        let stdout = 0x0000_0000_6000_0002; // FAKE_STDOUT_HANDLE
        if handle == stdout || handle == 0x0000_0000_6000_0003 {
            Some(PRIMARY_BUFFER_HANDLE)
        } else {
            self.buffer(handle).map(|_| handle)
        }
    }
}



impl ConsoleState {
    /// Borrow the screen buffer behind an output handle.
    #[must_use]
    pub fn buffer(&self, handle: u64) -> Option<&ScreenBuffer> {
        self.buffers
            .iter()
            .find(|(key, _)| *key == handle)
            .map(|(_, buffer)| buffer)
    }

    /// Mutably borrow the screen buffer behind an output handle.
    pub fn buffer_mut(&mut self, handle: u64) -> Option<&mut ScreenBuffer> {
        self.buffers
            .iter_mut()
            .find(|(key, _)| *key == handle)
            .map(|(_, buffer)| buffer)
    }

    /// Current mode word for an output handle.
    #[must_use]
    pub fn output_mode(&self, handle: u64) -> u32 {
        self.output_modes
            .iter()
            .find(|(key, _)| *key == handle)
            .map_or(DEFAULT_OUTPUT_MODE, |(_, mode)| *mode)
    }

    /// Replace the mode word for an output handle.
    pub fn set_output_mode(&mut self, handle: u64, mode: u32) {
        if let Some(slot) = self
            .output_modes
            .iter_mut()
            .find(|(key, _)| *key == handle)
            .map(|(_, mode)| mode)
        {
            *slot = mode;
        } else {
            self.output_modes.push((handle, mode));
        }
    }

    /// Allocate a screen buffer and return its handle.
    pub fn create_buffer(&mut self) -> u64 {
        let (columns, rows) = host_term::window_size();
        let handle = self.next_buffer_handle;
        self.next_buffer_handle = self.next_buffer_handle.saturating_add(1);
        self.buffers.push((handle, ScreenBuffer::new(columns, rows)));
        self.output_modes.push((handle, DEFAULT_OUTPUT_MODE));
        // A guest that allocates its own buffer is double-buffering, which only
        // makes sense against the cell APIs.
        self.render_mode = RenderMode::Cells;
        handle
    }

    /// Adopt the host terminal's current size, if it changed.
    ///
    /// Returns the new size when a resize actually happened, so the caller can
    /// queue a `WINDOW_BUFFER_SIZE_EVENT`.
    pub fn sync_window_size(&mut self) -> Option<Coord> {
        let (columns, rows) = host_term::window_size();
        let changed = self
            .buffer(self.active_buffer)
            .is_some_and(|buffer| buffer.width != columns || buffer.height != rows);
        if !changed {
            return None;
        }
        for (_, buffer) in &mut self.buffers {
            buffer.resize(columns, rows);
        }
        // The terminal reflowed underneath us; nothing drawn before is trusted.
        self.rendered = None;
        self.repaint_forced = true;
        Some(Coord::new(
            i16::try_from(columns).unwrap_or(i16::MAX),
            i16::try_from(rows).unwrap_or(i16::MAX),
        ))
    }

    /// Note that raw bytes went to the terminal outside the cell model.
    pub fn note_stream_escape(&mut self) {
        if self.render_mode == RenderMode::Cells {
            self.rendered = None;
            self.repaint_forced = true;
        }
    }
}
