//! Rect-level repaint scope: which pixels `paint_control` actually erases.
//!
//! This is the machinery that most benefits from a regression test after the
//! file move. `control_dirty_rect` decides the erase rect, and the erase fill
//! is what feeds the B3 dirty-region accumulator, so a wrong rect is not a
//! cosmetic bug — it either leaves stale pixels (too small) or widens the
//! published frame's `region` (too big).
//!
//! Two complementary oracles:
//!
//! * the pure function `control_dirty_rect` (pending rect / clean / full /
//!   stale-size-stamp), and
//! * the [`SENTINEL`](super::SENTINEL) trick — a colour in no control palette
//!   is written over the control's rect and a repaint is asked which pixels it
//!   overwrote. That distinguishes "erased exactly the pending rect" from
//!   "erased nothing", which a before/after frame diff cannot.

use super::{
    BTNFACE, BTNFACE_PRESSED, SENTINEL, frame_pixel, paint, published, push_control, sentinel,
    set_pending_label_invalid,
};
use crate::gdi32::IRect;
use crate::user32::controls::Dimension;
use crate::user32::controls::paint::control_dirty_rect;
use crate::user32::controls::{LabelInvalidRect, LabelInvalidation};

/// A 40×20 STATIC at (10, 10) — text-free, so every pixel of its face is a
/// pure erase and a repaint's write scope is unambiguous.
fn label() -> (crate::WinApiState, u64, u64) {
    let mut state = super::test_state();
    let (top, label) = push_control(&mut state, "STATIC", (10, 10, 40, 20), "");
    (state, top, label)
}

/// The rect the erase is computed for: a clean control and a full one both
/// yield the WHOLE client, and a pending rect is honoured only when its size
/// stamp still matches the control's current extent.
#[test]
fn dirty_rect_honours_a_current_pending_rect_and_falls_back_to_the_whole_client() {
    let (mut state, _top, label) = label();
    let size = Dimension::new(40, 20);
    let whole = IRect::from_xywh(0, 0, 40, 20);

    // Clean (nothing pending) and Full (the first paint / a structural change)
    // are both whole-client erases.
    set_pending_label_invalid(&mut state, label, LabelInvalidation::Clean);
    assert_eq!(control_dirty_rect(&state, label, size), whole);

    set_pending_label_invalid(&mut state, label, LabelInvalidation::Full);
    assert_eq!(control_dirty_rect(&state, label, size), whole);

    // A pending rect at the CURRENT size is returned verbatim — a small rect,
    // not the whole client. This is the assertion a "always return the full
    // client" regression breaks.
    let small = IRect::from_xywh(12, 4, 10, 6);
    set_pending_label_invalid(
        &mut state,
        label,
        LabelInvalidation::Rect(LabelInvalidRect {
            rect: small,
            width: 40,
            height: 20,
        }),
    );
    assert_eq!(
        control_dirty_rect(&state, label, size),
        small,
        "a current pending rect must be honoured as-is, not widened to the client"
    );

    // A rect stamped at a DIFFERENT size is stale (the layout reflowed) and
    // escalates to a whole-client repaint.
    set_pending_label_invalid(
        &mut state,
        label,
        LabelInvalidation::Rect(LabelInvalidRect {
            rect: small,
            width: 40,
            height: 21,
        }),
    );
    assert_eq!(
        control_dirty_rect(&state, label, size),
        whole,
        "a stale size stamp must escalate to a full repaint"
    );
    // …and the caller must pass the CURRENT size for the stamp to be believed:
    // asking for the size the rect was stamped at returns it again.
    assert_eq!(
        control_dirty_rect(&state, label, Dimension::new(40, 21)),
        small,
        "the stamp is compared against the size the paint passes in"
    );
}

/// The pixel-level proof of erase confinement: a partial repaint overwrites
/// exactly the pending rect and leaves every other pixel of the control
/// untouched (sentinel included).
#[test]
fn a_partial_repaint_erases_exactly_the_pending_rect() {
    let mut engine = super::test_engine();
    let (mut state, top, label) = label();
    paint(&mut engine, &mut state, label);
    let frame = published(&mut state, top);
    assert_eq!(
        frame_pixel(&frame, 30, 20),
        BTNFACE,
        "the first paint covers the whole client"
    );

    // Foreign content over the label's whole rect, then a small pending scope.
    let whole = IRect::from_xywh(10, 10, 40, 20);
    sentinel(&mut state, top, whole);
    let dirty = IRect::from_xywh(12, 4, 10, 6); // client-relative
    set_pending_label_invalid(
        &mut state,
        label,
        LabelInvalidation::Rect(LabelInvalidRect {
            rect: dirty,
            width: 40,
            height: 20,
        }),
    );
    paint(&mut engine, &mut state, label);
    let frame = published(&mut state, top);

    // Inside the pending rect (offset by the control's position): erased.
    for y in 14_i32..20 {
        for x in 22_i32..32 {
            assert_eq!(
                frame_pixel(&frame, x, y),
                BTNFACE,
                "the pending rect must be erased at ({x},{y})"
            );
        }
    }
    // Outside it: still the sentinel — proof the erase did not widen.
    for y in 10_i32..30 {
        for x in 10_i32..50 {
            let inside = (22..32).contains(&x) && (14..20).contains(&y);
            if !inside {
                assert_eq!(
                    frame_pixel(&frame, x, y),
                    SENTINEL,
                    "a pixel outside the pending rect ({x},{y}) must survive the partial repaint"
                );
            }
        }
    }
}

/// The negative control for the test above: a `Full` scope over the same
/// sentinel-covered rect erases the WHOLE client. Without this the confinement
/// assertion could pass on a `control_dirty_rect` that always returns an empty
/// rect (or one the paint silently ignores).
#[test]
fn a_full_scope_erases_the_whole_client() {
    let mut engine = super::test_engine();
    let (mut state, top, label) = label();
    paint(&mut engine, &mut state, label);
    sentinel(&mut state, top, IRect::from_xywh(10, 10, 40, 20));

    set_pending_label_invalid(&mut state, label, LabelInvalidation::Full);
    paint(&mut engine, &mut state, label);
    let frame = published(&mut state, top);
    for y in 10_i32..30 {
        for x in 10_i32..50 {
            assert_eq!(
                frame_pixel(&frame, x, y),
                BTNFACE,
                "a full scope must erase every pixel of the client at ({x},{y})"
            );
        }
    }
}

/// The pending rect is expressed in CLIENT coordinates and the erase is
/// applied at the control's surface offset — a control at (10, 10) must erase
/// the surface rect (22, 14, 32, 20) for a client rect (12, 4, 10, 6). If the
/// offset were dropped the erase would land 10 px up and to the left; this
/// pins the composition.
#[test]
fn the_pending_rect_is_offset_by_the_controls_position() {
    let mut engine = super::test_engine();
    let mut state = super::test_state();
    let (top, label) = push_control(&mut state, "STATIC", (30, 25, 40, 20), "");
    paint(&mut engine, &mut state, label);
    sentinel(&mut state, top, IRect::from_xywh(30, 25, 40, 20));

    let dirty = IRect::from_xywh(2, 2, 6, 4);
    set_pending_label_invalid(
        &mut state,
        label,
        LabelInvalidation::Rect(LabelInvalidRect {
            rect: dirty,
            width: 40,
            height: 20,
        }),
    );
    paint(&mut engine, &mut state, label);
    let frame = published(&mut state, top);
    // Expected surface rect: (30+2, 25+2) .. (30+8, 25+6).
    assert_eq!(
        frame_pixel(&frame, 32, 27),
        BTNFACE,
        "the erase must land at the control's surface offset"
    );
    assert_eq!(
        frame_pixel(&frame, 31, 27),
        SENTINEL,
        "one pixel left of the offset rect must survive"
    );
    assert_eq!(
        frame_pixel(&frame, 30, 25),
        SENTINEL,
        "the control's own top-left corner must survive"
    );
}

/// A partial BUTTON repaint erases with the CURRENT face colour: pressing and
/// releasing the button twice must land on `COLOR_BTNFACE_PRESSED` while
/// pushed and `COLOR_BTNFACE` when released — through the partial branch, with
/// the sentinel proving the erase is the interior only.
#[test]
fn a_partial_button_repaint_erases_the_interior_with_the_current_face() {
    let mut engine = super::test_engine();
    let mut state = super::test_state();
    let (top, button) = push_control(&mut state, "BUTTON", (10, 10, 40, 20), "");
    paint(&mut engine, &mut state, button);
    sentinel(&mut state, top, IRect::from_xywh(10, 10, 40, 20));

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::wm::WinMsg::BM_SETSTATE.as_u32(),
        1,
        0,
    )
    .expect("BM_SETSTATE ok")
    .expect("BM_SETSTATE result");
    paint(&mut engine, &mut state, button);
    let frame = published(&mut state, top);
    assert_eq!(
        frame_pixel(&frame, 30, 20),
        BTNFACE_PRESSED,
        "the partial erase must use the PRESSED face colour"
    );
    assert_eq!(
        frame_pixel(&frame, 10, 10),
        SENTINEL,
        "the interior-only scope must not touch the border corner"
    );

    // Release → the released face over the same interior.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::wm::WinMsg::BM_SETSTATE.as_u32(),
        0,
        0,
    )
    .expect("BM_SETSTATE ok")
    .expect("BM_SETSTATE result");
    sentinel(&mut state, top, IRect::from_xywh(10, 10, 40, 20));
    paint(&mut engine, &mut state, button);
    let frame = published(&mut state, top);
    assert_eq!(
        frame_pixel(&frame, 30, 20),
        BTNFACE,
        "the release must erase with the RELEASED face colour"
    );
}

/// `rect_touches_border` decides whether a partial erase re-strokes the 1 px
/// border. A rect that reaches an edge must trigger the re-stroke (the erase
/// overpainted it); an interior rect must not (the re-stroke would widen the
/// published region past the true changed band).
#[test]
fn rect_touches_border_matches_the_edge_test() {
    use crate::user32::controls::paint::rect_touches_border;
    let size = Dimension::new(40, 20);
    assert!(
        !rect_touches_border(IRect::from_xywh(1, 1, 38, 18), size),
        "the interior touches no border"
    );
    assert!(
        rect_touches_border(IRect::from_xywh(0, 5, 40, 10), size),
        "a full-width band reaches the left and right edges"
    );
    assert!(
        rect_touches_border(IRect::from_xywh(5, 0, 10, 20), size),
        "a full-height column reaches the top and bottom edges"
    );
    // The test is `>=` on the far edges: a rect ending exactly at the last
    // pixel row still touches it.
    assert!(
        rect_touches_border(IRect::from_xywh(5, 5, 35, 15), size),
        "a rect ending on the last row/column touches the border"
    );
}
