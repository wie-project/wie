//! Dialog rendering: the dialog window's face + border painted into the
//! owner's present surface.

use crate::gdi32::IRect;
use crate::gdi32::fill_rect_surface;
use crate::gdi32::resolve_window_ancestor;
use crate::gdi32::subtract_rect;
use crate::user32::{WinApiState, find_window};

/// `GetSysColor(COLOR_BTNFACE)` — the classic dialog face color.
const DIALOG_BG: u32 = 0x00F0_F0F0;
/// `GetSysColor(COLOR_BTNSHADOW)` — the dialog border gray.
const DIALOG_BORDER: u32 = 0x00A0_A0A0;

/// Paint a dialog window's face + border into the owner's surface.
///
/// The face honors the dialog's forced `WS_CLIPCHILDREN` (subtract_rect
/// decomposition, mirroring `blit.rs`), so a dialog repaint can never erase
/// its controls. Publishes the owner surface so headless frame capture sees
/// the dialog the moment it opens; control paints publish afterwards.
pub(crate) fn paint_dialog(state: &mut WinApiState, hwnd: u64) {
    let (width, height) = find_window(state, hwnd).map_or((0, 0), |w| (w.width, w.height));
    if width <= 0 || height <= 0 {
        return;
    }
    let Some(info) = resolve_window_ancestor(state, hwnd) else {
        return;
    };
    // WS_CLIPCHILDREN: decompose the face rect around every visible child so
    // the face fill cannot cover a control that painted first.
    let children: Vec<IRect> = state
        .window_state()
        .windows
        .iter()
        .filter(|w| w.parent_handle == crate::handles::Hwnd::from(hwnd) && w.visible)
        .map(|w| IRect::from_xywh(w.x, w.y, w.width, w.height))
        .collect();
    let mut rects = vec![IRect {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    }];
    for child in children {
        rects = subtract_rect(rects, child);
    }
    for rect in rects {
        if rect.width() <= 0 || rect.height() <= 0 {
            continue;
        }
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            rect.left.saturating_add(info.offset_x),
            rect.top.saturating_add(info.offset_y),
            rect.width(),
            rect.height(),
            DIALOG_BG,
        );
    }
    // 1 px border around the dialog (classic 3D shadow gray).
    if width > 2 && height > 2 {
        let right = info.offset_x.saturating_add(width).saturating_sub(1);
        let bottom = info.offset_y.saturating_add(height).saturating_sub(1);
        for rect in [
            (info.offset_x, info.offset_y, width, 1),
            (info.offset_x, bottom, width, 1),
            (info.offset_x, info.offset_y, 1, height),
            (right, info.offset_y, 1, height),
        ] {
            fill_rect_surface(
                state,
                info.hwnd,
                info.width,
                info.height,
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                DIALOG_BORDER,
            );
        }
    }
    // One publish for the whole face + border (per-piece publishes would fire
    // the wake callback once per decomposed rect). B3.6: deferred — the
    // runtime drains pending publishes once per repaint cycle, so the
    // dialog face and the control paints that follow it emit one frame.
    state.present().publish_deferred(info.hwnd);
}
