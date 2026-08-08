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
/// Replace mode stacks the command buttons: Find Next (8), Replace (38),
/// Replace All (68), Cancel (98) — mirror comdlg32/find.rs `cancel_y`.
const FIND_DLG_CANCEL_REPLACE_Y: i32 = 98;
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

/// LIVE-BUG regression: ONE click on the REPLACE dialog's Cancel must close it.
///
/// Reported: "Cancel in the Replace dialog takes two clicks — the first does
/// not close it." The Find dialog's single-click close is covered above; the
/// Replace dialog is TALLER (190 vs 150) and stacks a fourth button row
/// (Cancel at dialog-client y=98), so its click path is independently pinned
/// here. Dialog-gone after ONE Cancel click is the pass signal.
#[test]
fn replace_cancel_closes_dialog_in_one_click() {
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
        h.saturating_sub(FIND_DLG_CY_REPLACE) / 2,
    );
    // Dialog-face pixel count inside the dialog rect of the owner frame
    // (COLOR_BTNFACE) — the stale-face oracle from cancel_removes_dialog_pixels.
    let face_count = |session: &wie_runtime::RuntimeSession| -> u32 {
        let Some(frame) = session.take_frame(main) else {
            return 0;
        };
        let mut n = 0_u32;
        for py in dy..(dy + FIND_DLG_CY_REPLACE) {
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

    // Open the modeless Replace dialog (Search → Replace = 0x122).
    handle.post_message(main, WM_COMMAND, u64::from(CMD_REPLACE), 0);
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
    assert_ne!(dialog, 0, "replace dialog opens");
    assert!(
        face_visible,
        "replace dialog face is composited in the owner frame"
    );

    // Settle for a few caret-blink periods (the live interaction: the user
    // reads the dialog before clicking Cancel). The main EDIT's pending row
    // band narrows to the caret row — without the destroy-time band reset the
    // edit's post-close repaint would skip the dialog's vacated region and the
    // face would stay in the frame (the "Cancel takes two clicks" symptom).
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    // ONE click on Cancel (replace-mode layout: (244, 98) 88x26).
    let (cx, cy) = (
        dx + FIND_DLG_BTN_X + FIND_DLG_BTN_W / 2,
        dy + FIND_DLG_CANCEL_REPLACE_Y + FIND_DLG_BTN_H / 2,
    );
    let (cancel_hwnd, _, _) = handle
        .window_at(cx, cy)
        .expect("Cancel button is hit-testable at its center");
    let lparam = u64::from((cy << 16 | cx) as u32);
    handle.post_message_at(cancel_hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam, cx, cy);
    handle.post_message_at(cancel_hwnd, WM_LBUTTONUP, 0, lparam, cx, cy);

    // Pump until the dialog is destroyed — must close after ONE click.
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
    assert!(closed, "ONE click on Cancel closes the replace dialog");
    assert!(
        frame_face < 1000,
        "the owner frame must drop the replace dialog's pixels after ONE Cancel \
         click (stale-face bug): got {frame_face}"
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

/// Count pixels in the owner frame inside the dialog rect's rows
/// `[top, bottom)` (dialog-client relative) that equal `color`.
fn count_dialog_rows(
    frame: &wie_winapi::present::SurfaceFrame,
    dx: i32,
    dy: i32,
    top: i32,
    bottom: i32,
    color: u32,
) -> u32 {
    let mut n = 0_u32;
    for py in (dy + top).max(0)..(dy + bottom) {
        for px in dx.max(0)..(dx + FIND_DLG_CX) {
            if frame
                .pixels
                .get(py as usize * frame.width as usize + px as usize)
                .is_some_and(|&p| p == color)
            {
                n = n.saturating_add(1);
            }
        }
    }
    n
}

/// The shared scaffold for the dialog-face regression: an RNotepad session
/// with typed document text, no dialog open. Returns
/// `(session, handle, main, main_edit, dx, dy)`.
struct FaceSession {
    session: wie_runtime::RuntimeSession,
    handle: wie_runtime::GuestHandle,
    main: u64,
    main_edit: u64,
    dx: i32,
    dy: i32,
}

fn face_session_new(cy: i32) -> Option<FaceSession> {
    use wie_runtime::EntryTraceTermination;
    // None when real_exes/notepad.exe is absent (CI has no real exes) — the
    // callers skip instead of panicking.
    let path = real_exe("notepad.exe")?;
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

    // Document text so Find/Replace have a real match to act on, then park
    // the caret at the document start (RNotepad's forward search starts from
    // the caret).
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

    let (_, _, w, h) = handle
        .first_guest_window_info()
        .unwrap_or((0, String::new(), 0, 0));
    let (dx, dy) = (w.saturating_sub(FIND_DLG_CX) / 2, h.saturating_sub(cy) / 2);
    Some(FaceSession {
        session,
        handle,
        main,
        main_edit,
        dx,
        dy,
    })
}

/// Open the Find/Replace dialog (`cmd`), type "hello" into its search EDIT,
/// and settle the face paint. Returns the dialog hwnd.
fn face_dialog_open(
    session: &mut wie_runtime::RuntimeSession,
    handle: &wie_runtime::GuestHandle,
    main: u64,
    dx: i32,
    dy: i32,
    cmd: u32,
) -> u64 {
    use wie_runtime::EntryTraceTermination;
    handle.post_message(main, WM_COMMAND, u64::from(cmd), 0);
    let mut dialog = 0;
    let mut find_edit = 0;
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
        if dialog != 0
            && let Some((h1, _, _)) = handle.window_at(dx + FIND_DLG_FIELD_X + 10, dy + 8 + 11)
        {
            find_edit = h1;
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
    for ch in "hello".chars() {
        handle.post_message(find_edit, WM_CHAR, u64::from(ch as u32), 0);
    }
    for _ in 0..10 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    dialog
}

/// The "upper region" of the dialog: dialog-client rows 9..27, full width —
/// the face around the "Find what:" label (10,8,78,20) and the search EDIT
/// (92,8). A painted face makes BTNFACE the dominant color; a missing face
/// exposes the owner EDIT's COLOR_WINDOW.
fn upper_region_face(
    session: &wie_runtime::RuntimeSession,
    main: u64,
    dx: i32,
    dy: i32,
) -> (u32, u32) {
    let Some(frame) = session.take_frame(main) else {
        return (0, 0);
    };
    (
        count_dialog_rows(&frame, dx, dy, 9, 27, 0x00F0_F0F0),
        count_dialog_rows(&frame, dx, dy, 9, 27, 0x00FF_FFFF),
    )
}

/// Click the dialog button at dialog-client `(x, y)` (the center), pumping a
/// few cycles after the click.
fn click_dialog_button(
    session: &mut wie_runtime::RuntimeSession,
    handle: &wie_runtime::GuestHandle,
    dx: i32,
    dy: i32,
    x: i32,
    y: i32,
) {
    use wie_runtime::EntryTraceTermination;
    let (bx, by) = (dx + x, dy + y);
    let (button_hwnd, _, _) = handle
        .window_at(bx, by)
        .expect("dialog button is hit-testable at its center");
    let lparam = u64::from((by << 16 | bx) as u32);
    handle.post_message_at(button_hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam, bx, by);
    handle.post_message_at(button_hwnd, WM_LBUTTONUP, 0, lparam, bx, by);
    for _ in 0..15 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// LIVE-symptom regression: the Find/Replace dialog's upper-region background
/// must stay BTNFACE gray through a button click.
///
/// The user reports the strip around the "Find what:" label / edit row turns
/// WHITE (the owner EDIT's COLOR_WINDOW showing through) when a dialog button
/// is clicked — and sometimes just on open. The dialog composites into the
/// owner surface (it has no winit window), so the gray face is a paint
/// (`paint_dialog`) on the dialog's own WM_PAINT. The dangerous interaction:
/// the owner's full-client EDIT sits BELOW the dialog in z-order but paints
/// into the SAME surface, so an EDIT repaint over the dialog's rows erases
/// the face unless the edit's paint clips around the overlapping dialog.
/// Loop up to 50 open→click cycles to catch the timing-dependent form.
#[test]
fn find_dialog_face_stays_gray_through_find_next_click() {
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;
    let Some(FaceSession {
        mut session,
        handle,
        main,
        main_edit: _,
        dx,
        dy,
    }) = face_session_new(FIND_DLG_CY)
    else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };

    let mut post_click_failures = 0_u32;
    let mut open_failures = 0_u32;
    for cycle in 0..50 {
        let dialog = face_dialog_open(&mut session, &handle, main, dx, dy, CMD_FIND);
        assert_ne!(dialog, 0, "cycle {cycle}: find dialog opens");
        for _ in 0..6 {
            let summary = session.run_until_stop(1_000_000).expect("run");
            if let EntryTraceTermination::WaitingForMessage = summary.termination {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        let (gray_open, white_open) = upper_region_face(&session, main, dx, dy);
        if gray_open <= white_open {
            open_failures += 1;
            eprintln!("DIAG cycle {cycle} OPEN: gray={gray_open} white={white_open}");
        }

        // ONE Find Next click (find-mode layout: (244, 8) 88x26) — the guest
        // EM_SETSELs the match in the main EDIT, which repaints.
        click_dialog_button(
            &mut session,
            &handle,
            dx,
            dy,
            FIND_DLG_FIND_NEXT_X + FIND_DLG_BTN_W / 2,
            FIND_DLG_FIND_NEXT_Y + FIND_DLG_BTN_H / 2,
        );

        let (gray_after, white_after) = upper_region_face(&session, main, dx, dy);
        if gray_after <= white_after {
            post_click_failures += 1;
            eprintln!("DIAG cycle {cycle} AFTER-CLICK: gray={gray_after} white={white_after}");
        }

        // Close (Cancel, (244,40) 88x26) so the next cycle opens a fresh one.
        click_dialog_button(
            &mut session,
            &handle,
            dx,
            dy,
            FIND_DLG_BTN_X + FIND_DLG_BTN_W / 2,
            FIND_DLG_CANCEL_FIND_Y + FIND_DLG_BTN_H / 2,
        );
    }

    assert_eq!(
        open_failures, 0,
        "the find dialog's upper region must be BTNFACE gray on open"
    );
    assert_eq!(
        post_click_failures, 0,
        "Find Next must not turn the dialog's upper region white (owner EDIT leak)"
    );
}

/// The Replace button applies EM_REPLACESEL — the main EDIT's text changes
/// and the whole document reflows, so the EDIT repaints every row, including
/// the dialog's rows. This is the aggressive form of the leak.
#[test]
fn replace_dialog_face_stays_gray_through_replace_click() {
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    let Some(FaceSession {
        mut session,
        handle,
        main,
        main_edit: _,
        dx,
        dy,
    }) = face_session_new(FIND_DLG_CY_REPLACE)
    else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let dialog = face_dialog_open(&mut session, &handle, main, dx, dy, CMD_REPLACE);
    assert_ne!(dialog, 0, "replace dialog opens");

    // Focus + fill the replace field (the second #129 EDIT at dialog (92,36)).
    let (rx, ry) = (dx + FIND_DLG_FIELD_X + 10, dy + 36 + 11);
    let (replace_field, _, _) = handle
        .window_at(rx, ry)
        .expect("replace EDIT is hit-testable at its center");
    let rlparam = u64::from((ry << 16 | rx) as u32);
    handle.post_message_at(replace_field, WM_LBUTTONDOWN, MK_LBUTTON, rlparam, rx, ry);
    handle.post_message_at(replace_field, WM_LBUTTONUP, 0, rlparam, rx, ry);
    let replace_edit = session
        .guest_windows_snapshot()
        .iter()
        .filter(|(_, cls, ..)| cls == "#129")
        .nth(1)
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(replace_edit, 0, "replace dialog has its replace EDIT");
    for ch in "world".chars() {
        handle.post_message(replace_edit, WM_CHAR, u64::from(ch as u32), 0);
    }
    for _ in 0..10 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    let mut post_click_failures = 0_u32;
    for cycle in 0..30 {
        let (gray_open, white_open) = upper_region_face(&session, main, dx, dy);
        if gray_open <= white_open {
            eprintln!("DIAG REPLACE cycle {cycle} OPEN: gray={gray_open} white={white_open}");
        }

        // ONE Replace click (replace-mode layout: (244, 38) 88x26) — the
        // guest EM_SETSELs + EM_REPLACESELs, reflowing the whole document.
        click_dialog_button(
            &mut session,
            &handle,
            dx,
            dy,
            FIND_DLG_FIND_NEXT_X + FIND_DLG_BTN_W / 2,
            FIND_DLG_REPLACE_BTN_Y + FIND_DLG_BTN_H / 2,
        );

        let (gray_after, white_after) = upper_region_face(&session, main, dx, dy);
        if gray_after <= white_after {
            post_click_failures += 1;
            eprintln!(
                "DIAG REPLACE cycle {cycle} AFTER-CLICK: gray={gray_after} white={white_after}"
            );
        }
    }

    assert_eq!(
        post_click_failures, 0,
        "Replace must not turn the dialog's upper region white — the main \
         EDIT's full-reflow repaint must not overwrite the overlapping dialog \
         face (found {post_click_failures}/30 leaks)"
    );
}

/// The dialog composites into the owner surface at `(dx, dy)`. The main
/// EDIT — a SIBLING BELOW the dialog in z-order — paints into that SAME
/// surface, so a main-EDIT repaint over the dialog's rows erases the dialog
/// face unless the edit's paint clips around the overlapping dialog. A Find
/// Next click that lands the selection INSIDE the dialog's row range is the
/// trigger: the guest EM_SETSELs the match and the edit repaints those rows
/// white, destroying the face. This test drives the caret deep enough that
/// the match lands in the dialog's rows.
#[test]
fn find_dialog_face_survives_selection_in_its_row_range() {
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;
    let Some(FaceSession {
        mut session,
        handle,
        main,
        main_edit,
        dx,
        dy,
    }) = face_session_new(FIND_DLG_CY)
    else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };

    // Grow the document tall enough that line ~10 lands inside the dialog's
    // row range (dy=165, dialog rows 9..27 = owner rows 174..192; line N at
    // ~19 px each → line 10 at owner row ~190). Type 24 lines of "hello"
    // WITHOUT a trailing newline (a caret on the trailing empty line makes
    // VK_HOME a no-op).
    for _ in 0..23 {
        for ch in "hello".chars() {
            handle.post_message(main_edit, WM_CHAR, u64::from(ch as u32), 0);
        }
        handle.post_message(main_edit, WM_CHAR, 0x0D, 0); // Enter → next line
    }
    for ch in "hello".chars() {
        handle.post_message(main_edit, WM_CHAR, u64::from(ch as u32), 0);
    }
    // Park the caret at the document start so Find Next walks DOWN the
    // document (VK_HOME only reaches the current line's start on a multiline
    // EDIT; EM_SETSEL(0,0) is the document start).
    handle.post_message(main_edit, 0x00B1, 0, 0); // EM_SETSEL(0, 0)
    for _ in 0..10 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    let dialog = face_dialog_open(&mut session, &handle, main, dx, dy, CMD_FIND);
    assert_ne!(dialog, 0, "find dialog opens");

    // Drive Find Next repeatedly: each click advances the caret to the next
    // match, so after ~10 clicks the selection sits inside the dialog's rows.
    let mut leaked = 0_u32;
    for click in 0..18 {
        let (gray_before, white_before) = upper_region_face(&session, main, dx, dy);
        click_dialog_button(
            &mut session,
            &handle,
            dx,
            dy,
            FIND_DLG_FIND_NEXT_X + FIND_DLG_BTN_W / 2,
            FIND_DLG_FIND_NEXT_Y + FIND_DLG_BTN_H / 2,
        );
        let (gray_after, white_after) = upper_region_face(&session, main, dx, dy);
        if gray_after <= white_after {
            leaked += 1;
            eprintln!(
                "DIAG zorder click {click}: before gray={gray_before} white={white_before}, \
                 after gray={gray_after} white={white_after}"
            );
        }
    }
    assert_eq!(
        leaked, 0,
        "a Find Next selection inside the dialog's row range must not erase the \
         dialog face (the main EDIT repaints over the overlapping dialog): \
         {leaked}/18 clicks leaked"
    );
}

/// Drive one session through doc "catalog cat" → caret home → Find dialog →
/// type "cat" → (optional) "Match whole word" checkbox → ONE Find Next click.
///
/// Returns the leftmost x of the selection highlight (COLOR_HIGHLIGHT
/// 0x0000_78D7) in the owner frame, or `u32::MAX` when no selection appears.
/// With whole-word OFF the first match is the leading "catalog" (x ≈ text
/// origin); with whole-word ON the embedded match is rejected and the first
/// match is the standalone trailing "cat" (~8 chars to the right).
fn whole_word_probe(whole_word: bool) -> Option<u32> {
    use wie_runtime::EntryTraceTermination;
    // None when real_exes/notepad.exe is absent (CI has no real exes) — the
    // caller skips instead of panicking.
    let path = real_exe("notepad.exe")?;
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

    // 1. Document where whole-word matters: "cat" is embedded in "catalog" at
    // position 0 AND standalone at position 8. Park the caret at the start
    // (before the dialog opens — the dialog grabs keyboard focus).
    for ch in "catalog cat".chars() {
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

    let (dx, dy) = (
        w.saturating_sub(FIND_DLG_CX) / 2,
        h.saturating_sub(FIND_DLG_CY) / 2,
    );

    // 3. Type the search text; optionally check "Match whole word"
    // (dialog-client (16, 64) 140x20 → center (86, 74)); then Find Next.
    for ch in "cat".chars() {
        handle.post_message(find_edit, WM_CHAR, u64::from(ch as u32), 0);
    }
    if whole_word {
        let (wx, wy) = (dx + 16 + 70, dy + 64 + 10);
        let (whole_word_button, _, _) = handle
            .window_at(wx, wy)
            .expect("Match whole word checkbox is hit-testable at its center");
        let wlparam = u64::from((wy << 16 | wx) as u32);
        handle.post_message_at(
            whole_word_button,
            WM_LBUTTONDOWN,
            MK_LBUTTON,
            wlparam,
            wx,
            wy,
        );
        handle.post_message_at(whole_word_button, WM_LBUTTONUP, 0, wlparam, wx, wy);
    }
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

    // 4. The guest runs the search (with whole-word ON this exercises
    // msvcrt.dll!iswctype — the pre-fix crash: the dispatch bailed with
    // "unsupported UCRT export: iswctype" and the session stopped). Pump until
    // the selection highlight appears; fail loudly on any non-idle termination.
    for _ in 0..300 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if !matches!(
            summary.termination,
            EntryTraceTermination::WaitingForMessage
        ) {
            panic!(
                "session stopped during Find Next (whole_word={whole_word}): {:?} \
                 (rip={:#x}, last_api={})",
                summary.termination,
                summary.final_rip,
                summary
                    .events
                    .last()
                    .map(|e| format!("{}!{}", e.library.as_ref(), e.name.as_ref()))
                    .unwrap_or_else(|| "-".to_owned()),
            );
        }
        if let Some(owner) = session.first_guest_window_handle()
            && let Some(frame) = session.take_frame(owner)
        {
            let leftmost = frame
                .pixels
                .iter()
                .enumerate()
                .filter(|&(_, &p)| p == 0x0000_78D7)
                .map(|(i, _)| u32::try_from(i % frame.width as usize).unwrap_or(0))
                .min();
            if let Some(left) = leftmost {
                return Some(left);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Some(u32::MAX)
}

/// LIVE-BUG regression: "Match whole word" in the Find dialog must work (and
/// must not crash the session).
///
/// The guest (RNotepad) implements the whole-word check guest-side in
/// `NOTEPAD_FindTextAt` via `_istalnum` — which calls the imported
/// `msvcrt.dll!iswctype(c, _ALPHA|_DIGIT)`. WIE had no `iswctype` handler, so
/// the dispatch bailed with "unsupported UCRT export: iswctype" and the whole
/// session stopped the moment a whole-word search compared the characters
/// around a match ("checking Match whole word CRASHES the app").
///
/// Semantic proof: on "catalog cat", whole-word Find Next must reject the
/// embedded match at position 0 and select the standalone trailing "cat"
/// (~8 chars right of the substring selection), while the plain Find Next
/// selects the leading "catalog".
#[test]
fn whole_word_find_next_does_not_crash() {
    if real_exe("notepad.exe").is_none() {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    }
    let _suite = gui_suite_serialize();

    let Some(sub_left) = whole_word_probe(false) else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let Some(ww_left) = whole_word_probe(true) else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };

    assert_ne!(sub_left, u32::MAX, "plain Find Next selects a match");
    assert_ne!(ww_left, u32::MAX, "whole-word Find Next selects a match");
    assert!(
        sub_left < 30,
        "plain Find Next selects the leading 'catalog' (leftmost highlight \
         x={sub_left}, expected near the text origin)"
    );
    assert!(
        ww_left >= sub_left.saturating_add(40),
        "whole-word Find Next must skip the embedded 'catalog' match and \
         select the standalone trailing 'cat' (whole-word leftmost x={ww_left} \
         vs substring {sub_left})"
    );
}
