//! Core EDIT message tests: EM_SETSEL / EM_GETSEL packing, WM_CHAR insertion, selection replacement, backspace / delete, arrow keys, multiline keyboard navigation, and the multiline text model + EM_LINEFROMCHAR family.
use super::*;

#[test]
fn test_edit_em_setsel_getsel_packing() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // EM_SETSEL(2, 4) → selection [2, 4), caret at the end edge (4).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        2,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_SETSEL returns TRUE");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (2, 4, 4));

    // EM_GETSEL (no pointers) returns MAKELONG(start, end) = 2 | 4 << 16.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETSEL,
        0,
        0,
    )
    .expect("getsel ok")
    .expect("some result");
    assert_eq!(r, 0x0004_0002, "EM_GETSEL packs MAKELONG(start, end)");
}

#[test]
fn test_edit_em_setsel_negative_selects_all() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // EM_SETSEL(0, -1) → select everything: [0, len).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        u64::from(u32::MAX),
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETSEL,
        0,
        0,
    )
    .expect("getsel ok")
    .expect("some result");
    assert_eq!(r, 0x0005_0000, "select-all = MAKELONG(0, 5)");
}

#[test]
fn test_edit_em_getsel_writes_output_pointers() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        3,
    )
    .expect("setsel ok")
    .expect("some result");

    // EM_GETSEL with output pointers at 0x3000 (start) / 0x3004 (end).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETSEL,
        0x3000,
        0x3004,
    )
    .expect("getsel ok")
    .expect("some result");
    let mut start = [0u8; 4];
    let mut end = [0u8; 4];
    engine.mem_read(0x3000, &mut start).expect("read start");
    engine.mem_read(0x3004, &mut end).expect("read end");
    assert_eq!(
        u32::from_le_bytes(start),
        1,
        "wParam pointer receives start"
    );
    assert_eq!(u32::from_le_bytes(end), 3, "lParam pointer receives end");
}

#[test]
fn test_edit_wm_char_inserts_at_caret() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Type 'X' at caret 0 → "Xhello", caret advances to 1.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("WM_CHAR that mutates the text delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "Xhello");
    assert_eq!(control_ui(&state, edit).caret, 1);

    // Home + End, then 'Y' appends at the end → "XhelloY".
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('Y')),
        0,
    )
    .expect_err("WM_CHAR that mutates the text delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "XhelloY");
    assert_eq!(control_ui(&state, edit).caret, 7);

    // Enter does not change the text and delivers nothing.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        0x0D,
        0,
    )
    .expect("enter ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, edit), "XhelloY");
}

#[test]
fn test_edit_char_replaces_selection_and_delivers_en_change() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_edit_pair(&mut state);

    // Select "ell" (chars 1..4) of "hello".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");

    // Typing 'X' replaces [1, 4) → "hXo", clears the selection.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    );
    let error = result.expect_err("WM_CHAR must deliver EN_CHANGE to the parent");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0300_000C
        ),
        "WM_CHAR must deliver WM_COMMAND(MAKEWPARAM(12, EN_CHANGE)), got {signal:?}"
    );

    assert_eq!(control_text(&state, edit), "hXo");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (2, 2, 2));
}

#[test]
fn test_edit_backspace_and_delete_at_caret() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Home + Right + Right (caret 2), Backspace → deletes the char before
    // the caret ('e', index 1) → "hllo", caret back at 1.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    assert_eq!(control_ui(&state, edit).caret, 2);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        0x08,
        0,
    )
    .expect_err("backspace delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hllo");
    assert_eq!(control_ui(&state, edit).caret, 1);

    // VK_DELETE at caret 1 deletes 'l' (index 1) → "hlo".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DELETE,
        0,
    )
    .expect_err("delete delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hlo");

    // Delete past the end: no text change, no notification.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DELETE,
        0,
    )
    .expect("delete at end ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, edit), "hlo");
}

#[test]
fn test_edit_arrow_keys_move_caret() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Home → caret 0.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 0);

    // Right → caret 1; End → caret 5 (len of "hello"); Left → 4.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    assert_eq!(control_ui(&state, edit).caret, 1);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 5);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_LEFT);
    assert_eq!(control_ui(&state, edit).caret, 4);

    // Left at the start is a no-op (stays 0 after Home).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_LEFT);
    assert_eq!(control_ui(&state, edit).caret, 0);
}

#[test]
fn test_edit_shift_arrow_extends_selection() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // End (no shift) → caret 5, no selection; then hold Shift.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 5);
    let held = state.window_state().keyboard_state.get(0x10) | 0x80;
    state.window_state().keyboard_state.set(0x10, held); // VK_SHIFT held

    // Shift+Left selects the last char: [4, 5), caret 4.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_LEFT);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (4, 5, 4));

    // Shift+Left again extends: [3, 5), caret 3.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_LEFT);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (3, 5, 3));

    // Release Shift; Right collapses the selection and moves the caret.
    let released = state.window_state().keyboard_state.get(0x10) & !0x80;
    state.window_state().keyboard_state.set(0x10, released);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (4, 4, 4));
}

// ── Task 2.3: multiline EDIT keyboard navigation (vertical moves, line-aware
// Home/End, goal-column memory, page keys) ──

#[test]
fn test_edit_multiline_up_down_moves_between_lines() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");
    // "ab\ncd": line 0 = chars 0..2, line 1 = chars 3..5. Caret 1 is column 1
    // of line 0; VK_DOWN keeps the column on line 1 → caret 4 (3 + 1).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    assert_eq!(control_ui(&state, edit).caret, 1);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.goal_column), (4, Some(1)));
    // VK_UP returns to the same column on line 0.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.goal_column), (1, Some(1)));
    // Horizontal movement clears the goal column.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.goal_column), (2, None));
}

#[test]
fn test_edit_multiline_up_down_column_memory() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd\nefgh");
    // Lines: "ab" 0..2, "cd" 3..5, "efgh" 6..10. EM_SETSEL(9, 9) → caret 9 =
    // column 3 of the long line.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        9,
        9,
    )
    .expect("setsel ok")
    .expect("some result");
    // Up to "cd" (len 2): the goal column 3 clamps to 2 → caret 3 + 2 = 5.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    assert_eq!(control_ui(&state, edit).caret, 5);
    // Up to "ab" (len 2): still clamped → caret 0 + 2 = 2.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    assert_eq!(control_ui(&state, edit).caret, 2);
    // Down returns to the goal column 3 on "cd" → caret 3 + 2 = 5.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    assert_eq!(control_ui(&state, edit).caret, 5);
    // Down again on the long line: the remembered column 3 → caret 6 + 3 = 9.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.goal_column), (9, Some(3)));
}

#[test]
fn test_edit_multiline_home_end_line_aware() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd\nefgh");
    // Home/End are LINE-aware: from caret 0, End → line 0's end (2), not the
    // document end (10).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 2);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 0);
    // Caret at line 1's end (5): Home → line 1 start (3), End → 5, NOT the
    // document end (10).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        5,
        5,
    )
    .expect("setsel ok")
    .expect("some result");
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 3);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 5);
    // Ctrl+Home / Ctrl+End are document-wide.
    state.window_state().keyboard_state.set(0x11, 0x80); // VK_CONTROL held
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 0);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 10);
    state.window_state().keyboard_state.set(0x11, 0);
}

#[test]
fn test_edit_multiline_pgup_pgdn_move_a_page() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    // The page heuristic is the client height / 16 px default line height;
    // an 80 px tall control gives a 5-line page.
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.height = 80;
        }
    }
    // PgDn from line 0 → line 5 (char index 10); PgDn again clamps to the
    // last line 9 (index 18).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_NEXT);
    assert_eq!(control_ui(&state, edit).caret, 10);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_NEXT);
    assert_eq!(control_ui(&state, edit).caret, 18);
    // PgUp steps back a page → line 4 (index 8), then line 0; past the top
    // is a no-op.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_PRIOR);
    assert_eq!(control_ui(&state, edit).caret, 8);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_PRIOR);
    assert_eq!(control_ui(&state, edit).caret, 0);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_PRIOR);
    assert_eq!(control_ui(&state, edit).caret, 0);
}

#[test]
fn test_edit_shift_up_down_extends_selection_across_lines() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd\nefgh");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        5,
        5,
    )
    .expect("setsel ok")
    .expect("some result");
    state.window_state().keyboard_state.set(0x10, 0x80); // VK_SHIFT held
    // Shift+Down: caret 5 (line 1, col 2) → line 2, goal 2 → caret 8,
    // selection [5, 8).
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 8, 8));
    // Shift+Up back to the anchor collapses the selection.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 5, 5));
    // Shift+Up again extends upward across the line break: [2, 5), caret 2.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (2, 5, 2));
    // Shift+Down returns the caret to the anchor (5) and collapses the
    // selection — Windows EDIT anchor semantics, not a re-extension.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 5, 5));
    // Shift+Down again extends downward from the anchor across the break.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_DOWN);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 8, 8));
    // Release Shift: Up collapses the selection and moves the caret.
    state.window_state().keyboard_state.set(0x10, 0);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_UP);
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end, ui.caret), (5, 5, 5));
}

#[test]
fn test_edit_single_line_vertical_keys_noop() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello"
    // Vertical keys do not move the single-line caret; Home/End keep their
    // document-wide meaning (identical to line-aware on a single line).
    for vk in [
        crate::user32::VK_UP,
        crate::user32::VK_DOWN,
        crate::user32::VK_PRIOR,
        crate::user32::VK_NEXT,
    ] {
        press_key(&mut engine, &mut state, edit, vk);
        assert_eq!(control_ui(&state, edit).caret, 0);
    }
    let ui = control_ui(&state, edit);
    assert_eq!(ui.goal_column, None, "single-line edits never set a goal");
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    assert_eq!(control_ui(&state, edit).caret, 5);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);
    assert_eq!(control_ui(&state, edit).caret, 0);
}

// ── Task 2.1: multiline EDIT text model + EM_* state messages ──

#[test]
fn test_edit_wm_char_enter_multiline_inserts_newline() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab");

    // End + WM_CHAR 0x0D on a multiline EDIT appends '\n'.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        0x0D,
        0,
    )
    .expect_err("multiline Enter inserts '\n' and delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "ab\n");
    assert_eq!(control_ui(&state, edit).caret, 3);

    // The single-line EDIT keeps the historical no-op.
    let (_, single) = push_edit_pair(&mut state);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        single,
        crate::user32::WM_CHAR,
        0x0D,
        0,
    )
    .expect("single-line enter ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, single), "hello");
}

#[test]
fn test_edit_em_limitext_caps_insertion() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello", len 5

    // EM_LIMITTEXT(5) == the current length: typing at the cap is a no-op.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LIMITTEXT,
        5,
        0,
    )
    .expect("limitext ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_LIMITTEXT returns TRUE");
    assert_eq!(control_ui(&state, edit).limit, 5);
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_GETLIMITTEXT,
            0,
            0,
        )
        .expect("getlimitext ok")
        .expect("some result"),
        5
    );

    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect("at-cap typing ok")
    .expect("some result");
    assert_eq!(r, 0, "insertion beyond the limit must be ignored");
    assert_eq!(control_text(&state, edit), "hello");
    assert!(
        !control_ui(&state, edit).modified,
        "a blocked insert must not dirty the modify flag"
    );

    // Deletion is never capped: backspace removes a char.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        0x08,
        0,
    )
    .expect_err("backspace delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hell");
    assert!(
        control_ui(&state, edit).modified,
        "a real deletion must dirty the modify flag"
    );

    // EM_REPLACESEL truncates a too-long replacement to the remaining room
    // (limit 5, selecting 'h' leaves room for 5 - 3 = 2 chars).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        1,
    )
    .expect("setsel ok")
    .expect("some result");
    write_guest_ansi(&mut engine, 0x4000, "PQRST");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_REPLACESEL,
        0,
        0x4000,
    )
    .expect_err("EM_REPLACESEL delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "PQell");
}

#[test]
fn test_edit_em_line_messages_on_multiline_text() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // EM_GETLINECOUNT: '\n' separates lines; "ab\ncd" has 2.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETLINECOUNT,
        0,
        0,
    )
    .expect("linecount ok")
    .expect("some result");
    assert_eq!(r, 2);

    // Windows semantics: an empty multiline edit still reports 1 line. Uses a
    // push_edit_pair edit (0x6610_0012) so it does not collide with the
    // fixture's fixed multiline handle.
    let (_, empty_edit) = push_edit_pair(&mut state);
    {
        let ws = state.window_state();
        let window = ws
            .windows
            .iter_mut()
            .find(|w| w.handle == crate::handles::Hwnd::from(empty_edit))
            .expect("empty edit window");
        window.style |= crate::user32::controls::ES_MULTILINE;
        window.control_text = "".to_owned();
    }
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        empty_edit,
        crate::user32::EM_GETLINECOUNT,
        0,
        0,
    )
    .expect("empty linecount ok")
    .expect("some result");
    assert_eq!(r, 1, "an empty multiline edit has one (empty) line");

    // EM_LINEFROMCHAR: '\n' belongs to the line it terminates.
    for (index, expected) in [(0_u64, 0_u64), (2, 0), (3, 1), (4, 1)] {
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_LINEFROMCHAR,
            index,
            0,
        )
        .expect("linefromchar ok")
        .expect("some result");
        assert_eq!(r, expected, "LINEFROMCHAR({index})");
    }

    // EM_LINEINDEX: the char index of each line start; -1 out of range.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LINEINDEX,
        0,
        0,
    )
    .expect("lineindex0 ok")
    .expect("some result");
    assert_eq!(r, 0);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LINEINDEX,
        1,
        0,
    )
    .expect("lineindex1 ok")
    .expect("some result");
    assert_eq!(r, 3, "line 1 starts after \"ab\" + the '\n'");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LINEINDEX,
        2,
        0,
    )
    .expect("lineindex2 ok")
    .expect("some result");
    assert_eq!(r, u64::MAX, "out-of-range line returns -1");

    // EM_LINELENGTH: chars in the line, excluding its '\n'.
    for (index, expected) in [(0_u64, 2_u64), (3, 2)] {
        let r = crate::user32::controls::dispatch_control_proc(
            &mut engine,
            &mut state,
            edit,
            crate::user32::EM_LINELENGTH,
            index,
            0,
        )
        .expect("linelength ok")
        .expect("some result");
        assert_eq!(r, expected, "LINELENGTH({index})");
    }
    // wParam == -1: the length of the caret's line (caret starts at 0).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_LINELENGTH,
        u64::from(u32::MAX),
        0,
    )
    .expect("linelength caret ok")
    .expect("some result");
    assert_eq!(r, 2, "LINELENGTH(-1) uses the caret's line");

    // EM_GETLINE: the buffer's first WORD is the capacity (incl. the NUL);
    // the copy strips the line's '\n' and NUL-terminates.
    engine
        .mem_write(0x4000, &64_u16.to_le_bytes())
        .expect("line buffer capacity");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETLINE,
        0,
        0x4000,
    )
    .expect("getline0 ok")
    .expect("some result");
    assert_eq!(r, 2, "EM_GETLINE returns the char count");
    let mut line0 = [0_u8; 4];
    engine.mem_read(0x4000, &mut line0).expect("read line 0");
    assert_eq!(line0, *b"ab\0\0", "line 0 copies \"ab\" without the EOL");
    engine
        .mem_write(0x4000, &64_u16.to_le_bytes())
        .expect("line buffer capacity");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETLINE,
        1,
        0x4000,
    )
    .expect("getline1 ok")
    .expect("some result");
    assert_eq!(r, 2);
    let mut line1 = [0_u8; 4];
    engine.mem_read(0x4000, &mut line1).expect("read line 1");
    assert_eq!(line1, *b"cd\0\0", "line 1 copies \"cd\" without the EOL");
    // Out-of-range line → 0.
    engine
        .mem_write(0x4000, &64_u16.to_le_bytes())
        .expect("line buffer capacity");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETLINE,
        7,
        0x4000,
    )
    .expect("getline7 ok")
    .expect("some result");
    assert_eq!(r, 0, "out-of-range line copies nothing");
}

#[test]
fn test_edit_em_replacesel_replaces_selection_and_fires_en_change() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_edit_pair(&mut state); // "hello"

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        1,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    write_guest_ansi(&mut engine, 0x4000, "XY");
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_REPLACESEL,
        0,
        0x4000,
    );
    let error = result.expect_err("EM_REPLACESEL must deliver EN_CHANGE");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0300_000C
        ),
        "EM_REPLACESEL must deliver WM_COMMAND(MAKEWPARAM(12, EN_CHANGE)), got {signal:?}"
    );

    assert_eq!(control_text(&state, edit), "hXYo");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (3, 3, 3));
}

#[test]
fn test_edit_em_scrollcaret_updates_first_visible_line() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd\nef");

    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETFIRSTVISIBLELINE,
        0,
        0,
    )
    .expect("firstvisible ok")
    .expect("some result");
    assert_eq!(r, 0, "fresh edit starts at the first line");

    // Caret to line 2 ('e', char index 6), then EM_SCROLLCARET brings that
    // line into view.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        6,
        6,
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SCROLLCARET,
        0,
        0,
    )
    .expect("scrollcaret ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_SCROLLCARET returns TRUE");
    assert_eq!(control_ui(&state, edit).first_visible_line, 2);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETFIRSTVISIBLELINE,
        0,
        0,
    )
    .expect("firstvisible ok")
    .expect("some result");
    assert_eq!(r, 2);
}
