//! RNotepad's Edit → Go To... flow (real_exes/notepad.exe): the line-number
//! dialog must resolve from the PE's RT_DIALOG resources (a `DLGTEMPLATEEX`
//! template — id 0x207), build its EDIT + OK/Cancel controls, accept a typed
//! line number, and move the main EDIT's caret to that line's start.
//!
//! This pins the BP4 regression: `main_module_dialogs` was empty because the
//! resource parser skipped `DLGTEMPLATEEX` templates, so `DialogBoxParamW`
//! fell back to the synthesized default — a 300×200 "WIE Dialog" with NO
//! controls. The dialog opened but had no EDIT, no OK/Cancel, and the Go To
//! action could not complete. The `CMD_GOTO` id (0x123) is read from the
//! guest's own menu tree (the same tree the macOS bar mirrors), so the host
//! dispatch was never the breakage.

use crate::helpers::{gui_suite_serialize, pump_until_windows_ready, real_exe};

/// Post `WM_COMMAND(CMD_GOTO)` to notepad's main window, then assert the
/// whole flow: a Dialog window appears (not the synthesized fallback — it
/// must carry the EDIT + OK/Cancel children from the RT_DIALOG template),
/// typing a line number + OK closes it, and the main EDIT's caret lands on
/// the requested line.
#[test]
fn goto_dialog_resolves_edit_and_moves_the_caret() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const EM_SETSEL: u32 = 0x00B1;
    const VK_END: u64 = 0x23;
    const IDOK: u64 = 1;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    assert_ne!(main, 0, "notepad main window exists");
    let tree = handle.window_menu_items();
    let goto_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("go to"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(
        goto_id, 0,
        "the Edit menu must contain a Go To command (host menu mirrors it)"
    );

    // The main EDIT (host-side multiline control).
    let main_edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(main);
    assert_ne!(main_edit, 0, "notepad main EDIT exists");

    // Type three lines so line 2 is a real caret target, then park the caret
    // at the end of the document (Home → the flow reports "current line").
    for c in "line one".chars() {
        handle.post_message(main_edit, WM_CHAR, u64::from(c as u32), 0);
    }
    handle.post_message(main_edit, WM_CHAR, 0x0D, 0); // Enter → '\n'
    for c in "line two".chars() {
        handle.post_message(main_edit, WM_CHAR, u64::from(c as u32), 0);
    }
    handle.post_message(main_edit, WM_CHAR, 0x0D, 0);
    for c in "line three".chars() {
        handle.post_message(main_edit, WM_CHAR, u64::from(c as u32), 0);
    }
    handle.post_message(main_edit, WM_KEYDOWN, VK_END, 0);
    handle.post_message(main_edit, WM_KEYUP, VK_END, 0);
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run after typing");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    // Post the exact WM_COMMAND the macOS menu bar posts on a Go To click.
    handle.post_message(main, WM_COMMAND, u64::from(goto_id), 0);

    // The dialog must appear AND carry the template's controls: a Dialog
    // window with an EDIT child (the synthesized fallback has none). The
    // WM_INITDIALOG callback (which pre-fills the field) is delivered by the
    // pump AFTER the children exist, so keep pumping until the guest is
    // parked in the modal GetMessage loop (callback depth 1) and the prefill
    // landed.
    let mut dialog_hwnd = 0_u64;
    let mut saw_dialog_edit = false;
    let mut prefill_seen = false;
    for _ in 0..200 {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("run after CMD_GOTO");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            panic!("notepad exited while the Go To dialog should be open");
        }
        let windows = session.guest_windows_snapshot();
        if let Some((dhwnd, ..)) = windows.iter().find(|(_, cls, ..)| cls == "Dialog") {
            dialog_hwnd = *dhwnd;
        }
        if dialog_hwnd != 0 {
            // The dialog EDIT is a child window; count controls owned by it.
            let children = windows
                .iter()
                .filter(|(h, ..)| *h != dialog_hwnd && *h != main && *h != main_edit)
                .count();
            // STATIC label + EDIT + OK + Cancel = 4 template items.
            if children >= 4 {
                saw_dialog_edit = true;
            }
        }
        let depth = session.pending_callback_depth();
        let prefill = handle
            .control_text(dialog_edit(&session, dialog_hwnd))
            .unwrap_or_default();
        if saw_dialog_edit && depth <= 1 && !prefill.is_empty() {
            prefill_seen = true;
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert_ne!(
        dialog_hwnd, 0,
        "CMD_GOTO must open a Dialog window — the action does not complete if no dialog appears"
    );
    assert!(
        saw_dialog_edit,
        "the Go To dialog must carry its EDIT + OK/Cancel controls from the \
         RT_DIALOG template (id 0x207) — if the dialog has no children, \
         DialogBoxParamW fell back to the synthesized empty default"
    );
    assert!(
        prefill_seen,
        "the Go To dialog's WM_INITDIALOG must reach the guest dialog proc \
         (the field pre-fills with the current line) — a missing prefill means \
         the 5th-arg (dwInitParam) handoff to the dialog proc is broken"
    );

    // Find the dialog's EDIT (the ID_LINENUMBER field) and OK button: the
    // snapshot lists every control window (dialog children are created with
    // the built-in class atoms — EDIT=0x0081→"#129", BUTTON=0x0080→"#128").
    let edit_hwnd = dialog_edit(&session, dialog_hwnd);
    let ok_hwnd = {
        let windows = session.guest_windows_snapshot();
        windows
            .iter()
            .find(|(_, cls, title, _)| cls == "#128" && title == "OK")
            .map(|(h, ..)| *h)
            .unwrap_or(0)
    };
    assert_ne!(edit_hwnd, 0, "Go To dialog has its line-number EDIT");
    assert_ne!(ok_hwnd, 0, "Go To dialog has its OK button");

    // The dialog pre-fills the field with the current line; select-all then
    // type the target line so the old value is replaced, not appended.
    let prefill = handle.control_text(edit_hwnd).unwrap_or_default();
    handle.post_message(edit_hwnd, EM_SETSEL, 0, u64::from(u32::MAX)); // (0, -1)
    for c in "2".chars() {
        handle.post_message(edit_hwnd, WM_CHAR, u64::from(c as u32), 0);
    }
    for _ in 0..20 {
        let _ = session
            .run_until_stop(1_000_000)
            .expect("run after typing line");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let typed = handle.control_text(edit_hwnd).unwrap_or_default();
    assert_eq!(
        typed, "2",
        "the dialog field must hold the typed line number (prefill was {prefill:?})"
    );

    // Click OK: the guest dialog proc reads the number and EndDialog's.
    handle.post_message(dialog_hwnd, WM_COMMAND, IDOK, 0);

    // The dialog must close and the main EDIT's caret must land on line 2.
    let mut closed = false;
    let mut caret_on_line_two = false;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run after OK");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            break;
        }
        if !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "Dialog")
        {
            closed = true;
        }
        // Line 2 starts after "line one\n" = 9 chars (index 9).
        if let Some((caret, sel_start, sel_end)) = handle.edit_selection(main_edit)
            && caret == 9
            && sel_start == 9
            && sel_end == 9
        {
            caret_on_line_two = true;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if closed && caret_on_line_two {
            break;
        }
    }
    assert!(
        closed,
        "OK must close the Go To dialog (EndDialog removes the subtree)"
    );
    assert!(
        caret_on_line_two,
        "Go To line 2 must move the main EDIT's caret to index 9 (start of \
         line two) via EM_LINEINDEX+EM_SETSEL+EM_SCROLLCARET"
    );
}

/// Locate the Go To dialog's line-number EDIT child (built-in class atom
/// EDIT=0x0081 → snapshot class "#129").
fn dialog_edit(session: &wie_runtime::RuntimeSession, dialog_hwnd: u64) -> u64 {
    session
        .guest_windows_snapshot()
        .iter()
        .find(|(h, cls, ..)| *h != dialog_hwnd && cls == "#129")
        .map(|(h, ..)| *h)
        .unwrap_or(0)
}

/// Drive the full Go To flow on a live session: open the dialog, type
/// `line` (replacing the prefill), click OK, and pump until the dialog
/// closes and the main EDIT's caret lands on the requested line's start.
/// Returns the session's main-window frame after the flow settles.
///
/// Shares the flow of `goto_dialog_resolves_edit_and_moves_the_caret`; the
/// caller supplies the session/handles so a custom document can be typed
/// before the Go To jump.
fn run_goto_flow(
    session: &mut wie_runtime::RuntimeSession,
    handle: &wie_runtime::GuestHandle,
    main: u64,
    goto_id: u32,
    line: &str,
) -> wie_winapi::present::SurfaceFrame {
    use wie_runtime::EntryTraceTermination;
    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const EM_SETSEL: u32 = 0x00B1;
    const IDOK: u64 = 1;

    handle.post_message(main, WM_COMMAND, u64::from(goto_id), 0);

    // Wait for the dialog + its controls (STATIC label + EDIT + OK + Cancel).
    let mut dialog_hwnd = 0_u64;
    for _ in 0..200 {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("run after CMD_GOTO");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            panic!("notepad exited while the Go To dialog should be open");
        }
        if let Some((dhwnd, ..)) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "Dialog")
        {
            dialog_hwnd = *dhwnd;
        }
        if dialog_hwnd != 0 && dialog_edit(session, dialog_hwnd) != 0 {
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert_ne!(dialog_hwnd, 0, "CMD_GOTO must open a Dialog window");

    // Select-all the prefill and type the target line, then click OK.
    let edit_hwnd = dialog_edit(session, dialog_hwnd);
    assert_ne!(edit_hwnd, 0, "Go To dialog has its line-number EDIT");
    handle.post_message(edit_hwnd, EM_SETSEL, 0, u64::from(u32::MAX)); // (0, -1)
    for c in line.chars() {
        handle.post_message(edit_hwnd, WM_CHAR, u64::from(c as u32), 0);
    }
    for _ in 0..20 {
        let _ = session
            .run_until_stop(1_000_000)
            .expect("run after typing line");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let ok_hwnd = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, title, _)| cls == "#128" && title == "OK")
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(ok_hwnd, 0, "Go To dialog has its OK button");
    handle.post_message(dialog_hwnd, WM_COMMAND, IDOK, 0);

    // Pump until the dialog closes; settle a few cycles so the frame paints.
    let mut frame = None;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run after OK");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            break;
        }
        if let Some(f) = session.take_frame(main) {
            frame = Some(f);
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    frame.expect("a frame must be published after the Go To flow")
}

/// LIVE-symptom regression: after Go To line x, every visible row BELOW x
/// must still render its glyphs in the published frame.
///
/// The user reports the caret lands on line x but the rows after it come up
/// blank until a click forces a repaint. The trigger is the Go To flow's
/// modal-dialog close: `EndDialog` invalidates the owner with a pending
/// erase, the owner's `WM_ERASEBKGND` fills the WHOLE surface white (the
/// owner has no `WS_CLIPCHILDREN`, so the erase paints over the EDIT), and
/// the EDIT's subsequent repaint covers only its pending row band — leaving
/// every row outside the band blank. The rows below the target line are
/// exactly the rows the Go To's `EM_SETSEL` band does not cover.
#[test]
fn goto_line_deep_keeps_rows_below_visible() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_CHAR: u32 = 0x0102;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const VK_HOME: u64 = 0x24;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    assert_ne!(main, 0, "notepad main window exists");
    let tree = handle.window_menu_items();
    let goto_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("go to"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(goto_id, 0, "the Edit menu must contain a Go To command");

    let main_edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(main);
    assert_ne!(main_edit, 0, "notepad main EDIT exists");

    // A tall document: 45 numbered lines, far beyond the ~24-row viewport,
    // so a deep Go To jump changes which rows are visible.
    for n in 0..45 {
        for ch in format!("line {n}\r").chars() {
            handle.post_message(main_edit, WM_CHAR, u64::from(ch as u32), 0);
        }
    }
    handle.post_message(main_edit, WM_KEYDOWN, VK_HOME, 0);
    handle.post_message(main_edit, WM_KEYUP, VK_HOME, 0);
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run after typing");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    // Ink per 19 px text-row band across the whole client — non-white pixels
    // (the text glyphs; the edit background is COLOR_WINDOW-white).
    let ink_rows = |frame: &wie_winapi::present::SurfaceFrame| -> Vec<u32> {
        let mut rows = Vec::new();
        for y in (0..frame.height).step_by(19) {
            let mut ink = 0_u32;
            for x in 4..frame.width {
                let idx = usize::try_from(y).unwrap_or(0) * frame.width as usize
                    + usize::try_from(x).unwrap_or(0);
                if frame.pixels.get(idx).copied() != Some(0x00FF_FFFF) {
                    ink += 1;
                }
            }
            rows.push(ink);
        }
        rows
    };

    // Baseline: a first Go To (line 1) settles; then park the caret at the
    // current line's end (VK_END) and jump to line 20 — the reported case.
    // The deep jump's `EM_SETSEL` leaves a pending band over the top rows;
    // the modal close erases the owner surface, and the band-limited EDIT
    // repaint must NOT leave the rows below the target blank.
    let before = run_goto_flow(&mut session, &handle, main, goto_id, "1");
    handle.post_message(main_edit, WM_KEYDOWN, u64::from(0x23_u32), 0); // VK_END
    handle.post_message(main_edit, WM_KEYUP, u64::from(0x23_u32), 0);
    for _ in 0..10 {
        let summary = session.run_until_stop(1_000_000).expect("run after VK_END");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let after = run_goto_flow(&mut session, &handle, main, goto_id, "20");
    assert_eq!(after.width, before.width, "the frame size is stable");
    assert_eq!(after.height, before.height, "the frame size is stable");
    assert_eq!(
        handle.edit_selection(main_edit),
        Some((142, 142, 142)),
        "the Go To landed the caret on line 20 (char 142)"
    );

    let before_rows = ink_rows(&before);
    let after_rows = ink_rows(&after);
    // The rows below the target line — the 19 px bands spanning the edit's
    // lower client (the last band is the status bar and is excluded) — must
    // keep their glyph ink after the jump. A blank-rows regression collapses
    // every one of them to the ~16 px of the edit's left/right borders.
    let baseline_below: u32 = before_rows[14..25].iter().copied().sum();
    let after_below: u32 = after_rows[14..25].iter().copied().sum();
    assert!(
        after_below >= baseline_below.saturating_mul(3) / 4,
        "the rows below the Go To target line must still render their glyphs \
         (the owner erase + band-limited EDIT repaint blanks them): baseline \
         rows14..25 ink {baseline_below} -> after {after_below} (rows {after_rows:?})"
    );
}

/// The Go To command must NOT be dispatched by an unknown id: post id 999 and
/// require the guest to ignore it (no dialog, no exit). Guards the goto test
/// against a false positive where "any WM_COMMAND opens a dialog".
#[test]
fn goto_ignores_unknown_command_id() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, 999, 0);

    for _ in 0..60 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "Dialog")
        {
            panic!("an unknown WM_COMMAND id must not open the Go To dialog");
        }
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            panic!("notepad exited on an unknown command id");
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
