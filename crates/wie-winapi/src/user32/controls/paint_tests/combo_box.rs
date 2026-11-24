//! COMBOBOX painting.
//!
//! The relocated `paint.rs` COMBOBOX arm is three lines — paint the released
//! face + border, render the FIRST item's text, done — and it has no direct
//! coverage. Two things are worth pinning:
//!
//! * an EMPTY item list must render (the face + border) instead of panicking
//!   on `items.first().map_or("", …)`'s empty side, and
//! * the caption is the FIRST item's text, NOT the selected one — a combo with
//!   a selection of 2 and no items, or with a selection pointing at a
//!   different item, still shows item 0.
//!
//! NOTE: there is no dropdown state in WIE. `ControlState::ComboBox` carries
//! only `items` + `sel_index` — no "dropped/open" flag, no drop-list window,
//! and no drop-arrow chrome in the paint — so the only paintable state is the
//! closed one and that is what these tests cover. A future dropdown
//! implementation has to extend this file, not assume it was covered.

use super::{
    BTNFACE, BTNSHADOW, count_off_fill, frame_pixel, guest_ansi, paint, published, push_control,
    send,
};
use crate::gdi32::IRect;
use crate::user32::wm::WinMsg;

const X: i32 = 10;
const Y: i32 = 10;
const WIDTH: i32 = 120;
const HEIGHT: i32 = 24;

fn combo() -> (crate::WinApiState, u64, u64) {
    let mut state = super::test_state();
    let (top, combo) = push_control(&mut state, "COMBOBOX", (X, Y, WIDTH, HEIGHT), "");
    (state, top, combo)
}

/// An empty COMBOBOX paints the released face inside a `COLOR_BTNSHADOW`
/// border and does not panic. `items.first()` on an empty list is exactly the
/// edge the relocated arm can get wrong.
#[test]
fn an_empty_combo_paints_its_face_without_panicking() {
    let mut engine = super::test_engine();
    let (mut state, top, combo) = combo();
    assert_eq!(
        send(
            &mut engine,
            &mut state,
            combo,
            WinMsg::CB_GETCOUNT.as_u32(),
            0,
            0
        ),
        0,
        "the fixture starts with no items"
    );
    paint(&mut engine, &mut state, combo);
    let frame = published(&mut state, top);

    assert_eq!(
        frame_pixel(&frame, X + 60, Y + 12),
        BTNFACE,
        "the closed combo's interior is COLOR_BTNFACE"
    );
    assert_eq!(
        frame_pixel(&frame, X, Y),
        BTNSHADOW,
        "the combo draws the same 1 px BTNSHADOW border as a button"
    );
    assert_eq!(
        count_off_fill(
            &frame,
            IRect::from_xywh(X + 1, Y + 1, WIDTH - 2, HEIGHT - 2),
            BTNFACE
        ),
        0,
        "an empty combo renders nothing on its face"
    );
}

/// The combo's caption is the FIRST item — a combo with items but no
/// selection shows item 0, and a selection pointing elsewhere does NOT change
/// what the closed face shows. This is the relocated arm's actual behaviour and
/// it is deliberately not "the selected item" (WIE draws item 0 regardless).
#[test]
fn the_combo_shows_the_selected_item_not_the_first_one() {
    let mut engine = super::test_engine();
    let (mut state, top, combo) = combo();
    // Deliberately unequal lengths: the assertion below compares ink footprint,
    // which is only decisive when the two captions cannot render alike.
    for text in ["a", "bbbbbbbb", "c"] {
        let address = guest_ansi(&mut engine, text);
        send(
            &mut engine,
            &mut state,
            combo,
            WinMsg::CB_ADDSTRING.as_u32(),
            0,
            address,
        );
    }
    paint(&mut engine, &mut state, combo);
    let frame = published(&mut state, top);

    let interior = IRect::from_xywh(X + 1, Y + 1, WIDTH - 2, HEIGHT - 2);
    let ink_before = count_off_fill(&frame, interior, BTNFACE);
    assert!(
        ink_before > 0,
        "a combo with items renders the first item's caption"
    );
    // The caption starts at the 4 px left padding, so ink must appear in the
    // left half of the face — not centred, not at the right edge.
    let left_half = IRect::from_xywh(X + 1, Y + 1, 40, HEIGHT - 2);
    assert!(
        count_off_fill(&frame, left_half, BTNFACE) > 0,
        "the combo caption is drawn at the 4 px left padding (left-aligned)"
    );

    // Selecting item 1 ("bbbbbbbb") must repaint the caption with *that*
    // item's text. Index 1, not 2 — index 2 is "c", also one character, so it
    // could not be told apart from the initial caption by footprint.
    // WIE previously painted `items.first()` unconditionally, so a combo whose
    // selection was index 2 still displayed item 0 — a divergence from real
    // Windows, where a combo shows the selected item.
    send(
        &mut engine,
        &mut state,
        combo,
        WinMsg::CB_SETCURSEL.as_u32(),
        1,
        0,
    );
    paint(&mut engine, &mut state, combo);
    let after = published(&mut state, top);

    assert!(
        count_off_fill(&after, interior, BTNFACE) > ink_before,
        "CB_SETCURSEL must repaint the combo's caption with the selected item, \
         not the first (a 1-char caption inked {ink_before} px, the selected \
         8-char one must ink more)"
    );
    assert!(
        count_off_fill(&after, interior, BTNFACE) > 0,
        "the selected item's caption still renders"
    );
    // Bottom-right of the face: a left-aligned caption starting at the 4 px
    // padding cannot reach here, at any caption length. (Sampling mid-face at a
    // fixed x is not safe — a long selected item legitimately inks that pixel.)
    assert_eq!(
        frame_pixel(&after, X + WIDTH - 5, Y + HEIGHT - 5),
        BTNFACE,
        "the face is still COLOR_BTNFACE with items present"
    );
}

/// A combo's face survives a repaint after an item append — the `CB_ADDSTRING`
/// arm marks the window and republishes through the same `paint_control` entry
/// point, and a combo whose items list is non-empty must still erase its whole
/// client (it has no row-band scope, so the dirty rect is always the full
/// client).
#[test]
fn adding_an_item_repaints_the_whole_combo_face() {
    let mut engine = super::test_engine();
    let (mut state, top, combo) = combo();
    let first = guest_ansi(&mut engine, "alpha");
    send(
        &mut engine,
        &mut state,
        combo,
        WinMsg::CB_ADDSTRING.as_u32(),
        0,
        first,
    );
    paint(&mut engine, &mut state, combo);
    super::sentinel(&mut state, top, IRect::from_xywh(X, Y, WIDTH, HEIGHT));
    let second = guest_ansi(&mut engine, "beta");
    send(
        &mut engine,
        &mut state,
        combo,
        WinMsg::CB_ADDSTRING.as_u32(),
        0,
        second,
    );
    paint(&mut engine, &mut state, combo);
    let frame = published(&mut state, top);

    // No pixel of the client is left as the sentinel: the combo has no rect
    // scope, so its erase always covers the whole client regardless of what
    // changed. (The pixel VALUES inside the client vary — the border, the face,
    // and anti-aliased glyph ink — so the assertion is about coverage.)
    for y in Y..Y + HEIGHT {
        for x in X..X + WIDTH {
            assert_ne!(
                frame_pixel(&frame, x, y),
                super::SENTINEL,
                "the combo erase must cover every pixel of the client ({x},{y})"
            );
        }
    }
    assert_eq!(
        frame_pixel(&frame, X + 100, Y + 3),
        BTNFACE,
        "the erased face is COLOR_BTNFACE where no glyph ink landed"
    );
}
