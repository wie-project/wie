//! Tests for the `GuestHandle` accessors (split out of `window.rs`
//! for the file-size policy; test files are exempt).

#[cfg(test)]
use super::GuestHandle;
use crate::memory::DEFAULT_LAYOUT;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use wie_winapi::handles::Hwnd;
use wie_winapi::user32::Dimension;
use wie_winapi::user32::menu::{MenuEntry, MenuRecord};
use wie_winapi::vfs::VolumeConfig;

/// `window_menu_items` returns the cached tree while `menu_dirty` is
/// false and rebuilds (seeing new items) once a mutation dirties it.
#[test]
fn window_menu_items_cache_invalidates_on_dirty() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "menu.exe".to_owned(),
        module_path: r"C:\App\menu.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "menu.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    let menu_handle = 0x0000_0000_6620_0000_u64;
    let submenu = 0x0000_0000_6620_0001_u64;
    {
        let ws = winapi_state.window_state();
        ws.menus.push(MenuRecord {
            handle: wie_winapi::handles::Hmenu::from(menu_handle),
            items: vec![MenuEntry::Popup {
                text: "File".to_owned(),
                submenu: wie_winapi::handles::Hmenu::from(submenu),
            }],
        });
        ws.menus.push(MenuRecord {
            handle: wie_winapi::handles::Hmenu::from(submenu),
            items: vec![
                MenuEntry::Item {
                    id: 100,
                    text: "Exit".to_owned(),
                    enabled: true,
                    checked: false,
                },
                MenuEntry::Separator,
                MenuEntry::Item {
                    id: 200,
                    text: "About".to_owned(),
                    enabled: true,
                    checked: false,
                },
            ],
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(0x100),
            menu_handle,
            ..Default::default()
        });
        winapi_state.sync_window_mirror();
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    let first = handle.window_menu_items();
    assert_eq!(first.len(), 1, "File popup at the top level");
    let file = first.first().expect("popup");
    // The RNotepad File shape: Exit / separator / About.
    assert_eq!(
        file.children.len(),
        3,
        "Exit / separator / About inside File"
    );
    assert!(
        file.children.get(1).expect("separator").separator,
        "the MF_SEPARATOR between Exit and About renders as a separator node"
    );
    // Same tree from the cache (dirty is false — no rebuild).
    assert_eq!(handle.window_menu_items(), first);

    // Mutate through the guest-facing state and dirty the tree.
    {
        let mut state = handle.state.lock().expect("lock state");
        let ws = state.window_state();
        let submenu_record = ws
            .menus
            .iter_mut()
            .find(|m| m.handle == wie_winapi::handles::Hmenu::from(submenu))
            .expect("submenu record");
        submenu_record.items.push(MenuEntry::Item {
            id: 300,
            text: "Open".to_owned(),
            enabled: true,
            checked: false,
        });
        ws.menu_dirty = true;
        state.sync_window_mirror();
    }
    let second = handle.window_menu_items();
    assert_eq!(
        second.first().expect("popup").children.len(),
        4,
        "rebuild must reflect the appended item"
    );
}

/// The node-side half of the F4 menu-state round trip: a guest
/// `EnableMenuItem` / `CheckMenuItem` mutation dirties the tree, and the
/// rebuilt `MenuNode` carries the new enabled/checked flags — the state
/// the host bar (`menu_item_states` in wie-cli) turns into muda
/// `set_enabled` / `set_checked` calls.
#[test]
fn menu_node_state_reflects_guest_enable_check_mutations() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "state.exe".to_owned(),
        module_path: r"C:\App\state.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "state.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    let menu_handle = 0x0000_0000_6620_0000_u64;
    {
        let ws = winapi_state.window_state();
        ws.menus.push(MenuRecord {
            handle: wie_winapi::handles::Hmenu::from(menu_handle),
            items: vec![MenuEntry::Item {
                id: 100,
                text: "Paste".to_owned(),
                enabled: true,
                checked: false,
            }],
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(0x100),
            menu_handle,
            ..Default::default()
        });
        winapi_state.sync_window_mirror();
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    let first = handle.window_menu_items();
    let paste = first.first().expect("item");
    assert!(paste.enabled, "fresh items start enabled");
    assert!(!paste.checked, "fresh items start unchecked");

    // EnableMenuItem(MF_GRAYED|MF_BYCOMMAND) — greys the item in the
    // native tree, exactly what the user32 handler's mutate_item does.
    {
        let mut state = handle.state.lock().expect("lock state");
        let ws = state.window_state();
        let record = ws
            .menus
            .iter_mut()
            .find(|m| m.handle == wie_winapi::handles::Hmenu::from(menu_handle))
            .expect("menu record");
        if let Some(MenuEntry::Item { enabled, .. }) = record.items.first_mut() {
            *enabled = false;
        }
        ws.menu_dirty = true;
        state.sync_window_mirror();
    }
    let greyed = handle.window_menu_items();
    assert!(
        !greyed.first().expect("item").enabled,
        "rebuild must surface the greyed state"
    );

    // CheckMenuItem(MF_CHECKED|MF_BYCOMMAND) — checks the item.
    {
        let mut state = handle.state.lock().expect("lock state");
        let ws = state.window_state();
        let record = ws
            .menus
            .iter_mut()
            .find(|m| m.handle == wie_winapi::handles::Hmenu::from(menu_handle))
            .expect("menu record");
        if let Some(MenuEntry::Item {
            enabled, checked, ..
        }) = record.items.first_mut()
        {
            *enabled = false;
            *checked = true;
        }
        ws.menu_dirty = true;
        state.sync_window_mirror();
    }
    let checked = handle.window_menu_items();
    let item = checked.first().expect("item");
    assert!(!item.enabled, "greyed state survives the check mutation");
    assert!(item.checked, "rebuild must surface the checked state");
}

/// The macOS dynamic-menu pattern: `window_menu_items` mirrors the
/// FOCUSED guest window's menu. Focus can sit on a child (an EDIT inside
/// the focused top-level), so the selection ascends to the top-level that
/// carries the menu; a child's nonzero `menu_handle` slot (its child id)
/// must never be mistaken for a menu. With no focus — or a focused window
/// without a menu — the bar falls back to the first menu-bearing
/// top-level (the pre-focus behavior).
#[test]
fn window_menu_items_prefers_the_focused_windows_menu() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "focus-menu.exe".to_owned(),
        module_path: r"C:\App\focus-menu.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "focus-menu.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    let menu_a = 0x0000_0000_6620_0001_u64;
    let menu_b = 0x0000_0000_6620_0002_u64;
    let a = 0x100_u64;
    let b = 0x200_u64;
    let b_edit = 0x201_u64;
    let c = 0x300_u64;
    {
        let ws = winapi_state.window_state();
        ws.menus.push(MenuRecord {
            handle: wie_winapi::handles::Hmenu::from(menu_a),
            items: vec![MenuEntry::Item {
                id: 1,
                text: "Exit".to_owned(),
                enabled: true,
                checked: false,
            }],
        });
        ws.menus.push(MenuRecord {
            handle: wie_winapi::handles::Hmenu::from(menu_b),
            items: vec![MenuEntry::Item {
                id: 2,
                text: "Paste".to_owned(),
                enabled: true,
                checked: false,
            }],
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(a),
            menu_handle: menu_a,
            ..Default::default()
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(b),
            menu_handle: menu_b,
            ..Default::default()
        });
        // B's EDIT: its menu_handle slot holds the CHILD ID (5), not a
        // menu — the selection must not mistake it for one.
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(b_edit),
            parent_handle: wie_winapi::handles::Hwnd::from(b),
            menu_handle: 5,
            ..Default::default()
        });
        // A top-level with no menu at all.
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(c),
            ..Default::default()
        });
        winapi_state.sync_window_mirror();
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    // Focus sits on B's child EDIT → the bar mirrors B's menu.
    {
        let mut state = handle.state.lock().expect("lock state");
        state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::from(b_edit);
        state.sync_window_mirror();
    }
    assert_eq!(
        handle.window_menu_items().first().expect("item").title,
        "Paste",
        "a focused child must resolve to its top-level's menu"
    );

    // Focus on A → A's menu.
    {
        let mut state = handle.state.lock().expect("lock state");
        state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::from(a);
        state.sync_window_mirror();
    }
    assert_eq!(
        handle.window_menu_items().first().expect("item").title,
        "Exit",
        "focus on A switches the bar to A's menu"
    );

    // Focus on C (no menu) → fall back to the first menu-bearing
    // top-level (A).
    {
        let mut state = handle.state.lock().expect("lock state");
        state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::from(c);
        state.sync_window_mirror();
    }
    assert_eq!(
        handle.window_menu_items().first().expect("item").title,
        "Exit",
        "a focused window without a menu falls back to the first menu-bearing top-level"
    );

    // No focus at all → same fallback.
    {
        let mut state = handle.state.lock().expect("lock state");
        state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::NULL;
        state.sync_window_mirror();
    }
    assert_eq!(
        handle.window_menu_items().first().expect("item").title,
        "Exit",
        "no focus → the first menu-bearing top-level (pre-focus behavior)"
    );
}

/// `take_host_geometry_request` reads the guest-set pending geometry —
/// target hwnd + rect — and clears the slot (the SetWindowPlacement
/// host-forwarding seam).
#[test]
fn take_host_geometry_request_reads_and_clears_the_pending_slot() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "placement.exe".to_owned(),
        module_path: r"C:\App\placement.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "placement.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    {
        let ws = winapi_state.window_state();
        ws.host_geometry_request = Some((20, 30, 200, 100));
        ws.host_geometry_hwnd = Some(0x200);
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    assert_eq!(
        handle.take_host_geometry_request(),
        Some((0x200, 20, 30, 200, 100)),
        "the pending geometry must be handed to the host presenter, tagged \
         with the window it targets"
    );
    assert_eq!(
        handle.take_host_geometry_request(),
        None,
        "take clears the slot so a stale move is never re-applied"
    );
}

/// `window_at_in` roots the hit-test at the CALLER's top-level, so with
/// two top-level windows each resolves its own controls: window B's child
/// is found through window B, window A's child through window A, and the
/// plain `window_at` keeps resolving the first top-level (the headless
/// caller).
#[test]
fn window_at_in_hit_tests_the_named_top_level_subtree() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "two-win.exe".to_owned(),
        module_path: r"C:\App\two-win.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "two-win.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    let a = 0x100_u64;
    let a_button = 0x101_u64;
    let b = 0x200_u64;
    let b_edit = 0x201_u64;
    {
        let ws = winapi_state.window_state();
        // Top-level A (the main window) with a button at (10, 10, 120x40).
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(a),
            width: 360,
            height: 140,
            ..Default::default()
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(a_button),
            parent_handle: wie_winapi::handles::Hwnd::from(a),
            x: 10,
            y: 10,
            width: 120,
            height: 40,
            visible: true,
            ..Default::default()
        });
        // Top-level B (a dialog) with an edit at (5, 5, 50x50).
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(b),
            width: 200,
            height: 200,
            ..Default::default()
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(b_edit),
            parent_handle: wie_winapi::handles::Hwnd::from(b),
            x: 5,
            y: 5,
            width: 50,
            height: 50,
            visible: true,
            ..Default::default()
        });
        winapi_state.sync_window_mirror();
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    // A click at (20, 20) in window B's space hits B's edit, NOT A's
    // button — the pre-L2 window_at would have resolved A's button here.
    assert_eq!(
        handle.window_at_in(b, 20, 20),
        Some((b_edit, 15, 15)),
        "window_at_in(B) must descend B's subtree with child-relative coords"
    );
    // The same point through window A hits A's button.
    assert_eq!(
        handle.window_at_in(a, 20, 20),
        Some((a_button, 10, 10)),
        "window_at_in(A) must descend A's subtree"
    );
    // A point outside B's children resolves to B itself.
    assert_eq!(
        handle.window_at_in(b, 150, 150),
        Some((b, 150, 150)),
        "no child hit → the root window, client-relative"
    );
    // An unknown window yields nothing (a destroyed handle must not
    // hit-test stale children).
    assert_eq!(
        handle.window_at_in(0x999, 20, 20),
        None,
        "an unknown root is not a live guest window"
    );
    // The thin window_at wrapper still roots at the first top-level (A).
    assert_eq!(
        handle.window_at(20, 20),
        Some((a_button, 10, 10)),
        "window_at keeps resolving the first parentless record"
    );
}

/// `focus_window` surfaces the guest's `SetFocus` state — the keyboard
/// routing target (`focus_window().unwrap_or(event window)`).
#[test]
fn focus_window_reflects_the_guest_focus_handle() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "focus.exe".to_owned(),
        module_path: r"C:\App\focus.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "focus.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    {
        let ws = winapi_state.window_state();
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(0x201),
            ..Default::default()
        });
        ws.focus_window_handle = wie_winapi::handles::Hwnd::from(0x201);
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    assert_eq!(
        handle.focus_window(),
        Some(0x201),
        "the guest focus handle is the keyboard routing target"
    );
    {
        let mut state = handle.state.lock().expect("lock state");
        state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::NULL;
    }
    assert_eq!(
        handle.focus_window(),
        None,
        "no focus → the caller falls back to the event window"
    );
}

/// `focused_top_level` ascends a focused CHILD to its parentless
/// top-level — the window a modal MessageBox should parent to (a dialog
/// is a child of its owner here, so its focused controls resolve to the
/// owner top-level). `None` when nothing is focused.
#[test]
fn focused_top_level_ascends_to_the_parentless_ancestor() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "focus-top.exe".to_owned(),
        module_path: r"C:\App\focus-top.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "focus-top.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    let main = 0x100_u64;
    let dialog = 0x200_u64;
    let dialog_button = 0x201_u64;
    {
        let ws = winapi_state.window_state();
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(main),
            ..Default::default()
        });
        // The modal dialog is a CHILD of the owner (it composites into
        // the owner's surface), and the button is a child of the dialog.
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(dialog),
            parent_handle: wie_winapi::handles::Hwnd::from(main),
            ..Default::default()
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(dialog_button),
            parent_handle: wie_winapi::handles::Hwnd::from(dialog),
            ..Default::default()
        });
        winapi_state.sync_window_mirror();
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    // Focus on the dialog's button → the owner top-level (main).
    {
        let mut state = handle.state.lock().expect("lock state");
        state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::from(dialog_button);
        state.sync_window_mirror();
    }
    assert_eq!(
        handle.focused_top_level(),
        Some(main),
        "a focused dialog control ascends to the owner top-level"
    );

    // Focus on a top-level directly → itself.
    {
        let mut state = handle.state.lock().expect("lock state");
        state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::from(main);
        state.sync_window_mirror();
    }
    assert_eq!(handle.focused_top_level(), Some(main));

    // No focus → None (the caller falls back to the primary window).
    {
        let mut state = handle.state.lock().expect("lock state");
        state.window_state().focus_window_handle = wie_winapi::handles::Hwnd::NULL;
        state.sync_window_mirror();
    }
    assert_eq!(
        handle.focused_top_level(),
        None,
        "no focus yields None so the bridge falls back to the primary window"
    );
}

/// `resize_window` must put the RESIZED window itself into the
/// erase/paint cycle — the exact bug pattern behind the black-background
/// regression (gui_edit, notepad): the guest DIB is reallocated ZEROED on
/// resize, so any unpainted area uploads as black unless the class-brush
/// erase fills it first. The erase runs only when `ERASE_BACKGROUND` is
/// set; the SetWindowPlacement show path sets it (cb38299) but the resize
/// path only marked the subtree `invalidated`. Descendants keep
/// `invalidated` WITHOUT erase — the EDIT paints its own white background.
#[test]
fn resize_window_erases_background_of_the_resized_window() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "resize.exe".to_owned(),
        module_path: r"C:\App\resize.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "resize.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    let top = 0x100_u64;
    let child = 0x101_u64;
    {
        let ws = winapi_state.window_state();
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(top),
            width: 640,
            height: 420,
            ..Default::default()
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(child),
            parent_handle: wie_winapi::handles::Hwnd::from(top),
            ..Default::default()
        });
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    handle.resize_window(Hwnd::from(top), Dimension::new(800, 600));

    let mut state = handle.state.lock().expect("lock state");
    let ws = state.window_state();
    let top_record = ws
        .windows
        .iter()
        .find(|w| w.handle == wie_winapi::handles::Hwnd::from(top))
        .expect("top window record exists");
    assert_eq!(
        (top_record.width, top_record.height),
        (800, 600),
        "the resized window record must carry the new size"
    );
    assert!(
        top_record.invalidated,
        "the resized window itself must enter the repaint cycle"
    );
    assert!(
        top_record
            .flags
            .contains(wie_winapi::WindowFlags::ERASE_BACKGROUND),
        "the resized window must request a class-brush erase (the DIB was \
         reallocated zeroed — without the erase the background stays black)"
    );
    let child_record = ws
        .windows
        .iter()
        .find(|w| w.handle == wie_winapi::handles::Hwnd::from(child))
        .expect("child window record exists");
    assert!(
        child_record.invalidated,
        "a descendant must repaint itself after the ancestor surface grew"
    );
    assert!(
        !child_record
            .flags
            .contains(wie_winapi::WindowFlags::ERASE_BACKGROUND),
        "descendants paint their own background — no erase flag on the child"
    );
}

/// `set_drop_files` maps host drop paths into guest `C:\…` / `D:\…` paths
/// through the session's volume config, skips unmapped files, stores the
/// point, and returns the fake HDROP for the WM_DROPFILES wParam.
#[test]
fn set_drop_files_maps_host_paths_to_guest_paths() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "drop.exe".to_owned(),
        module_path: r"C:\App\drop.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "drop.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    winapi_state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: Some(PathBuf::from("/Users/me/data")),
    };
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    let hdrop = handle.set_drop_files(
        vec![
            PathBuf::from("/tmp/bottle/drive_c/App/out.txt"),
            PathBuf::from("/Users/me/data/archive/a.7z"),
            // Outside both volumes — the guest filesystem cannot see it.
            PathBuf::from("/etc/passwd"),
        ],
        (5, 6),
    );
    assert_eq!(
        hdrop,
        wie_winapi::user32::dragdrop::FAKE_HDROP,
        "set_drop_files returns the fake HDROP"
    );

    let mut state = handle.state.lock().expect("lock state");
    let drop = state.drag_drop();
    assert_eq!(
        drop.files(),
        &[r"C:\App\out.txt".to_owned(), r"D:\archive\a.7z".to_owned()],
        "unmapped host paths are skipped"
    );
    assert_eq!(drop.point(), (5, 6));
}

/// A drop in which NO host path maps into a guest volume must be skipped
/// entirely: return 0 (the caller posts no WM_DROPFILES) rather than a
/// fake HDROP over an empty list — the guest would otherwise open an
/// empty path (notepad: CreateFileW("") → ERROR_PATH_NOT_FOUND → error
/// dialog).
#[test]
fn set_drop_files_returns_zero_when_nothing_maps() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "drop.exe".to_owned(),
        module_path: r"C:\App\drop.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "drop.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    winapi_state.file_io.volumes = VolumeConfig {
        bottle_root: Some(PathBuf::from("/tmp/bottle")),
        drive_d_root: Some(PathBuf::from("/Users/me/data")),
    };
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    let hdrop = handle.set_drop_files(
        vec![
            // Outside both volumes — the guest filesystem cannot see it.
            PathBuf::from("/etc/passwd"),
            PathBuf::from("/tmp/elsewhere/file.txt"),
        ],
        (5, 6),
    );
    assert_eq!(hdrop, 0, "no mappable path → the drop is skipped");

    let mut state = handle.state.lock().expect("lock state");
    assert!(
        state.drag_drop().files().is_empty(),
        "the drop list must stay empty"
    );
}

/// `z_snapshot` reads the revision AND the ordered list under ONE lock:
/// the Frame handler's reorder always applies the list the revision it
/// compared actually describes. Separate locked reads could observe the
/// list mid-mutation.
#[test]
fn z_snapshot_reads_rev_and_order_under_one_lock() {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "z.exe".to_owned(),
        module_path: r"C:\App\z.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "z.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    {
        let present = winapi_state.present();
        present.register_top_level(wie_winapi::handles::Hwnd::from(0x100));
        present.register_top_level(wie_winapi::handles::Hwnd::from(0x200));
        present.register_top_level(wie_winapi::handles::Hwnd::from(0x300));
        // HWND_TOP: 0x100 to the front → back-to-front [0x200, 0x300, 0x100].
        present.z_order_to_top(wie_winapi::handles::Hwnd::from(0x100));
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };

    let (rev, order) = handle.z_snapshot();
    assert_eq!(rev, handle.z_rev(), "snapshot rev == the z_rev accessor");
    assert_eq!(order, vec![0x200, 0x300, 0x100], "back-to-front order");

    // A guest z-change bumps the revision; the NEXT snapshot reflects the
    // new order atomically (HWND_BOTTOM: 0x300 to the back).
    {
        let mut state = handle.state.lock().expect("lock state");
        state
            .present()
            .z_order_to_bottom(wie_winapi::handles::Hwnd::from(0x300));
    }
    let (rev2, order2) = handle.z_snapshot();
    assert!(rev2 > rev, "the z-change bumps the revision");
    assert_eq!(order2, vec![0x300, 0x200, 0x100], "reordered snapshot");
    assert_eq!(rev2, handle.z_rev(), "still consistent after the change");
}

// ── Task 7: presenter-side lock-wait timing ────────────────────────

/// A presenter-side `GuestHandle` + shared stats fixture.
fn handle_with_stats() -> (GuestHandle, Arc<crate::mt_runtime::LockWaitStats>) {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "wait.exe".to_owned(),
        module_path: r"C:\App\wait.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "wait.exe".to_owned(),
    };
    let winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    let stats = Arc::new(crate::mt_runtime::LockWaitStats::new());
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::clone(&stats),
    };
    (handle, stats)
}

/// `take_frame` (the presenter's per-frame read path) reads the present
/// channel WITHOUT the big WinApiState lock: even while the guest holds
/// `shared_winapi` for 10 ms, the presenter's take returns immediately
/// and never lands in the presenter-side lock stats (the Wave 1a
/// contract — rendering no longer serializes behind guest WinAPI work).
#[test]
fn take_frame_times_presenter_wait_when_enabled() {
    let (handle, stats) = handle_with_stats();
    stats.set_enabled(true);
    let state = Arc::clone(&handle.state);
    let (tx, rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = state.lock().ok();
        let _ = tx.send(());
        std::thread::sleep(std::time::Duration::from_millis(10));
    });
    let _ = rx.recv();
    let t0 = std::time::Instant::now();
    let frame = handle.take_frame(0x100);
    let elapsed = t0.elapsed();
    assert!(frame.is_none(), "no published frame yet");
    assert!(
        elapsed < std::time::Duration::from_millis(10),
        "take_frame must not wait out the guest-held big lock (took {elapsed:?})"
    );
    let _ = holder.join();
    let snap = stats.snapshot();
    assert_eq!(
        snap.presenter_total_ns, 0,
        "the channel read is lock-free — no presenter-side wait is recorded"
    );
    assert_eq!(snap.presenter_max_ns, 0);
    assert_eq!(
        snap.guest_total_ns, 0,
        "presenter reads never mix with guest"
    );
    assert_eq!(snap.guest_max_ns, 0);
}

/// Disabled mode: presenter-side reads take the lock without recording —
/// the shared stats stay zero, so the per-frame path pays only the
/// atomic gate load.
#[test]
fn take_frame_disabled_records_nothing() {
    let (handle, stats) = handle_with_stats();
    assert!(!stats.enabled(), "stats start disabled");
    let _ = handle.take_frame(0x100);
    let _ = handle.z_snapshot();
    let snap = stats.snapshot();
    assert_eq!(snap.presenter_total_ns, 0);
    assert_eq!(snap.presenter_max_ns, 0);
    assert_eq!(snap.guest_total_ns, 0);
    assert_eq!(snap.guest_max_ns, 0);
}

// ── Wave 2 Step 2: mirror accessors are big-lock-free ──────────────

/// Fixture for the non-blocking proofs: one top-level `A` with a button
/// child (hit-test target) carrying A's menu, mirror synced from the
/// seeded records.
fn handle_with_seeded_mirror() -> (GuestHandle, u64, u64) {
    let process = wie_pe::ProcessIdentity {
        module_file_name: "mirror.exe".to_owned(),
        module_path: r"C:\App\mirror.exe".to_owned(),
        current_directory: r"C:\App".to_owned(),
        command_line: "mirror.exe".to_owned(),
    };
    let mut winapi_state =
        crate::memory::default_winapi_state(&DEFAULT_LAYOUT, Arc::new(Vec::new()), &process)
            .expect("winapi state");
    let a = 0x100_u64;
    let a_button = 0x101_u64;
    let menu_handle = 0x0000_0000_6620_0042_u64;
    {
        let ws = winapi_state.window_state();
        ws.menus.push(MenuRecord {
            handle: wie_winapi::handles::Hmenu::from(menu_handle),
            items: vec![MenuEntry::Item {
                id: 7,
                text: "Exit".to_owned(),
                enabled: true,
                checked: false,
            }],
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(a),
            menu_handle,
            width: 360,
            height: 140,
            ..Default::default()
        });
        ws.windows.push(wie_winapi::WindowRecord {
            handle: wie_winapi::handles::Hwnd::from(a_button),
            parent_handle: wie_winapi::handles::Hwnd::from(a),
            x: 10,
            y: 10,
            width: 120,
            height: 40,
            visible: true,
            ..Default::default()
        });
        ws.focus_window_handle = wie_winapi::handles::Hwnd::from(a);
        winapi_state.sync_window_mirror();
    }
    let __state_arc = Arc::new(Mutex::new(winapi_state));
    let __present_channel = {
        let mut __guard = __state_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        __guard.present().channel_arc()
    };
    let handle = GuestHandle {
        state: Arc::clone(&__state_arc),
        present_channel: Arc::clone(&__present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::new(RwLock::new(None)),
        lock_wait_stats: Arc::new(crate::mt_runtime::LockWaitStats::new()),
    };
    (handle, a, a_button)
}

/// Runs `probe` on a worker thread while the main thread HOLDS the big
/// `WinApiState` lock, with a hard timeout: a mirror accessor that still
/// took the big lock would deadlock here and surface as the timeout
/// panic, not a hang. This is the "proving it no longer takes the big
/// lock" test the Wave 2 handoff requires per accessor.
fn probe_without_big_lock<R: Send + 'static>(
    handle: &GuestHandle,
    what: &'static str,
    probe: impl FnOnce(&GuestHandle) -> R + Send + 'static,
) -> R {
    let _guard = handle.state.lock().expect("hold the big lock");
    let handle = GuestHandle {
        state: Arc::clone(&handle.state),
        present_channel: Arc::clone(&handle.present_channel),
        queue: Arc::new(Mutex::new(wie_winapi::present::MessageQueue::default())),
        menu_tree_cache: Arc::clone(&handle.menu_tree_cache),
        lock_wait_stats: Arc::clone(&handle.lock_wait_stats),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(probe(&handle));
    });
    rx.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_else(|_| panic!("{what} blocked on the big WinApiState lock"))
}

/// The hot mouse/keyboard/menu accessors read ONLY the presenter-side
/// window mirror: with the big `WinApiState` lock held by a guest-side
/// task, every one of them still returns the mirrored answer within the
/// probe timeout. Covers `window_at`, `window_at_in`, `mouse_tracking`,
/// `first_guest_window_handle`, `first_guest_window_info`,
/// `focused_top_level`, and the `window_menu_items` cache-hit fast path
/// (`capture_target` and `set_key_state` have their own proofs below).
#[test]
fn mirror_accessors_return_while_the_big_lock_is_held() {
    let (handle, a, a_button) = handle_with_seeded_mirror();
    // Warm the menu cache so the menu probe exercises the lock-free
    // fast path (a cold cache legitimately rebuilds under the lock).
    let warm = handle.window_menu_items();
    assert_eq!(warm.len(), 1, "the fixture menu builds one node");

    assert_eq!(
        probe_without_big_lock(&handle, "window_at", move |h| h.window_at(20, 20)),
        Some((a_button, 10, 10)),
        "window_at resolves the mirror while the big lock is held"
    );
    assert_eq!(
        probe_without_big_lock(&handle, "window_at_in", move |h| h.window_at_in(a, 20, 20)),
        Some((a_button, 10, 10)),
        "window_at_in resolves the mirror while the big lock is held"
    );
    assert!(
        !probe_without_big_lock(&handle, "mouse_tracking", move |h| h.mouse_tracking(a)),
        "mouse_tracking reads the mirror while the big lock is held"
    );
    assert_eq!(
        probe_without_big_lock(&handle, "first_guest_window_handle", move |h| {
            h.first_guest_window_handle()
        }),
        Some(a),
        "first_guest_window_handle reads the mirror while the big lock is held"
    );
    assert_eq!(
        probe_without_big_lock(&handle, "first_guest_window_info", move |h| {
            h.first_guest_window_info()
                .map(|(hwnd, _, w, hgt)| (hwnd, w, hgt))
        }),
        Some((a, 360, 140)),
        "first_guest_window_info reads the mirror while the big lock is held"
    );
    assert_eq!(
        probe_without_big_lock(&handle, "focused_top_level", move |h| h.focused_top_level()),
        Some(a),
        "focused_top_level reads the mirror while the big lock is held"
    );
    assert_eq!(
        probe_without_big_lock(&handle, "window_menu_items fast path", move |h| {
            h.window_menu_items().len()
        }),
        1,
        "the menu cache-hit fast path skips the big lock"
    );
}

/// `capture_target` is the SetCapture destination resolver — under
/// active capture it must never block on the guest. The capture window
/// comes from the mirror (`mirror_meta`).
#[test]
fn capture_target_resolves_while_the_big_lock_is_held() {
    let (handle, _a, a_button) = handle_with_seeded_mirror();
    {
        let mut state = handle.state.lock().expect("lock state");
        state.window_state().capture_window_handle = wie_winapi::handles::Hwnd::from(a_button);
        state.sync_window_mirror();
    }
    // The button sits at (10, 10) inside A: a (15, 20) point in capture
    // space is (5, 10) relative to the button.
    assert_eq!(
        probe_without_big_lock(&handle, "capture_target", move |h| h.capture_target(15, 20)),
        Some((a_button, 5, 10)),
        "capture_target resolves the mirror while the big lock is held"
    );
}

/// `set_key_state` pushes a (vk, pressed) event onto the mirror — no big
/// lock while the guest holds it — and the guest-side readers drain the
/// event into `keyboard_state` under the big lock they already hold.
#[test]
fn set_key_state_queues_without_the_big_lock_and_guest_drains_it() {
    let (handle, _a, _a_button) = handle_with_seeded_mirror();
    probe_without_big_lock(&handle, "set_key_state", move |h| {
        h.set_key_state(0x41, true); // 'A' down
        h.set_key_state(0x41, false); // 'A' up again
    });
    // The writes happened on the mirror queue; the guest-side drain (the
    // same call the GetKeyState/GetAsyncKeyState/GetKeyboardState
    // handlers make under the big lock) applies both events.
    {
        let mut state = handle.state.lock().expect("lock state");
        state.drain_key_writes();
        let key = state.window_state().keyboard_state[usize::from(0x41_u16)];
        assert_eq!(
            key & 0x80,
            0,
            "down-then-up leaves the key released after the drain"
        );
    }
    // One press without the release sets the down bit.
    probe_without_big_lock(&handle, "set_key_state", move |h| {
        h.set_key_state(0x41, true);
    });
    {
        let mut state = handle.state.lock().expect("lock state");
        state.drain_key_writes();
        let key = state.window_state().keyboard_state[usize::from(0x41_u16)];
        assert_eq!(key & 0x80, 0x80, "the pressed bit lands after the drain");
    }
}

/// `set_cursor_pos` pushes the latest host cursor position onto the mirror —
/// no big lock while the guest holds it — and the guest-side `GetCursorPos`
/// reader copies it out under the big lock it already holds. Read-current,
/// not drain: repeated reads report the same position, and a newer push
/// overwrites (latest wins).
#[test]
fn set_cursor_pos_without_the_big_lock_and_guest_reads_it() {
    let (handle, _a, _a_button) = handle_with_seeded_mirror();
    probe_without_big_lock(&handle, "set_cursor_pos", move |h| {
        h.set_cursor_pos(12, 34);
    });
    {
        let mut state = handle.state.lock().expect("lock state");
        assert_eq!(
            state.cursor_pos(),
            Some((12, 34)),
            "the pushed position reads back"
        );
        assert_eq!(
            state.cursor_pos(),
            Some((12, 34)),
            "cursor reads are non-destructive (no drain)"
        );
    }
    probe_without_big_lock(&handle, "set_cursor_pos", move |h| {
        h.set_cursor_pos(-5, 300);
    });
    {
        let mut state = handle.state.lock().expect("lock state");
        assert_eq!(state.cursor_pos(), Some((-5, 300)), "the latest push wins");
    }
}
