//! Regression coverage for the comdlg32 Find/Replace dialog re-test
//! (2026-08-05): the guest must receive `FINDMSGSTRING` under the message id
//! IT registered (`commdlg_FindReplace`), Find Next must drive a real
//! selection in the main EDIT, a Replace click must repaint immediately, and
//! closing the dialog must drop its composited face pixels from the owner
//! frame (no stale-face "second click" artifact).

use crate::helpers::{gui_suite_serialize, pump_until_windows_ready, real_exe};

const WM_COMMAND: u32 = 0x0111;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_CHAR: u32 = 0x0102;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const MK_LBUTTON: u64 = 0x0001;
const VK_HOME: u16 = 0x24;
const CMD_FIND: u32 = 288; // Search (Find) = 0x120
const CMD_REPLACE: u32 = 290; // Search (Replace) = 0x122

/// Find-dialog control rects (pixels in dialog-client space) — mirror
/// comdlg32/find.rs layout.
const FIND_DLG_CX: i32 = 340;
const FIND_DLG_CY: i32 = 150;
const FIND_DLG_CY_REPLACE: i32 = 190;
const FIND_DLG_FIND_NEXT_X: i32 = 244;
const FIND_DLG_FIND_NEXT_Y: i32 = 8;
const FIND_DLG_BTN_X: i32 = 244;
const FIND_DLG_FIELD_X: i32 = 92;
const FIND_DLG_REPLACE_BTN_Y: i32 = 38;
const FIND_DLG_CANCEL_FIND_Y: i32 = 40;
const FIND_DLG_BTN_W: i32 = 88;
const FIND_DLG_BTN_H: i32 = 26;

/// The modeless Replace dialog composites into the OWNER surface (it has no
/// winit window of its own). A Replace click must repaint the main EDIT's
/// selection immediately — the "Replace mutates the EDIT but the change
/// doesn't show until the next input" report. The guest selects the replaced
/// run in COLOR_HIGHLIGHT (0x0000_78D7); its appearance in the owner frame
/// after a SINGLE Replace click, with no further input, is the pass signal.
#[test]
fn replace_repaints_the_selection_without_a_second_input() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let main_edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(main, 0, "notepad main window exists");
    assert_ne!(main_edit, 0, "notepad main EDIT exists");
    let (_, _, w, h) = handle
        .first_guest_window_info()
        .unwrap_or((0, String::new(), 0, 0));
    let (dx, dy) = (
        w.saturating_sub(FIND_DLG_CX) / 2,
        h.saturating_sub(FIND_DLG_CY_REPLACE) / 2,
    );

    // 1. Document text, caret to start.
    for ch in "hello hello hello".chars() {
        handle.post_message(main_edit, WM_CHAR, u64::from(ch as u32), 0);
    }
    handle.post_message(main_edit, WM_KEYDOWN, u64::from(VK_HOME), 0);
    handle.post_message(main_edit, WM_KEYUP, u64::from(VK_HOME), 0);
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    assert!(
        crate::helpers::count_edit_ink(&session, main) > 0,
        "typed text renders in the main EDIT"
    );

    // 2. Open the modeless REPLACE dialog.
    handle.post_message(main, WM_COMMAND, u64::from(CMD_REPLACE), 0);
    let mut dialog = 0;
    let mut find_edit = 0;
    let mut replace_edit = 0;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some(hwnd) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FindDialog")
            .map(|(h, ..)| *h)
        {
            dialog = hwnd;
        }
        if dialog != 0 {
            // The replace dialog has TWO #129 EDITs: find at (92,8), replace
            // at (92,36) in dialog-client space; resolve by hit-testing.
            if let Some((h1, _, _)) = handle.window_at(dx + FIND_DLG_FIELD_X + 10, dy + 8 + 11) {
                find_edit = h1;
            }
            if let Some((h2, _, _)) = handle.window_at(dx + FIND_DLG_FIELD_X + 10, dy + 36 + 11) {
                replace_edit = h2;
            }
            if find_edit != 0 && replace_edit != 0 {
                break;
            }
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert_ne!(dialog, 0, "replace dialog opens");
    assert_ne!(find_edit, 0, "replace dialog has its find EDIT");
    assert_ne!(replace_edit, 0, "replace dialog has its replace EDIT");

    // 3. Find text, focus + fill the replace edit, then ONE click on Replace.
    for ch in "hello".chars() {
        handle.post_message(find_edit, WM_CHAR, u64::from(ch as u32), 0);
    }
    let (rx, ry) = (dx + FIND_DLG_FIELD_X + 10, dy + 36 + 11);
    let (replace_field, _, _) = handle
        .window_at(rx, ry)
        .expect("replace EDIT is hit-testable at its center");
    let replace_lparam = u64::from((ry << 16 | rx) as u32);
    handle.post_message_at(
        replace_field,
        WM_LBUTTONDOWN,
        MK_LBUTTON,
        replace_lparam,
        rx,
        ry,
    );
    handle.post_message_at(replace_field, WM_LBUTTONUP, 0, replace_lparam, rx, ry);
    for ch in "world".chars() {
        handle.post_message(replace_edit, WM_CHAR, u64::from(ch as u32), 0);
    }
    let (bx, by) = (
        dx + FIND_DLG_FIND_NEXT_X + FIND_DLG_BTN_W / 2,
        dy + FIND_DLG_REPLACE_BTN_Y + FIND_DLG_BTN_H / 2,
    );
    let (replace_button, _, _) = handle
        .window_at(bx, by)
        .expect("Replace button is hit-testable at its center");
    let lparam = u64::from((by << 16 | bx) as u32);
    handle.post_message_at(replace_button, WM_LBUTTONDOWN, MK_LBUTTON, lparam, bx, by);
    handle.post_message_at(replace_button, WM_LBUTTONUP, 0, lparam, bx, by);

    // 4. The guest applies FR_REPLACE → EM_SETSEL + EM_REPLACESEL and the
    // frame repaints the selected run WITHOUT any further input.
    let mut sel_highlight = 0_u32;
    for _ in 0..300 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some(owner) = session.first_guest_window_handle()
            && let Some(frame) = session.take_frame(owner)
        {
            sel_highlight = frame.pixels.iter().filter(|&&p| p == 0x0000_78D7).count() as u32;
            if sel_highlight > 0 {
                break;
            }
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    assert!(
        sel_highlight > 0,
        "one Replace click repaints the replaced run (COLOR_HIGHLIGHT) — no \
         second input required"
    );
}

/// The dialog face composites into the OWNER surface, so destroying it must
/// repaint the owner — otherwise the frame keeps the dead dialog's pixels
/// until an unrelated input ("Cancel takes two clicks": the first closes the
/// dialog guest-side, the second finally triggers the repaint that hides it).
/// The owner subtree invalidate in `destroy_find_dialog` is the regression
/// under test: after ONE Cancel click the dialog rect of the owner frame must
/// lose its COLOR_BTNFACE (0xF0F0F0) pixels.
#[test]
fn cancel_removes_dialog_pixels_from_owner_frame() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    assert_ne!(main, 0, "notepad main window exists");
    let (_, _, w, h) = handle
        .first_guest_window_info()
        .unwrap_or((0, String::new(), 0, 0));
    let (dx, dy) = (
        w.saturating_sub(FIND_DLG_CX) / 2,
        h.saturating_sub(FIND_DLG_CY) / 2,
    );

    // Dialog face pixels inside the dialog rect of the owner frame.
    let face_count = |session: &wie_runtime::RuntimeSession| -> u32 {
        let Some(frame) = session.take_frame(main) else {
            return 0;
        };
        let mut n = 0_u32;
        for py in dy..(dy + FIND_DLG_CY) {
            for px in dx..(dx + FIND_DLG_CX) {
                if frame
                    .pixels
                    .get(py as usize * frame.width as usize + px as usize)
                    .is_some_and(|&p| p & 0x00FF_FFFF == 0x00F0_F0F0)
                {
                    n = n.saturating_add(1);
                }
            }
        }
        n
    };

    // Open the find dialog; the face must appear in the owner frame.
    handle.post_message(main, WM_COMMAND, u64::from(CMD_FIND), 0);
    let mut dialog = 0;
    let mut face_visible = false;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some(hwnd) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FindDialog")
            .map(|(h, ..)| *h)
        {
            dialog = hwnd;
        }
        if dialog != 0 && face_count(&session) > 1000 {
            face_visible = true;
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert_ne!(dialog, 0, "find dialog opens");
    assert!(face_visible, "dialog face is composited in the owner frame");

    // ONE click on Cancel (find-mode layout: (244, 40) 88x26).
    let (cx, cy) = (
        dx + FIND_DLG_BTN_X + FIND_DLG_BTN_W / 2,
        dy + FIND_DLG_CANCEL_FIND_Y + FIND_DLG_BTN_H / 2,
    );
    let (cancel_hwnd, _, _) = handle
        .window_at(cx, cy)
        .expect("Cancel button is hit-testable at its center");
    let lparam = u64::from((cy << 16 | cx) as u32);
    handle.post_message_at(cancel_hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam, cx, cy);
    handle.post_message_at(cancel_hwnd, WM_LBUTTONUP, 0, lparam, cx, cy);

    // Pump until the dialog is destroyed; the frame must drop its pixels.
    let mut closed = false;
    let mut frame_face = u32::MAX;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FindDialog")
        {
            closed = true;
            frame_face = face_count(&session);
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(closed, "ONE click on Cancel closes the find dialog");
    assert!(
        frame_face < 1000,
        "the owner frame must drop the dialog's pixels after Cancel \
         (stale-face bug): got {frame_face}"
    );
}

/// Drive the guest through type → caret-home → Find → type-into-find-edit →
/// Find Next, then assert a selection highlight (COLOR_HIGHLIGHT) appears in
/// the owner surface.
///
/// The guest is RNotepad (real_exes/notepad.exe), which implements the whole
/// search guest-side: it registers `commdlg_FindReplace`, handles the posted
/// `FINDMSGSTRING`, reads the `FINDREPLACE` struct the host wrote back from
/// the find EDIT, runs `NOTEPAD_FindTextAt` from its caret, and applies
/// `EM_SETSEL` on the main EDIT. The caret must be at the document start —
/// RNotepad's forward search does not wrap around the end.
#[test]
fn find_next_selects_the_match_under_the_guest_registered_message_id() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let main_edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(main, 0, "notepad main window exists");
    assert_ne!(main_edit, 0, "notepad main EDIT exists");
    let (_, _, w, h) = handle
        .first_guest_window_info()
        .unwrap_or((0, String::new(), 0, 0));

    // 1. Type the document text, then move the caret to the document start
    // (before the dialog opens, so the main edit is still focused and the
    // VK_HOME reaches it — an open dialog would grab the key).
    for ch in "hello hello hello".chars() {
        handle.post_message(main_edit, WM_CHAR, u64::from(ch as u32), 0);
    }
    handle.post_message(main_edit, WM_KEYDOWN, u64::from(VK_HOME), 0);
    handle.post_message(main_edit, WM_KEYUP, u64::from(VK_HOME), 0);
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    assert!(
        crate::helpers::count_edit_ink(&session, main) > 0,
        "typed text renders in the main EDIT"
    );

    // 2. Open the modeless Find dialog.
    handle.post_message(main, WM_COMMAND, u64::from(CMD_FIND), 0);
    let mut dialog = 0;
    let mut find_edit = 0;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        for (hwnd, cls, _, _) in session.guest_windows_snapshot() {
            if cls == "FindDialog" {
                dialog = hwnd;
            } else if cls == "#129" && dialog != 0 {
                find_edit = hwnd;
            }
        }
        if dialog != 0 && find_edit != 0 {
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert_ne!(dialog, 0, "find dialog opens");
    assert_ne!(find_edit, 0, "find dialog has its EDIT");

    // 3. Type the search text into the find EDIT, then click Find Next.
    for ch in "hello".chars() {
        handle.post_message(find_edit, WM_CHAR, u64::from(ch as u32), 0);
    }
    let (dx, dy) = (
        w.saturating_sub(FIND_DLG_CX) / 2,
        h.saturating_sub(FIND_DLG_CY) / 2,
    );
    let (bx, by) = (
        dx + FIND_DLG_FIND_NEXT_X + FIND_DLG_BTN_W / 2,
        dy + FIND_DLG_FIND_NEXT_Y + FIND_DLG_BTN_H / 2,
    );
    let (button_hwnd, _, _) = handle
        .window_at(bx, by)
        .expect("Find Next button is hit-testable at its center");
    let lparam = u64::from((by << 16 | bx) as u32);
    handle.post_message_at(button_hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam, bx, by);
    handle.post_message_at(button_hwnd, WM_LBUTTONUP, 0, lparam, bx, by);

    // 4. The guest receives FINDMSGSTRING, finds the match, and EM_SETSELs it
    // — which repaints the selected run in COLOR_HIGHLIGHT (0x0000_78D7).
    let mut sel_highlight = 0_u32;
    for _ in 0..300 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some(owner) = session.first_guest_window_handle()
            && let Some(frame) = session.take_frame(owner)
        {
            sel_highlight = frame.pixels.iter().filter(|&&p| p == 0x0000_78D7).count() as u32;
            if sel_highlight > 0 {
                break;
            }
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    assert!(
        sel_highlight > 0,
        "Find Next selects the first match (COLOR_HIGHLIGHT visible); guest got \
         the FINDMSGSTRING the id it registered"
    );
}

/// The modeless Find dialog must be fully reusable after Cancel: closing posts
/// `FINDMSGSTRING` with `FR_DIALOGTERM` under the guest-registered id, the
/// guest drops its dialog state, and the next `FindTextW` builds a fresh
/// dialog (a new hwnd). Before the message-name fix the guest never saw
/// `FR_DIALOGTERM` and a second Find command silently reused the dead dialog —
/// the "cannot reopen after closing" report.
#[test]
fn find_dialog_reopens_after_cancel_close() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    assert_ne!(main, 0, "notepad main window exists");
    let (_, _, w, h) = handle
        .first_guest_window_info()
        .unwrap_or((0, String::new(), 0, 0));
    let (dx, dy) = (
        w.saturating_sub(FIND_DLG_CX) / 2,
        h.saturating_sub(FIND_DLG_CY) / 2,
    );

    let open_dialog = |session: &mut wie_runtime::RuntimeSession| -> u64 {
        handle.post_message(main, WM_COMMAND, u64::from(CMD_FIND), 0);
        for _ in 0..150 {
            let summary = session.run_until_stop(1_000_000).expect("run");
            if let Some(hwnd) = session
                .guest_windows_snapshot()
                .iter()
                .find(|(_, cls, ..)| cls == "FindDialog")
                .map(|(h, ..)| *h)
            {
                return hwnd;
            }
            if let EntryTraceTermination::WaitingForMessage = summary.termination {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        0
    };

    let first = open_dialog(&mut session);
    assert_ne!(first, 0, "first Find dialog opens");

    // Click Cancel (find-mode layout: (244, 40) 88x26 in dialog-client space).
    let (cx, cy) = (dx + 244 + 44, dy + 40 + 13);
    let (cancel_hwnd, _, _) = handle
        .window_at(cx, cy)
        .expect("Cancel button is hit-testable at its center");
    let lparam = u64::from((cy << 16 | cx) as u32);
    handle.post_message_at(cancel_hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam, cx, cy);
    handle.post_message_at(cancel_hwnd, WM_LBUTTONUP, 0, lparam, cx, cy);

    // Pump until the guest processed FR_DIALOGTERM and tore the dialog down.
    let mut closed = false;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FindDialog")
        {
            closed = true;
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(closed, "Cancel closes the Find dialog");

    let second = open_dialog(&mut session);
    assert_ne!(second, 0, "second Find dialog opens after close");
    assert_ne!(
        second, first,
        "the reopened dialog is a fresh window, not the dead one"
    );
}
