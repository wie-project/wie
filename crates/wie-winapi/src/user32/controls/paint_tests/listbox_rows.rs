//! LISTBOX row painting: which row gets `COLOR_HIGHLIGHT`, and how that
//! tracks the scroll offset.
//!
//! NOTE ON "BANDING": there is no alternating-row (zebra) banding in WIE's
//! LISTBOX paint — `paint_item_lines` in `listbox.rs` fills exactly one rect
//! per row, and only for the selected one. The tests below therefore pin the
//! invariant that actually exists (a uniform `COLOR_WINDOW` background, the
//! highlight on the selected ITEM's row) and the parity question that does
//! have an answer: row 0 is not special. The highlight is anchored to the item
//! index, so scrolling moves it with the item and leaves no stale band behind.
//! If alternating banding is ever implemented, these assertions are the ones
//! that must change.

use super::{
    HIGHLIGHT, INK, SENTINEL, WINDOW_WHITE, count_color, frame_pixel, guest_ansi, line_height,
    paint, pending_label_invalid, published, push_control, send, sentinel,
};
use crate::gdi32::IRect;
use crate::user32::controls::{LabelInvalidRect, LabelInvalidation};
use crate::user32::wm::WinMsg;

/// The listbox occupies `(10, 10)` with a 100×66 client — four 16 px rows with
/// 2 px to spare, so the viewport band is a proper sub-rect of the client and a
/// scroll is a PARTIAL repaint.
const X: i32 = 10;
const Y: i32 = 10;
const WIDTH: i32 = 100;
const HEIGHT: i32 = 66;

/// The default font's row pitch.
const LINE_H: i32 = 16;

fn listbox_with_items(state: &mut crate::WinApiState, count: usize) -> (u64, u64) {
    let (top, listbox) = push_control(state, "LISTBOX", (X, Y, WIDTH, HEIGHT), "");
    let _ = (top, listbox, count);
    (top, listbox)
}

/// Seed `count` items ("one", "two", …) through the real `LB_ADDSTRING`
/// dispatch.
fn seed_items(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut crate::WinApiState,
    hwnd: u64,
    count: usize,
) {
    for index in 0..count {
        let text = format!("item{index}");
        let address = guest_ansi(engine, &text);
        let result = send(
            engine,
            state,
            hwnd,
            WinMsg::LB_ADDSTRING.as_u32(),
            0,
            address,
        );
        assert_eq!(
            result,
            u64::try_from(index).unwrap_or(u64::MAX),
            "LB_ADDSTRING returns the new item's index"
        );
    }
}

/// The y of screen row `row`'s top edge in surface coordinates.
fn row_y(row: i32) -> i32 {
    Y.saturating_add(row.saturating_mul(LINE_H))
}

/// A column far to the right of the item text (the default font's widest item
/// is well under 60 px), so the row-background colour can be read without
/// colliding with glyph ink.
const BACKGROUND_PROBE_X: i32 = X + 90;

/// An unselected LISTBOX paints every visible row with `COLOR_WINDOW` — no
/// banding, no per-row tint. Row 0 in particular is not special.
#[test]
fn every_row_is_the_window_colour_when_nothing_is_selected() {
    let mut engine = super::test_engine();
    let mut state = super::test_state();
    let (_top, listbox) = listbox_with_items(&mut state, 0);
    seed_items(&mut engine, &mut state, listbox, 4);
    // An explicit clear (the default seed is already -1) so the test does not
    // depend on the seed value.
    send(
        &mut engine,
        &mut state,
        listbox,
        WinMsg::LB_SETCURSEL.as_u32(),
        u64::MAX, // -1 as a Win64 wParam
        0,
    );
    paint(&mut engine, &mut state, listbox);
    let top = _top;
    let frame = published(&mut state, top);

    for row in 0..4 {
        assert_eq!(
            frame_pixel(&frame, BACKGROUND_PROBE_X, row_y(row) + 2),
            WINDOW_WHITE,
            "unselected row {row} is COLOR_WINDOW"
        );
    }
    assert_eq!(
        count_color(&frame, IRect::from_xywh(X, Y, WIDTH, 50), HIGHLIGHT),
        0,
        "no row may be filled with COLOR_HIGHLIGHT without a selection"
    );
}

/// The selected item's row is filled with `COLOR_HIGHLIGHT` and no other row
/// is — including row 0 when a LATER item is selected, which is the parity
/// question: the highlight follows the item index, not an odd/even row
/// alternation.
#[test]
fn only_the_selected_items_row_is_highlighted() {
    let mut engine = super::test_engine();
    let mut state = super::test_state();
    let (top, listbox) = listbox_with_items(&mut state, 0);
    seed_items(&mut engine, &mut state, listbox, 4);
    paint(&mut engine, &mut state, listbox);

    // Select item 1: row 1 is highlighted, rows 0/2/3 are not.
    send(
        &mut engine,
        &mut state,
        listbox,
        WinMsg::LB_SETCURSEL.as_u32(),
        1,
        0,
    );
    paint(&mut engine, &mut state, listbox);
    let frame = published(&mut state, top);
    for row in 0..4 {
        let expected = if row == 1 { HIGHLIGHT } else { WINDOW_WHITE };
        assert_eq!(
            frame_pixel(&frame, BACKGROUND_PROBE_X, row_y(row) + 2),
            expected,
            "row {row} background after selecting item 1"
        );
    }
    // The highlight covers the row's FULL width, not just the text extent:
    // the right-hand third carries no glyphs at all, so every one of its pixels
    // must be exactly COLOR_HIGHLIGHT. (Over the text itself the fill is mixed
    // with COLOR_HIGHLIGHTTEXT glyph ink, so those pixels are not one colour.)
    let clear = IRect::from_xywh(X + 70, row_y(1), 30, LINE_H);
    for y in row_y(1)..row_y(1) + LINE_H {
        for x in X + 70..X + WIDTH {
            assert_eq!(
                frame_pixel(&frame, x, y),
                HIGHLIGHT,
                "the selected row's glyph-free area must be solid COLOR_HIGHLIGHT at ({x},{y})"
            );
        }
    }
    assert!(
        count_color(&frame, clear, HIGHLIGHT) > 0,
        "the selected row carries the COLOR_HIGHLIGHT fill"
    );
    // The fill stops at the row: the next row down is plain background again.
    assert_eq!(
        frame_pixel(&frame, X + 90, row_y(2) + 2),
        WINDOW_WHITE,
        "the row below the selection must not be highlighted"
    );

    // Move the selection to row 0: the highlight moves UP, and specifically
    // row 0 gets it only because item 0 is selected — not because it is the
    // first row.
    send(
        &mut engine,
        &mut state,
        listbox,
        WinMsg::LB_SETCURSEL.as_u32(),
        0,
        0,
    );
    paint(&mut engine, &mut state, listbox);
    let frame = published(&mut state, top);
    assert_eq!(
        frame_pixel(&frame, BACKGROUND_PROBE_X, row_y(0) + 2),
        HIGHLIGHT,
        "selecting item 0 highlights row 0"
    );
    for row in 1..4 {
        assert_eq!(
            frame_pixel(&frame, BACKGROUND_PROBE_X, row_y(row) + 2),
            WINDOW_WHITE,
            "row {row} must lose the highlight when the selection moves away"
        );
    }
}

/// The highlight is anchored to the ITEM, so a scroll carries it with the item
/// and never leaves a stale highlight on the screen row that used to show it.
#[test]
fn the_highlight_follows_the_item_across_a_scroll() {
    let mut engine = super::test_engine();
    let mut state = super::test_state();
    let (top, listbox) = listbox_with_items(&mut state, 0);
    seed_items(&mut engine, &mut state, listbox, 8);
    let line_h = line_height(&mut state, listbox);
    assert_eq!(
        line_h, LINE_H,
        "the fixture's row geometry assumes the 16 px default line height"
    );

    // Item 1 selected → screen row 1 highlighted.
    send(
        &mut engine,
        &mut state,
        listbox,
        WinMsg::LB_SETCURSEL.as_u32(),
        1,
        0,
    );
    paint(&mut engine, &mut state, listbox);
    assert_eq!(
        frame_pixel(
            &published(&mut state, top),
            BACKGROUND_PROBE_X,
            row_y(1) + 2
        ),
        HIGHLIGHT,
        "item 1 is on screen row 1 before the scroll"
    );

    // One wheel notch down scrolls 3 rows (`WHEEL_SCROLL_LINES`), so item 1
    // scrolls out of the viewport: NO row may stay highlighted — the screen
    // row that used to show it must be repainted to the plain background.
    send(
        &mut engine,
        &mut state,
        listbox,
        WinMsg::WM_MOUSEWHEEL.as_u32(),
        wheel(-120),
        0,
    );
    paint(&mut engine, &mut state, listbox);
    let frame = published(&mut state, top);
    assert_eq!(
        count_color(&frame, IRect::from_xywh(X, Y, WIDTH, HEIGHT), HIGHLIGHT),
        0,
        "a scrolled-off selection must leave no highlight anywhere"
    );
    assert_eq!(
        frame_pixel(&frame, BACKGROUND_PROBE_X, row_y(1) + 2),
        WINDOW_WHITE,
        "the screen row the selection vacated is back to COLOR_WINDOW"
    );

    // Now select item 5, which is at screen row 5 − 3 = 2 after the scroll:
    // the highlight lands on row 2, proving it tracks the item, not the row
    // index it happened to occupy before the scroll.
    send(
        &mut engine,
        &mut state,
        listbox,
        WinMsg::LB_SETCURSEL.as_u32(),
        5,
        0,
    );
    paint(&mut engine, &mut state, listbox);
    let frame = published(&mut state, top);
    assert_eq!(
        frame_pixel(&frame, BACKGROUND_PROBE_X, row_y(2) + 2),
        HIGHLIGHT,
        "item 5 is on screen row 2 after the scroll — the highlight follows it"
    );
    assert_eq!(
        frame_pixel(&frame, BACKGROUND_PROBE_X, row_y(0) + 2),
        WINDOW_WHITE,
        "screen row 0 shows item 3, which is not selected"
    );
}

/// A scroll's repaint scope is the viewport band — a proper sub-rect of the
/// client, not the whole control — and the erase covers exactly it. This is
/// the LISTBOX half of the relocated rect-scope machinery, seen from the
/// pixels: the 2 px strip below the last row keeps the sentinel.
#[test]
fn a_scroll_repaints_only_the_viewport_band() {
    let mut engine = super::test_engine();
    let mut state = super::test_state();
    let (top, listbox) = listbox_with_items(&mut state, 0);
    seed_items(&mut engine, &mut state, listbox, 8);
    paint(&mut engine, &mut state, listbox);

    send(
        &mut engine,
        &mut state,
        listbox,
        WinMsg::WM_MOUSEWHEEL.as_u32(),
        wheel(-120),
        0,
    );
    let band = 4 * LINE_H; // 4 visible rows
    assert_eq!(
        pending_label_invalid(&state, listbox),
        LabelInvalidation::Rect(LabelInvalidRect {
            rect: IRect::from_xywh(0, 0, WIDTH, band),
            width: WIDTH,
            height: HEIGHT,
        }),
        "a scroll marks the visible row band — a sub-rect of the {HEIGHT} px client"
    );

    sentinel(&mut state, top, IRect::from_xywh(X, Y, WIDTH, HEIGHT));
    paint(&mut engine, &mut state, listbox);
    let frame = published(&mut state, top);
    // Inside the band: repainted (white or ink), never the sentinel.
    for y in Y..Y + band {
        for x in X + 40..X + WIDTH {
            assert_ne!(
                frame_pixel(&frame, x, y),
                SENTINEL,
                "a pixel inside the scroll band ({x},{y}) must be repainted"
            );
        }
    }
    // Below the band: untouched.
    for y in Y + band..Y + HEIGHT {
        for x in X..X + WIDTH {
            assert_eq!(
                frame_pixel(&frame, x, y),
                SENTINEL,
                "a pixel below the scroll band ({x},{y}) must survive"
            );
        }
    }
}

/// A partial repaint renders only the rows overlapping the dirty rect, so an
/// `LB_ADDSTRING` that lands outside the current viewport changes nothing on
/// screen — the untouched rows keep their exact pixels (including the glyphs).
#[test]
fn an_item_appended_below_the_viewport_changes_no_visible_pixel() {
    let mut engine = super::test_engine();
    let mut state = super::test_state();
    let (top, listbox) = listbox_with_items(&mut state, 0);
    seed_items(&mut engine, &mut state, listbox, 4);
    paint(&mut engine, &mut state, listbox);
    let before = published(&mut state, top);

    // A fifth item lands on screen row 4 — below the 4 visible rows. Its mark
    // is off-screen (`listbox_row_rect` returns `None`), so the pending scope
    // stays whatever it was and the next paint must not disturb the rows.
    let address = guest_ansi(&mut engine, "item4");
    send(
        &mut engine,
        &mut state,
        listbox,
        WinMsg::LB_ADDSTRING.as_u32(),
        0,
        address,
    );
    // Force a clean scope so the repaint is the "nothing visible changed"
    // case rather than inheriting an earlier mark.
    super::set_pending_label_invalid(&mut state, listbox, LabelInvalidation::Clean);
    sentinel(&mut state, top, IRect::from_xywh(X, Y, WIDTH, HEIGHT));
    paint(&mut engine, &mut state, listbox);
    let after = published(&mut state, top);

    let mut diffs = 0;
    for y in Y..Y + HEIGHT {
        for x in X..X + WIDTH {
            if frame_pixel(&before, x, y) != frame_pixel(&after, x, y) {
                diffs += 1;
            }
        }
    }
    assert_eq!(
        diffs, 0,
        "a clean scope must repaint the rows byte-identically"
    );
    // Sanity: the pre-paint frame really did have content in the rows (the
    // erase colour is COLOR_WINDOW, the glyphs are `INK`).
    assert!(
        count_color(&before, IRect::from_xywh(X, Y, WIDTH, HEIGHT), WINDOW_WHITE) > 0
            && count_color(&before, IRect::from_xywh(X, Y, WIDTH, HEIGHT), INK) > 0,
        "the fixture must render item text on the window background"
    );
}

/// A WM_MOUSEWHEEL wParam carrying the given signed delta in the high word.
fn wheel(delta: i32) -> u64 {
    u64::from(u16::from_ne_bytes(
        delta.to_ne_bytes()[..2].try_into().expect("delta"),
    )) << 16
}
