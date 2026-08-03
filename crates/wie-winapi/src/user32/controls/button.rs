//! BUTTON-class painting (push buttons) and the shared control-paint entry
//! (split from `controls.rs`).

use anyhow::Result;

use super::edit::paint_edit;
use super::listbox::paint_item_lines;
use super::r#static::paint_label;
use super::{
    COLOR_BTNFACE, COLOR_BTNFACE_PRESSED, COLOR_BTNSHADOW, COLOR_WINDOW, ControlClassKind,
    control_items, control_sel_index,
};
use crate::gdi32::ResolvedWindow;
use crate::gdi32::fill_rect_surface;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::{FontEngine, FontKey, ResolvedFont};
use crate::state::WindowFlags;
use crate::user32::{WinApiState, find_window};

/// Paint a control into its ancestor's surface at its parent-relative offset.
pub(super) fn paint_control(
    state: &mut WinApiState,
    engine: &mut dyn wie_cpu::CpuEngine,
    hwnd: u64,
    kind: ControlClassKind,
) -> Result<()> {
    tracing::trace!(target: "wiegui", kind = ?kind, hwnd, "control paint");
    let Some(info) = resolve_window_ancestor(state, hwnd) else {
        return Ok(());
    };
    let text = find_window(state, hwnd).map_or_else(String::new, |w| w.control_text.clone());
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));

    if kind == ControlClassKind::StatusBar {
        // Task 0.12: status bars render as an empty face-colored child rect;
        // parts, per-part text, and the grip are plan Task 3.1.
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            info.offset_x,
            info.offset_y,
            width,
            height,
            COLOR_BTNFACE,
        );
        return Ok(());
    }

    let pressed = find_window(state, hwnd).is_some_and(|w| w.flags.contains(WindowFlags::PRESSED));
    let items = control_items(state, hwnd).to_vec();
    let sel_index = control_sel_index(state, hwnd);

    // Controls draw with the font a WM_SETFONT stored on the window (notepad
    // sends one to its EDIT right after creation); a window without one — or
    // with an unknown HFONT — falls back to the system default (sans-serif
    // 16 px). Both resolve through the same engine cache. The engine is taken
    // out of gdi state so the paint can pass `&mut state` and `&mut font_engine`
    // side by side (a plain field cannot be split-borrowed alongside `state`);
    // it is put back unconditionally after the body. This is safe under the
    // single shared WinApiState mutex: every API handler — this WM_PAINT and
    // any concurrent one on another host thread — runs while holding it, so
    // the take and the put cannot interleave.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = FontKey::default();
    let key_and_resolved = match crate::gdi32::window_font_resolution(state, hwnd, &mut font_engine)
    {
        Some(key_and_resolved) => Some(key_and_resolved),
        None => font_engine
            .resolve(&default_key, 16)
            .map(|resolved| (default_key, resolved)),
    };
    let result = (|| -> Result<()> {
        let Some((key, resolved)) = &key_and_resolved else {
            // No system font: paint faces/borders but skip the text.
            return Ok(());
        };
        match kind {
            ControlClassKind::Button => {
                paint_face_and_border(state, &info, width, height, pressed);
                // The ampersand is a mnemonic marker, not caption glyph.
                let caption = strip_mnemonics(&text);
                let tx = centered_text_x(&info, width, &caption, &mut font_engine, resolved, key);
                paint_label(
                    state,
                    engine,
                    &info,
                    &caption,
                    tx,
                    width,
                    height,
                    pressed,
                    &mut font_engine,
                    resolved,
                    key,
                )?;
            }
            ControlClassKind::Static => {
                // COLOR_BTNFACE, not COLOR_WINDOW: a label sits on the dialog
                // face and must not show as a white box (full WM_CTLCOLOR* is
                // deferred).
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x,
                    info.offset_y,
                    width,
                    height,
                    COLOR_BTNFACE,
                );
                let caption = strip_mnemonics(&text);
                let tx = info.offset_x.saturating_add(2);
                paint_label(
                    state,
                    engine,
                    &info,
                    &caption,
                    tx,
                    width,
                    height,
                    false,
                    &mut font_engine,
                    resolved,
                    key,
                )?;
            }
            ControlClassKind::Edit => {
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x,
                    info.offset_y,
                    width,
                    height,
                    COLOR_WINDOW,
                );
                stroke_border(state, &info, width, height, 0x0000_0000);
                let tx = info.offset_x.saturating_add(2);
                paint_edit(
                    state,
                    engine,
                    &info,
                    &text,
                    tx,
                    width,
                    height,
                    &mut font_engine,
                    resolved,
                    key,
                )?;
            }
            ControlClassKind::ListBox => {
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    info.offset_x,
                    info.offset_y,
                    width,
                    height,
                    COLOR_WINDOW,
                );
                stroke_border(state, &info, width, height, 0x0000_0000);
                paint_item_lines(
                    state,
                    engine,
                    &info,
                    &items,
                    width,
                    height,
                    sel_index,
                    &mut font_engine,
                    resolved,
                    key,
                )?;
            }
            ControlClassKind::ComboBox => {
                paint_face_and_border(state, &info, width, height, false);
                let first = items.first().map_or("", String::as_str);
                let tx = info.offset_x.saturating_add(4);
                paint_label(
                    state,
                    engine,
                    &info,
                    first,
                    tx,
                    width,
                    height,
                    false,
                    &mut font_engine,
                    resolved,
                    key,
                )?;
            }
            // Handled by the early return above (no font needed for the empty
            // face rect); kept only to keep the match exhaustive.
            ControlClassKind::StatusBar => {}
        }
        Ok(())
    })();
    state.gdi_state().font_engine = font_engine;
    result
}

/// Fill a control's face (COLOR_BTNFACE) and draw its 1 px BTNSHADOW border.
fn paint_face_and_border(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    width: i32,
    height: i32,
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
        width,
        height,
        face,
    );
    stroke_border(state, info, width, height, COLOR_BTNSHADOW);
}

/// Draw a 1 px border around a control's rect.
fn stroke_border(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    width: i32,
    height: i32,
    color: u32,
) {
    if width <= 0 || height <= 0 {
        return;
    }
    let (x, y) = (info.offset_x, info.offset_y);
    let right = x.saturating_add(width).saturating_sub(1);
    let bottom = y.saturating_add(height).saturating_sub(1);
    fill_rect_surface(
        state,
        info.hwnd,
        info.width,
        info.height,
        x,
        y,
        width,
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
        width,
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
        height,
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
        height,
        color,
    );
}

/// Remove `&` mnemonic markers from a caption so they are not rendered
/// literally (the underline + Alt activation are deferred). `&&` is the
/// escaped form of a literal ampersand, matching Windows.
#[must_use]
fn strip_mnemonics(text: &str) -> String {
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
fn centered_text_x(
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

#[cfg(test)]
mod tests {
    use super::strip_mnemonics;

    #[test]
    fn strip_mnemonics_drops_single_marker() {
        assert_eq!(strip_mnemonics("&About"), "About");
        assert_eq!(strip_mnemonics("&Quit"), "Quit");
        assert_eq!(strip_mnemonics("Plain"), "Plain");
    }

    #[test]
    fn strip_mnemonics_keeps_doubled_ampersand() {
        // `&&` is the escaped form and renders as a literal `&`.
        assert_eq!(strip_mnemonics("A&&B"), "A&B");
        assert_eq!(strip_mnemonics("&&"), "&");
        assert_eq!(strip_mnemonics("&A&&B"), "A&B");
    }

    #[test]
    fn strip_mnemonics_empty() {
        assert_eq!(strip_mnemonics(""), "");
    }
}
