//! Per-stage accounting for `BitBlt`.
//!
//! `gdi32!BitBlt` measured 1 268 calls and 504 ms in a DOOM Retro run — 397 µs
//! per call, which is implausible for a screen-sized `SRCCOPY` and has several
//! candidates: the source-DC lookup scanning handles, the destination surface
//! being reallocated, the row copy itself, or the deferred publish doing work
//! per call. These counters split the call so the fix targets the stage that
//! actually owns the time rather than the one that looks expensive.
//!
//! # The copy-avoidance fix, written before it is needed
//!
//! Working hypothesis: 397 µs is what copying one 1726×1146×4 frame (~7.9 MB)
//! costs at memcpy bandwidth. If the attribution puts the time in [`ROWS`], the
//! copy is not slow — it is *redundant*, and the fix is to stop doing it, not to
//! make it faster. That is a different diff from "optimise the loop", so the
//! shape is recorded here while the numbers are still unknown.
//!
//! **Where the copy could go away.** The guest's pixels already live in its own
//! DIB (guest memory at `src_va`, `src_stride` bytes per row). Today
//! `blit_row` reads each row out of guest memory into `surface.pixels`, and the
//! presenter then reads `surface.pixels`. For an `SRCCOPY` whose source is the
//! whole DIB and whose destination is an unclipped surface of the same geometry,
//! the destination surface could instead *refer* to the DIB — the same buffer
//! the presenter reads, with no intermediate — which is the shape the SDL2
//! palette path already uses.
//!
//! **What breaks if source and destination alias**, in the order it will bite:
//!
//! - **Overlapping rects.** `BitBlt` with a source and destination in the same
//!   DIB and overlapping rectangles has defined semantics (memmove, in raster
//!   order), which the current row-by-row copy gets right by accident of
//!   direction and a borrowed destination. Aliasing makes the copy a no-op,
//!   which is correct only when the rectangles are identical. Anything else
//!   must keep the copying path.
//! - **Lifetime.** `surface.pixels` is owned by the surface and handed to the
//!   presenter under a lock; a borrowed DIB means the guest can overwrite the
//!   buffer while a frame is being published. The SDL2 path solved this by
//!   writing into the buffer the presenter reads and keeping one writer per
//!   frame — the same invariant would have to hold here, and a guest that
//!   blits from a DIB it is still drawing into would break it.
//! - **Stride and orientation.** The surface stride is 64-padded and the DIB is
//!   not; `top_down` inverts the row order. A borrowed buffer must carry the
//!   DIB's own stride and orientation, so every consumer of `surface.stride`
//!   (presenter upload, dirty-region maths, `fill_rect_surface`) must learn to
//!   accept a stride that is not the surface's own.
//! - **Dirty regions and publish deferral.** The present is deferred and
//!   uploads only the union of dirty rects. A borrowed buffer still has to mark
//!   the same region, or the GPU staging keeps a stale copy — this is the one
//!   piece that must not change at all.
//! - **Clipping.** `WS_CLIPCHILDREN` decomposes the destination into pieces and
//!   the source offsets shift per piece. Only the single-piece, full-rect case
//!   can alias; a decomposed blit keeps copying.
//!
//! # What was tried and did not work
//!
//! The 233 µs hand-back copy in [`ENSURE_SURFACE`] was the first target, because
//! a full-surface blit provably writes every pixel, so preserving the previous
//! frame is waste. `ensure_surface_for_full_repaint` skips exactly that copy and
//! was wired behind [`covers_whole_surface`], with the source-stride guard
//! described below.
//!
//! It is **not a win here.** When the channel hands back no spare buffer —
//! `present_enqueued=0`, i.e. no presenter recycling frames — that path falls
//! into `vec![0u32; needed]`, so the 7.9 MB copy becomes 7.9 MB of first-touch
//! page faults on the row writes instead of one memcpy into warm pages.
//! Measured over 765 calls: `ensure_surface` 233 → 75 µs but `rows` 365 → 813 µs,
//! so the two together went 598 → 888 µs. Reverted; the call site keeps
//! `ensure_surface`. The predicate and its test remain because the rule is the
//! part worth keeping, and the fix becomes worthwhile only if the presenter
//! starts recycling buffers.
//!
//! ## The copy-avoidance slice
//!
//! So the next real target is [`ROWS`], and the safe slice is worth stating
//! precisely: **alias only when
//! the source is a DIB, the ROP is `SRCCOPY`, the destination has no children to
//! clip, the source and destination rectangles are equal, and the caller is the
//! only writer of that DIB for this frame.** Everything else keeps the copy and
//! the row loop stays as it is.

use crate::stage_timer::{Stage, field as stage_field, timed};
use std::sync::Mutex;

/// The last "copied nothing" reason, so the report names it instead of the
/// reader inferring it from a zero row count.
#[derive(Debug)]
struct LastNote(Mutex<Option<String>>);

impl LastNote {
    const fn new() -> Self {
        Self(Mutex::new(None))
    }

    fn set(&self, note: String) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(note);
        }
    }

    fn get(&self) -> Option<String> {
        self.0.lock().ok().and_then(|slot| slot.clone())
    }
}

/// `resolve_src_info`: finding the source DIB (surface or DC's selected bitmap).
static RESOLVE_SRC: Stage = Stage::new();
/// `resolve_dest_info` plus the window-style/children scan and clipping.
static RESOLVE_DST: Stage = Stage::new();
/// `ensure_surface`: creating or resizing the destination surface.
static ENSURE_SURFACE: Stage = Stage::new();
/// The per-row copy (`blit_row`) across all pieces of the clipped rect.
static ROWS: Stage = Stage::new();
/// `mark_dirty` + `publish_deferred` for the repainted region.
static PUBLISH: Stage = Stage::new();
/// Calls that returned without (or before) copying pixels, and why.
static NOTHING: Stage = Stage::new();
static NOTE: LastNote = LastNote::new();

pub(crate) fn timed_resolve_src<T>(f: impl FnOnce() -> T) -> T {
    timed(&RESOLVE_SRC, f)
}

pub(crate) fn timed_resolve_dst<T>(f: impl FnOnce() -> T) -> T {
    timed(&RESOLVE_DST, f)
}

pub(crate) fn timed_ensure_surface<T>(f: impl FnOnce() -> T) -> T {
    timed(&ENSURE_SURFACE, f)
}

pub(crate) fn timed_rows<T>(f: impl FnOnce() -> T) -> T {
    timed(&ROWS, f)
}

pub(crate) fn timed_publish<T>(f: impl FnOnce() -> T) -> T {
    timed(&PUBLISH, f)
}

/// Record a call that copied nothing, with the reason.
pub(crate) fn note_nothing(reason: impl Into<String>) {
    timed(&NOTHING, || ());
    NOTE.set(reason.into());
}

/// Report lines for the `WIE_RUNTIME_PROFILE` block; empty until something ran.
#[must_use]
pub(crate) fn report_lines() -> Vec<String> {
    // Any one stage having run is enough to print: a call that resolved a DC
    // and then copied nothing is exactly the shape worth seeing.
    let total: u64 = [
        &RESOLVE_SRC,
        &RESOLVE_DST,
        &ENSURE_SURFACE,
        &ROWS,
        &PUBLISH,
        &NOTHING,
    ]
    .iter()
    .map(|stage| stage.read().0)
    .sum();
    if total == 0 {
        return Vec::new();
    }
    let fields = [
        stage_field("resolve_src", &RESOLVE_SRC),
        stage_field("resolve_dst", &RESOLVE_DST),
        stage_field("ensure_surface", &ENSURE_SURFACE),
        stage_field("rows", &ROWS),
        stage_field("publish", &PUBLISH),
        stage_field("nothing", &NOTHING),
    ]
    .join(" ");
    let mut lines = vec![format!(
        "bitblt_stages: {fields} (calls/cost/per-call; rows is the pixel copy)"
    )];
    if let Some(note) = NOTE.get() {
        lines.push(format!("bitblt_nothing: {note}"));
    }
    lines
}

/// Zero every counter — tests only.
#[cfg(all(test, debug_assertions))]
pub(crate) fn reset_for_test() {
    for stage in [&RESOLVE_SRC, &RESOLVE_DST, &ENSURE_SURFACE, &ROWS, &PUBLISH] {
        stage.reset();
    }
    NOTHING.reset();
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::{report_lines, reset_for_test, timed_rows};

    #[test]
    fn stages_are_counted_and_reported() {
        reset_for_test();
        assert!(
            report_lines().is_empty(),
            "nothing recorded yet means no line"
        );
        timed_rows(|| 1_u8);
        let lines = report_lines();
        assert_eq!(lines.len(), 1, "one aggregate line");
        assert!(lines[0].starts_with("bitblt_stages:"), "{}", lines[0]);
        assert!(lines[0].contains("rows=1/"), "{}", lines[0]);
    }
}
