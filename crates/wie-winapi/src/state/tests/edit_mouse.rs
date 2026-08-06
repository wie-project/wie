//! EDIT mouse tests: char-index-at-point mapping, caret placement, drag selection, double-click word selection, and click-to-scroll into view.
use super::*;

// ── Task 2.5: EDIT mouse caret placement, drag selection, double-click ──

/// The client x of char `index`'s glyph-cell start — the 2 px left margin
/// plus the summed advances of the preceding characters (the paint's caret x).
fn char_cell_left(state: &mut WinApiState, hwnd: u64, index: usize) -> i32 {
    let text = control_text(state, hwnd);
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = crate::gdi32::FontKey::default();
    let resolved = font_engine.resolve(&default_key, 16).expect("default font");
    let x = 2_i32.saturating_add(font_engine.text_advance(&resolved, &default_key, &text, index));
    state.gdi_state().font_engine = font_engine;
    x
}

/// The char index the EDIT hit-test resolves for a client point, computed
/// with the real default font — the same metrics the dispatch handlers use,
/// so dispatch-level assertions track the paint exactly. `wrap` must match
/// the fixture's style (single-line edits: false; the ES_MULTILINE fixture
/// without WS_HSCROLL wraps).
fn edit_char_at(state: &mut WinApiState, hwnd: u64, x: i32, y: i32, wrap: bool) -> usize {
    use crate::user32::controls::HitTestLayout;
    use crate::user32::controls::edit_char_index_at_point;
    let (text, width) = {
        let ws = state.try_window_state().expect("window state");
        let w = ws
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
            .expect("edit record");
        (w.control_text.clone(), w.width)
    };
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let default_key = crate::gdi32::FontKey::default();
    let resolved = font_engine.resolve(&default_key, 16).expect("default font");
    let line_h = resolved.line_height();
    let advance = &mut |ch: char| font_engine.char_advance(&resolved, &default_key, ch);
    let index = edit_char_index_at_point(
        &text,
        x,
        y,
        &HitTestLayout {
            wrap_width: width.saturating_sub(4),
            line_height: line_h,
            first_visible: 0,
            wrap,
            alignment: 0,
        },
        advance,
    );
    state.gdi_state().font_engine = font_engine;
    index
}

#[test]
fn test_edit_char_index_at_point_maps_x_to_char_cells() {
    use crate::user32::controls::HitTestLayout;
    use crate::user32::controls::edit_char_index_at_point;
    // 8 px/char, no wrap, left-aligned: char i occupies the cell [8i, 8i+8);
    // the half-advance boundary puts the caret before a char when the click
    // is in its left half and after it in the right half (the paint's caret x
    // is the summed advance of the preceding chars — this is the inverse).
    let idx = |x: i32| {
        edit_char_index_at_point(
            "hello world",
            x,
            0,
            &HitTestLayout {
                wrap_width: 80,
                line_height: 16,
                first_visible: 0,
                wrap: false,
                alignment: 0,
            },
            &mut |_| 8_i32,
        )
    };
    assert_eq!(idx(0), 0, "left margin → before the first char");
    assert_eq!(idx(6), 1, "right half of 'h' → after 'h'");
    assert_eq!(idx(10), 1, "left half of 'e' → before 'e'");
    assert_eq!(idx(14), 2, "right half of 'e' → after 'e'");
    // "hello world" is 11 chars = 88 px; a click past the last glyph clamps
    // to the end of the text.
    assert_eq!(idx(90), 11, "past the last glyph → end of text");
    assert_eq!(idx(-5), 0, "left of the text → before the first char");
}

#[test]
fn test_edit_char_index_at_point_maps_wrapped_visual_rows() {
    use crate::user32::controls::HitTestLayout;
    use crate::user32::controls::edit_char_index_at_point;
    // 8 px/char in a 32 px column → 4 chars per visual row: "abcdef" wraps to
    // "abcd" at y=0 and "ef" at y=16 (the same layout the paint draws).
    let idx = |x: i32, y: i32| {
        edit_char_index_at_point(
            "abcdef",
            x,
            y,
            &HitTestLayout {
                wrap_width: 32,
                line_height: 16,
                first_visible: 0,
                wrap: true,
                alignment: 0,
            },
            &mut |_| 8_i32,
        )
    };
    // Row 0 ("abcd", chars 0..4): the half-advance boundary applies within
    // the row-local cell.
    assert_eq!(idx(2, 0), 0, "first row, left half of 'a'");
    assert_eq!(idx(6, 0), 1, "first row, right half of 'a'");
    // A click just past the wrap boundary (the end of 'd') lands at char 4 —
    // the start of the second visual row.
    assert_eq!(idx(34, 0), 4, "wrap boundary → 'e'");
    // Row 1 ("ef", chars 4..6) at y=16: the row for y is the visual row, and
    // the char index is the whole-text offset, not a row-local one.
    assert_eq!(idx(10, 16), 5, "second row, right half of 'e'");
    assert_eq!(idx(19, 16), 6, "second row, past 'f' → end of text");
    // A click above the first row clamps to it.
    assert_eq!(idx(2, -1), 0, "above the text → first row start");
}

#[test]
fn test_edit_mouse_down_places_caret_and_captures() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello", width 120, single-line

    let (x, y) = (20_i32, 10_i32);
    let expected = edit_char_at(&mut state, edit, x, y, false);
    let r = dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        u16::try_from(x).unwrap_or(0),
        u16::try_from(y).unwrap_or(0),
    );
    assert_eq!(r, 0);

    let ui = control_ui(&state, edit);
    assert_eq!(
        (ui.caret, ui.sel_start, ui.sel_end),
        (expected, expected, expected),
        "a click must collapse the selection at the hit-tested char"
    );
    assert!(ui.focused, "a click focuses the edit");
    assert_eq!(
        state.window_state().capture_window_handle,
        crate::handles::Hwnd::from(edit),
        "a pressed edit holds the mouse capture for the drag"
    );
}

#[test]
fn test_edit_mouse_drag_extends_selection_from_anchor() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello"

    let y = 10_u16;
    let anchor = 2_usize;
    let down_x = u16::try_from(char_cell_left(&mut state, edit, anchor)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        down_x,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (2, 2, 2));

    // Drag right to char 5: selection [anchor, current], caret at current.
    let x5 = u16::try_from(char_cell_left(&mut state, edit, 5)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x5,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (2, 5, 5));

    // Drag back past the anchor to char 1: the anchor stays at 2.
    let x1 = u16::try_from(char_cell_left(&mut state, edit, 1)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x1,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (1, 2, 1));

    // ...and forward again to char 4: still anchored at 2.
    let x4 = u16::try_from(char_cell_left(&mut state, edit, 4)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x4,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (2, 4, 4));

    // Release: the capture drops, the selection stays finalized, and the
    // completed click delivers EN_VSCROLL to the parent.
    release_mouse(&mut engine, &mut state, edit, x4, y);
    assert_eq!(
        state.window_state().capture_window_handle,
        crate::handles::Hwnd::NULL,
        "button-up must release the edit's mouse capture"
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end), (2, 4));

    // A hover move after the release must not touch the selection.
    let x3 = u16::try_from(char_cell_left(&mut state, edit, 3)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x3,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end), (2, 4), "hover must not select");
}

#[test]
fn test_edit_mouse_dblclk_selects_whitespace_word() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    state
        .window_state()
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record")
        .control_text = "hello world foo".to_owned();

    // Double-click inside "world" (chars 6..11): the whole word selects and
    // the caret lands at its end.
    let x = u16::try_from(char_cell_left(&mut state, edit, 8)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDBLCLK.as_u32(),
        x,
        10,
    );
    let ui = control_ui(&state, edit);
    assert_eq!(
        (ui.sel_start, ui.sel_end, ui.caret),
        (6, 11, 11),
        "double-click must select the whole word under the click"
    );
    assert_eq!(
        state.window_state().capture_window_handle,
        crate::handles::Hwnd::from(edit),
        "the dblclk press also captures for a subsequent drag"
    );

    // A double-click on the whitespace between words (char 5) selects nothing.
    let x = u16::try_from(char_cell_left(&mut state, edit, 5)).unwrap_or(0);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDBLCLK.as_u32(),
        x,
        10,
    );
    let ui = control_ui(&state, edit);
    assert_eq!(
        (ui.sel_start, ui.sel_end),
        (5, 5),
        "a double-click on whitespace must not select a word"
    );
}

#[test]
fn test_edit_mouse_click_in_wrapped_text_selects_the_visual_row() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, &"m".repeat(40)); // wraps
    let line_h = {
        let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
        let line_h = font_engine
            .resolve(&crate::gdi32::FontKey::default(), 16)
            .map_or(16, |f| f.line_height());
        state.gdi_state().font_engine = font_engine;
        line_h
    };

    // A click in the SECOND visual row (below the first wrap row) must land
    // on a later char than the same x in the first row.
    let x = 5_i32;
    let first_row = edit_char_at(&mut state, edit, x, line_h / 2, true);
    let second_row = edit_char_at(&mut state, edit, x, line_h + line_h / 2, true);
    assert!(
        second_row > first_row,
        "the wrapped second row must hold later chars ({second_row} > {first_row})"
    );

    let r = dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        u16::try_from(x).unwrap_or(0),
        u16::try_from(line_h + line_h / 2).unwrap_or(0),
    );
    assert_eq!(r, 0);
    let ui = control_ui(&state, edit);
    assert_eq!(
        (ui.caret, ui.sel_start, ui.sel_end),
        (second_row, second_row, second_row),
        "the click caret must land on the hit-tested visual-row char"
    );
}

/// A multiline EDIT whose client shows `visible` full rows plus a `partial` px
/// strip: the partial strip is the only in-client region that maps to a row
/// past the viewport (the first pixels of the row after the last visible one),
/// so a click there exercises the click→scroll-caret handoff.
fn set_edit_viewport_with_partial_strip(
    state: &mut WinApiState,
    hwnd: u64,
    visible: usize,
    partial: i32,
) {
    set_edit_visible_rows(state, hwnd, visible);
    let ws = state.window_state();
    if let Some(w) = ws
        .windows
        .iter_mut()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
    {
        w.height = w.height.saturating_add(partial);
    }
}

#[test]
fn test_edit_mouse_click_scrolls_caret_row_into_view() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "aaaa\nbbbb\ncccc\ndddd\neeee\nffff");
    // 3 full rows + a 4 px partial strip: a click in the strip's first pixel
    // lands on visual row 3 (off-screen) and must scroll it onto the last
    // visible row.
    set_edit_viewport_with_partial_strip(&mut state, edit, 3, 4);
    let height = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record")
        .height;
    let y = u16::try_from(height.saturating_sub(4)).unwrap_or(0);

    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        4,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!(
        ui.first_visible_line, 1,
        "a click on the off-viewport row must scroll it into view"
    );
    assert_eq!(
        (ui.caret, ui.sel_start, ui.sel_end),
        (15, 15, 15),
        "the click caret lands at the start of the clicked row ('dddd')"
    );
}

#[test]
fn test_edit_mouse_dblclk_scrolls_word_row_into_view() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "aaaa\nbbbb\ncccc\ndddd\neeee\nffff");
    set_edit_viewport_with_partial_strip(&mut state, edit, 3, 4);
    let height = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record")
        .height;
    let y = u16::try_from(height.saturating_sub(4)).unwrap_or(0);

    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDBLCLK.as_u32(),
        4,
        y,
    );
    let ui = control_ui(&state, edit);
    assert_eq!(
        ui.first_visible_line, 1,
        "a double-click on the off-viewport row must scroll the word into view"
    );
    assert_eq!(
        (ui.sel_start, ui.sel_end, ui.caret),
        (15, 19, 19),
        "the double-click selects the whole off-screen word"
    );
}

#[test]
fn test_edit_mouse_selection_fires_no_en_change() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    let original = control_text(&state, edit);

    let y = 10_u16;
    let x_a = u16::try_from(char_cell_left(&mut state, edit, 1)).unwrap_or(0);
    let x_b = u16::try_from(char_cell_left(&mut state, edit, 4)).unwrap_or(0);
    // A full click-drag-release cycle leaves the text and modify flag
    // untouched — EN_CHANGE fires only for text mutations. The release does
    // bridge EN_VSCROLL (the F5 status-bar caret refresh), which is a
    // navigation notification, not a text change.
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        x_a,
        y,
    );
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEMOVE.as_u32(),
        x_b,
        y,
    );
    release_mouse(&mut engine, &mut state, edit, x_b, y);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDBLCLK.as_u32(),
        x_a,
        y,
    );
    assert_eq!(
        control_text(&state, edit),
        original,
        "selection must not mutate the control text"
    );
    assert!(
        !control_ui(&state, edit).modified,
        "selection must not set EM_GETMODIFY"
    );
}
