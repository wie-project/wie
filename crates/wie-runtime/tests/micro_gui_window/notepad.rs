//! RNotepad (real_exes/notepad.exe) scenarios: the WM_COMMAND menu dispatch,
//! the menu-tree stability gate, the idle-exit control, and the non-dialog
//! File actions (New Window). The font/confirm/save dialog flows live in
//! `dialogs.rs`.

use crate::helpers::{gui_suite_serialize, pump_until_windows_ready, real_exe};

/// In-process repro for the interactive notepad menubar regression: every
/// File-menu action (New/Open/Save/Save As/Exit) does nothing in the real
/// app. The host bar delivers the click by posting `WM_COMMAND(id, 0)` to the
/// main window's hwnd — this test posts the SAME message with the ids the bar
/// itself stamps (the parsed `window_menu_items` tree) and checks whether the
/// guest reacts. If the guest exits here, the posted-message → WndProc
/// dispatch works and the break is host-side (the muda MenuEvent → proxy →
/// user_event delivery). If not, the guest dispatch is broken.
#[test]
fn notepad_menu_command_reaches_the_guest_wndproc() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!(
            "skip: real_exes/notepad.exe not present (fetch with ./scripts/fetch-rnotepad.sh)"
        );
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();

    // Pump until notepad's main window exists and its menu is loaded (the
    // bar would mirror it via window_menu_items).
    let mut main_hwnd = 0;
    for _ in 0..300 {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("notepad run_until_stop");
        if let Some(hwnd) = session.first_guest_window_handle() {
            main_hwnd = hwnd;
            if !handle.window_menu_items().is_empty() {
                break;
            }
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) before its menu was ready");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly: {other:?}"),
        }
    }
    assert_ne!(main_hwnd, 0, "notepad must create its main window");
    assert!(
        !handle.window_menu_items().is_empty(),
        "notepad must load its menu (the bar mirrors it)"
    );

    // The File menu's Exit command — the ids the macOS bar stamps into the
    // native items and decodes back on click.
    let tree = handle.window_menu_items();
    let exit_id = tree
        .iter()
        .find_map(|top| {
            top.children
                .iter()
                .find(|child| child.title.to_lowercase().contains("xit"))
                .map(|child| child.id)
        })
        .unwrap_or(0);
    assert_ne!(exit_id, 0, "the File menu must contain an Exit command");

    // Post the exact WM_COMMAND the MenuEvent handler posts.
    handle.post_message(main_hwnd, WM_COMMAND, u64::from(exit_id), 0);

    let mut exited = false;
    for _ in 0..150 {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("notepad run_until_stop after WM_COMMAND");
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
        "WM_COMMAND(CMD_EXIT={exit_id}) posted to the main window must make \
         notepad exit — the guest dispatch is broken if it idles on"
    );
}

/// Control for the Exit reaction: an UNKNOWN command id (999, not in the
/// menu) must NOT make notepad exit — otherwise the Exit test's reaction was
/// "any WM_COMMAND exits", not a real CMD_EXIT dispatch.
#[test]
fn notepad_ignores_unknown_command_ids() {
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

    for _ in 0..300 {
        let _ = session.run_until_stop(1_000_000).expect("run");
        if session.first_guest_window_handle().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, 999, 0);

    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            panic!(
                "notepad exited on an unknown WM_COMMAND id — the Exit test is a false positive"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// L4 rebuild-gate diagnostic: `window_menu_items` must return a STABLE tree
/// for a single-window guest (same Arc from the cache), so the host bar's
/// per-Frame `sync_menu_bar` never rebuilds (and never tears down the native
/// menu mid-click). A focus move between windows is the only intended rebuild
/// trigger.
#[test]
fn notepad_menu_tree_is_stable_across_calls() {
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

    for _ in 0..300 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if session.first_guest_window_handle().is_some() && !handle.window_menu_items().is_empty() {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) before its menu was ready");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly: {other:?}"),
        }
    }

    let first = handle.window_menu_items();
    let mut saw_cache_hit = false;
    let mut saw_content_change = false;
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = session.run_until_stop(1_000_000).expect("run");
        let next = handle.window_menu_items();
        if std::sync::Arc::ptr_eq(&first, &next) {
            saw_cache_hit = true;
        }
        if next.as_ref() != first.as_ref() {
            saw_content_change = true;
        }
    }
    assert!(
        saw_cache_hit,
        "an idle single-window guest must hit the menu-tree cache (same Arc) \
         so the bar never rebuilds per frame — the menu_dirty flag must be \
         re-armed after the tree is consumed (regression: it was never reset, \
         so every call rebuilt and the native bar was torn down repeatedly)"
    );
    assert!(
        !saw_content_change,
        "an idle guest's menu tree must not change content between calls — \
         a changing tree would rebuild the native bar every Frame (and tear \
         it down mid-click)"
    );
}

/// Control for [`notepad_menu_command_reaches_the_guest_wndproc`]: an idle
/// notepad session must NOT exit on its own within the same pumping budget.
/// If this exits, the repro test's "guest reacted" was a false positive (the
/// runtime's idle-exit, not the WM_COMMAND).
#[test]
fn notepad_does_not_exit_while_idle() {
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

    for _ in 0..300 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if session.first_guest_window_handle().is_some() && !handle.window_menu_items().is_empty() {
            break;
        }
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => {
                panic!("notepad exited (code {code}) before its menu was ready");
            }
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => panic!("notepad stopped unexpectedly: {other:?}"),
        }
    }

    // Same budget as the repro's post-WM_COMMAND pump — but NO message posted.
    for _ in 0..150 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if matches!(
            summary.termination,
            EntryTraceTermination::ExitProcess { .. }
        ) {
            panic!("notepad exited while idle — the WM_COMMAND repro is a false positive");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// WM_COMMAND(CMD_NEW_WINDOW) must fire ShellExecuteW without stopping the
/// session. ShellExecuteW is a shell32 stub; the command must at least not
/// crash the guest.
#[test]
fn notepad_file_new_window_does_not_stop_the_session() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const CMD_NEW_WINDOW: u32 = 257;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    handle.post_message(main, WM_COMMAND, u64::from(CMD_NEW_WINDOW), 0);

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
                eprintln!("DIAG NEW_WINDOW RuntimeStop: {msg}");
                break;
            }
            other => {
                eprintln!("DIAG NEW_WINDOW other: {other:?}");
            }
        }
    }
    assert!(
        kept_running,
        "CMD_NEW_WINDOW must not stop the session (ShellExecuteW runs) — an \
         emulation error here is the missing-instruction bug"
    );
}

/// Regression for the revision-driven pull-based repaint latch: after ONE
/// File→New command (dirty doc → save prompt → Don't Save), the cleared EDIT
/// must be visible in the published owner frame with NO further input. The
/// guest's `SetWindowText(hEdit, NULL)` clear is a visible mutation; its
/// repaint has to reach the host on the command's own pump cycles — a stale
/// surface would keep showing the old text until an unrelated input.
#[test]
fn new_clears_edit_visibly_on_same_frame() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!(
            "skip: real_exes/notepad.exe not present (fetch with ./scripts/fetch-rnotepad.sh)"
        );
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
    assert_ne!(main, 0, "notepad main window exists");

    // Type enough text that the edit band carries real ink.
    for c in "hello world from wie".chars() {
        handle.post_message(edit, WM_CHAR, u64::from(c as u32), 0);
    }
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let ink_before = crate::helpers::count_edit_ink(&session, main);
    assert!(
        ink_before > 40,
        "typed text must render ink in the edit band before FileNew (got {ink_before} px)"
    );

    // ONE command; after it NO further input is posted — the cleared edit
    // must appear in a published frame on the command's own pump cycles.
    handle.post_message(main, WM_COMMAND, u64::from(CMD_NEW), 0);
    let mut ink_after = ink_before;
    for _ in 0..60 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        ink_after = crate::helpers::count_edit_ink(&session, main);
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
        "the cleared EDIT must be visible in the published frame with NO \
         further input (stale-surface repaint bug): ink went \
         {ink_before} -> {ink_after}"
    );
}

/// Count `BTNFACE` pixels in the bottom `rows` rows of `frame` — the
/// status-bar strip's face color. The frame is the main window's client; the
/// status bar hugs its bottom edge, so a visible bar contributes thousands of
/// BTNFACE pixels there, while a hidden bar (the EDIT repainted over the
/// strip) contributes none (the EDIT background is COLOR_WINDOW-white).
fn count_status_bar_face(frame: &wie_winapi::present::SurfaceFrame, rows: u32) -> u32 {
    let mut face = 0_u32;
    let h = frame.height;
    for y in h.saturating_sub(rows)..h {
        for x in 0..frame.width {
            let idx = usize::try_from(y).unwrap_or(0) * frame.stride as usize
                + usize::try_from(x).unwrap_or(0);
            if frame.pixels.get(idx).copied() == Some(0x00F0_F0F0) {
                face += 1;
            }
        }
    }
    face
}

/// LIVE-symptom regression: toggling the status bar OFF (View → Status Bar)
/// must remove the bar from the PUBLISHED frame with NO further input.
///
/// The user sees the bar STILL VISIBLE after the toggle until a click forces
/// a repaint — the stale strip pixels survive because hiding the bar mutates
/// state that the repaint latch must surface to the host. RNotepad's toggle
/// (`CMD_STATUSBAR`, notepad_res.h) flips `bShowStatusBar`, re-sizes the EDIT
/// over the bar's strip (`MoveWindow`), and `ShowWindow(hStatusBar, SW_HIDE)`.
/// The hidden bar's paint is skipped (real Windows discards a hidden window's
/// invalid region) and the EDIT's full-height repaint must cover the strip —
/// this test asserts that the strip is actually gone from the frame the idle
/// boundary publishes, with NOTHING posted after the command.
#[test]
fn status_bar_toggle_off_removes_the_strip_with_no_further_input() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!(
            "skip: real_exes/notepad.exe not present (fetch with ./scripts/fetch-rnotepad.sh)"
        );
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
    assert_ne!(main, 0, "notepad main window exists");

    // The status bar must exist before the toggle (notepad defaults to
    // showing it; the View menu mirrors the checked item).
    let status_bar = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "msctls_statusbar32")
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(
        status_bar, 0,
        "notepad must create its status bar on startup"
    );

    // The View menu's Status Bar command — the id the macOS bar stamps into
    // the native item and decodes back on click (mirrors the goto.rs
    // convention of reading the guest's own menu tree).
    let tree = handle.window_menu_items();
    let status_bar_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("status bar"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(
        status_bar_id, 0,
        "the View menu must contain a Status Bar command"
    );

    // Pump a few cycles so the startup frame (with the bar) is published.
    for _ in 0..10 {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("run to first frame");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let Some(before) = session.take_frame(main) else {
        panic!("the main window must publish a startup frame");
    };
    let before_face = count_status_bar_face(&before, 40);
    assert!(
        before_face > 1000,
        "precondition: the status-bar strip must be visible in the startup \
         frame (BTNFACE face pixels in the bottom 40 rows, got {before_face})"
    );

    // ONE command; after it NO further input is posted — the hidden bar's
    // strip must vanish from a published frame on the command's own pump
    // cycles (the stale-surface repaint bug the user reports), and STAY gone
    // across every subsequent frame. The bar is hidden but its SB_SETTEXTW
    // invalidation outlives the hide; if the paint synthesizer queued its
    // WM_PAINT, the bar would re-draw its strip over the edit's erase on a
    // later cycle (the live ordering), so the assertion is that the strip
    // never reappears in ANY published frame.
    handle.post_message(main, WM_COMMAND, u64::from(status_bar_id), 0);

    let mut after_face = before_face;
    let mut observations = 0_u32;
    let mut reappeared = false;
    for _ in 0..80 {
        let summary = session.run_until_stop(1_000_000).expect("run after toggle");
        if let Some(frame) = session.take_frame(main) {
            observations += 1;
            after_face = count_status_bar_face(&frame, 40);
            if after_face != 0 {
                reappeared = true;
            }
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    assert!(
        observations > 1,
        "the pump must observe several published frames after the toggle"
    );
    assert_eq!(
        after_face, 0,
        "toggling the status bar OFF must remove its BTNFACE strip from the \
         published frame with NO further input (stale-surface repaint bug): \
         face pixels went {before_face} -> {after_face} — the user sees the \
         bar until a click forces a repaint"
    );
    assert!(
        !reappeared,
        "the hidden bar's strip must never re-appear in a later frame — the \
         SB_SETTEXTW invalidation outlives the hide, and a synthesized \
         WM_PAINT for the hidden bar would re-draw it over the edit's erase"
    );
}

/// LIVE-symptom regression: Edit → Time/Date on a full selection replaces
/// the whole document with the date string, and every row BELOW the inserted
/// date must come up blank in the published frame — the vacated rows must
/// not keep the old text's pixels (the "line under the replaced selection
/// takes a few instants to clear" report).
#[test]
fn time_date_replace_all_clears_rows_below_the_date() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const EM_SETSEL: u32 = 0x00B1;
    const CMD_TIME_DATE: u32 = 0x117;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
    pump_until_windows_ready(&mut session);

    let main = session.first_guest_window_handle().unwrap_or(0);
    let edit = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "EDIT")
        .map(|(h, ..)| *h)
        .unwrap_or(main);
    assert_ne!(main, 0, "notepad main window exists");
    assert_ne!(edit, 0, "notepad main EDIT exists");

    // A multi-line document so a full replace leaves many vacated rows.
    for n in 0..20 {
        for ch in format!("line {n}\r").chars() {
            handle.post_message(edit, WM_CHAR, u64::from(ch as u32), 0);
        }
    }
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run after typing");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    // Ink in the rows below the first text line, BEFORE the replace — the
    // rows that must end up blank.
    let ink_below = |session: &wie_runtime::RuntimeSession| -> u32 {
        let Some(frame) = session.take_frame(main) else {
            return 0;
        };
        let mut ink = 0_u32;
        for y in 60..frame.height.saturating_sub(40) {
            for x in 4..frame.width.saturating_sub(620) {
                let idx = usize::try_from(y).unwrap_or(0) * frame.stride as usize
                    + usize::try_from(x).unwrap_or(0);
                if frame.pixels.get(idx).copied() != Some(0x00FF_FFFF) {
                    ink += 1;
                }
            }
        }
        ink
    };
    assert!(
        ink_below(&session) > 500,
        "precondition: the rows below the first line carry the multi-line text"
    );

    // Select all, then ONE Time/Date command; after it NO further input is
    // posted — the vacated rows must be blank in a published frame on the
    // command's own pump cycles.
    handle.post_message(edit, EM_SETSEL, 0, u64::from(u32::MAX)); // (0, -1)
    for _ in 0..10 {
        let _ = session
            .run_until_stop(1_000_000)
            .expect("run after select-all");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    handle.post_message(main, WM_COMMAND, u64::from(CMD_TIME_DATE), 0);

    let mut ink_after = u32::MAX;
    for _ in 0..80 {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("run after Time/Date");
        ink_after = ink_below(&session);
        if ink_after < 60 {
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    assert!(
        ink_after < 60,
        "Time/Date on a full selection must clear the vacated rows with NO \
         further input (stale-text repaint bug): below-rows ink stayed \
         {ink_after} px"
    );
}

/// LIVE-symptom regression: New resets the caret to the document start, so
/// the status-bar Ln/Col indicator must read "Line 1, column 1" on the New
/// command's OWN pump cycles (the "Col N doesn't return to 1 instantly"
/// report). The guest refreshes the bar via `DIALOG_StatusBarUpdateAll` after
/// `SetWindowText(hEdit, NULL)`; if the host fails to reset the edit caret,
/// the refresh reads the stale position and the bar keeps the old column.
#[test]
fn new_resets_status_bar_line_col_to_one() {
    let Some(path) = real_exe("notepad.exe") else {
        eprintln!("skip: real_exes/notepad.exe not present");
        return;
    };
    let _suite = gui_suite_serialize();
    use wie_runtime::EntryTraceTermination;

    const WM_COMMAND: u32 = 0x0111;
    const WM_CHAR: u32 = 0x0102;
    const CMD_NEW: u32 = 0x100;
    const SBPART_CURPOS: usize = 0;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("notepad session starts");
    let handle = session.guest_handle();
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
    let status_bar = session
        .guest_windows_snapshot()
        .iter()
        .find(|(_, cls, ..)| cls == "msctls_statusbar32")
        .map(|(h, ..)| *h)
        .unwrap_or(0);
    assert_ne!(main, 0, "notepad main window exists");
    assert_ne!(edit, 0, "notepad main EDIT exists");
    assert_ne!(status_bar, 0, "notepad status bar exists");

    // Type a multi-line document and leave the caret somewhere with a column
    // > 1, so the pre-New status-bar part reads e.g. "Line 2, column 4".
    for n in 0..3 {
        for ch in format!("line {n}\r").chars() {
            handle.post_message(edit, WM_CHAR, u64::from(ch as u32), 0);
        }
    }
    for _ in 0..30 {
        let summary = session.run_until_stop(1_000_000).expect("run after typing");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let before_text = handle.status_bar_part_text(status_bar, SBPART_CURPOS);

    // ONE New command; the guest clears the edit and refreshes the bar. The
    // save prompt bridge answers IDNO, so no further input is needed.
    handle.post_message(main, WM_COMMAND, u64::from(CMD_NEW), 0);

    let mut after_text = None;
    for _ in 0..80 {
        let summary = session.run_until_stop(1_000_000).expect("run after New");
        after_text = handle.status_bar_part_text(status_bar, SBPART_CURPOS);
        if after_text
            .as_deref()
            .is_some_and(|t| t == "Line 1, column 1")
        {
            break;
        }
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    assert!(
        prompt_fired.load(std::sync::atomic::Ordering::SeqCst),
        "New on a dirty doc must fire the save prompt (the bridge)"
    );
    assert_eq!(
        after_text.as_deref(),
        Some("Line 1, column 1"),
        "New must reset the status-bar Ln/Col indicator to the document start \
         on its own pump cycles (the guest refreshes the bar after \
         SetWindowText(NULL); a stale edit caret makes it read the old \
         position) — before New it was {before_text:?}"
    );
}

/// Wake/publish protocol pin for the "status bar persists until a click"
/// class: the status-bar toggle publishes exactly ONE frame (the edit's
/// full repaint over the strip), and that frame's publish MUST fire the
/// presenter wake in the SAME run_until_stop that makes the strip-free frame
/// observable. A wake that fires BEFORE the paint (a pre-paint
/// `request_paint` latch bump) would make the host's redraw present the
/// pre-toggle frame, with no follow-up wake to correct it — the stale strip
/// stays until the next input. The host presenter itself is not headlessly
/// testable (it needs a real wgpu/winit window), so this pins the
/// wake/publish half of the protocol the presenter consumes.
#[test]
fn status_bar_toggle_wakes_with_its_publish() {
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
    assert_ne!(main, 0, "notepad main window exists");
    let tree = handle.window_menu_items();
    let status_bar_id = tree
        .iter()
        .flat_map(|top| top.children.iter())
        .find(|child| child.title.to_lowercase().contains("status bar"))
        .map(|child| child.id)
        .unwrap_or(0);
    assert_ne!(
        status_bar_id, 0,
        "the View menu must contain a Status Bar command"
    );

    // A wake counter — the wake runs on the guest thread inside publish
    // (which holds the state lock), so it can only bump the counter; the
    // pump loop reads the count around each run_until_stop.
    let wake_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let wc = std::sync::Arc::clone(&wake_count);
    handle.set_wake(Box::new(move || {
        wc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));

    // Settle the startup frame; the wake count must be quiescent before the
    // toggle (no pending pre-paint wake).
    for _ in 0..10 {
        let summary = session.run_until_stop(1_000_000).expect("run");
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let wakes_before_toggle = wake_count.load(std::sync::atomic::Ordering::SeqCst);

    handle.post_message(main, WM_COMMAND, u64::from(status_bar_id), 0);

    let mut face = u32::MAX;
    let mut wakes_before_iter = wakes_before_toggle;
    let mut publish_woke_with_frame = false;
    for _ in 0..80 {
        let summary = session.run_until_stop(1_000_000).expect("run after toggle");
        let wakes_after = wake_count.load(std::sync::atomic::Ordering::SeqCst);
        if let Some(frame) = session.take_frame(main) {
            face = count_status_bar_face(&frame, 40);
            if face == 0 {
                // The strip-free frame first became observable in THIS
                // run_until_stop. The publish that produced it must have
                // fired the presenter wake WITHIN this iteration (the publish
                // fires the stored wake inside drain_pending_publishes) — a
                // publish that forgot to wake would leave the count flat, and
                // the host's redraw (from any earlier wake) would present the
                // pre-toggle frame with no follow-up. The toggle's ShowWindow
                // also fires a legitimate pre-paint wake (the hidden child's
                // owner-invalidation at mgr.rs), so the DELTA may be > 1; the
                // invariant is that the frame's own iteration woke.
                publish_woke_with_frame = wakes_after > wakes_before_iter;
                break;
            }
        }
        wakes_before_iter = wakes_after;
        if let EntryTraceTermination::WaitingForMessage = summary.termination {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    assert_eq!(
        face, 0,
        "precondition: the toggle must drop the strip from the published frame"
    );
    assert!(
        publish_woke_with_frame,
        "the status-bar toggle's strip-free frame must be published in the          same run_until_stop that fires the presenter wake (the publish wakes          inside drain_pending_publishes; a flat wake count means the frame          would never reach the OS window): wakes before toggle          {wakes_before_toggle}"
    );
}
