//! Shared control-paint helpers: clipped fills and guest text copies
//! (split from `controls.rs`).

use anyhow::Result;

use super::Dimension;
use crate::gdi32::{IRect, ResolvedWindow};
use crate::gdi32::{fill_rect_surface, subtract_rect};
use crate::user32::{WinApiState, write_guest_ansi_c_string, write_guest_utf16_c_string};

/// The direct child of `top` on the ancestor chain of `w` — the window in
/// `top`'s immediate child list that is `w` itself (when `w` is a direct
/// child) or the ancestor of `w` at that level. `None` when `w` is not in
/// `top`'s subtree (a different surface, or an orphan).
fn z_child_of(windows: &[crate::WindowRecord], top: u64, w: u64) -> Option<u64> {
    let mut current = w;
    loop {
        let window = windows.iter().find(|win| win.handle.as_u64() == current)?;
        let parent = window.parent_handle.as_u64();
        if parent == top {
            return Some(current);
        }
        if parent == 0 || parent == current {
            return None;
        }
        current = parent;
    }
}

/// The position of `w` inside `top`'s surface (accumulated child offsets).
fn surface_position_of(windows: &[crate::WindowRecord], top: u64, w: u64) -> (i32, i32) {
    let (mut x, mut y) = (0_i32, 0_i32);
    let mut current = w;
    loop {
        let Some(window) = windows.iter().find(|win| win.handle.as_u64() == current) else {
            return (x, y);
        };
        x = x.saturating_add(window.x);
        y = y.saturating_add(window.y);
        let parent = window.parent_handle.as_u64();
        if parent == top {
            return (x, y);
        }
        if parent == 0 || parent == current {
            return (x, y);
        }
        current = parent;
    }
}

/// The surface rects of the VISIBLE windows that composite ABOVE
/// `dc_window` in the surface of `top_hwnd` — a paint of `dc_window` must
/// never overwrite them.
///
/// Windows clips a window's update region to exclude the windows above it in
/// the z-order; WIE's shared-surface compositing (children paint directly
/// into the top-level surface, z-order-blind) must mirror that. Without it a
/// z-order-lower sibling's repaint destroys an overlapping dialog: the owner
/// EDIT's full-width `COLOR_WINDOW` band erase wipes the FindDialog's face
/// (the live "dialog upper region turns white" bug). Two windows in the same
/// surface are ordered by their z-child under the top-level: the
/// later-created z-child (and everything beneath it) composites on top. A
/// window on the SAME z-child branch as the painter is its ancestor (paints
/// below it) or its descendant (paints on top of it, own paint) — neither is
/// a clip for the painter.
pub(crate) fn above_window_rects(state: &WinApiState, top_hwnd: u64, dc_window: u64) -> Vec<IRect> {
    let Some(ws) = state.try_window_state() else {
        return Vec::new();
    };
    let windows = &ws.windows;
    let Some(dc_zchild) = z_child_of(windows, top_hwnd, dc_window) else {
        return Vec::new();
    };
    let Some(dc_zindex) = windows.iter().position(|w| w.handle.as_u64() == dc_zchild) else {
        return Vec::new();
    };
    let mut clip = Vec::new();
    for window in windows {
        if !window.visible || window.handle.as_u64() == dc_window {
            continue;
        }
        let Some(zchild) = z_child_of(windows, top_hwnd, window.handle.as_u64()) else {
            continue;
        };
        if zchild == dc_zchild {
            // Same branch: ancestor of the painter or painter's descendant.
            continue;
        }
        let above = windows
            .iter()
            .position(|w| w.handle.as_u64() == zchild)
            .is_none_or(|index| index > dc_zindex);
        if !above {
            continue;
        }
        let (x, y) = surface_position_of(windows, top_hwnd, window.handle.as_u64());
        clip.push(IRect::from_xywh(x, y, window.width, window.height));
    }
    clip
}

/// Decompose `rects` around the windows that composite above `dc_window` in
/// the surface of `top_hwnd` — the paint must skip the pixels those windows
/// own (see [`above_window_rects`]). Passed rects are in surface
/// coordinates; a rect with no overlap is unchanged.
pub(crate) fn clip_rects_around_above(
    state: &WinApiState,
    top_hwnd: u64,
    dc_window: u64,
    rects: Vec<IRect>,
) -> Vec<IRect> {
    let above = above_window_rects(state, top_hwnd, dc_window);
    if above.is_empty() {
        return rects;
    }
    let mut out = rects;
    for window in above {
        out = subtract_rect(out, window);
    }
    out
}

/// Fill a rect with `color`, clipped to the control's own bounds so a
/// selection or caret running past the right edge cannot bleed into the
/// ancestor surface — and around the windows that composite above the
/// control, so the fill cannot overwrite an overlapping dialog (the
/// z-order-aware update-region clip).
pub(super) fn fill_rect_clipped(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    control: Dimension,
    rect: IRect,
    color: u32,
) {
    let x0 = rect.left.max(info.offset_x);
    let y0 = rect.top.max(info.offset_y);
    let x1 = rect.right.min(info.offset_x.saturating_add(control.width));
    let y1 = rect
        .bottom
        .min(info.offset_y.saturating_add(control.height));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let rects = clip_rects_around_above(
        state,
        info.hwnd.as_u64(),
        info.dc_window.as_u64(),
        vec![IRect {
            left: x0,
            top: y0,
            right: x1,
            bottom: y1,
        }],
    );
    for rect in rects {
        if rect.width() <= 0 || rect.height() <= 0 {
            continue;
        }
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            rect.left,
            rect.top,
            rect.width(),
            rect.height(),
            color,
        );
    }
}

/// Fill a control-paint rect in `info`'s surface, decomposed around the
/// windows that composite above `info.dc_window` (the z-order-aware
/// update-region clip — see [`clip_rects_around_above`]). The rect is in
/// SURFACE coordinates (already offset by the control's position) and
/// pre-clipped to the control's own bounds by the caller.
pub(crate) fn fill_surface_rect_above_clipped(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    rect: IRect,
    color: u32,
) {
    let rects = clip_rects_around_above(
        state,
        info.hwnd.as_u64(),
        info.dc_window.as_u64(),
        vec![rect],
    );
    for rect in rects {
        if rect.width() <= 0 || rect.height() <= 0 {
            continue;
        }
        fill_rect_surface(
            state,
            info.hwnd,
            info.width,
            info.height,
            rect.left,
            rect.top,
            rect.width(),
            rect.height(),
            color,
        );
    }
}

/// Copy a control's text into a guest buffer (WM_GETTEXT / LB_GETTEXT).
pub(super) fn write_control_text(
    engine: &mut dyn wie_cpu::CpuEngine,
    unicode: bool,
    buffer_ptr: u64,
    max_characters: u64,
    text: &str,
) -> Result<u64> {
    if buffer_ptr == 0 {
        return Ok(0);
    }
    let capacity = usize::try_from(max_characters).unwrap_or(0);
    let copied = if unicode {
        write_guest_utf16_c_string(engine, buffer_ptr, capacity, text)?
    } else {
        write_guest_ansi_c_string(engine, buffer_ptr, capacity, text)?
    };
    Ok(u64::try_from(copied).unwrap_or(0))
}
