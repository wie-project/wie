//! RNotepad (real_exes/notepad.exe) scenarios: the WM_COMMAND menu dispatch,
//! the menu-tree stability gate, the idle-exit control, and the non-dialog
//! File actions (New Window). The font/confirm/save dialog flows live in
//! `dialogs.rs`.

use std::sync::Arc;

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
        if Arc::ptr_eq(&first, &next) {
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
