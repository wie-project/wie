//! The comctl32 status bar (`msctls_statusbar32`): its `SB_*` message
//! dispatch and its painters.
//!
//! This control lives here, in one place, rather than split across the
//! comctl32 dispatcher and the control-paint entry point: the `SB_*` messages
//! own the per-part text and the `part_rights` layout, and the paint consumes
//! exactly that. Before this module the dispatcher sat in `crate::comctl32`
//! (reaching back into `user32::controls` for `ControlClassKind` /
//! `ControlState` / the `SB_*` constants) while the painters sat in
//! `controls/button.rs` — a seam violation in both directions.
//!
//! The generic control arms in `super::dispatch_control_proc` (WM_PAINT,
//! WM_GETTEXT, WM_SETTEXT, WM_SETFONT, …) run first; [`dispatch_status_bar_message`]
//! handles what falls through.

use anyhow::{Context, Result};

use super::listbox::render_control_text;
use super::{
    CCS_BOTTOM, CCS_NOPARENTALIGN, CCS_NORESIZE, COLOR_BTNFACE, COLOR_BTNHIGHLIGHT,
    COLOR_BTNSHADOW, ControlClassKind, ControlState, Dimension, PaintCtx, PaintFont, SB_GETPARTS,
    SB_GETTEXTA, SB_GETTEXTLENGTHA, SB_GETTEXTLENGTHW, SB_GETTEXTW, SB_SETPARTS, SB_SETTEXTA,
    SB_SETTEXTW, SBT_NOBORDERS, SBT_OWNERDRAW, SBT_POPOUT, control_state,
};
use crate::gdi32::fill_rect_surface;
use crate::gdi32::window_font_resolution_or_default;
use crate::gdi32::{FontEngine, FontKey, IRect, ResolvedFont, ResolvedWindow};
use crate::guest_memory::{read_i32, write_i32 as write_guest_i32};
use crate::user32::{
    WinApiState, find_window, find_window_mut, read_guest_ansi_lossy, read_guest_utf16_lossy,
    window_client_size, write_guest_ansi_c_string, write_guest_utf16_c_string,
};

/// `WM_SIZE` — the message the status bar intercepts to reposition itself.
const WM_SIZE_MSG: u32 = crate::user32::wm::WinMsg::WM_SIZE.as_u32();

/// Cap for the `SB_GETTEXTW`/`SB_GETTEXTA` guest output buffers (the message
/// has no size argument — the guest is expected to provide a big-enough
/// buffer, so a fixed generous cap bounds the write).
const SB_GETTEXT_CAP: usize = 1024;

// ── Task 3.1: the STATUSCLASSNAMEW control's own message dispatch ────────
//
// The generic control arms in `dispatch_control_proc` (WM_PAINT, WM_GETTEXT,
// WM_SETTEXT, WM_SETFONT, …) run first; `dispatch_status_bar_message` handles
// what falls through: the comctl32 SB_* messages (WM_USER+ offsets that
// `WinMsg` cannot name) and WM_SIZE, which repositions the bar inside its
// parent at the default font-derived height.

/// The status-bar state for `hwnd`, seeded on first touch.
///
/// The generic seeder (`ControlClassKind::new_state`) leaves `part_texts`
/// empty; this seeder additionally folds the `CreateStatusWindowA/W` text
/// into part 0 — real comctl32 applies that text as `SB_SETTEXT(0, …)`
/// internally, so part 0 shows it until a guest overwrites part 0.
fn status_state_mut(state: &mut WinApiState, hwnd: u64) -> Option<&mut ControlState> {
    let ws = state.window_state();
    let seed_text = ws
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .map(|w| w.control_text.clone());
    let control = ws
        .control_states
        .entry(crate::handles::Hwnd::from(hwnd))
        .or_insert_with(|| ControlClassKind::StatusBar.new_state());
    if let ControlState::StatusBar { part_texts, .. } = control
        && part_texts.is_empty()
        && let Some(text) = seed_text
    {
        part_texts.push(text);
    }
    Some(control)
}

/// The text of status-bar `part`: the stored per-part text, or — for part 0
/// with nothing stored yet — the window's `control_text` (the
/// `CreateStatusWindowA/W` text, which real comctl32 applies to part 0).
/// The painter in [`super::paint_control`] reads the same resolved text the
/// SB_GETTEXT* handlers return.
#[must_use]
pub(super) fn status_part_text(state: &WinApiState, hwnd: u64, part: usize) -> String {
    let stored = match state
        .try_window_state()
        .and_then(|ws| ws.control_states.get(&crate::handles::Hwnd::from(hwnd)))
    {
        Some(ControlState::StatusBar { part_texts, .. }) => part_texts.get(part).cloned(),
        _ => None,
    };
    match stored {
        Some(text) => text,
        None if part == 0 => state
            .try_window_state()
            .and_then(|ws| {
                ws.windows
                    .iter()
                    .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            })
            .map_or_else(String::new, |w| w.control_text.clone()),
        None => String::new(),
    }
}

/// Mark the bar for a future synthesized WM_PAINT after an SB_* mutation.
fn status_bar_invalidate(state: &mut WinApiState, hwnd: u64) {
    if let Some(window) = find_window_mut(state, hwnd) {
        window.invalidated = true;
    }
}

/// Host-side message dispatch for a STATUSCLASSNAMEW status-bar window.
pub(super) fn dispatch_status_bar_message(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut WinApiState,
    hwnd: u64,
    message: u32,
    word_parameter: u64,
    long_parameter: u64,
) -> Result<Option<u64>> {
    match message {
        WM_SIZE_MSG => {
            // WM_SIZE to a status bar repositions it in its parent (notepad
            // sends SendMessageW(hStatusBar, WM_SIZE, 0, 0) after creating
            // the bar, then reads the resulting rect to size the EDIT).
            status_bar_reposition(state, hwnd)?;
            Ok(Some(0))
        }
        // SB_SETPARTS: wParam = part count, lParam = int array of right-edge
        // coordinates (client coords); -1 = extend to the right edge.
        SB_SETPARTS => {
            let count = usize::try_from(word_parameter).unwrap_or(0);
            let mut rights = Vec::with_capacity(count);
            for index in 0..count {
                let off = u64::try_from(index).unwrap_or(u64::MAX).saturating_mul(4);
                let value = if long_parameter == 0 {
                    -1 // no array: the whole strip is one part
                } else {
                    read_i32(engine, long_parameter.saturating_add(off))
                        .context("SB_SETPARTS part width read failed")?
                };
                rights.push(value);
            }
            if let Some(ControlState::StatusBar { part_rights, .. }) = status_state_mut(state, hwnd)
            {
                *part_rights = rights;
            }
            status_bar_invalidate(state, hwnd);
            Ok(Some(1)) // TRUE
        }
        // SB_SETTEXTW (WM_USER+11): wParam = part | SBT_* flags (the part is
        // the low byte; the SBT_NOBORDERS/POPOUT/OWNERDRAW bits are masked
        // off and ignored), lParam = wide string. Modern comctl32 splits
        // SB_SETTEXT into A/W variants — notepad sends the W variant (0x040B)
        // with UTF-16 text.
        SB_SETTEXTW => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let _flags = word_parameter
                & (u64::from(SBT_NOBORDERS) | u64::from(SBT_POPOUT) | u64::from(SBT_OWNERDRAW));
            let text = if long_parameter == 0 {
                String::new()
            } else {
                read_guest_utf16_lossy(engine, long_parameter, 32_768)
                    .context("SB_SETTEXTW text read failed")?
            };
            tracing::debug!(
                target: "wie_winapi",
                hwnd = format_args!("{hwnd:#x}"),
                part,
                text = %text,
                "SB_SETTEXTW"
            );
            status_bar_set_text(state, hwnd, part, text);
            Ok(Some(1)) // TRUE
        }
        // SB_SETTEXTA (WM_USER+1, the legacy pre-v6 single-variant value too):
        // same message with an ANSI string.
        SB_SETTEXTA => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let _flags = word_parameter
                & (u64::from(SBT_NOBORDERS) | u64::from(SBT_POPOUT) | u64::from(SBT_OWNERDRAW));
            let text = if long_parameter == 0 {
                String::new()
            } else {
                read_guest_ansi_lossy(engine, long_parameter, 32_768)
                    .context("SB_SETTEXTA text read failed")?
            };
            tracing::debug!(
                target: "wie_winapi",
                hwnd = format_args!("{hwnd:#x}"),
                part,
                text = %text,
                "SB_SETTEXTA"
            );
            status_bar_set_text(state, hwnd, part, text);
            Ok(Some(1)) // TRUE
        }
        // SB_GETPARTS: wParam = max parts to copy, lParam = int buffer;
        // returns the part count.
        SB_GETPARTS => {
            let rights = match state
                .try_window_state()
                .and_then(|ws| ws.control_states.get(&crate::handles::Hwnd::from(hwnd)))
            {
                Some(ControlState::StatusBar { part_rights, .. }) => part_rights.clone(),
                _ => Vec::new(),
            };
            // No parts configured = one implicit part spanning the strip.
            let count = if rights.is_empty() { 1 } else { rights.len() };
            if long_parameter != 0 {
                let max = usize::try_from(word_parameter).unwrap_or(0).min(count);
                for index in 0..max {
                    let value = if rights.is_empty() {
                        -1 // the implicit single part extends to the edge
                    } else {
                        rights.get(index).copied().unwrap_or(-1)
                    };
                    let off = u64::try_from(index).unwrap_or(u64::MAX).saturating_mul(4);
                    write_guest_i32(engine, long_parameter.saturating_add(off), value)
                        .context("SB_GETPARTS width write failed")?;
                }
            }
            Ok(Some(u64::try_from(count).unwrap_or(0)))
        }
        // SB_GETTEXTW: wParam = part, lParam = WCHAR buffer; returns the
        // char count (excluding the NUL).
        SB_GETTEXTW => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let text = status_part_text(state, hwnd, part);
            let copied = write_guest_utf16_c_string(engine, long_parameter, SB_GETTEXT_CAP, &text)
                .context("SB_GETTEXTW buffer write failed")?;
            Ok(Some(u64::try_from(copied).unwrap_or(0)))
        }
        // SB_GETTEXTA: same with an ANSI buffer.
        SB_GETTEXTA => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let text = status_part_text(state, hwnd, part);
            let copied = write_guest_ansi_c_string(engine, long_parameter, SB_GETTEXT_CAP, &text)
                .context("SB_GETTEXTA buffer write failed")?;
            Ok(Some(u64::try_from(copied).unwrap_or(0)))
        }
        // SB_GETTEXTLENGTHW / SB_GETTEXTLENGTHA: the stored text length in
        // the message's units (WCHARs for W, bytes for A).
        SB_GETTEXTLENGTHW => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let text = status_part_text(state, hwnd, part);
            Ok(Some(
                u64::try_from(text.encode_utf16().count()).unwrap_or(0),
            ))
        }
        SB_GETTEXTLENGTHA => {
            let part = usize::try_from(word_parameter & 0xFF).unwrap_or(0);
            let text = status_part_text(state, hwnd, part);
            Ok(Some(u64::try_from(text.len()).unwrap_or(0)))
        }
        _ => Ok(None),
    }
}

/// Store a per-part text (SB_SETTEXTW/SB_SETTEXTA), extending the part vec
/// with empty cells so `part` is always addressable.
fn status_bar_set_text(state: &mut WinApiState, hwnd: u64, part: usize, text: String) {
    if let Some(ControlState::StatusBar { part_texts, .. }) = status_state_mut(state, hwnd) {
        if part >= part_texts.len() {
            part_texts.resize(part.saturating_add(1), String::new());
        }
        if let Some(slot) = part_texts.get_mut(part) {
            *slot = text;
        }
    }
    status_bar_invalidate(state, hwnd);
}

/// Reposition the bar inside its parent — the WM_SIZE behavior real comctl32
/// gives the built-in class: full parent width at the default font-derived
/// height, flush at the parent bottom (CCS_BOTTOM) or top (CCS_TOP), unless
/// CCS_NOPARENTALIGN / CCS_NORESIZE opt out.
fn status_bar_reposition(state: &mut WinApiState, hwnd: u64) -> Result<()> {
    let (parent_handle, style) = find_window(state, hwnd)
        .map(|w| (w.parent_handle, w.style))
        .context("status bar record missing for WM_SIZE")?;
    if parent_handle == crate::handles::Hwnd::NULL {
        return Ok(());
    }
    let (parent_w, parent_h) = window_client_size(state, parent_handle.as_u64());
    let height = status_bar_default_height(state, hwnd)?;
    let width = if style & CCS_NORESIZE != 0 {
        find_window(state, hwnd).map_or(0, |w| w.width)
    } else {
        parent_w
    };
    let (x, y) = if style & CCS_NOPARENTALIGN != 0 {
        // The guest placed the bar itself; leave its position alone.
        find_window(state, hwnd).map_or((0, 0), |w| (w.x, w.y))
    } else if style & CCS_BOTTOM != 0 {
        // notepad's bar: flush at the parent's bottom edge.
        (0, parent_h.saturating_sub(height))
    } else {
        // CCS_TOP (and the unaligned default): flush at the top edge.
        (0, 0)
    };
    if let Some(window) = find_window_mut(state, hwnd) {
        window.x = x;
        window.y = y;
        window.width = width;
        window.height = height;
        window.invalidated = true;
    }
    Ok(())
}

/// The bar's default height: the stored control font's line height (the
/// system default when none is set) plus the two client-edge border rows.
fn status_bar_default_height(state: &mut WinApiState, hwnd: u64) -> Result<i32> {
    let line_h = state.with_font_engine(|state, font_engine| {
        window_font_resolution_or_default(state, hwnd, font_engine)
            .map_or(16, |(_key, resolved)| resolved.line_height())
    });
    Ok(line_h.saturating_add(4))
}
/// Paint a STATUSCLASSNAMEW strip: the BTNFACE face plus the classic raised
/// client edge — a light (COLOR_BTNHIGHLIGHT) line along the top and the
/// shadow (COLOR_BTNSHADOW) line along the bottom (Task 3.1).
pub(super) fn paint_status_bar_strip(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    size: Dimension,
) {
    if size.width <= 0 || size.height <= 0 {
        return;
    }
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        size.width,
        size.height,
        COLOR_BTNFACE,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        size.width,
        1,
        COLOR_BTNHIGHLIGHT,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y.saturating_add(size.height).saturating_sub(1),
        size.width,
        1,
        COLOR_BTNSHADOW,
    );
}

/// Draw the classic comctl32 vertical groove at every part boundary.
///
/// Real Windows divides status-bar parts with a sunken groove: a BTNSHADOW
/// vertical line at the boundary column with a BTNHIGHLIGHT line immediately
/// to its right — shadow-left/highlight-right, the `EDGE_SUNKEN` direction,
/// mirroring the raised client edge (`paint_status_bar_strip`). The lines
/// span only the interior rows (one below the top highlight, one above the
/// bottom shadow) so the corners join the client edges cleanly. The last
/// part always extends to the right edge of the strip and gets no right
/// separator; a boundary at or past the strip's edge leaves no room either.
pub(super) fn paint_status_bar_separators(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    size: Dimension,
    part_rights: &[i32],
) {
    if part_rights.is_empty() || size.height < 3 {
        return;
    }
    let top = info.offset_y.saturating_add(1);
    let rows = size.height.saturating_sub(2);
    let interior = part_rights.len().saturating_sub(1);
    for right in part_rights.iter().take(interior) {
        // -1 (SB_SETPARTS) means "extend to the right edge".
        let boundary = if *right < 0 { size.width } else { *right };
        if boundary >= size.width {
            continue;
        }
        let bx = info.offset_x.saturating_add(boundary);
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            bx,
            top,
            1,
            rows,
            COLOR_BTNSHADOW,
        );
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            bx.saturating_add(1),
            top,
            1,
            rows,
            COLOR_BTNHIGHLIGHT,
        );
    }
}

/// Horizontal inset (px) on each side of a status-bar part's text cell, so
/// the ink never touches the cell boundary or the next part's groove.
const STATUS_BAR_TEXT_INSET: i32 = 3;

/// The px size a status bar with NO `WM_SETFONT` draws its part text at.
///
/// Real Windows defaults such a bar to the DEFAULT_GUI_FONT (~12 px); WIE's
/// 16 px system default is ~25% wider, and RNotepad lays its parts out for
/// the smaller font WITHOUT measuring the text: `DIALOG_StatusBarAlignParts`
/// sets the EOL cell ("Windows (CR + LF)") to a FIXED 120 px box
/// (`max(client_w - 120, 240)` right edge, part 0 at `max(client_w - 240,
/// 120)`). At 16 px that text is ~125 px and the last glyph clips at the 120
/// px boundary — the ")" lands under the next part. Resolving the no-font
/// bar at the same 13 px the guest uses for its own UI font keeps the fixed
/// geometry working, exactly like real Windows' default-GUI-font behavior.
const STATUS_BAR_DEFAULT_FONT_PX: i32 = 13;

/// The font the status-bar part text renders with.
///
/// A bar with a `WM_SETFONT` stored font keeps the paint's normal resolution
/// (the stored font or the 16 px system default fallback). A bar without one
/// — RNotepad never sends WM_SETFONT to its status bar — drops to
/// [`STATUS_BAR_DEFAULT_FONT_PX`] instead of the 16 px default (see the
/// constant's doc). The caller's `key`/`resolved` are owned so the override
/// can return a freshly resolved pair.
pub(super) fn status_bar_part_font(
    state: &mut WinApiState,
    hwnd: u64,
    font_engine: &mut FontEngine,
    key: &FontKey,
    resolved: &ResolvedFont,
) -> (FontKey, ResolvedFont) {
    let stored = find_window(state, hwnd)
        .map(|w| w.font_handle)
        .unwrap_or(crate::handles::Hfont::NULL);
    if stored != crate::handles::Hfont::NULL {
        return (key.clone(), resolved.clone());
    }
    font_engine
        .resolve(key, STATUS_BAR_DEFAULT_FONT_PX)
        .map_or((key.clone(), resolved.clone()), |smaller| {
            (key.clone(), smaller)
        })
}

/// Draw each status-bar part's text, left-aligned in its cell with a small
/// horizontal inset and vertically centered, CLIPPED to the cell so a long
/// text cannot bleed into the next part. The last part always extends to the
/// right edge; with no `SB_SETPARTS` the whole strip is one part.
pub(super) fn paint_status_bar_parts(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    info: &ResolvedWindow,
    hwnd: u64,
    font: &mut PaintFont<'_>,
) -> Result<()> {
    // The control's own client size (the part cells are laid out inside it).
    let size = find_window(state, hwnd).map_or(
        Dimension {
            width: 0,
            height: 0,
        },
        |w| Dimension {
            width: w.width,
            height: w.height,
        },
    );
    let part_rights = match control_state(state, hwnd) {
        Some(ControlState::StatusBar { part_rights, .. }) => part_rights.clone(),
        _ => Vec::new(),
    };
    let parts = if part_rights.is_empty() {
        1
    } else {
        part_rights.len()
    };
    let line_h = font.resolved.line_height();
    // Vertically centered between the strip's edges, at least one row below
    // the top border so the ink never touches the client edge.
    let ty = info
        .offset_y
        .saturating_add(size.height.saturating_sub(line_h).saturating_div(2))
        .max(info.offset_y.saturating_add(1));
    let strip_bottom = info.offset_y.saturating_add(size.height);
    let mut left = 0_i32;
    for index in 0..parts {
        let right = if part_rights.is_empty() {
            size.width
        } else {
            let value = part_rights.get(index).copied().unwrap_or(size.width);
            if value < 0 { size.width } else { value }
        };
        let cell_left = left;
        left = right;
        let text = super::statusbar::status_part_text(state, hwnd, index);
        if text.is_empty() {
            continue;
        }
        let tx = info
            .offset_x
            .saturating_add(cell_left)
            .saturating_add(STATUS_BAR_TEXT_INSET);
        // The clip mirrors the left inset on the right: text stops
        // `STATUS_BAR_TEXT_INSET` px before the cell edge (real Windows
        // status-bar cells carry the same margin), so the last glyph never
        // touches the boundary or the next part's groove.
        let clip_right = right.saturating_sub(STATUS_BAR_TEXT_INSET);
        render_control_text(
            &mut PaintCtx { state, engine },
            info.hwnd,
            IRect::from_xywh(
                tx,
                ty,
                i32::try_from(info.width).unwrap_or(0),
                i32::try_from(info.height).unwrap_or(0),
            ),
            &text,
            0, // COLOR_BTNTEXT / COLOR_WINDOWTEXT: black
            Some(IRect {
                left: info.offset_x.saturating_add(cell_left),
                top: info.offset_y,
                right: info.offset_x.saturating_add(clip_right),
                bottom: strip_bottom,
            }),
            font,
        )?;
    }
    Ok(())
}
