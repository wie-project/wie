//! BUTTON-class drawing: the face, the 1 px border, and the caption
//! mnemonic handling.
//!
//! Split out of the old `button.rs`, which despite its name also held the
//! all-control paint entry point (now `super::paint`) and the comctl32
//! status-bar painters (now `super::statusbar`).

use super::{COLOR_BTNFACE, COLOR_BTNFACE_PRESSED, COLOR_BTNSHADOW, Dimension};
use crate::gdi32::IRect;
use crate::gdi32::fill_rect_surface;
use crate::gdi32::{FontEngine, FontKey, ResolvedFont, ResolvedWindow};
use crate::user32::WinApiState;

/// Fill a control's face (COLOR_BTNFACE) and draw its 1 px BTNSHADOW border.
pub(super) fn paint_face_and_border(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    size: Dimension,
    pressed: bool,
) {
    let face = if pressed {
        COLOR_BTNFACE_PRESSED
    } else {
        COLOR_BTNFACE
    };
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        info.offset_x,
        info.offset_y,
        size.width,
        size.height,
        face,
    );
    stroke_border(state, info, size, COLOR_BTNSHADOW);
}

/// Draw a 1 px border around a control's rect.
pub(super) fn stroke_border(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    size: Dimension,
    color: u32,
) {
    if size.width <= 0 || size.height <= 0 {
        return;
    }
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(size.width).saturating_sub(1);
    let bottom = y.saturating_add(size.height).saturating_sub(1);
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        y,
        size.width,
        1,
        color,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        bottom,
        size.width,
        1,
        color,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        y,
        1,
        size.height,
        color,
    );
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        right,
        y,
        1,
        size.height,
        color,
    );
}

/// Re-stroke ONLY the 1 px border edges a partial repaint overpainted — the
/// LISTBOX's row bands span the client's full width and start at its top
/// edge, so a mid-client erase wipes the left/right edges (and the top/bottom
/// only when the band reaches them) but must not re-draw untouched edges:
/// the border fill marks the surface dirty, so an over-eager full re-stroke
/// would widen the published region beyond the true changed band. The edges
/// are clipped to the dirty rect's vertical extent so the re-stroked pixels
/// always land inside the erased area.
pub(super) fn stroke_border_partial(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    size: Dimension,
    dirty: IRect,
    color: u32,
) {
    if size.width <= 0 || size.height <= 0 {
        return;
    }
    let (x, y) = (info.offset_x, info.offset_y);
    let band_top = dirty.top.max(0);
    let band_bottom = dirty.bottom.min(size.height);
    if dirty.top <= 0 {
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            x,
            y,
            size.width,
            1,
            color,
        );
    }
    if dirty.bottom >= size.height {
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            x,
            y.saturating_add(size.height).saturating_sub(1),
            size.width,
            1,
            color,
        );
    }
    if dirty.left <= 0 && band_bottom > band_top {
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            x,
            y.saturating_add(band_top),
            1,
            band_bottom.saturating_sub(band_top),
            color,
        );
    }
    if dirty.right >= size.width && band_bottom > band_top {
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            x.saturating_add(size.width).saturating_sub(1),
            y.saturating_add(band_top),
            1,
            band_bottom.saturating_sub(band_top),
            color,
        );
    }
}

/// Remove `&` mnemonic markers from a caption so they are not rendered
/// literally (the underline + Alt activation are deferred). `&&` is the
/// escaped form of a literal ampersand, matching Windows.
#[must_use]
pub(super) fn strip_mnemonics(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut iter = text.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '&' {
            if iter.peek() == Some(&'&') {
                out.push('&');
                iter.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// X origin for a horizontally centered single-line control caption.
pub(super) fn centered_text_x(
    info: &ResolvedWindow,
    width: i32,
    text: &str,
    font_engine: &mut FontEngine,
    resolved: &ResolvedFont,
    key: &FontKey,
) -> i32 {
    let text_w = font_engine.text_advance(resolved, key, text, text.chars().count());
    info.offset_x
        .saturating_add(width.saturating_sub(text_w).saturating_div(2))
}
