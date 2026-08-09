//! GDI32 region-object handlers — `CreateRectRgn` / `CreateEllipticRgn` /
//! `CreatePolygonRgn` / `CombineRgn` / `SetRectRgn` / `GetRgnBox`.
//!
//! Regions live in a process-global handle table (`0x5400_0000` base —
//! disjoint from the per-session GDI object bases 0x6810–0x6850 and from the
//! fake-VA stop range), mirroring the strtok `SAVE` static in
//! ucrt/string.rs. A region is a KISS axis-aligned rect set; the combine ops
//! do rect-list union / intersection / subtraction.
//!
//! Leak note: the dense `Gdi32DeleteObject` classifier only knows the 0x68xx
//! object bases, so `DeleteObject` on a region handle returns TRUE without
//! freeing the table entry (it lives in gdi32/state/mod.rs, which is outside
//! this milestone's editable files). Regions are a few rects each — the leak
//! is bounded by the session's region churn and is documented, not fixed.

use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::gdi32::blit::{IRect, intersect_rect, subtract_rect};
use crate::guest_layout::Rect;
use crate::guest_memory::{checked_address, read_i32, with_typed_write};
use crate::user32::low_i32;
use crate::{HandlerContext, WinApiHandlerResult};

/// Region handles start here — disjoint from every other GDI handle range.
const REGION_HANDLE_BASE: u64 = 0x5400_0000;

/// `CombineRgn` region-kind return codes (wingdi.h).
const NULLREGION: i32 = 1;
const SIMPLEREGION: i32 = 2;
const COMPLEXREGION: i32 = 3;
/// `ERROR` is the failure code for both `CombineRgn` and `GetRgnBox`.
const ERROR: u64 = 0;

/// `CombineRgn` `fnMode` codes (wingdi.h).
const RGN_AND: u32 = 1;
const RGN_OR: u32 = 2;
const RGN_XOR: u32 = 3;
const RGN_DIFF: u32 = 4;
const RGN_COPY: u32 = 5;

/// Cap on `CreatePolygonRgn` points / scan-line counts so a hostile guest
/// cannot drive a huge host loop (real regions are small).
const MAX_POLYGON_POINTS: u32 = 4096;

/// One region: a set of axis-aligned rects (exclusive right/bottom edges).
#[derive(Debug, Clone, Default)]
struct RegionData {
    rects: Vec<IRect>,
}

/// The process-global region table: bump handle counter + (handle, region)
/// pairs. Linear scan is fine — real programs hold a handful of regions.
struct RegionTable {
    next_handle: u64,
    regions: Vec<(u64, RegionData)>,
}

static REGIONS: Mutex<RegionTable> = Mutex::new(RegionTable {
    next_handle: REGION_HANDLE_BASE,
    regions: Vec::new(),
});

/// Lock the region table, surviving poisoning (the poison is a panic in
/// another thread, never a corrupt table).
fn lock_regions() -> std::sync::MutexGuard<'static, RegionTable> {
    REGIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The region-kind code for a rect list (what `CombineRgn` returns).
#[must_use]
fn classify(rects: &[IRect]) -> i32 {
    match rects.len() {
        0 => NULLREGION,
        1 => SIMPLEREGION,
        _ => COMPLEXREGION,
    }
}

/// The bounding box of a rect list — `(left, top, right, bottom)`, or `None`
/// for an empty region.
#[must_use]
fn bounding_box(rects: &[IRect]) -> Option<(i32, i32, i32, i32)> {
    let mut left = i32::MAX;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
    for rect in rects {
        left = left.min(rect.left);
        top = top.min(rect.top);
        right = right.max(rect.right);
        bottom = bottom.max(rect.bottom);
    }
    (left < right && top < bottom).then_some((left, top, right, bottom))
}

/// Rect-list AND: the pairwise intersections of A and B (the standard region
/// intersection formula; overlapping pieces may remain, which is fine for the
/// bounding box `GetRgnBox` reports).
#[must_use]
fn and_lists(a: &[IRect], b: &[IRect]) -> Vec<IRect> {
    let mut out = Vec::new();
    for rect_a in a {
        for rect_b in b {
            let hit = intersect_rect(*rect_a, *rect_b);
            if hit.width() > 0 && hit.height() > 0 {
                out.push(hit);
            }
        }
    }
    out
}

/// Rect-list OR: A followed by B (KISS — overlapping pieces are kept; the
/// bounding box stays exact, which is all `GetRgnBox` needs).
#[must_use]
fn or_lists(a: &[IRect], b: &[IRect]) -> Vec<IRect> {
    let mut out = Vec::with_capacity(a.len().saturating_add(b.len()));
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    out
}

/// Rect-list DIFF: A with each B rect subtracted (exact decomposition).
#[must_use]
fn diff_lists(a: &[IRect], b: &[IRect]) -> Vec<IRect> {
    let mut out = a.to_vec();
    for rect_b in b {
        out = subtract_rect(out, *rect_b);
    }
    out
}

/// A single rect (or an empty region when it is degenerate) — the shared body
/// of `CreateRectRgn` / `CreateEllipticRgn` / `SetRectRgn`.
fn single_rect_region(left: i32, top: i32, right: i32, bottom: i32) -> RegionData {
    let rects = if left < right && top < bottom {
        vec![IRect {
            left,
            top,
            right,
            bottom,
        }]
    } else {
        Vec::new()
    };
    RegionData { rects }
}

/// Allocate a handle and insert `region`, returning the handle (0 on
/// counter overflow).
fn insert_region(table: &mut RegionTable, region: RegionData) -> u64 {
    let handle = table.next_handle;
    table.next_handle = table.next_handle.wrapping_add(1);
    if handle == 0 {
        return 0;
    }
    table.regions.push((handle, region));
    handle
}

/// Handles `GDI32.dll!CreateRectRgn` — a region holding one rect.
pub fn handle_create_rect_rgn(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let left = low_i32(
        engine
            .read_rcx()
            .context("failed to read RCX for CreateRectRgn")?,
        "CreateRectRgn left",
    )?;
    let top = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for CreateRectRgn")?,
        "CreateRectRgn top",
    )?;
    let right = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for CreateRectRgn")?,
        "CreateRectRgn right",
    )?;
    let bottom = low_i32(
        engine
            .read_r9()
            .context("failed to read R9 for CreateRectRgn")?,
        "CreateRectRgn bottom",
    )?;

    let handle = {
        let mut table = lock_regions();
        insert_region(&mut table, single_rect_region(left, top, right, bottom))
    };
    ctx.finish(handle)
}

/// Handles `GDI32.dll!CreateEllipticRgn` — KISS: the bounding rect.
///
/// The ellipse itself is not rasterized (regions only feed `GetRgnBox` /
/// `CombineRgn` for this milestone), so the axis-aligned bounding box is a
/// documented approximation of the true ellipse.
pub fn handle_create_elliptic_rgn(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let left = low_i32(
        engine
            .read_rcx()
            .context("failed to read RCX for CreateEllipticRgn")?,
        "CreateEllipticRgn left",
    )?;
    let top = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for CreateEllipticRgn")?,
        "CreateEllipticRgn top",
    )?;
    let right = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for CreateEllipticRgn")?,
        "CreateEllipticRgn right",
    )?;
    let bottom = low_i32(
        engine
            .read_r9()
            .context("failed to read R9 for CreateEllipticRgn")?,
        "CreateEllipticRgn bottom",
    )?;

    let handle = {
        let mut table = lock_regions();
        insert_region(&mut table, single_rect_region(left, top, right, bottom))
    };
    ctx.finish(handle)
}

/// Handles `GDI32.dll!CreatePolygonRgn` — KISS: the points' bounding box.
///
/// `HRGN CreatePolygonRgn(const POINT *ppt, int cPoints, int fnPolyFillMode)`
/// — reads `cPoints` `POINT`s (8 bytes each: `LONG x` @0, `LONG y` @4) and
/// builds the single rect that encloses them (a documented approximation of
/// the true polygon).
pub fn handle_create_polygon_rgn(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let ppt = engine
        .read_rcx()
        .context("failed to read RCX for CreatePolygonRgn")?;
    let c_points = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for CreatePolygonRgn")?,
        "CreatePolygonRgn cPoints",
    )?;
    let _fill_mode = engine
        .read_r8()
        .context("failed to read R8 for CreatePolygonRgn")?;

    if ppt == 0 || c_points <= 0 {
        return ctx.finish(0);
    }
    let n = u32::try_from(c_points).unwrap_or(0).min(MAX_POLYGON_POINTS);

    let mut left = i32::MAX;
    let mut top = i32::MAX;
    let mut right = i32::MIN;
    let mut bottom = i32::MIN;
    for i in 0..n {
        let point_va = checked_address(
            ppt,
            u64::from(i).saturating_mul(8),
            "CreatePolygonRgn POINT",
        );
        let x = read_i32(engine, checked_address(point_va, 0, "POINT.x"))
            .context("failed to read CreatePolygonRgn POINT.x")?;
        let y = read_i32(engine, checked_address(point_va, 4, "POINT.y"))
            .context("failed to read CreatePolygonRgn POINT.y")?;
        left = left.min(x);
        top = top.min(y);
        right = right.max(x);
        bottom = bottom.max(y);
    }

    let handle = {
        let mut table = lock_regions();
        insert_region(&mut table, single_rect_region(left, top, right, bottom))
    };
    ctx.finish(handle)
}

/// Handles `GDI32.dll!CombineRgn` — compute a region-mode result into the
/// destination region.
///
/// `int CombineRgn(HRGN hrgnDst, HRGN hrgnSrc1, HRGN hrgnSrc2, int fnMode)`.
/// KISS rect-list semantics: `RGN_AND` intersects the lists, `RGN_OR` unions
/// (concatenates), `RGN_DIFF` subtracts src2 from src1, `RGN_XOR` is the
/// union of the two one-way differences, `RGN_COPY` copies src1. Returns
/// NULLREGION / SIMPLEREGION / COMPLEXREGION, or ERROR (0) for an unknown
/// mode or an unknown destination.
pub fn handle_combine_rgn(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hrgn_dst = engine
        .read_rcx()
        .context("failed to read RCX for CombineRgn")?;
    let hrgn_src1 = engine
        .read_rdx()
        .context("failed to read RDX for CombineRgn")?;
    let hrgn_src2 = engine
        .read_r8()
        .context("failed to read R8 for CombineRgn")?;
    let fn_mode_raw = engine
        .read_r9()
        .context("failed to read R9 for CombineRgn")?;
    let fn_mode = u32::try_from(fn_mode_raw & u64::from(u32::MAX)).unwrap_or(0);

    let mut table = lock_regions();
    // Clone both source rect lists so the destination can borrow the table
    // mutably afterwards.
    let Some(src1) = table
        .regions
        .iter()
        .find(|(handle, _)| *handle == hrgn_src1)
        .map(|(_, region)| region.rects.clone())
    else {
        tracing::debug!(hrgn_src1, "CombineRgn: unknown source region");
        return ctx.finish(ERROR);
    };
    let src2 = table
        .regions
        .iter()
        .find(|(handle, _)| *handle == hrgn_src2)
        .map(|(_, region)| region.rects.clone())
        .unwrap_or_default();

    let result = match fn_mode {
        RGN_AND => and_lists(&src1, &src2),
        RGN_OR => or_lists(&src1, &src2),
        RGN_XOR => {
            let mut out = diff_lists(&src1, &src2);
            out.append(&mut diff_lists(&src2, &src1));
            out
        }
        RGN_DIFF => diff_lists(&src1, &src2),
        RGN_COPY => src1,
        _ => {
            tracing::debug!(fn_mode, "CombineRgn: unknown mode");
            return ctx.finish(ERROR);
        }
    };
    let kind = classify(&result);
    let Some(dst) = table
        .regions
        .iter_mut()
        .find(|(handle, _)| *handle == hrgn_dst)
        .map(|(_, region)| region)
    else {
        tracing::debug!(hrgn_dst, "CombineRgn: unknown destination region");
        return ctx.finish(ERROR);
    };
    dst.rects = result;
    ctx.finish(u64::try_from(kind).unwrap_or(0))
}

/// Handles `GDI32.dll!SetRectRgn` — replace a region's contents with one
/// rect.
///
/// `BOOL SetRectRgn(HRGN hrgn, int left, int top, int right, int bottom)` —
/// 5 args, `bottom` at `[rsp+0x28]`. Returns TRUE (1), or FALSE (0) for an
/// unknown region.
pub fn handle_set_rect_rgn(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hrgn = engine
        .read_rcx()
        .context("failed to read RCX for SetRectRgn")?;
    let left = low_i32(
        engine
            .read_rdx()
            .context("failed to read RDX for SetRectRgn")?,
        "SetRectRgn left",
    )?;
    let top = low_i32(
        engine
            .read_r8()
            .context("failed to read R8 for SetRectRgn")?,
        "SetRectRgn top",
    )?;
    let right = low_i32(
        engine
            .read_r9()
            .context("failed to read R9 for SetRectRgn")?,
        "SetRectRgn right",
    )?;
    let rsp = engine
        .read_rsp()
        .context("failed to read RSP for SetRectRgn")?;
    let bottom = read_i32(engine, checked_address(rsp, 0x28, "SetRectRgn bottom"))
        .context("failed to read SetRectRgn bottom")?;

    let mut table = lock_regions();
    match table.regions.iter_mut().find(|(handle, _)| *handle == hrgn) {
        Some((_, region)) => {
            region.rects = single_rect_region(left, top, right, bottom).rects;
            ctx.finish(1)
        }
        None => ctx.finish(0),
    }
}

/// Handles `GDI32.dll!GetRgnBox` — the region's bounding rect.
///
/// `int GetRgnBox(HRGN hrgn, LPRECT lprc)` — writes the 16-byte `RECT`
/// (`left` @0, `top` @4, `right` @8, `bottom` @12) and returns 1 on success,
/// 0 for an unknown or empty region. (Real GDI distinguishes the region's
/// complexity class here; KISS collapses it to 1 as the milestone specifies.)
pub fn handle_get_rgn_box(ctx: &mut HandlerContext<'_>) -> Result<WinApiHandlerResult> {
    let engine = &mut *ctx.engine;
    let hrgn = engine
        .read_rcx()
        .context("failed to read RCX for GetRgnBox")?;
    let lprc = engine
        .read_rdx()
        .context("failed to read RDX for GetRgnBox")?;
    if lprc == 0 {
        return ctx.finish(ERROR);
    }

    let boxed = {
        let table = lock_regions();
        table
            .regions
            .iter()
            .find(|(handle, _)| *handle == hrgn)
            .and_then(|(_, region)| bounding_box(&region.rects))
    };
    let Some((left, top, right, bottom)) = boxed else {
        return ctx.finish(ERROR);
    };

    with_typed_write::<Rect, _, _>(engine, lprc, |rect| {
        rect.left = left;
        rect.top = top;
        rect.right = right;
        rect.bottom = bottom;
        Ok(())
    })
    .context("failed to write GetRgnBox RECT")?;
    ctx.finish(1)
}
