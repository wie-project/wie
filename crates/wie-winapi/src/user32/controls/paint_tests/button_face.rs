//! BUTTON pressed/released face pixels.
//!
//! Covers `button/mod.rs::paint_face_and_border` (the `COLOR_BTNFACE` vs
//! `COLOR_BTNFACE_PRESSED` choice) on BOTH of the branches
//! `paint_control`'s BUTTON arm can take — the full-repaint branch
//! (`paint_face_and_border`) and the partial-repaint branch (the inline face
//! fill in `paint.rs`) — plus the border colour, which must be identical in
//! both states.

use super::{
    BTNFACE, BTNFACE_PRESSED, BTNSHADOW, INK, frame_pixel, paint, pending_label_invalid, published,
    push_control, send,
};
use crate::gdi32::IRect;
use crate::user32::controls::LabelInvalidation;
use crate::user32::wm::WinMsg;

/// The button's client rect, at `(10, 10)` in a 200×100 surface.
const X: i32 = 10;
const Y: i32 = 10;
const WIDTH: i32 = 40;
const HEIGHT: i32 = 20;

fn interior(x: i32, y: i32) -> (i32, i32) {
    (X.saturating_add(x), Y.saturating_add(y))
}

/// A visible 40×20 push button captioned "OK" under a 200×100 top level.
fn button() -> (crate::WinApiState, u64, u64) {
    let mut state = super::test_state();
    let (top, button) = push_control(&mut state, "BUTTON", (X, Y, WIDTH, HEIGHT), "OK");
    (state, top, button)
}

/// A released push button paints `COLOR_BTNFACE` and a pushed one
/// `COLOR_BTNFACE_PRESSED`, both with the same `COLOR_BTNSHADOW` border — the
/// face is the observable that distinguishes the two states, so it is asserted
/// as an actual pixel at a point inside the 1 px border and clear of the
/// caption ink.
#[test]
fn pushed_button_paints_the_pressed_face_and_released_the_plain_one() {
    let mut engine = super::test_engine();
    let (mut state, top, button) = button();

    // First paint: a fresh control's scope is Full, so this is the
    // `paint_face_and_border` branch.
    paint(&mut engine, &mut state, button);
    let frame = published(&mut state, top);
    let (ix, iy) = interior(WIDTH / 2, 2);
    assert_eq!(
        frame_pixel(&frame, ix, iy),
        BTNFACE,
        "a released button's face is COLOR_BTNFACE"
    );
    assert_eq!(
        frame_pixel(&frame, X, Y),
        BTNSHADOW,
        "the 1 px top border is COLOR_BTNSHADOW"
    );
    assert_eq!(
        pending_label_invalid(&state, button),
        LabelInvalidation::Clean,
        "the paint consumes the pending scope"
    );

    // BM_SETSTATE(1) is the programmatic press: it sets `WindowFlags::PRESSED`
    // and marks the FACE rect (the interior inside the border) — which is the
    // partial-repaint branch of the BUTTON arm, a DIFFERENT code path from the
    // full repaint above even though both must produce the pressed face.
    let previous = send(
        &mut engine,
        &mut state,
        button,
        WinMsg::BM_SETSTATE.as_u32(),
        1,
        0,
    );
    assert_eq!(
        previous, 0,
        "BM_SETSTATE(1) reports the previous state: released"
    );
    assert!(
        matches!(
            pending_label_invalid(&state, button),
            LabelInvalidation::Rect(_)
        ),
        "a press marks the face rect, got {:?}",
        pending_label_invalid(&state, button)
    );
    paint(&mut engine, &mut state, button);
    let frame = published(&mut state, top);
    assert_eq!(
        frame_pixel(&frame, ix, iy),
        BTNFACE_PRESSED,
        "a pushed button's face is COLOR_BTNFACE_PRESSED"
    );
    assert_eq!(
        frame_pixel(&frame, X, Y),
        BTNSHADOW,
        "the press must NOT change the border colour"
    );

    // Release: the face returns to COLOR_BTNFACE.
    send(
        &mut engine,
        &mut state,
        button,
        WinMsg::BM_SETSTATE.as_u32(),
        0,
        0,
    );
    paint(&mut engine, &mut state, button);
    let frame = published(&mut state, top);
    assert_eq!(
        frame_pixel(&frame, ix, iy),
        BTNFACE,
        "the release restores the released face"
    );
}

/// The pressed face is genuinely DARKER than the released one, so the test
/// above cannot be satisfied by a palette that paints both states the same
/// shade (a swapped `COLOR_BTNFACE_PRESSED`/`COLOR_BTNFACE` in the partial
/// branch would otherwise still leave the released assertions intact on a
/// control whose first paint never ran).
#[test]
fn the_two_faces_are_distinct_colours() {
    assert_ne!(BTNFACE, BTNFACE_PRESSED);
    let blue = |c: u32| c & 0xFF;
    assert!(
        blue(BTNFACE_PRESSED) < blue(BTNFACE),
        "the pressed face must be the darker one ({BTNFACE_PRESSED:#x} vs {BTNFACE:#x})"
    );
}

/// The face rect a press marks is the INTERIOR — the 1 px border is excluded,
/// which is why the border survives a press repaint byte-for-byte. Pins the
/// geometry `button_invalidate_pressed` computes (relocated with the rest of
/// the rect-scope machinery into `paint.rs`).
#[test]
fn a_press_marks_the_face_interior_not_the_border() {
    let mut engine = super::test_engine();
    let (mut state, top, button) = button();
    paint(&mut engine, &mut state, button);
    let before = published(&mut state, top);

    send(
        &mut engine,
        &mut state,
        button,
        WinMsg::BM_SETSTATE.as_u32(),
        1,
        0,
    );
    assert_eq!(
        pending_label_invalid(&state, button),
        crate::user32::controls::LabelInvalidation::Rect(
            crate::user32::controls::LabelInvalidRect {
                rect: IRect::from_xywh(1, 1, WIDTH - 2, HEIGHT - 2),
                width: WIDTH,
                height: HEIGHT,
            }
        ),
        "a press marks exactly the interior inside the 1 px border"
    );
    paint(&mut engine, &mut state, button);
    let after = published(&mut state, top);

    // Every border pixel must be untouched: the erase rect excludes them and
    // `rect_touches_border` is false for an interior rect, so nothing re-strokes
    // the border either.
    let mut changed = 0;
    for x in X..X + WIDTH {
        for y in [Y, Y + HEIGHT - 1] {
            if frame_pixel(&before, x, y) != frame_pixel(&after, x, y) {
                changed += 1;
            }
        }
    }
    for y in Y..Y + HEIGHT {
        for x in [X, X + WIDTH - 1] {
            if frame_pixel(&before, x, y) != frame_pixel(&after, x, y) {
                changed += 1;
            }
        }
    }
    assert_eq!(
        changed, 0,
        "a press repaint must leave the 1 px border byte-identical"
    );
    // …and the interior did change, so the assertion above is not vacuous.
    let mut interior_changed = 0;
    for y in Y + 1..Y + HEIGHT - 1 {
        for x in X + 1..X + WIDTH - 1 {
            if frame_pixel(&before, x, y) != frame_pixel(&after, x, y) {
                interior_changed += 1;
            }
        }
    }
    assert!(
        interior_changed > 0,
        "a press repaint must actually repaint the face"
    );
}

/// A pressed button's caption shifts one px down/right (the classic 3D
/// look, `static.rs::paint_label`'s `pressed` branch) — so the caption ink
/// MOVES inside the erased interior. Pinned here because the shift is the only
/// thing distinguishing the two `paint_label` calls in the BUTTON arm, and a
/// face-only change would make this branch dead code.
#[test]
fn a_pressed_caption_shifts_one_pixel_inside_the_face() {
    let mut engine = super::test_engine();
    let (mut state, top, button) = button();
    paint(&mut engine, &mut state, button);
    let released = published(&mut state, top);

    send(
        &mut engine,
        &mut state,
        button,
        WinMsg::BM_SETSTATE.as_u32(),
        1,
        0,
    );
    paint(&mut engine, &mut state, button);
    let pressed = published(&mut state, top);

    // The ink's bounding box moves by (+1, +1): compare the first and last ink
    // columns/rows inside the face.
    let ink_bounds = |frame: &crate::present::SurfaceFrame| {
        let mut min_x = None;
        let mut max_x = None;
        let mut min_y = None;
        let mut max_y = None;
        for y in Y + 1..Y + HEIGHT - 1 {
            for x in X + 1..X + WIDTH - 1 {
                if frame_pixel(frame, x, y) == INK {
                    min_x = Some(min_x.map_or(x, |v: i32| v.min(x)));
                    max_x = Some(max_x.map_or(x, |v: i32| v.max(x)));
                    min_y = Some(min_y.map_or(y, |v: i32| v.min(y)));
                    max_y = Some(max_y.map_or(y, |v: i32| v.max(y)));
                }
            }
        }
        (min_x, min_y, max_x, max_y)
    };
    let before = ink_bounds(&released);
    let after = ink_bounds(&pressed);
    assert!(
        before.0.is_some(),
        "the caption must render ink when released"
    );
    assert!(
        after.0 == before.0.map(|v| v + 1) && after.1 == before.1.map(|v| v + 1),
        "the pressed caption must shift one px down/right: released {before:?} vs pressed {after:?}"
    );
}

/// `paint_control` refuses to paint a hidden control — a hidden window's
/// invalid region is discarded by Windows, and the dispatch arm already gates
/// on visibility, so this is the defense-in-depth check in the relocated
/// entry point. A just-hidden button must leave the surface alone (the
/// "hidden status bar repaints over the control that grew into its space"
/// regression, generalized).
#[test]
fn a_hidden_control_does_not_paint() {
    let mut engine = super::test_engine();
    let (mut state, top, button) = button();
    paint(&mut engine, &mut state, button);

    // Hide it, then scribble the sentinel over its whole rect so any paint is
    // unmistakable.
    for window in &mut state.window_state().windows {
        if window.handle.as_u64() == button {
            window.visible = false;
        }
    }
    super::sentinel(&mut state, top, IRect::from_xywh(X, Y, WIDTH, HEIGHT));

    // WM_PAINT through the dispatch: the arm skips `paint_control` entirely for
    // an invisible window. A skipped paint publishes nothing, so the LIVE
    // surface (not the stale published frame) is what witnesses it.
    paint(&mut engine, &mut state, button);
    assert_eq!(
        super::surface_pixel(&mut state, top, X + 5, Y + 5),
        super::SENTINEL,
        "the dispatch arm must not paint a hidden control"
    );

    // The guard inside `paint_control` (the relocated entry point) is the
    // second line of defence: call it directly and the sentinel must survive.
    crate::user32::controls::dispatch_control_proc_host_default(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("hidden paint is not an error");
    assert_eq!(
        super::surface_pixel(&mut state, top, X + 5, Y + 5),
        super::SENTINEL,
        "paint_control itself must refuse a hidden control"
    );
    // The `invalidated` flag is cleared by the dispatch either way, so a
    // hidden control cannot re-enter the paint cycle every idle drain.
    let still_invalidated = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle.as_u64() == button)
        .map(|w| w.invalidated)
        .expect("button record");
    assert!(
        !still_invalidated,
        "the hidden control's invalidated flag must be consumed"
    );
}
