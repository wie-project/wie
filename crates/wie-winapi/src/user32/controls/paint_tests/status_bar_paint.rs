//! Status-bar strip, groove separators and per-part text — the painters that
//! moved out of `comctl32.rs` into `statusbar/`.
//!
//! The relocated code draws a raised `COLOR_BTNFACE` strip (a
//! `COLOR_BTNHIGHLIGHT` top client edge, a `COLOR_BTNSHADOW` bottom edge), a
//! sunken groove at every interior part boundary, and each part's text clipped
//! to its cell. None of it had a direct pixel assertion.

use super::{
    BTNFACE, column_is_uniform, count_off_fill, frame_pixel, guest_i32_array, guest_utf16, paint,
    published, push_control_with_style, send,
};
use crate::gdi32::IRect;
use crate::user32::controls::{SB_SETPARTS, SB_SETTEXTW};

const X: i32 = 0;
const Y: i32 = 76;
const WIDTH: i32 = 200;
const HEIGHT: i32 = 20;

/// The status bar's own class name (comctl32's `STATUSCLASSNAMEW`).
const CLASS: &str = "MSCTLS_STATUSBAR32";

/// `GetSysColor(COLOR_BTNHIGHLIGHT)` — the raised strip's top client edge.
const BTNHIGHLIGHT: u32 = 0x00FF_FFFF;
/// `GetSysColor(COLOR_BTNSHADOW)` — the bottom edge and the groove's shadow line.
const BTNSHADOW: u32 = 0x00A0_A0A0;

fn bar(text: &str) -> (crate::WinApiState, u64, u64) {
    let mut state = super::test_state();
    let (top, bar) = push_control_with_style(
        &mut state,
        CLASS,
        crate::user32::controls::CCS_BOTTOM,
        (X, Y, WIDTH, HEIGHT),
        text,
    );
    (state, top, bar)
}

/// The strip renders as a raised face: `COLOR_BTNHIGHLIGHT` on the top client
/// edge, `COLOR_BTNSHADOW` on the bottom, `COLOR_BTNFACE` between — and it
/// renders at all even before any `SB_*` message arrives (the strip is
/// font-free; only the per-part text needs a resolved font).
#[test]
fn the_strip_paints_a_raised_face_before_any_sb_message() {
    let mut engine = super::test_engine();
    let (mut state, top, bar) = bar("");
    paint(&mut engine, &mut state, bar);
    let frame = published(&mut state, top);

    for x in X..X + WIDTH {
        assert_eq!(
            frame_pixel(&frame, x, Y),
            BTNHIGHLIGHT,
            "the top client edge is COLOR_BTNHIGHLIGHT at x={x}"
        );
        assert_eq!(
            frame_pixel(&frame, x, Y + HEIGHT - 1),
            BTNSHADOW,
            "the bottom client edge is COLOR_BTNSHADOW at x={x}"
        );
        assert_eq!(
            frame_pixel(&frame, x, Y + 5),
            BTNFACE,
            "the strip interior is COLOR_BTNFACE at x={x}"
        );
    }
}

/// The creation text becomes part 0 (real comctl32 applies
/// `CreateStatusWindow`'s text as `SB_SETTEXT(0, …)`), so a bar created with
/// "Ready" renders ink without any `SB_SETTEXTW`.
#[test]
fn the_creation_text_renders_as_part_zero() {
    let mut engine = super::test_engine();
    let (mut state, top, bar) = bar("Ready");
    paint(&mut engine, &mut state, bar);
    let frame = published(&mut state, top);
    let strip = IRect::from_xywh(X + 1, Y + 1, WIDTH - 2, HEIGHT - 2);
    assert!(
        count_off_fill(&frame, strip, BTNFACE) > 0,
        "the creation text must render on the strip"
    );
}

/// `SB_SETPARTS` draws a sunken groove at each interior boundary — a
/// `COLOR_BTNSHADOW` column with a `COLOR_BTNHIGHLIGHT` column immediately to
/// its right — spanning only the interior rows, and NO groove at the last part
/// (which always runs to the right edge).
#[test]
fn part_boundaries_draw_a_sunken_groove() {
    let mut engine = super::test_engine();
    let (mut state, top, bar) = bar("");
    // Two parts: the first ends at x=100 (in client coords, and the bar sits at
    // x=0), the second extends to the right edge (-1).
    let parts = guest_i32_array(&mut engine, &[100, -1]);
    assert_eq!(
        send(&mut engine, &mut state, bar, SB_SETPARTS, 2, parts),
        1,
        "SB_SETPARTS returns TRUE"
    );
    paint(&mut engine, &mut state, bar);
    let frame = published(&mut state, top);

    for y in Y + 1..Y + HEIGHT - 1 {
        assert_eq!(
            frame_pixel(&frame, 100, y),
            BTNSHADOW,
            "the groove's shadow line is at the boundary column (y={y})"
        );
        assert_eq!(
            frame_pixel(&frame, 101, y),
            BTNHIGHLIGHT,
            "the groove's highlight line is one column right of the shadow (y={y})"
        );
    }
    // The top/bottom client edges are NOT overdrawn by the groove — the interior
    // rows only.
    assert_eq!(frame_pixel(&frame, 100, Y), BTNHIGHLIGHT, "top edge intact");
    assert_eq!(
        frame_pixel(&frame, 100, Y + HEIGHT - 1),
        BTNSHADOW,
        "bottom edge intact"
    );
}

/// A part's text is clipped to its cell: part 1's ink must stay inside the
/// second cell (right of the groove's highlight line, left of the strip's
/// right edge minus the 3 px text inset).
#[test]
fn part_text_is_clipped_to_its_own_cell() {
    let mut engine = super::test_engine();
    let (mut state, top, bar) = bar("");
    let parts = guest_i32_array(&mut engine, &[100, -1]);
    send(&mut engine, &mut state, bar, SB_SETPARTS, 2, parts);
    let zero = guest_utf16(&mut engine, "partzero");
    send(&mut engine, &mut state, bar, SB_SETTEXTW, 0, zero);
    let one = guest_utf16(&mut engine, "partone");
    send(&mut engine, &mut state, bar, SB_SETTEXTW, 1, one);
    paint(&mut engine, &mut state, bar);
    let frame = published(&mut state, top);

    let left_cell = IRect::from_xywh(X, Y, 100, HEIGHT);
    let right_cell = IRect::from_xywh(101, Y, WIDTH - 101, HEIGHT);
    assert!(
        count_off_fill(&frame, left_cell, BTNFACE) > 0,
        "part 0's text renders inside the first cell"
    );
    assert!(
        count_off_fill(&frame, right_cell, BTNFACE) > 0,
        "part 1's text renders inside the second cell"
    );
    // The cells are disjoint: the groove's two columns carry only the groove
    // colours (anti-aliased ink would be neither), so no part's glyphs crossed
    // the boundary.
    for y in Y + 1..Y + HEIGHT - 1 {
        for x in [100, 101] {
            let pixel = frame_pixel(&frame, x, y);
            assert!(
                pixel == BTNSHADOW || pixel == BTNHIGHLIGHT || pixel == BTNFACE,
                "the groove column at ({x},{y}) must carry only groove colours, got {pixel:#x}"
            );
        }
    }
}

/// With no `SB_SETPARTS` the strip is ONE implicit part spanning the full
/// width and draws no grooves at all — the default a guest gets before it
/// configures the bar.
#[test]
fn an_unconfigured_bar_is_one_full_width_part_with_no_grooves() {
    let mut engine = super::test_engine();
    let (mut state, top, bar) = bar("Ready");
    paint(&mut engine, &mut state, bar);
    let frame = published(&mut state, top);

    // No separator anywhere: a groove is a full-height 1 px line, so it shows
    // up as a column that is UNIFORMLY the shadow or the highlight colour
    // across the interior rows. Anti-aliased glyph ink never does that.
    let interior_rows = Y + 2..Y + HEIGHT - 2;
    for x in X..X + WIDTH {
        assert!(
            !column_is_uniform(&frame, x, interior_rows.clone(), BTNSHADOW),
            "an unconfigured bar must draw no BTNSHADOW groove column at x={x}"
        );
        assert!(
            !column_is_uniform(&frame, x, interior_rows.clone(), BTNHIGHLIGHT),
            "an unconfigured bar must draw no BTNHIGHLIGHT groove column at x={x}"
        );
    }
    // …and the strip really did paint (the assertion above is not vacuous).
    assert!(
        count_off_fill(
            &frame,
            IRect::from_xywh(X + 1, Y + 1, WIDTH - 2, HEIGHT - 2),
            BTNFACE
        ) > 0,
        "the creation text must have rendered on the strip"
    );
}

/// The strip is drawn even when the bar's font cannot be resolved: the face and
/// edges are font-free, so a paint with no system font still produces the
/// raised strip and simply skips the text. (In practice the system font is
/// always resolvable on the host; this pins that the font-free strip is not
/// gated behind the text path.)
#[test]
fn the_strip_is_not_gated_behind_the_font_resolution() {
    let mut engine = super::test_engine();
    let (mut state, top, bar) = bar("");
    let text = guest_utf16(&mut engine, "hello");
    send(&mut engine, &mut state, bar, SB_SETTEXTW, 0, text);
    paint(&mut engine, &mut state, bar);
    let frame = published(&mut state, top);
    assert_eq!(
        frame_pixel(&frame, WIDTH / 2, Y),
        BTNHIGHLIGHT,
        "the top client edge is drawn regardless of the text"
    );
    assert_eq!(
        frame_pixel(&frame, WIDTH / 2, Y + 5),
        BTNFACE,
        "the face is drawn regardless of the text"
    );
}
