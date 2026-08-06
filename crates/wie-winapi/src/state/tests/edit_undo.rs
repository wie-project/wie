//! EDIT undo and clipboard tests: EM_CANUNDO / EM_UNDO / EM_EMPTYUNDOBUFFER, and WM_COPY / WM_PASTE / WM_CUT / WM_CLEAR against the process clipboard.
use super::*;

// ── Task 2.6: EDIT undo + clipboard ─────────────────────────────────────

/// EM_CANUNDO through the control dispatch (0 = no undo pending).
fn can_undo(engine: &mut IcedCpu, state: &mut WinApiState, edit: u64) -> u64 {
    crate::user32::controls::dispatch_control_proc(
        engine,
        state,
        edit,
        crate::user32::EM_CANUNDO,
        0,
        0,
    )
    .expect("canundo ok")
    .expect("some result")
}

/// `IsClipboardFormatAvailable(CF_TEXT)` through the full dispatch path.
fn clipboard_available(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
    write_regs(engine, u64::from(crate::clipboard::CF_TEXT), 0, 0, 0, 0);
    engine
        .mem_write(STACK_TOP, &0x1234_5678_u64.to_le_bytes())
        .expect("write sentinel return address");
    let id = crate::resolve_winapi_id("user32.dll", "IsClipboardFormatAvailable")
        .expect("IsClipboardFormatAvailable must resolve to a WinApiId");
    crate::dispatch_winapi_id(
        &mut HandlerContext::new(engine, test_environment(), state),
        id,
    )
    .expect("IsClipboardFormatAvailable must dispatch")
    .return_value
}

#[test]
fn test_edit_em_canundo_tracks_insert_delete_replace() {
    // Insert: typing 'X' at the caret captures a snapshot.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        0,
        "a fresh edit has nothing to undo"
    );
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        1,
        "an insert captures an undo snapshot"
    );

    // Delete: VK_DELETE at the caret captures a snapshot.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
    press_key(&mut engine, &mut state, edit, crate::user32::VK_RIGHT);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_DELETE,
        0,
    )
    .expect_err("delete delivers EN_CHANGE");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        1,
        "a delete captures an undo snapshot"
    );

    // Replace: EM_REPLACESEL over a selection captures a snapshot.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);
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
    write_guest_ansi(&mut engine, 0x4000, "Z");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_REPLACESEL,
        0,
        0x4000,
    )
    .expect_err("replacesel delivers EN_CHANGE");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        1,
        "a replace captures an undo snapshot"
    );
}

#[test]
fn test_edit_em_undo_restores_text_caret_and_selection() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Select "ell" [1,4) (the caret lands at 4) and type 'X' → "hXo".
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
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hXo");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (2, 2, 2));

    // EM_UNDO reverts the single operation: text, caret AND selection all
    // return to the pre-mutation state ("hello", caret 4, selection [1,4)).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_UNDO,
        0,
        0,
    )
    .expect_err("undo delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hello");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (4, 1, 4));

    // Single-level buffer: the undo consumed the snapshot, so a second
    // EM_UNDO does nothing (no redo) and EM_CANUNDO is false again.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_UNDO,
        0,
        0,
    )
    .expect("second undo ok")
    .expect("some result");
    assert_eq!(r, 0, "EM_UNDO with an empty buffer returns FALSE");
    assert_eq!(control_text(&state, edit), "hello");
    assert_eq!(can_undo(&mut engine, &mut state, edit), 0);
}

#[test]
fn test_edit_wm_undo_matches_em_undo() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // The Edit menu's Undo command sends WM_UNDO to the focused edit — it
    // must behave exactly like EM_UNDO.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "Xhello");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_UNDO,
        0,
        0,
    )
    .expect_err("WM_UNDO delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hello");
    assert_eq!(control_ui(&state, edit).caret, 0);
    assert_eq!(can_undo(&mut engine, &mut state, edit), 0);
}

#[test]
fn test_edit_em_emptyundobuffer_clears() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(can_undo(&mut engine, &mut state, edit), 1);

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_EMPTYUNDOBUFFER,
        0,
        0,
    )
    .expect("emptyundobuffer ok")
    .expect("some result");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        0,
        "EM_EMPTYUNDOBUFFER discards the snapshot"
    );
}

/// EM_CANUNDO parity through the real dispatch path, sending the RAW guest
/// values (winuser.h): the host used to declare EM_CANUNDO as 0x00A6, so a
/// guest's 0x00C6 never matched a dispatch arm and fell through to an
/// unhandled zero. SetWindowText must also clear the undo buffer (Windows
/// clears it on any program-set text).
#[test]
fn test_edit_em_canundo_parity_and_settext_clears_undo() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // The raw 0x00C6 must reach the EM_CANUNDO arm (a fresh edit has nothing
    // to undo). Pre-fix this fell through: dispatch returned None.
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(&mut engine, &mut state, edit, 0x00C6, 0, 0)
            .expect("dispatch ok")
            .expect("0x00C6 must hit the EM_CANUNDO arm"),
        0,
        "a fresh edit has nothing to undo"
    );

    // An editable change (typing) captures a snapshot → TRUE.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(&mut engine, &mut state, edit, 0x00C6, 0, 0)
            .expect("dispatch ok")
            .expect("0x00C6 must hit the EM_CANUNDO arm"),
        1,
        "an insert is undoable"
    );

    // EM_UNDO restores the text and consumes the snapshot → FALSE.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_UNDO,
        0,
        0,
    )
    .expect_err("undo delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hello");
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(&mut engine, &mut state, edit, 0x00C6, 0, 0)
            .expect("dispatch ok")
            .expect("0x00C6 must hit the EM_CANUNDO arm"),
        0,
        "undo consumed the snapshot"
    );

    // SetWindowText (the raw WM_SETTEXT = 0x000C a SendMessage carries) must
    // clear the undo buffer: edit again so a snapshot is pending, then set
    // the text — EM_CANUNDO goes false.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('Y')),
        0,
    )
    .expect_err("char delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "Yhello");
    write_guest_ansi(&mut engine, 0x4000, "fresh");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        0x000C,
        0,
        0x4000,
    )
    .expect("settext ok")
    .expect("some result");
    assert_eq!(control_text(&state, edit), "fresh");
    assert_eq!(
        crate::user32::controls::dispatch_control_proc(&mut engine, &mut state, edit, 0x00C6, 0, 0)
            .expect("dispatch ok")
            .expect("0x00C6 must hit the EM_CANUNDO arm"),
        0,
        "SetWindowText clears the undo buffer"
    );
}

#[test]
fn test_edit_wm_copy_stores_selection_on_clipboard() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // A fresh session has an empty clipboard.
    assert!(!state.clipboard().has_text());

    // WM_COPY without a selection is a no-op.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert!(!state.clipboard().has_text());

    // Select "ell" [1,4) and copy → the clipboard holds "ell".
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
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(state.clipboard().text(), Some("ell"));

    // Copy does not change the text or the selection.
    assert_eq!(control_text(&state, edit), "hello");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.sel_start, ui.sel_end), (1, 4));
}

#[test]
fn test_edit_wm_paste_inserts_clipboard_text_replacing_selection() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Seed the clipboard with the full text, then move to the start.
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
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    press_key(&mut engine, &mut state, edit, crate::user32::VK_HOME);

    // Paste at the caret (no selection) inserts the clipboard text.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PASTE,
        0,
        0,
    )
    .expect_err("paste delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hellohello");
    assert_eq!(control_ui(&state, edit).caret, 5);

    // Paste over a selection replaces it: [1,4) "ell" of "hellohello" is
    // replaced by "hello" → "h" + "hello" + "ohello" = "hhelloohello".
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
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PASTE,
        0,
        0,
    )
    .expect_err("paste delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hhelloohello");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (6, 6, 6));
}

#[test]
fn test_edit_wm_paste_empty_clipboard_is_noop() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Fresh session: nothing on the clipboard, so paste changes nothing.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_PASTE,
        0,
        0,
    )
    .expect("paste ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, edit), "hello");
    assert_eq!(
        can_undo(&mut engine, &mut state, edit),
        0,
        "an empty-clipboard paste is not a mutation"
    );
}

#[test]
fn test_edit_wm_cut_copies_and_deletes() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Select "ell" [1,4) and cut → the clipboard holds "ell" and the text
    // loses it ("ho"), with the caret collapsing at the deletion point.
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
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CUT,
        0,
        0,
    )
    .expect_err("cut delivers EN_CHANGE");
    assert_eq!(state.clipboard().text(), Some("ell"));
    assert_eq!(control_text(&state, edit), "ho");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (1, 1, 1));

    // The cut is a mutation: EM_UNDO restores the pre-cut state.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_UNDO,
        0,
        0,
    )
    .expect_err("undo delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "hello");
}

#[test]
fn test_edit_wm_clear_deletes_without_writing_clipboard() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Put the full text on the clipboard first (a copy).
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
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    assert!(state.clipboard().has_text());

    // Select "ell" [1,4) and clear → the text loses it ("ho") and the
    // clipboard is EMPTIED — Windows' edit control calls EmptyClipboard, so
    // the deleted text is never written to the clipboard (that is what
    // distinguishes CLEAR from CUT) and IsClipboardFormatAvailable goes false.
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
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CLEAR,
        0,
        0,
    )
    .expect_err("clear delivers EN_CHANGE");
    assert_eq!(control_text(&state, edit), "ho");
    assert!(
        !state.clipboard().has_text(),
        "WM_CLEAR must empty the clipboard"
    );

    // Clearing with no selection is a no-op (no text change, no notify).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CLEAR,
        0,
        0,
    )
    .expect("clear ok")
    .expect("some result");
    assert_eq!(r, 0);
    assert_eq!(control_text(&state, edit), "ho");
}

#[test]
fn test_is_clipboard_format_available_tracks_clipboard_text() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Fresh session: no text on the clipboard → FALSE.
    assert_eq!(clipboard_available(&mut engine, &mut state), 0);

    // WM_COPY the selection → TRUE.
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
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_COPY,
        0,
        0,
    )
    .expect("copy ok")
    .expect("some result");
    assert_eq!(clipboard_available(&mut engine, &mut state), 1);

    // WM_CLEAR empties the clipboard → FALSE again.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CLEAR,
        0,
        0,
    )
    .expect_err("clear delivers EN_CHANGE");
    assert_eq!(clipboard_available(&mut engine, &mut state), 0);
}
