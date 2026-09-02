//! The notepad dialog flows (real_exes/notepad.exe): interactive File→Open /
//! Save / Save As dialogs, the in-app font dialog, the New-flow save prompt
//! (confirm) branches, and the ghost-modal exit regressions.

use crate::helpers::{
    BTNFACE_0RGB, count_edit_ink, gui_suite_serialize, pump_until_windows_ready, real_exe,
    wait_for_window_class,
};

// ---------------------------------------------------------------------------
// File-menu action repros (real_exes/notepad.exe): the interactive host
// delivery is proven (MenuEvent → WM_COMMAND works, Exit reacts); these pin
// whether the guest ACTIONS complete. CMD ids verified against the RT_MENU
// 0x201 template: New=256, New Window=257, Open=258, Save=259, Save As=260.
// ---------------------------------------------------------------------------

/// WM_COMMAND(CMD_OPEN) with the interactive file dialog enabled must make
/// the guest call GetOpenFileNameW and BUILD the host dialog (a
/// "FileDialog"-class window appears). If the dialog fails to build live
/// (the bottle-confinement suspect), no such window ever appears.
#[test]
fn notepad_file_open_builds_interactive_dialog() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const CMD_OPEN: u32 = 258;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_OPEN), 0);

    let opened = wait_for_window_class(&mut session, &handle, "FileDialog");
    // Dismiss the dialog if it appeared (IDCANCEL) so the session can end.
    if let Some(dialog) = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "FileDialog")
        .map(|(hwnd, ..)| *hwnd)
    {
        handle.post_message(dialog, WM_COMMAND, 2, 0); // IDCANCEL
        for _ in 0..50 {
            let _ = session.run_until_stop(1_000_000).expect("run");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        opened,
        "CMD_OPEN must build the interactive file dialog (a FileDialog window) — \
         the action does not complete if no dialog appears"
    );
    let _ = EntryTraceTermination::WaitingForMessage;
}

/// WM_COMMAND(CMD_SAVE_AS) with the interactive policy must build the dialog
/// too (Save As always prompts).
#[test]
fn notepad_file_save_as_builds_interactive_dialog() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();

    const WM_COMMAND: u32 = 0x0111;
    const CMD_SAVE_AS: u32 = 260;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_SAVE_AS), 0);

    let opened = wait_for_window_class(&mut session, &handle, "FileDialog");
    if let Some(dialog) = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "FileDialog")
        .map(|(hwnd, ..)| *hwnd)
    {
        handle.post_message(dialog, WM_COMMAND, 2, 0); // IDCANCEL
        for _ in 0..50 {
            let _ = session.run_until_stop(1_000_000).expect("run");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        opened,
        "CMD_SAVE_AS must build the interactive file dialog — the action does \
         not complete if no dialog appears"
    );
}

/// The font dialog (ChooseFontW, comdlg32) must SURVIVE a click on one of
/// its controls: the click's repaint publishes a frame that still carries the
/// dialog's pixels in the OWNER surface (the dialog composites into its
/// owner — it has no winit window of its own).
///
/// In-process repro of the reported "the dialog becomes invisible if I click
/// on it": drive notepad's Format > Font (the REAL menu → WM_COMMAND path),
/// wait for the dialog face in the owner's published frame, click the family
/// LISTBOX through the host hit-test + posting path (exactly what app.rs does
/// for winit mouse events), then require the face to STAY in the owner frame
/// across the click's repaint cycle. The click must select the listbox row,
/// not erase the dialog.
#[test]
fn notepad_font_dialog_survives_control_click() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const MK_LBUTTON: u64 = 0x0001;
    // The font dialog's size (comdlg32): the dialog is centered in the owner
    // and its controls sit at fixed offsets inside it.
    const FONT_DLG_CX: i32 = 340;
    const FONT_DLG_CY: i32 = 260;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_font_dialog_policy(wie_winapi::FontDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    // The Format > Font command id (the guest's own menu tree, like the bar).
    let font_id = handle
        .window_menu_items()
        .iter()
        .find_map(|top| {
            top.children
                .iter()
                .find(|child| child.title.to_lowercase().contains("font"))
                .map(|child| child.id)
        })
        .unwrap_or(0);
    assert_ne!(font_id, 0, "the Format menu must contain a Font command");

    // The font dialog is centered in the owner; its face sample is 5 px in
    // from the dialog's top-left corner (clear of the 1 px border and the
    // "Font:" label at x=8).
    let (_hwnd, _title, owner_w, owner_h) =
        handle
            .first_guest_window_info()
            .unwrap_or((0, String::new(), 0, 0));
    let dx = owner_w.saturating_sub(FONT_DLG_CX).saturating_div(2);
    let dy = owner_h.saturating_sub(FONT_DLG_CY).saturating_div(2);
    let face_px_at = |frame: &wie_winapi::present::SurfaceFrame| {
        let x = usize::try_from(dx + 5).unwrap_or(0);
        let y = usize::try_from(dy + 5).unwrap_or(0);
        frame.pixels.get(y * frame.stride as usize + x).copied()
    };

    handle.post_message(main, WM_COMMAND, u64::from(font_id), 0);

    let opened = wait_for_window_class(&mut session, &handle, "FontDialog");
    assert!(
        opened,
        "Format > Font must build the font dialog (a FontDialog window)"
    );

    // The family LISTBOX is at dialog (72,8,168,140); click row 2's band.
    let click_owner_x = dx + 72 + 40;
    let click_owner_y = dy + 8 + 40;

    let mut saw_face_before_click = false;
    let mut clicked = false;
    let mut lost_face_after_click = false;
    let mut saw_sel_change_effect = false;
    let mut settle_after_effect = 0;
    let mut iterations = 0;
    loop {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("notepad run_until_stop");
        iterations += 1;
        if iterations >= 800 {
            let face_now = session
                .first_guest_window_handle()
                .and_then(|owner| session.take_frame(owner))
                .map(|f| face_px_at(&f));
            let (fw, fh) = session
                .first_guest_window_handle()
                .and_then(|owner| session.take_frame(owner))
                .map(|f| (f.width, f.height))
                .unwrap_or((0, 0));
            // Where is BTNFACE in the frame? The first few rows that contain
            // it tell us where the dialog actually sits.
            let rows: Vec<u32> = session
                .first_guest_window_handle()
                .and_then(|owner| session.take_frame(owner))
                .map(|f| {
                    (0..fh)
                        .filter(|y| {
                            f.pixels.get(*y as usize * f.width as usize).copied()
                                == Some(BTNFACE_0RGB)
                        })
                        .take(5)
                        .collect()
                })
                .unwrap_or_default();
            panic!(
                "notepad font-dialog click session stalled: \
                 saw_face={saw_face_before_click} clicked={clicked} \
                 lost_face={lost_face_after_click} effect={saw_sel_change_effect} \
                 face_now={face_now:?} frame={fw}x{fh} bfnface_col0_rows={rows:?} \
                 font_dialog_open={}",
                session
                    .guest_windows_snapshot()
                    .iter()
                    .any(|(_, cls, ..)| cls == "FontDialog")
            );
        }
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            break;
        }

        if let Some(owner) = session.first_guest_window_handle()
            && let Some(frame) = session.take_frame(owner)
        {
            let face = face_px_at(&frame);
            if face == Some(BTNFACE_0RGB) {
                saw_face_before_click = true;
            }
            if clicked && saw_face_before_click && face != Some(BTNFACE_0RGB) {
                // The dialog's face was in the owner frame and the click's
                // repaint removed it — the reported click-invisibility.
                lost_face_after_click = true;
            }
            if clicked {
                // The clicked row must be highlighted in the listbox area
                // (the selection followed the click through the repaint).
                let highlight = (dy + 8..dy + 8 + 140).fold(0_u32, |acc, y| {
                    acc + (dx + 72..dx + 72 + 168).fold(0_u32, |acc, x| {
                        let idx = usize::try_from(y).unwrap_or(0) * frame.stride as usize
                            + usize::try_from(x).unwrap_or(0);
                        acc + u32::from(frame.pixels.get(idx).copied() == Some(0x0000_78D7))
                    })
                });
                if highlight > 100 {
                    saw_sel_change_effect = true;
                }
            }
        }

        if !clicked && saw_face_before_click {
            // The dialog is visible: click the family LISTBOX (host path).
            if let Some((hwnd, rx, ry)) = handle.window_at(click_owner_x, click_owner_y)
                && hwnd != 0
                && rx < 168
                && ry < 140
            {
                let lparam = u64::from(ry << 16 | rx);
                handle.post_message_at(
                    hwnd,
                    WM_LBUTTONDOWN,
                    MK_LBUTTON,
                    lparam,
                    click_owner_x,
                    click_owner_y,
                );
                handle.post_message_at(hwnd, WM_LBUTTONUP, 0, lparam, click_owner_x, click_owner_y);
                clicked = true;
            }
        }

        // Once the click's repaint published (the highlight is visible), let
        // a few more frames settle, then verify the dialog face survived.
        if clicked && saw_sel_change_effect {
            settle_after_effect += 1;
            if settle_after_effect >= 5 {
                break;
            }
        }

        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    assert!(
        saw_face_before_click,
        "the font dialog face (0xF0F0F0) never appeared in the owner frame"
    );
    assert!(
        !lost_face_after_click,
        "the font dialog face disappeared from the owner frame after a click \
         on the family listbox — the click-invisibility regression"
    );
    assert!(
        saw_sel_change_effect,
        "the click must select + highlight a listbox row (the selection \
         followed the click)"
    );
}

/// WM_COMMAND(CMD_NEW) must not kill the session (no emulation error), even
/// on a doc with typed text — the save-prompt path must fire without the
/// guest stopping. On a clean doc FileNew is a no-op; the assertion here is
/// that the session survives the command.
#[test]
fn notepad_file_new_survives_with_typed_text() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const CMD_NEW: u32 = 256;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    // Record the save-prompt MessageBox instead of answering it: FileNew on a
    // dirty doc must call MessageBoxW (the prompt). The bridge returns IDNO
    // (discard) so the action completes.
    let prompt_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let prompt = std::sync::Arc::clone(&prompt_fired);
    handle.set_message_box_bridge(Box::new(move |_, _, _| {
        prompt.store(true, std::sync::atomic::Ordering::SeqCst);
        7 // IDNO — discard the changes
    }));
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    // Type into the EDIT so the doc is dirty (FileNew must offer to save).
    let edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(main);
    for c in "hello".chars() {
        handle.post_message(edit, WM_CHAR, u64::from(c as u32), 0);
    }

    // Drain the typed text, then trigger FileNew.
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    handle.post_message(main, WM_COMMAND, u64::from(CMD_NEW), 0);

    // The session must keep running (the save prompt / FileNew completes);
    // an emulation error (the missing SHUFPD bug) stops it with RuntimeStop.
    let mut kept_running = true;
    for _ in 0..60 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        match summary.termination {
            EntryTraceTermination::ExitProcess { .. } => break,
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            EntryTraceTermination::RuntimeStop(msg) => {
                kept_running = false;
                eprintln!("DIAG NEW RuntimeStop: {msg}");
                break;
            }
            other => {
                eprintln!("DIAG NEW other: {other:?}");
            }
        }
    }
    assert!(
        kept_running,
        "CMD_NEW on a dirty doc must not stop the session (the save-prompt \
         path runs) — an emulation error here is the missing-instruction bug"
    );
    assert!(
        prompt_fired.load(std::sync::atomic::Ordering::SeqCst),
        "FileNew on a dirty doc must invoke the save-prompt MessageBox bridge — \
         if the prompt never fires, the action does not complete"
    );
}

/// The interactive File→Open dialog's first paint must reach the OWNER's
/// published surface while the dialog is open.
///
/// The FileDialog (like the FontDialog) is PARENTED to its owner — it has no
/// winit window of its own and composites into the owner's surface — so its
/// face pixels appear in `published[owner]`, never in a frame keyed by the
/// dialog hwnd. This pins the paint → publish seam of the "dialog invisible
/// until hover/click" report: the dialog's frame reliably reaches the owner's
/// published surface, so the reported loss is in the host present path (a
/// surface-acquire skip marking a frame presented without drawing it), not in
/// the guest paint path.
#[test]
fn notepad_file_dialog_paints_into_the_owner_surface() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();

    const WM_COMMAND: u32 = 0x0111;
    const CMD_OPEN: u32 = 258;
    // The file dialog is 360×200 (comdlg32 FILE_DLG_CX/CY), centered in the
    // owner's client. The sample is dialog-local (3,33) — the BTNFACE face
    // margin clear of the 1 px border, the EDIT (8,8,344,22) and the LISTBOX
    // (8,36,344,116) — resolved to owner coordinates from the frame size.
    const FILE_DLG_CX: i32 = 360;
    const FILE_DLG_CY: i32 = 200;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_OPEN), 0);

    let mut saw_dialog_record = false;
    let mut saw_dialog_face_in_owner = false;
    let mut dialog_hwnd = 0_u64;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some((dhwnd, ..)) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FileDialog")
        {
            dialog_hwnd = *dhwnd;
            saw_dialog_record = true;
        }
        if let Some(frame) = session.take_frame(main) {
            // The dialog is centered in the owner: (owner - 360x200) / 2.
            let dx = (i32::try_from(frame.width).unwrap_or(0) - FILE_DLG_CX).max(0) / 2;
            let dy = (i32::try_from(frame.height).unwrap_or(0) - FILE_DLG_CY).max(0) / 2;
            let (sx, sy) = (
                u32::try_from(dx.saturating_add(3)).unwrap_or(0),
                u32::try_from(dy.saturating_add(33)).unwrap_or(0),
            );
            if sx < frame.width && sy < frame.height {
                let idx = usize::try_from(sy).unwrap_or(0) * frame.stride as usize
                    + usize::try_from(sx).unwrap_or(0);
                if frame.pixels.get(idx).copied() == Some(BTNFACE_0RGB) {
                    saw_dialog_face_in_owner = true;
                }
            }
        }
        match summary.termination {
            wie_runtime::EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while the file dialog should be open");
            }
            wie_runtime::EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {}
        }
        if saw_dialog_record && saw_dialog_face_in_owner {
            break;
        }
    }

    // Dismiss the dialog so the session can wind down (the assertion ran
    // while it was open).
    if dialog_hwnd != 0 {
        handle.post_message(dialog_hwnd, WM_COMMAND, 2, 0); // IDCANCEL
        for _ in 0..50 {
            let _ = session.run_until_stop(1_000_000).expect("run");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    assert!(
        saw_dialog_record,
        "CMD_OPEN must build the interactive file dialog (a FileDialog window record)"
    );
    assert!(
        saw_dialog_face_in_owner,
        "the file dialog's BTNFACE must appear in the OWNER's published frame \
         while the dialog is open — the dialog composites into the owner surface, \
         so an invisible dialog means a lost host present, not a missing paint"
    );
}

/// WM_COMMAND(CMD_SAVE) on an untitled doc must build the Save As dialog
/// (the guest has no filename yet, so Save routes to GetSaveFileName).
#[test]
fn notepad_file_save_builds_interactive_dialog() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();

    const WM_COMMAND: u32 = 0x0111;
    const CMD_SAVE: u32 = 259;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_SAVE), 0);

    let opened = wait_for_window_class(&mut session, &handle, "FileDialog");
    if let Some(dialog) = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "FileDialog")
        .map(|(hwnd, ..)| *hwnd)
    {
        handle.post_message(dialog, WM_COMMAND, 2, 0); // IDCANCEL
        for _ in 0..50 {
            let _ = session.run_until_stop(1_000_000).expect("run");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        opened,
        "CMD_SAVE on an untitled doc must build the interactive Save As dialog \
         — the action does not complete if no dialog appears"
    );
}

/// The ghost-modal regression: after the interactive file dialog closes (OK),
/// the FIRST File→Exit click must make the guest exit.
///
/// Reported live: after closing any in-app modal (File→Open/Save, Format→Font)
/// the dialog visually closes but a GHOST modal state persists — File→Exit
/// needs TWO clicks. This drives the exact sequence through the real guest
/// (notepad): CMD_OPEN → interactive FileDialog → CMD_OPEN's modal loop →
/// OK (EndDialog) → modal loop exits → ONE CMD_EXIT → guest must exit.
#[test]
fn notepad_file_dialog_close_then_first_exit_click_exits() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    // The in-app (host-built) file dialog — what the GUI presenter enables.
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let tree = handle.window_menu_items();
    // The File menu's Open and Exit ids (like the menu-bar decode does).
    let open_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("pen"))
        .map(|child| child.id)
        .unwrap_or(0);
    let exit_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("xit"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(open_id, 0, "the File menu must contain an Open command");
    assert_ne!(exit_id, 0, "the File menu must contain an Exit command");

    // Stage 1: File→Open builds the interactive dialog and parks the guest's
    // in-guest modal loop on an empty queue (dialog_depth == 1).
    handle.post_message(main, WM_COMMAND, u64::from(open_id), 0);
    let mut dialog_hwnd = 0_u64;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some((dhwnd, ..)) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FileDialog")
        {
            dialog_hwnd = *dhwnd;
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while the file dialog should be open");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while opening: {other:?}"),
        }
    }
    assert_ne!(
        dialog_hwnd, 0,
        "CMD_OPEN must build the interactive file dialog"
    );

    // Stage 2: OK closes the dialog (the guest's modal loop consumes the
    // WM_QUIT EndDialog posted — the depth returns to 0).
    handle.post_message(dialog_hwnd, WM_COMMAND, 1, 0); // IDOK
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FileDialog")
        {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while closing the file dialog");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while closing: {other:?}"),
        }
    }
    assert!(
        !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FileDialog"),
        "OK must close the file dialog (EndDialog removes the subtree)"
    );

    // Stage 3: ONE File→Exit after the dialog is gone. A ghost modal state
    // (stale dialog_depth or a leftover dialog window) swallows this first
    // command — the guest idles on instead of exiting.
    handle.post_message(main, WM_COMMAND, u64::from(exit_id), 0);

    let mut exited = false;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            exited = true;
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        exited,
        "the FIRST File→Exit click after closing the modal dialog must make \
         notepad exit — a swallowed first command means a ghost modal state persists"
    );
}

/// Same sequence as [`notepad_file_dialog_close_then_first_exit_click_exits`],
/// but the final Exit command is delivered through the REAL GUI pump —
/// [`run_windowed`] parked on the message-signal condvar, woken by a host
/// thread's post (exactly what `wie-cli run --gui` does). The direct-drive
/// test above proves the emulation; this one proves the GUI pump does not
/// lose the first post after a modal dialog closes.
#[test]
fn notepad_modal_dialog_exit_survives_run_windowed_pump() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let tree = handle.window_menu_items();
    let open_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("pen"))
        .map(|child| child.id)
        .unwrap_or(0);
    let exit_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("xit"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(open_id, 0, "the File menu must contain an Open command");
    assert_ne!(exit_id, 0, "the File menu must contain an Exit command");

    // Open the dialog, OK it, and wait until the subtree is gone.
    handle.post_message(main, WM_COMMAND, u64::from(open_id), 0);
    let mut dialog_hwnd = 0_u64;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some((dhwnd, ..)) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FileDialog")
        {
            dialog_hwnd = *dhwnd;
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while the file dialog should be open");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while opening: {other:?}"),
        }
    }
    assert_ne!(
        dialog_hwnd, 0,
        "CMD_OPEN must build the interactive file dialog"
    );
    handle.post_message(dialog_hwnd, WM_COMMAND, 1, 0); // IDOK
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FileDialog")
        {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while closing the file dialog");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while closing: {other:?}"),
        }
    }

    // The GUI's MenuEvent analog: a host thread posts ONE CMD_EXIT while the
    // pump is parked on the message-signal condvar.
    let poster = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        handle.post_message(main, WM_COMMAND, u64::from(exit_id), 0);
    });

    let control = wie_runtime::GuiControl::new();
    let outcome = wie_runtime::run_windowed(&mut session, &control)
        .expect("run_windowed after the modal dialog must succeed");
    poster.join().expect("exit poster thread");

    assert!(
        matches!(outcome, wie_runtime::GuiOutcome::Exited(0)),
        "run_windowed must return Exited after ONE post-close CMD_EXIT; got {outcome:?}"
    );
}

/// The ghost-modal regression through the FONT dialog and the REAL host click
/// path: Format→Font opens the in-app font dialog, a host-posted
/// WM_LBUTTONDOWN/UP on its OK button closes it via the control → BN_CLICKED →
/// dialog-proc → EndDialog chain (exactly what the live GUI mouse produces),
/// and the FIRST File→Exit after the close must make the guest exit.
#[test]
fn notepad_font_dialog_ok_click_then_first_exit_exits() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const MK_LBUTTON: u64 = 0x0001;
    const CMD_FONT: u32 = 320; // Format→Font...

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    // The in-app font dialog — always the in-guest modal loop (no native
    // bridge exists for ChooseFontW).
    session.set_font_dialog_policy(wie_winapi::FontDialogPolicy::Interactive);
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let tree = handle.window_menu_items();
    let exit_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("xit"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(exit_id, 0, "the File menu must contain an Exit command");

    // Stage 1: Format→Font builds the font dialog and parks the modal loop.
    handle.post_message(main, WM_COMMAND, u64::from(CMD_FONT), 0);
    let mut dialog_hwnd = 0_u64;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let Some((dhwnd, ..)) = session
            .guest_windows_snapshot()
            .iter()
            .find(|(_, cls, ..)| cls == "FontDialog")
        {
            dialog_hwnd = *dhwnd;
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while the font dialog should be open");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while opening: {other:?}"),
        }
    }
    assert_ne!(
        dialog_hwnd, 0,
        "CMD_FONT must build the interactive font dialog"
    );

    // Stage 2: host-posted click on the OK button (a child of the dialog).
    let ok_hwnd = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, _cls, title, _)| title == "OK")
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(ok_hwnd, 0, "the font dialog must have an OK button");
    // The button's client rect is 80×24; click its center.
    let lparam = u64::from(u32::try_from((12 << 16) | 40).unwrap_or(0));
    handle.post_message(ok_hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam);
    handle.post_message(ok_hwnd, WM_LBUTTONUP, 0, lparam);

    // The dialog must close (EndDialog removes the subtree).
    let mut closed = false;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if !session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, cls, ..)| cls == "FontDialog")
        {
            closed = true;
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) while closing the font dialog");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly while closing: {other:?}"),
        }
    }
    assert!(
        closed,
        "the OK click must close the font dialog (EndDialog removes the subtree)"
    );

    // Stage 3: ONE File→Exit after the dialog is gone.
    handle.post_message(main, WM_COMMAND, u64::from(exit_id), 0);
    let mut exited = false;
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            exited = true;
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    assert!(
        exited,
        "the FIRST File→Exit click after the font dialog closes must make \
         notepad exit — a swallowed first command means a ghost modal state"
    );
}

/// REPRO: the native-bridge Save flow (the crash seam). RNotepad's
/// File→Save As → GetSaveFileNameW with a scripted NATIVE bridge picking an
/// OUT-OF-BOTTLE host file: the accept registers a pick-mount
/// (`Z:\pick1\{name}`), writes it back into `lpstrFile`, finishes the modal
/// frame, and the guest then re-opens the returned path with
/// CreateFileW/WriteFile. This is the reported crash: the session must
/// complete the save (file created on the pick path, session keeps running)
/// instead of dying right after the frame's "depth down".
#[test]
fn notepad_file_save_native_bridge_accept_completes_save_flow() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const CMD_SAVE: u32 = 259;

    // A temp bottle (the standing "filesystem ⇒ bottle" policy) plus an
    // OUT-OF-BOTTLE pick target: the native panel pick lands outside the
    // guest volumes, so the accept registers a pick-mount and returns the
    // mounted guest path (`Z:\pick{N}\...`) — the exact seam in the trace.
    let bottle =
        std::env::temp_dir().join(format!("wie-ofn-bridge-save-bottle-{}", std::process::id()));
    let _ = std::fs::create_dir_all(bottle.join("drive_c"));
    let picked_host = std::env::temp_dir().join(format!(
        "wie-ofn-bridge-save-picked-{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&picked_host);

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_bottle_root(Some(bottle.clone()));
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    let picked = picked_host.clone();
    handle.set_file_dialog_bridge(Box::new(move |request| {
        assert!(
            request.is_save,
            "CMD_SAVE on an untitled doc opens a Save panel"
        );
        Some(wie_winapi::FileDialogPick {
            host_path: picked.clone(),
        })
    }));
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(main);
    // Type text so the save writes something, then trigger File→Save.
    for c in "save me".chars() {
        handle.post_message(edit, WM_CHAR, u64::from(c as u32), 0);
    }
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    handle.post_message(main, WM_COMMAND, u64::from(CMD_SAVE), 0);

    // Drive the save flow to completion: the accept → pick-mount → guest
    // CreateFileW/WriteFile/CloseHandle chain must finish without the
    // session stopping (the reported crash dies right after the frame's
    // "depth down").
    let mut session_alive = true;
    let mut stop_message = String::new();
    let mut api_sequence: Vec<String> = Vec::new();
    for _ in 0..200 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        for event in &summary.events {
            api_sequence.push(format!(
                "{}!{} handled={} ret={:?}",
                event.library, event.name, event.handled, event.return_value
            ));
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { .. } => break,
            EntryTraceTermination::WaitingForMessage => {
                // The save flow wrote the file once the guest idles again;
                // keep pumping a bounded time for the write-back to land.
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            EntryTraceTermination::RuntimeStop(message) => {
                session_alive = false;
                stop_message = message.clone();
                eprintln!("DIAG SAVE RuntimeStop: {message}");
                break;
            }
            EntryTraceTermination::UnsupportedApi(message) => {
                session_alive = false;
                stop_message = message.clone();
                eprintln!("DIAG SAVE UnsupportedApi: {message}");
                break;
            }
            other => {
                eprintln!("DIAG SAVE other: {other:?}");
            }
        }
        if picked_host.is_file() {
            break;
        }
    }

    assert!(
        picked_host.is_file(),
        "the Save flow must create the REAL host file at the picked location \
         (the guest's CreateFileW/WriteFile on the mounted path); \
         session_alive={session_alive} stop={stop_message}\napi sequence:\n{}",
        api_sequence.join("\n")
    );
    assert!(
        session_alive,
        "the native-bridge Save flow must not stop the session (the reported \
         crash dies right after the accept + frame finish); stop={stop_message}"
    );
    // The saved file must hold the typed text (the guest's WriteFile on the
    // mounted path carries the edit content).
    let saved = std::fs::read(&picked_host).unwrap_or_default();
    assert!(
        String::from_utf8_lossy(&saved).contains("save me"),
        "the saved file must contain the typed text; got {:?}",
        String::from_utf8_lossy(&saved)
    );
    let _ = std::fs::remove_file(&picked_host);
    let _ = std::fs::remove_dir_all(&bottle);
}

/// REPRO: the native-bridge OPEN flow. RNotepad's File→Open with a scripted
/// NATIVE bridge picking an OUT-OF-BOTTLE host file must read the REAL file
/// through the pick-mount and keep the session alive — the reported crash
/// family is Open/Save, and this pins the Open side of the seam.
#[test]
fn notepad_file_open_native_bridge_accept_reads_picked_file() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const CMD_OPEN: u32 = 258;

    let bottle =
        std::env::temp_dir().join(format!("wie-ofn-bridge-open-bottle-{}", std::process::id()));
    let _ = std::fs::create_dir_all(bottle.join("drive_c"));
    let picked_host = std::env::temp_dir().join(format!(
        "wie-ofn-bridge-open-picked-{}.txt",
        std::process::id()
    ));
    let original = b"hello from the picked host file";
    std::fs::write(&picked_host, original).expect("seed the picked file");

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_bottle_root(Some(bottle.clone()));
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    let picked = picked_host.clone();
    handle.set_file_dialog_bridge(Box::new(move |request| {
        assert!(!request.is_save, "CMD_OPEN opens an Open panel");
        Some(wie_winapi::FileDialogPick {
            host_path: picked.clone(),
        })
    }));
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_OPEN), 0);

    let mut session_alive = true;
    let mut stop_message = String::new();
    let mut api_sequence: Vec<String> = Vec::new();
    for _ in 0..200 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        for event in &summary.events {
            api_sequence.push(format!(
                "{}!{} handled={} ret={:?}",
                event.library, event.name, event.handled, event.return_value
            ));
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { .. } => break,
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            EntryTraceTermination::RuntimeStop(message) => {
                session_alive = false;
                stop_message = message.clone();
                eprintln!("DIAG OPEN RuntimeStop: {message}");
                break;
            }
            EntryTraceTermination::UnsupportedApi(message) => {
                session_alive = false;
                stop_message = message.clone();
                eprintln!("DIAG OPEN UnsupportedApi: {message}");
                break;
            }
            other => {
                eprintln!("DIAG OPEN other: {other:?}");
            }
        }
    }

    assert!(
        session_alive,
        "the native-bridge OPEN flow must not stop the session (the reported \
         crash family is Open/Save); stop={stop_message}\napi sequence:\n{}",
        api_sequence.join("\n")
    );
    // The opened file's content must have landed in the main EDIT (the guest
    // reads it through the CreateFileMappingW → MapViewOfFile view): the
    // window title (SetWindowTextW) carries the picked file's name.
    assert!(
        session
            .guest_windows_snapshot()
            .iter()
            .any(|(_, _, title, _)| title.to_lowercase().contains("pick")),
        "the OPEN flow must load the picked file (the title carries its name); \
         api sequence:\n{}",
        api_sequence.join("\n")
    );

    // THE content path: the EDIT must hold the picked file's bytes (notepad
    // reads opened files exclusively through the mapped view). The reported
    // live bug is that the dialog works and the title updates but the edit
    // stays EMPTY — this pins the byte copy from the view into the edit.
    let edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(edit, 0, "notepad must have an EDIT control");
    let mut edit_text = String::new();
    for _ in 0..50 {
        edit_text = handle.control_text(edit).unwrap_or_default();
        if edit_text == String::from_utf8_lossy(original) {
            break;
        }
        // Keep pumping so the guest's post-read SetWindowText/EM_SETHANDLE
        // dispatch lands on the host control.
        let _ = session.run_until_stop(1_000_000).expect("run");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        edit_text,
        String::from_utf8_lossy(original),
        "the OPEN flow must load the picked file's CONTENT into the EDIT \
         (the dialog + title work but the edit was empty in the live bug); \
         api sequence:\n{}",
        api_sequence.join("\n")
    );
    let _ = std::fs::remove_file(&picked_host);
    let _ = std::fs::remove_dir_all(&bottle);
}

/// REPRO: a LARGE picked file (>4 KiB) must load in FULL into the EDIT.
/// The live report: the imported content stops partway (line 103 of a 166-line
/// file). This pins the byte count that actually lands and prints the API
/// sequence so the truncating handler is identifiable.
#[test]
fn notepad_file_open_large_picked_file_loads_in_full() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const CMD_OPEN: u32 = 258;

    let bottle =
        std::env::temp_dir().join(format!("wie-ofn-large-open-bottle-{}", std::process::id()));
    let _ = std::fs::create_dir_all(bottle.join("drive_c"));
    let picked_host = std::env::temp_dir().join(format!(
        "wie-ofn-large-open-picked-{}.txt",
        std::process::id()
    ));
    // Distinctive per-line content: 200 lines x ~53 bytes ≈ 10.6 KiB, far
    // above any plausible read cap. notepad normalizes LF to CRLF when it
    // adopts the view into the EDIT, so the expected text is the seed with
    // every `\n` expanded to `\r\n`.
    let original: Vec<u8> = (1..=200)
        .flat_map(|i| {
            format!("REPRO line {i:03} of two hundred - filler filler filler\n").into_bytes()
        })
        .collect();
    let expected = String::from_utf8_lossy(&original).replace('\n', "\r\n");
    std::fs::write(&picked_host, &original).expect("seed the picked file");

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_bottle_root(Some(bottle.clone()));
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    let picked = picked_host.clone();
    handle.set_file_dialog_bridge(Box::new(move |request| {
        assert!(!request.is_save, "CMD_OPEN opens an Open panel");
        Some(wie_winapi::FileDialogPick {
            host_path: picked.clone(),
        })
    }));
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_OPEN), 0);

    let mut session_alive = true;
    let mut stop_message = String::new();
    let mut api_sequence: Vec<String> = Vec::new();
    for _ in 0..200 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        for event in &summary.events {
            api_sequence.push(format!(
                "{}!{} handled={} ret={:?}",
                event.library, event.name, event.handled, event.return_value
            ));
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { .. } => break,
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            EntryTraceTermination::RuntimeStop(message) => {
                session_alive = false;
                stop_message = message.clone();
                break;
            }
            EntryTraceTermination::UnsupportedApi(message) => {
                session_alive = false;
                stop_message = message.clone();
                break;
            }
            other => {
                eprintln!("DIAG LARGE OPEN other: {other:?}");
            }
        }
    }

    assert!(
        session_alive,
        "the large-file OPEN flow must not stop the session; stop={stop_message}"
    );
    let edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(edit, 0, "notepad must have an EDIT control");
    let mut edit_text = String::new();
    for _ in 0..50 {
        edit_text = handle.control_text(edit).unwrap_or_default();
        if edit_text.len() >= expected.len() {
            break;
        }
        let _ = session.run_until_stop(1_000_000).expect("run");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        edit_text.len(),
        expected.len(),
        "the EDIT must hold the FULL picked file ({} units, {} bytes with \
         CRLF); got {} — last received line: {:?}\napi sequence:\n{}",
        expected.len(),
        original.len(),
        edit_text.len(),
        edit_text.lines().last(),
        api_sequence.join("\n")
    );
    assert_eq!(
        edit_text, expected,
        "the EDIT content must match the picked file, LF normalized to CRLF"
    );
    let _ = std::fs::remove_file(&picked_host);
    let _ = std::fs::remove_dir_all(&bottle);
}
/// (via the pick-mount) must re-open with the SAME content in the EDIT. This
/// pins the full content path — the save's WriteFile, the re-open's
/// CreateFileMappingW → MapViewOfFile view, and the guest's EM_SETHANDLE
/// adoption of the view copy.
#[test]
fn notepad_file_save_then_open_roundtrips_content() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const CMD_SAVE: u32 = 259;
    const CMD_OPEN: u32 = 258;

    let bottle =
        std::env::temp_dir().join(format!("wie-ofn-roundtrip-bottle-{}", std::process::id()));
    let _ = std::fs::create_dir_all(bottle.join("drive_c"));
    let picked_host = std::env::temp_dir().join(format!(
        "wie-ofn-roundtrip-picked-{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&picked_host);
    let roundtrip_text = "roundtrip content 42";

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_bottle_root(Some(bottle.clone()));
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    let picked = picked_host.clone();
    handle.set_file_dialog_bridge(Box::new(move |_request| {
        Some(wie_winapi::FileDialogPick {
            host_path: picked.clone(),
        })
    }));
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(main);

    // Stage 1: type text and Save it through the native bridge.
    for c in roundtrip_text.chars() {
        handle.post_message(edit, WM_CHAR, u64::from(c as u32), 0);
    }
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    handle.post_message(main, WM_COMMAND, u64::from(CMD_SAVE), 0);
    let mut saved = false;
    for _ in 0..200 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if picked_host.is_file() {
            saved = true;
            break;
        }
    }
    assert!(saved, "the Save must create the picked host file");
    let on_disk = std::fs::read(&picked_host).unwrap_or_default();
    assert!(
        String::from_utf8_lossy(&on_disk).contains(roundtrip_text),
        "saved file must hold the typed text; got {:?}",
        String::from_utf8_lossy(&on_disk)
    );

    // Stage 2: File→New (discard via the save prompt bridge — the doc is
    // clean after Save, so no prompt fires), then Open the same file back.
    handle.post_message(main, WM_COMMAND, u64::from(256_u32), 0); // CMD_NEW
    for _ in 0..50 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    handle.post_message(main, WM_COMMAND, u64::from(CMD_OPEN), 0);

    // The re-opened EDIT must hold the SAME content (the mapping view path).
    let mut edit_text = String::new();
    let mut open_alive = true;
    for _ in 0..200 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        edit_text = handle.control_text(edit).unwrap_or_default();
        if edit_text.contains(roundtrip_text) {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { .. } => break,
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            EntryTraceTermination::RuntimeStop(message) => {
                open_alive = false;
                eprintln!("DIAG ROUNDTRIP RuntimeStop: {message}");
                break;
            }
            EntryTraceTermination::UnsupportedApi(message) => {
                open_alive = false;
                eprintln!("DIAG ROUNDTRIP UnsupportedApi: {message}");
                break;
            }
            other => {
                eprintln!("DIAG ROUNDTRIP other: {other:?}");
            }
        }
    }
    assert!(open_alive, "the roundtrip OPEN must not stop the session");
    assert!(
        edit_text.contains(roundtrip_text),
        "the re-opened EDIT must hold the saved content; got {:?}",
        edit_text
    );

    let _ = std::fs::remove_file(&picked_host);
    let _ = std::fs::remove_dir_all(&bottle);
}
/// post File→New (CMD_NEW=256), answer the save prompt with "Don't Save"
/// (IDNO — the discard path), and assert the EDIT is CLEARED and the repaint
/// reflects it (the text ink disappears from the owner's published frame).
///
/// This is the exp-4 coverage gap: the MessageBox bridge's IDYES/IDNO
/// resolution is unit-tested in isolation, but the FULL guest continuation
/// (WM_COMMAND 256 → the save prompt → the guest clears the EDIT via
/// `SetWindowText(hEdit, NULL)`) was untested. RNotepad's `DIALOG_FileNew`
/// (dialog.c) clears with `SetWindowText(Globals.hEdit, NULL)` — a NULL text
/// pointer that the host handler must treat as "clear the text".
#[test]
fn notepad_file_new_discard_clears_the_edit() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const CMD_NEW: u32 = 256;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    // IDNO = "Don't Save" → discard the changes → FileNew clears the edit.
    let prompt_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let prompt = std::sync::Arc::clone(&prompt_fired);
    handle.set_message_box_bridge(Box::new(move |_, _, _| {
        prompt.store(true, std::sync::atomic::Ordering::SeqCst);
        7 // IDNO — discard
    }));
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(main);

    // Type enough text that the first line spans well past the caret band.
    for c in "hello world from wie".chars() {
        handle.post_message(edit, WM_CHAR, u64::from(c as u32), 0);
    }
    // Drain so the text is inserted and its repaint is published.
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    let ink_before = count_edit_ink(&session, main);
    assert!(
        ink_before > 40,
        "typed text must render ink in the edit band before FileNew (got {ink_before} px)"
    );

    // FileNew on the dirty doc → the save prompt fires → IDNO → the edit is
    // cleared and repainted.
    handle.post_message(main, WM_COMMAND, u64::from(CMD_NEW), 0);
    let mut ink_after = ink_before;
    for _ in 0..60 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        ink_after = count_edit_ink(&session, main);
        if ink_after < 10 {
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    assert!(
        prompt_fired.load(std::sync::atomic::Ordering::SeqCst),
        "FileNew on a dirty doc must fire the save prompt (the bridge)"
    );
    assert!(
        ink_after < 10,
        "FileNew + Don't Save must clear the EDIT and repaint it: ink went \
         {ink_before} → {ink_after} px (expected ~0 after the clear)"
    );
}

/// The New-flow's "Yes" branch: FileNew on a dirty doc answered with IDYES
/// ("Save") must route into the Save As dialog (`GetSaveFileNameW`), NOT
/// clear the edit directly.
///
/// This pins the live "New → confirm → no clear" report: RNotepad's
/// `DoCloseFile` (dialog.c) treats IDYES as "save first" — `DIALOG_FileSave`
/// → `DIALOG_FileSaveAs` → `GetSaveFileNameW`. If that save is refused (an
/// out-of-bottle pick now surfaces `FNERR_INVALIDFILENAME` instead of a silent
/// cancel), `DoCloseFile` returns FALSE and FileNew aborts — the edit is
/// correctly NOT cleared. So the user-visible "no clear" is the SAVE path
/// failing, not the clear machinery.
#[test]
fn notepad_file_new_yes_save_prompt_routes_to_save_dialog() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const CMD_NEW: u32 = 256;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    session.set_file_dialog_policy(wie_winapi::FileDialogPolicy::Interactive);
    let handle = session.guest_handle();
    // IDYES = "Save" → FileNew routes into the Save As dialog.
    let prompt_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let prompt = std::sync::Arc::clone(&prompt_fired);
    handle.set_message_box_bridge(Box::new(move |_, _, _| {
        prompt.store(true, std::sync::atomic::Ordering::SeqCst);
        6 // IDYES — save
    }));
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(main);
    for c in "hello".chars() {
        handle.post_message(edit, WM_CHAR, u64::from(c as u32), 0);
    }
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    handle.post_message(main, WM_COMMAND, u64::from(CMD_NEW), 0);
    let opened = wait_for_window_class(&mut session, &handle, "FileDialog");
    // Dismiss the dialog (IDCANCEL) so the session can wind down.
    if let Some(dialog) = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "FileDialog")
        .map(|(hwnd, ..)| *hwnd)
    {
        handle.post_message(dialog, WM_COMMAND, 2, 0); // IDCANCEL
        for _ in 0..50 {
            let _ = session.run_until_stop(1_000_000).expect("run");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    assert!(
        prompt_fired.load(std::sync::atomic::Ordering::SeqCst),
        "FileNew on a dirty doc must fire the save prompt (the bridge)"
    );
    assert!(
        opened,
        "FileNew answered 'Yes' must route into the Save As dialog \
         (GetSaveFileNameW) — the 'New → confirm → no clear' report is the \
         save path failing (an out-of-bottle pick now surfaces \
         FNERR_INVALIDFILENAME), not the clear machinery"
    );
}
