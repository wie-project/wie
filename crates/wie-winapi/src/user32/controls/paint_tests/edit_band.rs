//! EDIT row-band invalidation: which rows the next paint erases and renders.
//!
//! The `EditInvalidation` band path is the row-level twin of the rect scope in
//! `paint.rs`; it lives in `edit/paint.rs` but its ERASE is driven from
//! `paint_control`'s EDIT arm (the `edit_dirty_band` → `fill_surface_rect_
//! above_clipped` handoff), which is exactly the code that moved. The
//! invariant pinned here is twofold and both halves are checked:
//!
//! * the band a mutation leaves pending is the rows it changed and no others,
//!   and
//! * the paint's erase covers exactly that band — proven with the sentinel,
//!   because "the rows outside the band are unchanged" is also what a paint
//!   that erased nothing would produce.

use super::{
    SENTINEL, WINDOW_WHITE, frame_pixel, line_height, paint, published, push_multiline_edit, send,
    sentinel,
};
use crate::gdi32::IRect;
use crate::user32::controls::{ControlState, EditInvalidRows, EditInvalidation};
use crate::user32::wm::WinMsg;

const X: i32 = 10;
const Y: i32 = 10;
const WIDTH: i32 = 120;
const HEIGHT: i32 = 60;
const LINE_H: i32 = 16;

/// `CARET_TIMER_ID` — the wParam the edit module arms its blink timer with.
const CARET_TIMER: u64 = 1;

fn invalid_rows(state: &mut crate::WinApiState, hwnd: u64) -> EditInvalidation {
    match state
        .window_state()
        .control_states
        .get(&crate::handles::Hwnd::from(hwnd))
    {
        Some(ControlState::Edit { invalid_rows, .. }) => *invalid_rows,
        other => panic!("not an EDIT: {other:?}"),
    }
}

fn last_paint_rows(state: &mut crate::WinApiState, hwnd: u64) -> usize {
    match state
        .window_state()
        .control_states
        .get(&crate::handles::Hwnd::from(hwnd))
    {
        Some(ControlState::Edit {
            last_paint_rows, ..
        }) => *last_paint_rows,
        other => panic!("not an EDIT: {other:?}"),
    }
}

/// The pending band's row range, panicking when nothing is pending — a caller
/// that wants the range has asserted a band exists.
fn band_rows(state: &mut crate::WinApiState, hwnd: u64) -> (usize, usize) {
    match invalid_rows(state, hwnd) {
        EditInvalidation::Band(EditInvalidRows { lo, hi, .. }) => (lo, hi),
        other => panic!("expected a pending row band, got {other:?}"),
    }
}

/// Focus + the first (full) paint, and return `(top, edit)` — every band test
/// needs a painted control first, because `band_is_current` ignores a band
/// whose control has never painted.
fn focused_and_painted(engine: &mut dyn wie_cpu::CpuEngine) -> (crate::WinApiState, u64, u64) {
    let mut state = super::test_state();
    let (top, edit) = push_multiline_edit(&mut state, "alpha\nbeta\ngamma");
    send(engine, &mut state, edit, crate::user32::WM_SETFOCUS, 0, 0);
    paint(engine, &mut state, edit);
    (state, top, edit)
}

/// Place the caret at character `index` and consume whatever band that raised
/// with a paint, so the only pending mark a test inspects is the one the test
/// itself makes afterwards.
fn park_caret(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut crate::WinApiState,
    edit: u64,
    index: u64,
) {
    send(
        engine,
        state,
        edit,
        WinMsg::EM_SETSEL.as_u32(),
        index,
        index,
    );
    paint(engine, state, edit);
    assert_eq!(
        invalid_rows(state, edit),
        EditInvalidation::Clean,
        "parking the caret must leave the control clean"
    );
}

fn type_char(
    engine: &mut dyn wie_cpu::CpuEngine,
    state: &mut crate::WinApiState,
    edit: u64,
    c: char,
) {
    send(
        engine,
        state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from(c)),
        0,
    );
}

/// The first paint covers every visible row and leaves the control clean.
#[test]
fn the_first_paint_covers_every_visible_row() {
    let mut engine = super::test_engine();
    let (mut state, _top, edit) = focused_and_painted(&mut engine);
    assert_eq!(
        last_paint_rows(&mut state, edit),
        3,
        "a 60 px multiline client at a 16 px row pitch shows three rows"
    );
    assert_eq!(
        invalid_rows(&mut state, edit),
        EditInvalidation::Clean,
        "the paint consumes the pending band"
    );
}

/// Typing one character on visual row 1 dirties EXACTLY row 1 — no full
/// repaint, and none of the sibling rows.
#[test]
fn typing_on_one_row_does_not_dirty_the_others() {
    let mut engine = super::test_engine();
    let (mut state, _top, edit) = focused_and_painted(&mut engine);
    assert_eq!(line_height(&mut state, edit), LINE_H);

    // Character 6 is the start of "beta" = visual row 1.
    park_caret(&mut engine, &mut state, edit, 6);
    type_char(&mut engine, &mut state, edit, 'X');
    assert_eq!(
        band_rows(&mut state, edit),
        (1, 1),
        "typing on visual row 1 must dirty exactly row 1"
    );

    paint(&mut engine, &mut state, edit);
    assert_eq!(
        last_paint_rows(&mut state, edit),
        1,
        "the band paint renders one row"
    );
}

/// The pixel gate of the band path: the paint's erase covers exactly the band,
/// so every interior pixel outside it still holds the sentinel the fixture
/// wrote before the repaint. The sentinel is what makes this non-vacuous — a
/// paint that erased nothing would look identical to a correct one under a
/// plain before/after frame comparison.
#[test]
fn a_band_repaint_erases_only_its_rows() {
    let mut engine = super::test_engine();
    let (mut state, top, edit) = focused_and_painted(&mut engine);
    // Park the caret FIRST: EM_SETSEL's own paint is part of the fixture, not
    // of the repaint under test, so the baseline has to be taken after it.
    park_caret(&mut engine, &mut state, edit, 6);

    type_char(&mut engine, &mut state, edit, 'X');
    assert_eq!(band_rows(&mut state, edit), (1, 1));
    // Foreign content over the control's INTERIOR, so the erase scope is
    // visible. The 1 px border is excluded deliberately: `paint_control` calls
    // the full `stroke_border` on every EDIT paint (band-limited or not), so
    // those columns are rewritten by any paint and cannot witness one.
    sentinel(
        &mut state,
        top,
        IRect::from_xywh(X + 1, Y + 1, WIDTH - 2, HEIGHT - 2),
    );
    paint(&mut engine, &mut state, edit);
    let after = published(&mut state, top);

    assert_eq!(
        last_paint_rows(&mut state, edit),
        1,
        "a single-row band repaints exactly one row"
    );
    assert_eq!(
        invalid_rows(&mut state, edit),
        EditInvalidation::Clean,
        "the paint consumes the band"
    );

    let band_top = Y + LINE_H;
    let band_bottom = Y + 2 * LINE_H;
    // The interior only: `paint_control` re-strokes the EDIT's whole 1 px
    // border on every paint, so the border columns cannot witness a band scope.
    for y in Y + 1..Y + HEIGHT - 1 {
        for x in X + 1..X + WIDTH - 1 {
            let pixel = frame_pixel(&after, x, y);
            if (band_top..band_bottom).contains(&y) {
                assert_ne!(
                    pixel, SENTINEL,
                    "row-band pixel ({x},{y}) must have been erased and repainted"
                );
            } else {
                assert_eq!(
                    pixel, SENTINEL,
                    "a pixel outside the dirty band ({x},{y}) must be untouched by the band repaint"
                );
            }
        }
    }
}

/// The band is expressed in VISUAL rows and the erase is applied at the
/// control's surface offset: row 1 of an EDIT at (10, 10) is surface rows
/// y 26..42. If either the offset or the row pitch were dropped, the erase
/// would land on row 0 and this catches it.
#[test]
fn the_erase_band_lands_at_the_controls_offset() {
    let mut engine = super::test_engine();
    let (mut state, top, edit) = focused_and_painted(&mut engine);
    park_caret(&mut engine, &mut state, edit, 6);
    // Sentinel AFTER the caret park: parking the caret triggers its own paint,
    // which would erase the sentinel before the repaint under test even runs.
    // Interior only — see `a_band_repaint_erases_only_its_rows`.
    sentinel(
        &mut state,
        top,
        IRect::from_xywh(X + 1, Y + 1, WIDTH - 2, HEIGHT - 2),
    );

    type_char(&mut engine, &mut state, edit, 'X');
    paint(&mut engine, &mut state, edit);
    let frame = published(&mut state, top);

    // Row 1's band was erased to COLOR_WINDOW and then the typed glyph (and the
    // caret bar) were drawn on top. The rasterizer is anti-aliased, so "ink"
    // means "darker than the white fill", not one exact colour.
    let mut white = 0;
    let mut ink = 0;
    for y in Y + LINE_H..Y + 2 * LINE_H {
        for x in X + 30..X + WIDTH {
            let pixel = frame_pixel(&frame, x, y);
            if pixel == WINDOW_WHITE {
                white += 1;
            } else if (pixel & 0xFF) < 0xFF {
                ink += 1;
            } else {
                panic!("unexpected pixel {pixel:#x} in the erased band at ({x},{y})");
            }
        }
    }
    assert!(
        white > 0 && ink > 0,
        "row 1 must be erased to COLOR_WINDOW and carry the typed glyph (white {white}, ink {ink})"
    );
    // Row 0's interior still holds the sentinel: it was outside the band. (The
    // 1 px border columns are excluded — `paint_control` re-strokes the whole
    // EDIT border on every paint.)
    for y in Y + 1..Y + LINE_H {
        for x in X + 1..X + WIDTH - 1 {
            assert_eq!(
                frame_pixel(&frame, x, y),
                SENTINEL,
                "row 0 must not be erased when only row 1 is dirty ({x},{y})"
            );
        }
    }
}

/// A caret blink dirties the row that last held the caret bar AND the caret's
/// current row — otherwise the old bar is stranded on the surface (the
/// stuck/ghost caret). With the caret still on row 0 a tick is a one-row band.
#[test]
fn a_caret_blink_on_a_moved_caret_dirties_both_rows() {
    let mut engine = super::test_engine();
    let (mut state, _top, edit) = focused_and_painted(&mut engine);

    // Move the caret down one row with the arrow key. The key delivers
    // EN_VSCROLL (the runtime's status-bar caret refresh) — an expected
    // control signal, not a failure.
    let moved = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        WinMsg::WM_KEYDOWN.as_u32(),
        crate::user32::VK_DOWN,
        0,
    );
    if let Err(error) = moved {
        let signal = error
            .downcast_ref::<crate::user32::WinApiControlSignal>()
            .unwrap_or_else(|| panic!("the arrow key must deliver EN_VSCROLL: {error:?}"));
        assert!(
            matches!(
                signal,
                crate::user32::WinApiControlSignal::GuestCallbackRequested { .. }
            ),
            "unexpected control signal: {signal:?}"
        );
    }
    let (lo, hi) = band_rows(&mut state, edit);
    assert_eq!(
        (lo, hi),
        (0, 1),
        "moving the caret from row 0 to row 1 dirties both rows"
    );

    // A blink tick on top of that keeps the two-row scope (it must not narrow
    // it to the caret's new row — the old row still holds the bar being
    // hidden).
    send(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        CARET_TIMER,
        0,
    );
    assert_eq!(
        band_rows(&mut state, edit),
        (0, 1),
        "the blink must not narrow the pending band"
    );
    paint(&mut engine, &mut state, edit);
    assert_eq!(
        last_paint_rows(&mut state, edit),
        2,
        "the two-row band repaints two rows"
    );
}

/// The band machinery's staleness rule: a band computed at a different wrap
/// width is ignored in favour of a FULL erase, because a resize reflowed the
/// rows underneath it. A stale band that were honoured would leave the reflowed
/// rows blank.
#[test]
fn a_stale_band_escalates_to_a_full_erase() {
    let mut engine = super::test_engine();
    let (mut state, top, edit) = focused_and_painted(&mut engine);

    // Plant a band for row 2 stamped with a wrap width the current layout does
    // not use.
    {
        let control = state
            .window_state()
            .control_states
            .get_mut(&crate::handles::Hwnd::from(edit))
            .expect("edit state");
        let ControlState::Edit { invalid_rows, .. } = control else {
            panic!("edit state");
        };
        *invalid_rows = EditInvalidation::Band(EditInvalidRows {
            lo: 2,
            hi: 2,
            wrap_width: 1,
        });
    }
    sentinel(&mut state, top, IRect::from_xywh(X, Y, WIDTH, HEIGHT));
    paint(&mut engine, &mut state, edit);
    let frame = published(&mut state, top);

    assert_eq!(
        last_paint_rows(&mut state, edit),
        3,
        "a stale band must fall back to a full repaint"
    );
    for y in Y + 1..Y + HEIGHT - 1 {
        for x in X + 1..X + WIDTH - 1 {
            assert_ne!(
                frame_pixel(&frame, x, y),
                SENTINEL,
                "the full fallback erase must cover every row ({x},{y})"
            );
        }
    }
}

/// A band that is only honoured after a first paint: a control that has never
/// painted has undefined content behind it, so a band planted before the first
/// paint must escalate to a full erase (otherwise the untouched rows stay
/// blank — the "first paint leaves holes" regression).
#[test]
fn a_band_before_the_first_paint_escalates_to_full() {
    let mut engine = super::test_engine();
    let mut state = super::test_state();
    let (_top, edit) = push_multiline_edit(&mut state, "alpha\nbeta\ngamma");
    {
        let control = state
            .window_state()
            .control_states
            .entry(crate::handles::Hwnd::from(edit))
            .or_insert_with(|| crate::user32::controls::ControlClassKind::Edit.new_state());
        let ControlState::Edit { invalid_rows, .. } = control else {
            panic!("edit state");
        };
        *invalid_rows = EditInvalidation::Band(EditInvalidRows {
            lo: 0,
            hi: 0,
            // The width a 120 px client wraps at (120 − 4 px gutter padding).
            wrap_width: WIDTH - 4,
        });
    }
    send(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    );
    paint(&mut engine, &mut state, edit);
    assert_eq!(
        last_paint_rows(&mut state, edit),
        3,
        "a never-painted EDIT must repaint every row even with a current-width band pending"
    );
}
