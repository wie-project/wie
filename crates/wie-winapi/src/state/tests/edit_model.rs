//! EDIT model tests: multiline creation style survival and the EM_* state messages (EM_POSFROMCHAR, EM_SETMODIFY, EM_GETHANDLE / EM_SETHANDLE, EM_SETTABSTOPS, EM_POSFROMCHAR, EM_SELECTIONTYPE).
use super::*;

// ── Windows-fidelity tier (★15): EDIT caret/selection + LISTBOX selection ──

#[test]
fn test_edit_multiline_real_creation_enter_inserts_newline() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let edit = push_multiline_edit_real(&mut state);

    // The first touch is a real WM_CHAR through dispatch_control_proc: the
    // control state seeds from the WindowRecord's creation style, so Enter
    // must insert a `\n` (ES_MULTILINE) rather than the single-line no-op.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        crate::user32::VK_RETURN, // 0x0D
        0,
    )
    .expect("char ok")
    .expect("some result");
    assert!(
        control_text(&state, edit).contains('\n'),
        "Enter must insert \\n in a real-created multiline EDIT, got {:?}",
        control_text(&state, edit)
    );
    let ui = control_ui(&state, edit);
    assert_ne!(
        ui.style_bits & crate::user32::controls::ES_MULTILINE,
        0,
        "the seeded EDIT state must carry ES_MULTILINE from the creation style"
    );
}

#[test]
fn test_edit_multiline_style_survives_control_state_seed_order() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let edit = push_multiline_edit_real(&mut state);

    // Poison the seed: `control_state_mut`'s Edit path seeds via
    // `ControlClassKind::Edit.new_state()` → `new_edit_state(0)`, so a message
    // routed through it FIRST leaves the state with style 0 — the race the
    // user hit when an early control message touched the edit before typing
    // ever reached it. The WM_CHAR that follows must still insert the newline:
    // the style capture must not depend on which seeder ran first. (The
    // Task 2.5 mouse arms are NOT a poison source — they seed through
    // `edit_state_mut` with the window's real style.)
    state
        .window_state()
        .control_states
        .entry(crate::handles::Hwnd::from(edit))
        .or_insert_with(|| crate::user32::controls::ControlClassKind::Edit.new_state());
    assert_eq!(
        control_ui(&state, edit).style_bits,
        0,
        "precondition: the control_state_mut seed carries style 0"
    );

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        crate::user32::VK_RETURN,
        0,
    )
    .expect("char ok")
    .expect("some result");
    assert!(
        control_text(&state, edit).contains('\n'),
        "Enter must still insert \\n after a style-0 control_state seed, got {:?}",
        control_text(&state, edit)
    );
    assert_ne!(
        control_ui(&state, edit).style_bits & crate::user32::controls::ES_MULTILINE,
        0,
        "the style-0 seed must be healed to the window's creation style"
    );
}

#[test]
fn test_edit_em_pos_from_char_uses_font_line_height() {
    // EM_POSFROMCHAR answers y = line × the REAL resolved font line height
    // (the 16 px default control font), not the DIALOG_BASE_UNIT_Y constant.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        3, // 'c', line 1
        3,
    )
    .expect("setsel ok")
    .expect("some result");
    let ok = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_POSFROMCHAR,
        3,
        0x4000,
    )
    .expect("posfromchar ok")
    .expect("some result");
    assert_eq!(ok, 1, "EM_POSFROMCHAR returns TRUE for a valid index");
    let mut bytes = [0_u8; 8];
    engine.mem_read(0x4000, &mut bytes).expect("read point");
    let x = i32::from_le_bytes(bytes[0..4].try_into().expect("x"));
    let y = i32::from_le_bytes(bytes[4..8].try_into().expect("y"));
    assert_eq!(x, 0, "x stays 0 (per-glyph x is Task 2.5)");
    // The same 16 px default font the paint path resolves.
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let line_h = font_engine
        .resolve(&crate::gdi32::FontKey::default(), 16)
        .expect("resolve default font")
        .line_height();
    state.gdi_state().font_engine = font_engine;
    assert_eq!(y, line_h, "line 1's y is one real line height");
}

#[test]
fn test_edit_em_modify_flags_round_trip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state);

    // Fresh edit is unmodified.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETMODIFY,
        0,
        0,
    )
    .expect("getmodify ok")
    .expect("some result");
    assert_eq!(r, 0);

    // EM_SETMODIFY(1) → GETMODIFY 1; EM_SETMODIFY(0) → GETMODIFY 0.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETMODIFY,
        1,
        0,
    )
    .expect("setmodify1 ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETMODIFY,
        0,
        0,
    )
    .expect("getmodify ok")
    .expect("some result");
    assert_eq!(r, 1);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETMODIFY,
        0,
        0,
    )
    .expect("setmodify0 ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETMODIFY,
        0,
        0,
    )
    .expect("getmodify ok")
    .expect("some result");
    assert_eq!(r, 0);

    // Typing sets the flag again.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("typing delivers EN_CHANGE");
    assert!(
        control_ui(&state, edit).modified,
        "typing must set the modify flag"
    );
}

#[test]
fn test_edit_em_get_handle_caches_and_invalidates() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello"

    // Prime the guest heap control block (bump cursor at 0x2000; the freelist
    // heads stay zeroed) so the LocalAlloc-style coherent allocation works —
    // the runtime seeds this block at session init.
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("guest heap bump cursor");

    let first = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle ok")
    .expect("some result");
    assert_ne!(first, 0, "EM_GETHANDLE returns a guest buffer");
    // ANSI: the handle points at "hello" + NUL.
    let mut head = [0_u8; 6];
    engine
        .mem_read(first, &mut head)
        .expect("read handle buffer");
    assert_eq!(head, *b"hello\0", "handle buffer holds a copy of the text");

    // A second GETHANDLE with no intervening mutation reuses the cached
    // buffer instead of leaking a fresh allocation per call.
    let again = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle again ok")
    .expect("some result");
    assert_eq!(
        again, first,
        "repeat GETHANDLE without a text change must return the cached handle"
    );

    // A keystroke mutates the text → the cache clears and the next GETHANDLE
    // allocates a fresh buffer holding the new text.
    press_key(&mut engine, &mut state, edit, crate::user32::VK_END);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(u32::from('X')),
        0,
    )
    .expect_err("typing delivers EN_CHANGE");
    let fresh = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle after mutation ok")
    .expect("some result");
    assert_ne!(fresh, first, "a mutation must invalidate the cached handle");
    let mut head = [0_u8; 7];
    engine
        .mem_read(fresh, &mut head)
        .expect("read fresh buffer");
    assert_eq!(head, *b"helloX\0", "the fresh buffer holds the new text");

    // WM_SETTEXT also changes the text: the next GETHANDLE is fresh again.
    write_guest_ansi(&mut engine, 0x4000, "set");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::wm::WinMsg::WM_SETTEXT.as_u32(),
        0,
        0x4000,
    )
    .expect("settext ok")
    .expect("some result");
    let after_settext = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle after settext ok")
    .expect("some result");
    assert_ne!(
        after_settext, fresh,
        "WM_SETTEXT must invalidate the cached handle"
    );
    let mut head = [0_u8; 4];
    engine
        .mem_read(after_settext, &mut head)
        .expect("read settext buffer");
    assert_eq!(head, *b"set\0", "the buffer holds the WM_SETTEXT text");
}

#[test]
fn test_edit_em_set_handle_adopts_guest_text() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_edit_pair(&mut state); // "hello"

    // Leave a selection behind so SETHANDLE's reset is observable.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        2,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    write_guest_ansi(&mut engine, 0x4000, "adopted");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETHANDLE,
        0,
        0x4000,
    )
    .expect("sethandle ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_SETHANDLE returns TRUE");
    assert_eq!(control_text(&state, edit), "adopted");
    let ui = control_ui(&state, edit);
    assert_eq!((ui.caret, ui.sel_start, ui.sel_end), (0, 0, 0));

    // The adopted buffer becomes the cached GETHANDLE result (the text is
    // unchanged since the adoption, so no fresh allocation happens).
    let handle = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_GETHANDLE,
        0,
        0,
    )
    .expect("gethandle ok")
    .expect("some result");
    assert_eq!(handle, 0x4000, "GETHANDLE returns the adopted buffer");
}

#[test]
fn test_edit_em_settabstops_stores_stops() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // Two explicit stops at 4 and 8 dialog units.
    engine
        .mem_write(0x4000, &4_u16.to_le_bytes())
        .expect("stop 0");
    engine
        .mem_write(0x4002, &8_u16.to_le_bytes())
        .expect("stop 1");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETTABSTOPS,
        2,
        0x4000,
    )
    .expect("settabstops ok")
    .expect("some result");
    assert_eq!(r, 1, "EM_SETTABSTOPS returns TRUE");
    assert_eq!(control_ui(&state, edit).tab_stops, vec![4, 8]);

    // wParam 0 resets to the default tab stops (the stored list clears).
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETTABSTOPS,
        0,
        0,
    )
    .expect("settabstops reset ok")
    .expect("some result");
    assert_eq!(r, 1);
    assert_eq!(
        control_ui(&state, edit).tab_stops,
        Vec::<u16>::new(),
        "wParam 0 restores default stops"
    );
}

#[test]
fn test_edit_em_posfromchar_basic_answer() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // Char 3 ('c') sits on line 1 → y = one REAL resolved-font line height
    // (the same 16 px default control font the paint path uses), x = 0.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_POSFROMCHAR,
        3,
        0x4000,
    )
    .expect("posfromchar ok")
    .expect("some result");
    assert_eq!(r, 1, "valid char returns TRUE");
    assert_eq!(read_test_i32(&mut engine, 0x4000), 0, "x is 0");
    let mut font_engine = std::mem::take(&mut state.gdi_state().font_engine);
    let line_h = font_engine
        .resolve(&crate::gdi32::FontKey::default(), 16)
        .expect("resolve the default control font")
        .line_height();
    state.gdi_state().font_engine = font_engine;
    assert_eq!(
        read_test_i32(&mut engine, 0x4004),
        line_h,
        "y = line × the font's line height"
    );

    // Out-of-range char → FALSE.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_POSFROMCHAR,
        100,
        0x4000,
    )
    .expect("posfromchar oob ok")
    .expect("some result");
    assert_eq!(r, 0, "invalid char returns FALSE");
}

#[test]
fn test_edit_em_selectiontype_basic_answers() {
    use crate::user32::controls::{SEL_EMPTY, SEL_MULTICHAR, SEL_MULTILINE, SEL_TEXT};
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");

    // No selection → SEL_EMPTY.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SELECTIONTYPE,
        0,
        0,
    )
    .expect("selectiontype ok")
    .expect("some result");
    assert_eq!(r, SEL_EMPTY);

    // One char → SEL_TEXT.
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
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SELECTIONTYPE,
        0,
        0,
    )
    .expect("selectiontype ok")
    .expect("some result");
    assert_eq!(r, SEL_TEXT);

    // Two chars on one line → SEL_TEXT | SEL_MULTICHAR.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        0,
        2,
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SELECTIONTYPE,
        0,
        0,
    )
    .expect("selectiontype ok")
    .expect("some result");
    assert_eq!(r, SEL_TEXT | SEL_MULTICHAR);

    // "\nc" spans two lines' characters → SEL_TEXT | SEL_MULTICHAR |
    // SEL_MULTILINE (a selection ending exactly at a '\n' stays on that line).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SETSEL,
        2,
        4,
    )
    .expect("setsel ok")
    .expect("some result");
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::EM_SELECTIONTYPE,
        0,
        0,
    )
    .expect("selectiontype ok")
    .expect("some result");
    assert_eq!(r, SEL_TEXT | SEL_MULTICHAR | SEL_MULTILINE);
}
