//! EDIT scrolling tests: EN_VSCROLL / EN_HSCROLL delivery, caret-blink timer, autoscroll, the viewport + WM_VSCROLL / mouse-wheel math, and minimal EM_SCROLLCARET.
use super::*;

#[test]
fn test_edit_wm_vscroll_delivers_en_vscroll_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "line1\nline2\nline3\nline4\nline5");

    // SB_LINEDOWN (1) scrolls the multiline EDIT (only 1 visible row in a
    // 20 px client) → the parent gets WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)).
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_VSCROLL.as_u32(),
        1, // SB_LINEDOWN
        0,
    );
    let error = result.expect_err("WM_VSCROLL must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "WM_VSCROLL must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), \
         got {signal:?}"
    );
}

#[test]
fn test_edit_wm_hscroll_delivers_en_hscroll_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "line1\nline2");

    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_HSCROLL.as_u32(),
        0,
        0,
    );
    let error = result.expect_err("WM_HSCROLL must deliver EN_HSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0601_000C
        ),
        "WM_HSCROLL must deliver WM_COMMAND(MAKEWPARAM(12, EN_HSCROLL)), \
         got {signal:?}"
    );
}

#[test]
fn test_edit_pgup_pgdn_keydown_delivers_en_vscroll_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    // An 80 px tall control gives a 5-line page (the 16 px default line
    // height) — the same fixture as the PgUp/PgDn caret-movement test.
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.height = 80;
        }
    }

    // PgDn moves the caret a page (line 0 → 5): the parent gets the same
    // WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)) a real vertical scroll delivers,
    // so notepad re-reads the caret position into its status bar.
    let pgdn_result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_NEXT,
        0,
    );
    let error = pgdn_result.expect_err("PgDn must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "PgDn must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), got {signal:?}"
    );

    // PgUp likewise (line 5 → 0).
    let pgup_result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_PRIOR,
        0,
    );
    let error = pgup_result.expect_err("PgUp must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "PgUp must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), got {signal:?}"
    );

    // PgUp at the first line: the caret cannot move up a page, so the key is
    // a no-op — no EN_VSCROLL fires (the pragmatic no-move semantic).
    let noop_result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_PRIOR,
        0,
    );
    let value = noop_result.expect("no-op PgUp at the top must not notify");
    assert_eq!(value, Some(0), "a no-op page key answers 0 silently");
    assert_eq!(
        control_ui(&state, edit).caret,
        0,
        "the caret stays at line 0"
    );
}

#[test]
fn test_edit_caret_blink_timer_toggles_caret_phase() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "line1\nline2\nline3");

    // Focus arms the internal blink timer and resets the caret to the on
    // phase (the caret bar shows solid until the first tick).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    let caret_timer = state
        .window_state()
        .timers
        .iter()
        .find(|t| t.window_handle == crate::handles::Hwnd::from(edit) && t.timer_id == 1)
        .expect("focus must arm the caret timer");
    assert_eq!(
        caret_timer.interval_ms, 530,
        "the blink half-period is the SPI_GETCARETTIMEOUT default"
    );
    assert!(
        control_ui(&state, edit).caret_on,
        "the caret starts in the on phase"
    );

    // Each WM_TIMER tick flips the phase, so the caret bar alternates
    // between drawn and hidden (the paint draws it only in the on phase).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        1,
        0,
    )
    .expect("timer ok")
    .expect("some result");
    assert!(
        !control_ui(&state, edit).caret_on,
        "the first tick hides the caret"
    );
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        1,
        0,
    )
    .expect("timer ok")
    .expect("some result");
    assert!(
        control_ui(&state, edit).caret_on,
        "the second tick shows it again"
    );

    // A WM_TIMER with a different id is not the caret timer: not the edit's
    // business (falls through, no state change).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        99,
        0,
    )
    .expect("other timer ok");
    assert_eq!(r, None, "an unknown timer id must not touch the edit");

    // Losing focus disarms the timer and clears the focus flag (the paint
    // already skips the caret while unfocused).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KILLFOCUS,
        0,
        0,
    )
    .expect("killfocus ok")
    .expect("some result");
    assert!(
        !state
            .window_state()
            .timers
            .iter()
            .any(|t| t.window_handle == crate::handles::Hwnd::from(edit)),
        "kill focus must disarm the caret timer"
    );
    assert!(
        !control_ui(&state, edit).focused,
        "kill focus clears the flag"
    );
}

/// The visual row where the last paint drew `hwnd`'s caret bar (the
/// `ControlState::Edit::last_caret_drawn_row` surface record).
fn edit_caret_drawn_row(state: &WinApiState, hwnd: u64) -> Option<usize> {
    match state
        .try_window_state()
        .and_then(|ws| ws.control_states.get(&crate::handles::Hwnd::from(hwnd)))
    {
        Some(crate::user32::controls::ControlState::Edit {
            last_caret_drawn_row,
            ..
        }) => *last_caret_drawn_row,
        _ => None,
    }
}

/// The caret-blink tick must repaint BOTH the row where the last paint drew
/// the caret bar AND the caret's current row. A caret that moved since the
/// last paint leaves the old bar on the surface; a tick that repaints only
/// the current row would let that bar survive forever (the stuck/ghost
/// caret).
#[test]
fn test_edit_blink_tick_invalidates_last_drawn_and_current_caret_rows() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // 120×60 client: all three rows are visible (no auto-scroll on moves).
    let (_, edit) = push_multiline_edit_pair(&mut state);

    // Focus + paint: the caret (row 0) is drawn and its row recorded — the
    // surface now shows the bar on row 0.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        Some(0),
        "the paint must record the row where it drew the bar"
    );

    // Simulate the ghost interleaving: the caret moved to row 2 (the char
    // `a` of "gamma") after that paint and no repaint followed, so the bar
    // is still on the surface at row 0 while the caret lives on row 2.
    {
        let ws = state.window_state();
        let crate::user32::controls::ControlState::Edit { caret, .. } = ws
            .control_states
            .get_mut(&crate::handles::Hwnd::from(edit))
            .expect("edit state")
        else {
            panic!("edit state");
        };
        *caret = 12;
    }

    // The blink tick hides the bar and must repaint BOTH rows: the stale
    // row 0 (erase the old bar) and the caret's current row 2.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        1, // CARET_TIMER_ID
        0,
    )
    .expect("blink ok")
    .expect("some result");
    assert!(
        matches!(
            control_ui(&state, edit).invalid_rows,
            crate::user32::controls::EditInvalidation::Band(band)
                if band.lo == 0 && band.hi >= 2
        ),
        "the tick must repaint the last-drawn row (0) and the current caret row (2), got {:?}",
        control_ui(&state, edit).invalid_rows
    );
}

/// A caret move while the blink phase is OFF must still repaint the row the
/// caret LEFT: the old bar is cleared once the phase returns (the span
/// invalidation covers the moved characters; the old caret's own row is
/// repainted unconditionally so a boundary move can never skip it).
#[test]
fn test_edit_caret_move_with_blink_off_covers_old_and_new_rows() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // 120×60 client: the caret move from row 0 to row 1 stays in view, so
    // no auto-scroll can widen the pending band to a full repaint.
    let (_, edit) = push_multiline_edit_pair(&mut state);

    // Focus + paint, then flip the blink phase OFF (one tick): the bar is
    // hidden, so the next repaint must still cover the rows the caret
    // travels — a repaint that skips the old row could leave a stale bar
    // from an earlier paint on the surface.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        1, // CARET_TIMER_ID
        0,
    )
    .expect("blink ok")
    .expect("some result");
    assert!(
        !control_ui(&state, edit).caret_on,
        "the tick must hide the caret before the move"
    );

    // VK_DOWN moves the caret from row 0 to row 1. The EN_VSCROLL
    // notification reaches the parent: with no guest WndProc in this
    // fixture it resolves to a silent Ok (a guest-proc parent would
    // deliver a control signal instead).
    let moved = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DOWN,
        0,
    );
    assert!(moved.is_ok(), "VK_DOWN must be handled by the edit");
    assert_eq!(
        control_ui(&state, edit).caret,
        6,
        "the caret must land at the start of row 1"
    );
    assert!(
        matches!(
            control_ui(&state, edit).invalid_rows,
            crate::user32::controls::EditInvalidation::Band(band)
                if band.lo == 0 && band.hi >= 1
        ),
        "the move must repaint the old caret row (0) and the new one (1), got {:?}",
        control_ui(&state, edit).invalid_rows
    );
}

/// A mouse click focuses an EDIT without a WM_SETFOCUS: the blink phase must
/// reset to ON and the blink timer must re-arm, or a click on an edit whose
/// phase was left OFF (and whose timer a kill-focus disarmed) would leave
/// the caret invisible until the next key focus.
#[test]
fn test_edit_mouse_click_focus_resets_caret_blink_and_arms_timer() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "alpha\nbeta\ngamma");

    // Focus, flip the phase OFF, then lose focus: the edit is left with the
    // caret hidden and the blink timer disarmed — the exact state a later
    // mouse click must repair.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        1, // CARET_TIMER_ID
        0,
    )
    .expect("blink ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KILLFOCUS,
        0,
        0,
    )
    .expect("killfocus ok")
    .expect("some result");
    assert!(
        !control_ui(&state, edit).caret_on,
        "the edit must be left in the hidden blink phase"
    );
    assert!(
        !state
            .window_state()
            .timers
            .iter()
            .any(|t| { t.window_handle == crate::handles::Hwnd::from(edit) && t.timer_id == 1 }),
        "kill focus must have disarmed the blink timer"
    );

    // A mouse click on the edit (the EDIT WM_LBUTTONDOWN arm) focuses it
    // without a WM_SETFOCUS: the phase must come back ON and the timer must
    // re-arm with the 530 ms blink half-period.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_LBUTTONDOWN,
        0,
        u64::from((5_u32 << 16) | 5_u32), // lParam = (y << 16) | x
    )
    .expect("click ok")
    .expect("some result");
    assert!(
        control_ui(&state, edit).caret_on,
        "the click must reset the caret to the on phase"
    );
    assert!(
        control_ui(&state, edit).focused,
        "the click must focus the edit"
    );
    assert!(
        state.window_state().timers.iter().any(|t| {
            t.window_handle == crate::handles::Hwnd::from(edit)
                && t.timer_id == 1
                && t.interval_ms == 530
        }),
        "the click must re-arm the 530 ms caret timer"
    );
}

/// The paint records the row where it actually drew the caret bar — the
/// surface record the blink tick relies on. The record follows the caret
/// across paints and stays put when a paint skips the bar (blink phase off).
#[test]
fn test_edit_paint_records_the_row_where_the_caret_was_drawn() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // 120×60 client: row 2 is on-screen, so the paint can actually draw the
    // bar there.
    let (_, edit) = push_multiline_edit_pair(&mut state);

    // Never painted: nothing recorded.
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        None,
        "a never-painted edit has no drawn bar row"
    );

    // Focus + paint: the bar lands on row 0 and is recorded.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFOCUS,
        0,
        0,
    )
    .expect("focus ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        Some(0),
        "the first paint records the caret's row"
    );

    // The caret moves to row 2; the next paint records row 2.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        12,
        12,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint2 ok")
    .expect("some result");
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        Some(2),
        "the record follows the caret to row 2"
    );

    // Blink off + paint: the bar is NOT drawn, so the record is untouched
    // (it still names the row the surface shows the bar on — the blink tick
    // erases it from there once the phase returns).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_TIMER,
        1, // CARET_TIMER_ID
        0,
    )
    .expect("blink ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint3 ok")
    .expect("some result");
    assert!(
        !control_ui(&state, edit).caret_on,
        "the blink phase must be off for this paint"
    );
    assert_eq!(
        edit_caret_drawn_row(&state, edit),
        Some(2),
        "a paint that skips the bar must leave the record untouched"
    );
}

#[test]
fn test_edit_typing_at_bottom_autoscrolls_caret_into_view() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
    // 5 visible rows in an 80 px client (the 16 px default line height):
    // typing on line 9 (char 18) must bring the caret into view instead of
    // leaving it off-screen below the last visible row.
    for w in &mut state.window_state().windows {
        if w.handle == crate::handles::Hwnd::from(edit) {
            w.height = 80;
        }
    }
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        18,
        18,
    )
    .expect("setsel ok")
    .expect("some result");
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        0,
        "the fixture starts at the top"
    );

    let error = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    )
    .expect_err("typing must deliver EN_CHANGE");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.message == 0x0111 && request.word_parameter == 0x0300_000C
        ),
        "typing must deliver WM_COMMAND(MAKEWPARAM(12, EN_CHANGE)), got {signal:?}"
    );
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        5,
        "typing past the last visible row scrolls the caret into view"
    );
    assert_eq!(
        control_ui(&state, edit).caret,
        19,
        "the typed char lands after the caret"
    );
}

#[test]
fn test_edit_arrow_keys_deliver_en_hscroll_and_en_vscroll() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "ab\ncd\nefgh");

    // A horizontal move (VK_RIGHT) delivers EN_HSCROLL — the status-bar
    // caret refresh for column changes.
    let error = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_RIGHT,
        0,
    )
    .expect_err("VK_RIGHT must deliver EN_HSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0601_000C
        ),
        "VK_RIGHT must deliver WM_COMMAND(MAKEWPARAM(12, EN_HSCROLL)), got {signal:?}"
    );

    // A vertical move (VK_DOWN) delivers EN_VSCROLL (caret 1 → line 1).
    let error = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DOWN,
        0,
    )
    .expect_err("VK_DOWN must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "VK_DOWN must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), got {signal:?}"
    );

    // Ctrl+End is a document-wide vertical jump → EN_VSCROLL (the line-aware
    // plain End would be EN_HSCROLL).
    state.window_state().keyboard_state.set(0x11, 0x80); // VK_CONTROL held
    let error = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_END,
        0,
    )
    .expect_err("Ctrl+End must deliver EN_VSCROLL");
    state.window_state().keyboard_state.set(0x11, 0);
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "Ctrl+End must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), got {signal:?}"
    );
}

#[test]
fn test_edit_click_release_delivers_en_vscroll_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // A completed click navigation (down then up) delivers EN_VSCROLL to the
    // parent — the status-bar caret refresh for click navigation.
    let (x, y) = (20_u16, 10_u16);
    dispatch_mouse(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONDOWN.as_u32(),
        x,
        y,
    );
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_LBUTTONUP.as_u32(),
        0,
        mouse_lparam(x, y),
    );
    let error = result.expect_err("a completed click must deliver EN_VSCROLL");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 0x0602_000C
        ),
        "a completed click must deliver WM_COMMAND(MAKEWPARAM(12, EN_VSCROLL)), \
         got {signal:?}"
    );
}

// ── Task 2.4: multiline-EDIT vertical scrolling (viewport, WM_VSCROLL, wheel,
// minimal EM_SCROLLCARET).

/// A WM_MOUSEWHEEL wParam whose high word carries the signed `delta`.
fn wheel_wparam(delta: i32) -> u64 {
    let hi = i16::try_from(delta).unwrap_or(0);
    u64::from(u16::from_le_bytes(hi.to_le_bytes())) << 16
}

/// Dispatch WM_MOUSEWHEEL with a signed delta and return the resulting
/// first-visible-line offset.
fn wheel_offset(engine: &mut IcedCpu, state: &mut WinApiState, edit: u64, delta: i32) -> usize {
    crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        edit,
        crate::user32::wm::WinMsg::WM_MOUSEWHEEL.as_u32(),
        wheel_wparam(delta),
        0,
    )
    .expect("wheel ok")
    .expect("some result");
    control_ui(state, edit).first_visible_line
}

#[test]
fn test_edit_visible_line_count_and_clamp() {
    use crate::user32::controls::{clamp_scroll_offset, visible_line_count};
    // Floor division: a partial row at the bottom is clipped.
    assert_eq!(visible_line_count(48, 16), 3);
    assert_eq!(visible_line_count(40, 16), 2);
    assert_eq!(visible_line_count(16, 16), 1);
    // Degenerate metrics still show one row so the caret stays reachable.
    assert_eq!(visible_line_count(0, 16), 1);
    assert_eq!(visible_line_count(48, 0), 1);
    // The offset stays inside [0, total − visible]: 0 when the text fits.
    assert_eq!(clamp_scroll_offset(0, 5, 3), 0);
    assert_eq!(clamp_scroll_offset(2, 5, 3), 2);
    assert_eq!(clamp_scroll_offset(4, 5, 3), 2);
    assert_eq!(clamp_scroll_offset(99, 5, 3), 2);
    assert_eq!(clamp_scroll_offset(3, 2, 3), 0);
}

#[test]
fn test_edit_wm_vscroll_scroll_codes() {
    // WM_VSCROLL scroll-bar codes (winuser.h) — the wParam low word.
    const SB_LINEUP: u16 = 0;
    const SB_LINEDOWN: u16 = 1;
    const SB_PAGEUP: u16 = 2;
    const SB_PAGEDOWN: u16 = 3;
    const SB_THUMBPOSITION: u16 = 4;
    const SB_THUMBTRACK: u16 = 5;
    const SB_TOP: u16 = 6;
    const SB_BOTTOM: u16 = 7;
    const SB_ENDSCROLL: u16 = 8;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "a\nb\nc\nd\ne");
    // 5 visual rows, 3 visible → the offset clamps to max(0, 5 − 3) = 2.
    set_edit_visible_rows(&mut state, edit, 3);

    // SB_TOP: first row; SB_BOTTOM: the last offset that keeps the final row
    // visible (NOT the last row itself).
    assert_eq!(vscroll_offset(&mut engine, &mut state, edit, SB_TOP, 0), 0);
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_BOTTOM, 0),
        2
    );

    // SB_LINEUP / SB_LINEDOWN step by one row, clamped at both ends.
    assert_eq!(vscroll_offset(&mut engine, &mut state, edit, SB_TOP, 0), 0);
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_LINEDOWN, 0),
        1
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_LINEUP, 0),
        0
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_BOTTOM, 0),
        2
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_LINEDOWN, 0),
        2
    ); // clamped

    // SB_PAGEUP / SB_PAGEDOWN move by the visible-row count (3).
    assert_eq!(vscroll_offset(&mut engine, &mut state, edit, SB_TOP, 0), 0);
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_PAGEDOWN, 0),
        2
    ); // 3 clamped
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_PAGEUP, 0),
        0
    );

    // SB_THUMBTRACK / SB_THUMBPOSITION jump to the high-word thumb position.
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_THUMBTRACK, 1),
        1
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_THUMBPOSITION, 2),
        2
    );

    // SB_ENDSCROLL is a no-op.
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_ENDSCROLL, 0),
        2
    );
}

#[test]
fn test_edit_wm_vscroll_short_text_stays_at_zero() {
    const SB_LINEDOWN: u16 = 1;
    const SB_THUMBTRACK: u16 = 5;
    const SB_BOTTOM: u16 = 7;

    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "a\nb");
    // Content (2 rows) fits entirely in a 3-row viewport: every code clamps
    // to 0.
    set_edit_visible_rows(&mut state, edit, 3);
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_BOTTOM, 0),
        0
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_LINEDOWN, 0),
        0
    );
    assert_eq!(
        vscroll_offset(&mut engine, &mut state, edit, SB_THUMBTRACK, 5),
        0
    );
}

#[test]
fn test_edit_em_scrollcaret_minimal_scroll() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "a\nb\nc\nd\ne");
    // 5 rows, 3 visible: the viewport shows rows [first, first + 3).
    set_edit_visible_rows(&mut state, edit, 3);

    // Caret BELOW the viewport (line 4, the last row): the offset advances
    // just enough to show it on the LAST visible row — NOT snapping it to
    // the top (the pre-Task-2.4 behavior).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        8, // 'e', line 4
        8,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SCROLLCARET,
        0,
        0,
    )
    .expect("scrollcaret ok")
    .expect("some result");
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        2,
        "caret below the viewport lands on the LAST visible row (4 − 3 + 1), not the top"
    );

    // Caret ABOVE the viewport: scrolls back up to reveal it at the top.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        2, // 'b', line 1
        2,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SCROLLCARET,
        0,
        0,
    )
    .expect("scrollcaret ok")
    .expect("some result");
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        1,
        "caret above the viewport scrolls back to it"
    );

    // Caret already visible: no movement.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        6, // 'd', line 3 — inside rows [1, 4)
        6,
    )
    .expect("setsel ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SCROLLCARET,
        0,
        0,
    )
    .expect("scrollcaret ok")
    .expect("some result");
    assert_eq!(
        control_ui(&state, edit).first_visible_line,
        1,
        "a visible caret must not move the offset"
    );
}

#[test]
fn test_edit_wm_mousewheel_scrolls() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "a\nb\nc\nd\ne");
    set_edit_visible_rows(&mut state, edit, 3);

    // Positive delta (wheel away from the user) scrolls UP — already at the
    // top, so nothing moves.
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, 120), 0);
    // A full notch scrolls 3 lines (the Windows default); clamped to 2.
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, -120), 2);
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, -240), 2); // clamped
    // Back up: 2 − 3 → 0.
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, 120), 0);
    // A partial notch (60 delta units) is below the 120-unit line threshold.
    assert_eq!(wheel_offset(&mut engine, &mut state, edit, -60), 0);
}
