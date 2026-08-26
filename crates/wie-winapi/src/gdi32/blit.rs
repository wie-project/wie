use anyhow::{Context, Result};

use crate::gdi32::{ArgReg, read_arg};
use crate::guest_layout::Rect;
use crate::guest_memory::{checked_address, read_i32, read_u64, with_typed_read};
use crate::user32::WS_CLIPCHILDREN;
use crate::{
    HandlerContext, WinApiHandlerResult, WinApiState, WindowRecord, gdi32::DcKind,
    gdi32::pixel::clip_blit_rect, gdi32::state::brush_color, user32::low_i32,
};
use wie_cpu::mask_bgra_to_0rgb;

/// ROP codes.
const SRCCOPY: u32 = 0x00CC_0020;
const BLACKNESS: u32 = 0x0000_0042;
const WHITENESS: u32 = 0x00FF_0062;

/// Axis-aligned integer rect with an exclusive right/bottom edge.
///
/// Also carried by [`crate::present::SurfaceFrame::region`] and
/// [`crate::present::WindowSurface::dirty`] as the B3 partial-repaint region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl IRect {
    /// Degenerate (empty) rect — "nothing dirty" in a [`crate::present`]
    /// dirty accumulator (the inverse of `None`, which means "full surface").
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }
    }

    /// Width (may be zero/negative for degenerate rects).
    #[must_use]
    pub fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    /// Height (may be zero/negative for degenerate rects).
    #[must_use]
    pub fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }

    /// Build a rect from an origin + extent — the `x + width` right/bottom
    /// math shared by the window-rect and WS_CLIPCHILDREN child-clip
    /// constructions (`gdi32/blit.rs`, `user32/message/synth.rs`,
    /// `user32/dialog.rs`). Saturating: the clip-construction inputs come from
    /// bounded guest window geometry.
    #[must_use]
    pub const fn from_xywh(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            right: left.saturating_add(width),
            bottom: top.saturating_add(height),
        }
    }

    /// Whether this rect overlaps `other` (strict, exclusive edges).
    #[must_use]
    fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.top < other.bottom
            && other.top < self.bottom
    }
}

/// Subtract one child rect from a set of rects (exact decomposition).
///
/// Every input rect that overlaps `child` is replaced by the up-to-four
/// disjoint pieces outside it (left / right / above / below). Applying this
/// once per child, with non-overlapping children, leaves a set whose union is
/// exactly the destination minus the children — the WS_CLIPCHILDREN clip.
pub(crate) fn subtract_rect(rects: Vec<IRect>, child: IRect) -> Vec<IRect> {
    let mut out = Vec::new();
    for rect in rects {
        if !rect.intersects(child) {
            out.push(rect);
            continue;
        }
        // Left of the child, spanning the rect's full height.
        if rect.left < child.left {
            out.push(IRect {
                left: rect.left,
                top: rect.top,
                right: child.left.min(rect.right),
                bottom: rect.bottom,
            });
        }
        // Right of the child, spanning the rect's full height.
        if rect.right > child.right {
            out.push(IRect {
                left: child.right.max(rect.left),
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            });
        }
        // Band above the child, clipped to the child's horizontal span.
        if rect.top < child.top {
            out.push(IRect {
                left: child.left.max(rect.left),
                top: rect.top,
                right: child.right.min(rect.right),
                bottom: child.top.min(rect.bottom),
            });
        }
        // Band below the child, clipped to the child's horizontal span.
        if rect.bottom > child.bottom {
            out.push(IRect {
                left: child.left.max(rect.left),
                top: child.bottom.max(rect.top),
                right: child.right.min(rect.right),
                bottom: rect.bottom,
            });
        }
    }
    out
}

/// The smallest axis-aligned rect covering both inputs (the min/max union the
/// dirty accumulators and the label-caption scopes apply). Callers keep their
/// own empty-sentinel handling (a degenerate rect is NOT a union identity —
/// blending a (0,0,0,0) origin into a write would widen the region to the
/// surface's top-left).
#[must_use]
pub(crate) fn union_rect(a: IRect, b: IRect) -> IRect {
    IRect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

/// The overlap of two rects (empty when they do not overlap).
#[must_use]
pub(crate) fn intersect_rect(a: IRect, b: IRect) -> IRect {
    IRect {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    }
}

/// Walk `windows` from `hwnd` to its top-level ancestor, accumulating the
/// starting window's position in the ancestor's client coordinate space.
///
/// A broken parent chain (parent not in `windows`) falls back to `hwnd` itself
/// with offset (0, 0), preserving the pre-child-model behavior for orphans.
/// Returns `None` when `hwnd` itself is unknown.
#[must_use]
pub(crate) fn ancestor_offset(
    windows: &[WindowRecord],
    hwnd: u64,
) -> Option<(crate::handles::Hwnd, i32, i32)> {
    let mut current = crate::handles::Hwnd::from(hwnd);
    let mut offset_x = 0_i32;
    let mut offset_y = 0_i32;
    loop {
        let window = windows.iter().find(|w| w.handle == current)?;
        let parent = window.parent_handle;
        if parent == crate::handles::Hwnd::NULL || parent == current {
            return Some((current, offset_x, offset_y));
        }
        if !windows.iter().any(|w| w.handle == parent) {
            return Some((current, offset_x, offset_y));
        }
        offset_x = offset_x.saturating_add(window.x);
        offset_y = offset_y.saturating_add(window.y);
        current = parent;
    }
}

/// Destination surface info for a window DC or a control paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedWindow {
    /// Top-level ancestor whose present surface receives the pixels.
    pub hwnd: crate::handles::Hwnd,
    /// Ancestor client size — the surface dimensions.
    pub width: u32,
    /// Ancestor client size — the surface dimensions.
    pub height: u32,
    /// Position of the DC's window within the ancestor client space.
    pub offset_x: i32,
    /// Position of the DC's window within the ancestor client space.
    pub offset_y: i32,
    /// The DC's own window handle (for the WS_CLIPCHILDREN child lookup).
    pub dc_window: crate::handles::Hwnd,
    /// The DC window's own client size — the destination clip bounds.
    pub dc_w: u32,
    /// The DC window's own client size — the destination clip bounds.
    pub dc_h: u32,
}

/// Resolve a window to its top-level ancestor surface + relative offset.
///
/// Children composite into the ancestor's surface at the accumulated
/// parent-relative offset, so a child's `BitBlt(0,0,…)` lands where the child
/// sits inside the top-level window.
pub(crate) fn resolve_window_ancestor(
    state: &mut WinApiState,
    hwnd: u64,
) -> Option<ResolvedWindow> {
    let (top_hwnd, offset_x, offset_y) = ancestor_offset(&state.window_state().windows, hwnd)?;
    let (w, h) = crate::user32::window_client_size(state, top_hwnd.as_u64());
    let (dc_w, dc_h) = crate::user32::window_client_size(state, hwnd);
    Some(ResolvedWindow {
        hwnd: top_hwnd,
        width: u32::try_from(w.max(1)).unwrap_or(1),
        height: u32::try_from(h.max(1)).unwrap_or(1),
        offset_x,
        offset_y,
        dc_window: crate::handles::Hwnd::from(hwnd),
        dc_w: u32::try_from(dc_w.max(1)).unwrap_or(1),
        dc_h: u32::try_from(dc_h.max(1)).unwrap_or(1),
    })
}

/// Resolve a DC handle to the destination surface it paints into.
///
/// `DcKind::Window` surfaces live on the top-level ancestor (children
/// composite at their offset); memory and screen DCs have no writable
/// destination and resolve to `None`.
pub(crate) fn resolve_dest_info(state: &mut WinApiState, dc_handle: u64) -> Option<ResolvedWindow> {
    let dc = state
        .gdi_state()
        .find_dc(crate::handles::Hdc::from(dc_handle))?
        .clone();
    match dc.kind {
        DcKind::Window(hwnd) => resolve_window_ancestor(state, hwnd.as_u64()),
        DcKind::Memory | DcKind::Screen => None,
        // Print DCs have no window destination — P1a rasterizes them on the
        // page canvas only (see gdi32::print).
        DcKind::Print(_) => None,
    }
}

/// Destination fill/blit rects for a BitBlt-style operation, in the DC
/// window's local coordinates.
///
/// The requested rect is clipped to the DC window's client area, then — when
/// the window has `WS_CLIPCHILDREN` — each visible child's rect is subtracted
/// so a parent repaint cannot erase its children.
fn dest_rects(
    state: &mut WinApiState,
    info: &ResolvedWindow,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
) -> Vec<IRect> {
    let dest_w = i32::try_from(info.dc_w).unwrap_or(i32::MAX);
    let dest_h = i32::try_from(info.dc_h).unwrap_or(i32::MAX);
    let Some((dx, dy, _sx, _sy, cw, ch)) =
        clip_blit_rect(dest_w, dest_h, i32::MAX, i32::MAX, x, y, x, y, cx, cy)
    else {
        return Vec::new();
    };
    let mut rects = vec![IRect {
        left: dx,
        top: dy,
        right: dx.saturating_add(cw),
        bottom: dy.saturating_add(ch),
    }];
    let dc_style = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == info.dc_window)
        .map_or(0, |w| w.style);
    if dc_style & WS_CLIPCHILDREN != 0 {
        let children: Vec<IRect> = state
            .window_state()
            .windows
            .iter()
            .filter(|w| w.parent_handle == info.dc_window && w.visible)
            .map(|w| IRect::from_xywh(w.x, w.y, w.width, w.height))
            .collect();
        for child in children {
            rects = subtract_rect(rects, child);
        }
    }
    rects
}

/// A resolved 32-bpp DIB surface behind an HDC.
///
/// Shared by the GDI32 blit paths and MSIMG32: the DC record must have a
/// bitmap selected and that bitmap must be a 32-bpp `CreateDIBSection`
/// record; `None` otherwise. Width/height are always positive (saturated at
/// `i32::MAX`: the old `as` wrapped i32::MIN's magnitude negative).
pub(crate) struct ResolvedDib {
    /// Guest VA of the pixel buffer.
    pub bits_va: u64,
    /// Row stride in bytes.
    pub stride: i32,
    /// Pixel width (always positive).
    pub width: i32,
    /// Pixel height (always positive).
    pub height: i32,
    /// `true` = top-down (buffer row 0 is the top); `false` = bottom-up.
    pub top_down: bool,
}

/// Resolve an HDC to its selected 32-bpp DIB (shared blit/MSIMG32 resolver).
pub(crate) fn resolve_32bpp_dib(
    state: &mut crate::WinApiState,
    dc_handle: u64,
) -> Option<ResolvedDib> {
    let dib = {
        let gdi = state.gdi_state();
        let dc = gdi.find_dc(crate::handles::Hdc::from(dc_handle))?;
        gdi.find_dib(dc.selected_bitmap?)?.clone()
    };
    if dib.bit_count != 32 {
        return None;
    }
    // Saturate at i32::MAX: the old `as` wrapped i32::MIN's magnitude negative.
    Some(ResolvedDib {
        bits_va: dib.bits_va,
        stride: dib.stride,
        width: i32::try_from(dib.width.unsigned_abs()).unwrap_or(i32::MAX),
        height: i32::try_from(dib.height.unsigned_abs()).unwrap_or(i32::MAX),
        top_down: dib.height < 0,
    })
}

/// Resolve a source DC to (bits_va, stride, src_w, src_h, top_down).
fn resolve_src_info(
    state: &mut crate::WinApiState,
    dc_handle: u64,
) -> Option<(u64, i32, i32, i32, bool)> {
    let dib = resolve_32bpp_dib(state, dc_handle)?;
    Some((dib.bits_va, dib.stride, dib.width, dib.height, dib.top_down))
}

// Reusable scratch buffer for the one-shot `mem_read` of a blit span.
//
// Growth-only: the buffer is resized only when a span needs more bytes and
// never shrunk, so repeated BitBlt calls in a repaint cycle stop
// re-allocating after the first, largest blit. The guest paints on a single
// thread (WM_PAINT cycles run on the GUI thread), so a thread_local needs no
// state changes. `mem_read` and `mask_bgra_to_0rgb` are leaf operations that
// never re-enter blit, so the `RefCell` borrow inside the closure cannot
// alias.
std::thread_local! {
    static BLIT_SCRATCH: std::cell::RefCell<Vec<u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Run `f` with a scratch slice of exactly `span_len` bytes.
///
/// Only the span prefix of a reused (larger) buffer is exposed — handing
/// callers the whole buffer would make `mem_read` read guest bytes past the
/// blit span.
fn with_blit_scratch<R>(span_len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    BLIT_SCRATCH.with(|cell| {
        let mut guard = cell.borrow_mut();
        if guard.len() < span_len {
            guard.resize(span_len, 0);
        }
        // resize-if-needed guarantees len >= span_len; the fallback is for the
        // impossible case and degrades the blit to a no-op rather than
        // panicking.
        let Some(scratch) = guard.get_mut(..span_len) else {
            return f(&mut []);
        };
        f(scratch)
    })
}

/// Copy 32-bpp pixels from guest memory into a destination surface row buffer.
// Wide signature: one blit row needs source geometry + dest surface + clip.
#[allow(clippy::too_many_arguments)]
fn blit_row(
    engine: &mut dyn wie_cpu::CpuEngine,
    src_va: u64,
    src_stride: i32,
    src_x: i32,
    src_y: i32,
    src_h: i32,
    top_down: bool,
    dest: &mut [u32],
    dest_w: u32,
    dest_x: i32,
    dest_y: i32,
    width: i32,
    height: i32,
) {
    let row_bytes = usize::try_from(width).unwrap_or(0).saturating_mul(4);
    if row_bytes == 0 {
        return;
    }

    let ch = usize::try_from(height).unwrap_or(0);
    let stride = usize::try_from(src_stride).unwrap_or(0);
    if ch == 0 || row_bytes > stride {
        return; // malformed source; skip rather than over-read
    }

    let src_x_us = usize::try_from(src_x).unwrap_or(0);
    let src_y_us = usize::try_from(src_y).unwrap_or(0);
    let src_h_us = usize::try_from(src_h).unwrap_or(0);
    let dest_w_us = usize::try_from(dest_w).unwrap_or(0);
    let dest_x_us = usize::try_from(dest_x).unwrap_or(0);
    let dest_y_us = usize::try_from(dest_y).unwrap_or(0);
    let width_us = usize::try_from(width).unwrap_or(0);

    // First guest row covered by the span (guest row coordinates).
    let span_first = if top_down {
        src_y_us
    } else {
        src_h_us
            .saturating_sub(1)
            .saturating_sub(src_y_us)
            .saturating_sub(ch.saturating_sub(1))
    };
    let span_va = src_va
        .saturating_add(u64::try_from(span_first.saturating_mul(stride)).unwrap_or(0))
        .saturating_add(u64::try_from(src_x_us.saturating_mul(4)).unwrap_or(0));
    let span_len = ch.saturating_mul(stride);

    // ONE mem_read for the entire blit span — a single translation and
    // bounds check instead of one per row (the old per-row loop dominated
    // the blit cost for large windows). The span buffer is a thread_local
    // scratch (`with_blit_scratch`) that only grows, so repeated BitBlt calls
    // reuse one allocation instead of re-allocating ~span bytes every call.
    with_blit_scratch(span_len, |scratch| {
        if engine.mem_read(span_va, scratch).is_err() {
            return;
        }

        for row in 0..ch {
            // Row index within the span (reversed for bottom-up DIBs).
            let span_row = if top_down {
                row
            } else {
                ch.saturating_sub(1).saturating_sub(row)
            };
            // `span_va` already advanced by `src_x * 4`, so the row read is a
            // plain stride offset — re-adding `src_x` would double the column.
            let src_off = span_row.saturating_mul(stride);
            let Some(row_slice) = scratch.get(src_off..src_off.saturating_add(row_bytes)) else {
                return;
            };

            let dst_row = dest_y_us.saturating_add(row);
            let dst_offset = dst_row.saturating_mul(dest_w_us).saturating_add(dest_x_us);
            let dst_end = dst_offset.saturating_add(width_us).min(dest.len());
            let Some(dst_slice) = dest.get_mut(dst_offset..dst_end) else {
                return;
            };

            // BGRA → 0RGB: DIB pixel is 0xAARRGGBB in LE, mask alpha.
            // NEON-vectorized (4 px/op) so the conversion is fast even in debug.
            mask_bgra_to_0rgb(dst_slice, row_slice);
        }
    });
}

/// Fill a rectangular region of a surface with a constant color.
///
/// `publish == false` is used by control painting: the control's WM_PAINT
/// draws into the ancestor surface without publishing — the ancestor's own
/// WM_PAINT BitBlt publishes the composite frame.
///
/// The written rect (clipped to the surface) is unioned into the surface's
/// dirty accumulator, so the next publish reports exactly the repainted
/// region.
// Wide signature: one rect fill carries the surface dims + rect + color.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_rect_surface(
    state: &mut crate::WinApiState,
    hwnd: crate::handles::Hwnd,
    dest_w: u32,
    dest_h: u32,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    color: u32,
) {
    state.present().ensure_surface(hwnd, dest_w, dest_h);
    if let Some(surf) = state.present().surfaces.get_mut(&hwnd) {
        let dest_w_u = usize::try_from(surf.width).unwrap_or(0);
        let row_bytes = usize::try_from(cx.max(0)).unwrap_or(0);
        let dest_y_s = usize::try_from(y.max(0)).unwrap_or(0);
        let dest_x_s = usize::try_from(x.max(0)).unwrap_or(0);
        let h = usize::try_from(cy.max(0)).unwrap_or(0);
        let surf_height = usize::try_from(surf.height).unwrap_or(0);
        for row in dest_y_s..dest_y_s.saturating_add(h).min(surf_height) {
            let start = row.saturating_mul(dest_w_u).saturating_add(dest_x_s);
            let end = start.saturating_add(row_bytes).min(surf.pixels.len());
            if let Some(pixels) = surf.pixels.get_mut(start..end) {
                for px in pixels {
                    *px = color;
                }
            }
        }
        // The effective written rect — the loop above clips x to the row end
        // and y to the surface height, so mirror that clip here.
        let x1 = dest_x_s
            .saturating_add(row_bytes)
            .min(surf.pixels.len())
            .min(dest_w_u);
        let y1 = dest_y_s.saturating_add(h).min(surf_height);
        if x1 > dest_x_s && y1 > dest_y_s {
            let written = IRect {
                left: i32::try_from(dest_x_s).unwrap_or(0),
                top: i32::try_from(dest_y_s).unwrap_or(0),
                right: i32::try_from(x1).unwrap_or(0),
                bottom: i32::try_from(y1).unwrap_or(0),
            };
            state.present().mark_dirty(hwnd, written);
        }
    }
    // Always defer — the runtime drains pending
    // publishes once per repaint cycle at the empty-queue quiescence point,
    // so a WM_PAINT cycle's erase + BitBlt + control paints + captions emit a
    // single complete frame.
    state.present().publish_deferred(hwnd);
}

/// Handles `GDI32.dll!BitBlt` — real 32-bpp SRCCOPY blit to window surfaces.
pub fn handle_bit_blt(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc_dst = read_arg(engine, ArgReg::Rcx, "BitBlt")?;
    let x = low_i32(read_arg(engine, ArgReg::Rdx, "BitBlt")?, "BitBlt x")?;
    let y = low_i32(read_arg(engine, ArgReg::R8, "BitBlt")?, "BitBlt y")?;
    let cx = low_i32(read_arg(engine, ArgReg::R9, "BitBlt")?, "BitBlt cx")?;
    let rsp = engine.read_rsp().context("failed to read RSP for BitBlt")?;
    let cy = read_i32(engine, checked_address(rsp, 0x28, "BitBlt cy"))
        .context("failed to read BitBlt cy")?;
    let hdc_src = read_u64(engine, checked_address(rsp, 0x30, "BitBlt hdcSrc"))
        .context("failed to read BitBlt hdcSrc")?;
    let x1 = read_i32(engine, checked_address(rsp, 0x38, "BitBlt x1"))
        .context("failed to read BitBlt x1")?;
    let y1 = read_i32(engine, checked_address(rsp, 0x40, "BitBlt y1"))
        .context("failed to read BitBlt y1")?;
    let rop_raw = read_i32(engine, checked_address(rsp, 0x48, "BitBlt rop"))
        .context("failed to read BitBlt rop")?;
    // ROP is a DWORD; a negative value is an unsupported code either way.
    let rop = u32::try_from(rop_raw).unwrap_or(0);

    tracing::trace!(target: "wiegui", x, y, cx, cy, rop, "BitBlt");

    if rop != SRCCOPY && rop != BLACKNESS && rop != WHITENESS {
        tracing::debug!(rop, "BitBlt unsupported ROP, returning success");
        return ctx.finish(1);
    }

    // BLACKNESS / WHITENESS: solid fill, no source needed.
    if rop == BLACKNESS || rop == WHITENESS {
        let color = if rop == BLACKNESS { 0 } else { 0x00FF_FFFF };
        if let Some(info) = resolve_dest_info(state, hdc_dst) {
            for rect in dest_rects(state, &info, x, y, cx, cy) {
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    rect.left.saturating_add(info.offset_x),
                    rect.top.saturating_add(info.offset_y),
                    rect.width(),
                    rect.height(),
                    color,
                );
            }
        }
        return ctx.finish(1);
    }

    // SRCCOPY path: read from source DIB, write to destination surface.
    let Some((src_va, src_stride, src_w, src_h, top_down)) = resolve_src_info(state, hdc_src)
    else {
        let dc_bitmap = {
            let gdi = state.gdi_state();
            gdi.find_dc(crate::handles::Hdc::from(hdc_src))
                .map(|dc| dc.selected_bitmap.map(|b| b.as_u64()))
        };
        tracing::debug!(
            target: "wie_gdi",
            hdc_src = format_args!("0x{hdc_src:#x}"),
            dc_found = dc_bitmap.is_some(),
            selected_bitmap = ?dc_bitmap.flatten(),
            "BitBlt: invalid source"
        );
        return ctx.finish(1);
    };
    let Some(info) = resolve_dest_info(state, hdc_dst) else {
        return ctx.finish(1);
    };

    // Clip. Window sizes beyond i32::MAX (unreachable for real screens)
    // saturate positive instead of wrapping negative like the old `as`.
    let dest_w_i = i32::try_from(info.dc_w).unwrap_or(i32::MAX);
    let dest_h_i = i32::try_from(info.dc_h).unwrap_or(i32::MAX);
    let Some((dx, dy, sx, sy, cw, ch)) =
        clip_blit_rect(dest_w_i, dest_h_i, src_w, src_h, x, y, x1, y1, cx, cy)
    else {
        return ctx.finish(1);
    };

    // WS_CLIPCHILDREN: decompose the dest rect around visible children so a
    // parent repaint cannot erase its controls.
    let mut rects = vec![IRect {
        left: dx,
        top: dy,
        right: dx.saturating_add(cw),
        bottom: dy.saturating_add(ch),
    }];
    let dc_style = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == info.dc_window)
        .map_or(0, |w| w.style);
    if dc_style & WS_CLIPCHILDREN != 0 {
        let children: Vec<IRect> = state
            .window_state()
            .windows
            .iter()
            .filter(|w| w.parent_handle == info.dc_window && w.visible)
            .map(|w| IRect::from_xywh(w.x, w.y, w.width, w.height))
            .collect();
        for child in children {
            rects = subtract_rect(rects, child);
        }
    }

    // Ensure destination surface.
    state
        .present()
        .ensure_surface(info.hwnd, info.width, info.height);

    // B9(c): time the mask copy (copy ①) — only when frame timing is enabled.
    let blit_t0 = crate::present::frame_timing_enabled().then(std::time::Instant::now);

    // Sampled source-pixel probe: is the guest framebuffer carrying real
    // content, or is every frame blank? Every 512th blit logs three pixels
    // (top-left, centre, bottom-right of the clipped rect).
    {
        use std::cell::Cell;
        thread_local!(static BLIT_N: Cell<u64> = const { Cell::new(0) });
        let n = BLIT_N.with(|c| c.replace(c.get().wrapping_add(1)));
        if n.is_multiple_of(512) {
            let mut px = |px_x: i32, px_y: i32| -> u32 {
                let va = src_va
                    .wrapping_add(i64::from(px_y).wrapping_mul(i64::from(src_stride)) as u64)
                    .wrapping_add(i64::from(px_x).wrapping_mul(4) as u64);
                let mut b = [0_u8; 4];
                if engine.mem_read(va, &mut b).is_ok() {
                    u32::from_le_bytes(b)
                } else {
                    0xDEAD_BEEF
                }
            };
            let mid_x = sx + cw / 2;
            let mid_y = sy + ch / 2;
            tracing::debug!(
                target: "wie_gdi",
                n,
                tl = format_args!("0x{:08x}", px(sx, sy)),
                mid = format_args!("0x{:08x}", px(mid_x, mid_y)),
                br = format_args!("0x{:08x}", px(sx + cw - 1, sy + ch - 1)),
                "BitBlt source pixels"
            );
        }
    }

    // Blit rows: scope the dest borrow so we can re-borrow state for publish.
    {
        let dest = state
            .present()
            .surfaces
            .get_mut(&info.hwnd)
            .map(|s| &mut s.pixels[..]);
        let Some(dest) = dest else {
            return ctx.finish(1);
        };
        for rect in &rects {
            let rect_w = rect.width();
            let rect_h = rect.height();
            if rect_w <= 0 || rect_h <= 0 {
                continue;
            }

            // Each piece keeps its alignment to the source DIB.
            let piece_src_x = sx.saturating_add(rect.left.saturating_sub(dx));
            let piece_src_y = sy.saturating_add(rect.top.saturating_sub(dy));
            let surface_x = rect.left.saturating_add(info.offset_x);
            let surface_y = rect.top.saturating_add(info.offset_y);
            blit_row(
                engine,
                src_va,
                src_stride,
                piece_src_x,
                piece_src_y,
                src_h,
                top_down,
                dest,
                info.width,
                surface_x,
                surface_y,
                rect_w,
                rect_h,
            );
        }
    }
    // The blit wrote the union of the (WS_CLIPCHILDREN-clipped) dest pieces
    // into the ancestor surface — report it as the repainted region so the
    // next publish uploads only it. Over-marking a row `blit_row` skipped is
    // safe; under-marking is what would corrupt the GPU staging.
    {
        let dirty = rects.iter().fold(None::<IRect>, |acc, r| {
            if r.width() <= 0 || r.height() <= 0 {
                return acc;
            }
            let piece = IRect {
                left: r.left.saturating_add(info.offset_x),
                top: r.top.saturating_add(info.offset_y),
                right: r.right.saturating_add(info.offset_x),
                bottom: r.bottom.saturating_add(info.offset_y),
            };
            Some(match acc {
                None => piece,
                Some(union) => union_rect(union, piece),
            })
        });
        if let Some(dirty) = dirty {
            state.present().mark_dirty(info.hwnd, dirty);
        }
    }

    if let Some(t0) = blit_t0 {
        state.present().record_blit_copy(t0.elapsed().as_nanos());
    }

    // B3.6: defer the publish to the repaint-cycle boundary (one frame per
    // full cycle). The surface buffer stays put and the dirty accumulator
    // keeps unioning until the drain, so the published frame is the
    // fully-painted composite with the union region.
    state.present().publish_deferred(info.hwnd);

    ctx.finish(1)
}

/// Handles `GDI32.dll!StretchBlt` (stub).
pub fn handle_stretch_blt(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let _hdc_dst = read_arg(engine, ArgReg::Rcx, "StretchBlt")?;
    let _x = read_arg(engine, ArgReg::Rdx, "StretchBlt")?;
    let _y = read_arg(engine, ArgReg::R8, "StretchBlt")?;
    let _cx = read_arg(engine, ArgReg::R9, "StretchBlt")?;
    ctx.finish(1)
}

/// Handles `GDI32.dll!PatBlt` — fills the rectangle with the DC's current
/// brush color.
pub fn handle_pat_blt(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = read_arg(engine, ArgReg::Rcx, "PatBlt")?;
    let x = low_i32(read_arg(engine, ArgReg::Rdx, "PatBlt")?, "PatBlt x")?;
    let y = low_i32(read_arg(engine, ArgReg::R8, "PatBlt")?, "PatBlt y")?;
    let cx = low_i32(read_arg(engine, ArgReg::R9, "PatBlt")?, "PatBlt cx")?;
    let rsp = engine.read_rsp().context("failed to read RSP for PatBlt")?;
    let cy = read_i32(engine, checked_address(rsp, 0x28, "PatBlt cy"))
        .context("failed to read PatBlt cy")?;
    let _rop = read_i32(engine, checked_address(rsp, 0x30, "PatBlt rop"))
        .context("failed to read PatBlt rop")?;

    // A DC's default brush is WHITE_BRUSH, mirroring real GDI.
    let selected_brush = state
        .gdi_state()
        .find_dc(crate::handles::Hdc::from(hdc))
        .and_then(|dc| dc.selected_brush)
        .unwrap_or(crate::handles::Hbrush::from(
            crate::gdi32::state::STOCK_WHITE_BRUSH_HANDLE,
        ));

    if let Some(color) = brush_color(state, selected_brush)
        && let Some(info) = resolve_dest_info(state, hdc)
    {
        for rect in dest_rects(state, &info, x, y, cx, cy) {
            fill_rect_surface(
                state,
                info.hwnd,
                info.width,
                info.height,
                rect.left.saturating_add(info.offset_x),
                rect.top.saturating_add(info.offset_y),
                rect.width(),
                rect.height(),
                color,
            );
        }
    }

    // NULL_BRUSH / unknown DC: no pixels change, but PatBlt still succeeds.
    ctx.finish(1)
}

/// Handles `GDI32.dll!FillRect` — fills the RECT with the given brush.
pub fn handle_fill_rect(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let state = &mut *ctx.state;
    let hdc = read_arg(engine, ArgReg::Rcx, "FillRect")?;
    let rect_va = read_arg(engine, ArgReg::Rdx, "FillRect")?;
    let brush = read_arg(engine, ArgReg::R8, "FillRect")?;

    let mut filled = false;
    if rect_va != 0 {
        let (left, top, right, bottom) = with_typed_read::<Rect, _, _>(engine, rect_va, |rect| {
            Ok((rect.left, rect.top, rect.right, rect.bottom))
        })
        .context("failed to read RECT")?;

        if let Some(color) = brush_color(state, crate::handles::Hbrush::from(brush))
            && let Some(info) = resolve_dest_info(state, hdc)
        {
            for rect in dest_rects(
                state,
                &info,
                left,
                top,
                right.saturating_sub(left),
                bottom.saturating_sub(top),
            ) {
                fill_rect_surface(
                    state,
                    info.hwnd,
                    info.width,
                    info.height,
                    rect.left.saturating_add(info.offset_x),
                    rect.top.saturating_add(info.offset_y),
                    rect.width(),
                    rect.height(),
                    color,
                );
            }
            filled = true;
        }
    }

    let return_value = u64::from(filled);
    ctx.finish(return_value)
}

#[cfg(test)]
mod tests {
    use super::{BLIT_SCRATCH, IRect, ancestor_offset, subtract_rect, with_blit_scratch};
    use crate::WindowRecord;
    use crate::handles::Hwnd;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> IRect {
        IRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn ancestor_offset_top_level_is_identity() {
        let windows = vec![WindowRecord {
            handle: Hwnd::from(1),
            ..Default::default()
        }];
        assert_eq!(ancestor_offset(&windows, 1), Some((Hwnd::from(1), 0, 0)));
    }

    #[test]
    fn ancestor_offset_sums_child_positions() {
        let windows = vec![
            WindowRecord {
                handle: Hwnd::from(1),
                ..Default::default()
            },
            WindowRecord {
                handle: Hwnd::from(2),
                parent_handle: Hwnd::from(1),
                x: 20,
                y: 30,
                ..Default::default()
            },
            WindowRecord {
                handle: Hwnd::from(3),
                parent_handle: Hwnd::from(2),
                x: 5,
                y: 7,
                ..Default::default()
            },
        ];
        assert_eq!(ancestor_offset(&windows, 3), Some((Hwnd::from(1), 25, 37)));
    }

    #[test]
    fn ancestor_offset_broken_parent_chain_falls_back() {
        let windows = vec![WindowRecord {
            handle: Hwnd::from(2),
            parent_handle: Hwnd::from(99),
            x: 20,
            y: 30,
            ..Default::default()
        }];
        assert_eq!(ancestor_offset(&windows, 2), Some((Hwnd::from(2), 0, 0)));
    }

    #[test]
    fn ancestor_offset_unknown_hwnd_is_none() {
        let windows = vec![WindowRecord {
            handle: Hwnd::from(1),
            ..Default::default()
        }];
        assert_eq!(ancestor_offset(&windows, 42), None);
    }

    #[test]
    fn subtract_rect_keeps_dest_when_child_outside() {
        let result = subtract_rect(vec![rect(0, 0, 100, 100)], rect(200, 200, 300, 300));
        assert_eq!(result, vec![rect(0, 0, 100, 100)]);
    }

    #[test]
    fn subtract_rect_centered_child_splits_into_four() {
        let result = subtract_rect(vec![rect(0, 0, 100, 100)], rect(20, 20, 80, 80));
        let total: i64 = result
            .iter()
            .map(|r| i64::from(r.width()).saturating_mul(i64::from(r.height())))
            .sum();
        assert_eq!(total, 6400); // 100×100 − 60×60
    }

    #[test]
    fn subtract_rect_covering_child_empties() {
        let result = subtract_rect(vec![rect(0, 0, 100, 100)], rect(0, 0, 100, 100));
        assert!(result.is_empty());
    }

    #[test]
    fn subtract_rect_multiple_children_keeps_exact_area() {
        let first = subtract_rect(vec![rect(0, 0, 320, 240)], rect(20, 20, 120, 50));
        let second = subtract_rect(first, rect(20, 60, 220, 90));
        let total: i64 = second
            .iter()
            .map(|r| i64::from(r.width()).saturating_mul(i64::from(r.height())))
            .sum();
        // 320×240 minus button (100×30) minus static (200×30).
        assert_eq!(total, 67_800);
    }

    #[test]
    fn blit_scratch_reuses_growth_only_buffer() {
        // Reset the thread-local so the test observes exactly its own history.
        BLIT_SCRATCH.with(|cell| cell.borrow_mut().clear());

        let mut first_len = 0;
        with_blit_scratch(1024, |s| first_len = s.len());
        let mut second_len = 0;
        with_blit_scratch(64, |s| second_len = s.len());
        let retained = BLIT_SCRATCH.with(|cell| cell.borrow().len());

        // Each call sees exactly its span — a smaller blit must not expose the
        // larger retained buffer to mem_read (that would read guest bytes past
        // the blit span) — and the buffer keeps its peak size instead of
        // re-allocating per call.
        assert_eq!(first_len, 1024);
        assert_eq!(second_len, 64);
        assert_eq!(retained, 1024);
    }
}
