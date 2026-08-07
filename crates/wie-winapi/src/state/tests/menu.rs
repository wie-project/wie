//! User32 menu tests: GetMenu, AppendMenuA, menu item state / enable / check, the menu_dirty flag, LoadMenuA/W from RT_MENU resources, and class-menu inheritance.
use super::*;

// ── USER32 ────────────────────────────────────────────────────────

#[test]
fn test_get_menu_returns_zero_for_unknown_window() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0xDEAD, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        user32::handle_get_menu(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_get_menu_returns_menu_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let hwnd = 0x100;
    let hmenu = 0x200;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        menu_handle: hmenu,
        ..Default::default()
    });
    write_regs(&mut engine, hwnd, 0, 0, 0, STACK_TOP);
    let r = user32::handle_get_menu(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetMenu");
    assert_eq!(r.return_value, hmenu);
}

// ── USER32 menu tree (P1): MenuRecord / MenuEntry / menu_dirty ─────

/// Invoke `CreateMenu` and return the allocated handle.
fn create_menu(state: &mut WinApiState) -> u64 {
    let mut engine = test_engine();
    write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
    let r = user32::handle_create_menu(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        state,
    ))
    .expect("CreateMenu");
    r.return_value
}

/// Invoke `AppendMenuA(menu, flags, item_id, text_va)`, writing `text`
/// into guest memory first (empty text uses a null pointer).
fn append_menu_a(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    menu: u64,
    flags: u32,
    item_id: u32,
    text: &str,
) {
    let text_va = if text.is_empty() {
        0
    } else {
        engine
            .mem_write(0x2000, text.as_bytes())
            .expect("write menu text");
        0x2000
    };
    write_regs(
        engine,
        menu,
        u64::from(flags),
        u64::from(item_id),
        text_va,
        0,
    );
    assert_return_value!(
        user32::handle_append_menu_a(&mut HandlerContext::new(engine, test_environment(), state,)),
        1
    );
}

/// A menu with `Item(100, "Exit")`, a `Separator`, and `Item(200, "About")`.
fn push_two_item_menu(state: &mut WinApiState) -> u64 {
    let menu = create_menu(state);
    let mut engine = test_engine();
    append_menu_a(&mut engine, state, menu, 0x0000, 100, "Exit");
    append_menu_a(&mut engine, state, menu, crate::user32::MF_SEPARATOR, 0, "");
    append_menu_a(&mut engine, state, menu, 0x0000, 200, "About");
    menu
}

#[test]
fn test_append_menu_builds_native_tree() {
    let mut state = default_winapi_state();
    let menu = create_menu(&mut state);
    assert!(
        !state.window_state().menu_dirty,
        "CreateMenu alone must not dirty the tree (empty menu changes nothing)"
    );
    let popup = create_menu(&mut state);
    let mut engine = test_engine();
    append_menu_a(&mut engine, &mut state, menu, 0x0000, 100, "Exit");
    append_menu_a(
        &mut engine,
        &mut state,
        menu,
        crate::user32::MF_SEPARATOR,
        0,
        "",
    );
    append_menu_a(
        &mut engine,
        &mut state,
        menu,
        crate::user32::MF_POPUP,
        u32::try_from(popup).expect("popup handle"),
        "File",
    );

    assert!(state.window_state().menu_dirty, "AppendMenu must dirty");
    let record = state
        .window_state()
        .menus
        .iter()
        .find(|m| m.handle == crate::handles::Hmenu::from(menu))
        .expect("menu record exists");
    assert_eq!(record.items.len(), 3);
    assert!(
        matches!(
            record.items.first(),
            Some(crate::user32::menu::MenuEntry::Item { id, text, enabled: true, checked: false })
                if *id == 100 && text == "Exit"
        ),
        "first entry must be the Exit item"
    );
    assert!(
        matches!(
            record.items.get(1),
            Some(crate::user32::menu::MenuEntry::Separator)
        ),
        "second entry must be a separator"
    );
    assert!(
        matches!(
            record.items.get(2),
            Some(crate::user32::menu::MenuEntry::Popup { text, submenu })
                if text == "File" && submenu.as_u64() == popup
        ),
        "third entry must be the File popup linking the submenu handle"
    );
}

#[test]
fn test_get_menu_state_by_command_and_position() {
    let mut state = default_winapi_state();
    let menu = push_two_item_menu(&mut state);
    let mut engine = test_engine();

    // By command: an enabled, unchecked item reports flag 0.
    write_regs(&mut engine, menu, 100, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );

    // By position: index 1 is the separator.
    write_regs(&mut engine, menu, 1, 0x0400, 0, 0); // MF_BYPOSITION
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        u64::from(crate::user32::MF_SEPARATOR)
    );

    // Unknown command id returns -1.
    write_regs(&mut engine, menu, 999, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        u64::from(u32::MAX)
    );
}

#[test]
fn test_enable_menu_item_mutates_item_state() {
    let mut state = default_winapi_state();
    let menu = push_two_item_menu(&mut state);
    let mut engine = test_engine();

    // First enable returns the previous state (0 = enabled) and grays it.
    write_regs(&mut engine, menu, 100, 0x0001, 0, 0); // MF_GRAYED
    assert_return_value!(
        user32::handle_enable_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    assert!(
        matches!(
            state
                .window_state()
                .menus
                .iter()
                .find(|m| m.handle == crate::handles::Hmenu::from(menu))
                .expect("record")
                .items
                .first(),
            Some(crate::user32::menu::MenuEntry::Item { enabled: false, .. })
        ),
        "MF_GRAYED must disable the item in the tree"
    );

    // GetMenuState now reports MF_GRAYED.
    write_regs(&mut engine, menu, 100, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x0001
    );

    // Re-enabling returns the previous MF_GRAYED state.
    write_regs(&mut engine, menu, 100, 0x0000, 0, 0); // MF_ENABLED
    assert_return_value!(
        user32::handle_enable_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x0001
    );

    // A missing item returns -1 and does not mutate.
    let dirty_before = state.window_state().menu_dirty;
    write_regs(&mut engine, menu, 999, 0x0001, 0, 0);
    assert_return_value!(
        user32::handle_enable_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        u64::from(u32::MAX)
    );
    assert_eq!(state.window_state().menu_dirty, dirty_before);
}

#[test]
fn test_check_menu_item_mutates_item_state() {
    let mut state = default_winapi_state();
    let menu = push_two_item_menu(&mut state);
    let mut engine = test_engine();

    write_regs(&mut engine, menu, 200, 0x0008, 0, 0); // MF_CHECKED
    assert_return_value!(
        user32::handle_check_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
    write_regs(&mut engine, menu, 200, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x0008
    );

    // Unchecking returns the previous MF_CHECKED state.
    write_regs(&mut engine, menu, 200, 0x0000, 0, 0);
    assert_return_value!(
        user32::handle_check_menu_item(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0x0008
    );
    write_regs(&mut engine, menu, 200, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        0
    );
}

#[test]
fn test_menu_dirty_flag_semantics() {
    let mut state = default_winapi_state();
    assert!(!state.window_state().menu_dirty);

    // SetMenu to a new menu handle dirties (the window's menu changed).
    let menu = create_menu(&mut state);
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(0x100),
        ..Default::default()
    });
    state.window_state().menu_dirty = false;
    let mut engine = test_engine();
    write_regs(&mut engine, 0x100, menu, 0, 0, 0);
    assert_return_value!(
        user32::handle_set_menu(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        1
    );
    assert!(state.window_state().menu_dirty, "SetMenu must dirty");

    // DestroyMenu removes the record and dirties.
    state.window_state().menu_dirty = false;
    write_regs(&mut engine, menu, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_destroy_menu(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        1
    );
    assert!(state.window_state().menu_dirty, "DestroyMenu must dirty");
    assert!(
        state
            .window_state()
            .menus
            .iter()
            .all(|m| m.handle != crate::handles::Hmenu::from(menu)),
        "DestroyMenu must drop the record"
    );

    // GetMenuState on the destroyed menu now returns -1.
    write_regs(&mut engine, menu, 100, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_menu_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        )),
        u64::from(u32::MAX)
    );
}

// ── LoadMenuA/W from RT_MENU resources ─────────────────────────────

/// Seed the main-module menu templates with notepad-like entries: menu id
/// 0x201 (the resource notepad's window class names via `lpszMenuName`), a
/// File popup with Exit/separator/About and an Edit popup with Paste.
fn push_menu_templates(state: &mut WinApiState) {
    use wie_pe::resources::{MenuItemTemplate, MenuTemplate};
    state.process.main_module_menus.push(MenuTemplate {
        id: 0x201,
        lang: 0x0409,
        items: vec![
            MenuItemTemplate {
                flags: crate::user32::MF_POPUP,
                id: 0,
                text: Some("File".to_owned()),
                sub: vec![
                    MenuItemTemplate {
                        flags: 0x00,
                        id: 0x0100,
                        text: Some("Exit".to_owned()),
                        sub: Vec::new(),
                    },
                    MenuItemTemplate {
                        // The windres separator: option 0 + id 0 + empty
                        // text, NOT the MF_SEPARATOR bit — the parser keeps
                        // it as flags 0 / text None (real notepad's bytes).
                        flags: 0x00,
                        id: 0,
                        text: None,
                        sub: Vec::new(),
                    },
                    MenuItemTemplate {
                        flags: 0x00,
                        id: 0x0101,
                        text: Some("About".to_owned()),
                        sub: Vec::new(),
                    },
                ],
            },
            MenuItemTemplate {
                flags: crate::user32::MF_POPUP,
                id: 0,
                text: Some("Edit".to_owned()),
                sub: vec![MenuItemTemplate {
                    flags: 0x00,
                    id: 0x0110,
                    text: Some("Paste".to_owned()),
                    sub: Vec::new(),
                }],
            },
        ],
    });
}

#[test]
fn test_load_menu_w_returns_handle_for_known_resource() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // MAKEINTRESOURCEW(0x201): hinst = image base, low word = menu id.
    write_regs(&mut engine, image_base, 0x201, 0, 0, 0);
    let first = dispatch_user32(&mut engine, &mut state, "LoadMenuW");
    assert_ne!(first, 0, "known menu id must return a nonzero HMENU");

    // Repeated loads of the same id cache to the same handle.
    write_regs(&mut engine, image_base, 0x201, 0, 0, 0);
    let second = dispatch_user32(&mut engine, &mut state, "LoadMenuW");
    assert_eq!(first, second, "the same menu id must return the same HMENU");

    // The bar records the two top-level popups, each with a submenu.
    let record = state
        .window_state()
        .menus
        .iter()
        .find(|m| m.handle == crate::handles::Hmenu::from(first))
        .expect("menu record exists");
    assert_eq!(record.items.len(), 2, "File + Edit top-level popups");
    assert!(
        matches!(
            record.items.first(),
            Some(crate::user32::menu::MenuEntry::Popup { text, submenu })
                if text == "File" && submenu.as_u64() != 0
        ),
        "first top-level entry must be the File popup with a submenu"
    );

    // RNotepad's real MAIN_MENU separators survive the template→tree
    // conversion as `Separator` entries even though windres emitted them
    // with a zero option word (no MF_SEPARATOR bit).
    let file_submenu = match record.items.first() {
        Some(crate::user32::menu::MenuEntry::Popup { submenu, .. }) => *submenu,
        _ => panic!("first entry must be the File popup"),
    };
    let file = state
        .window_state()
        .menus
        .iter()
        .find(|m| m.handle == file_submenu)
        .expect("File submenu record");
    assert_eq!(file.items.len(), 3, "Exit / separator / About");
    assert!(
        matches!(
            file.items.get(1),
            Some(crate::user32::menu::MenuEntry::Separator)
        ),
        "a windres separator (flags 0, empty text) must map to MenuEntry::Separator"
    );

    // GetMenuState answers by command for an item nested in the File popup.
    write_regs(&mut engine, first, 0x0100, 0, 0, 0); // MF_BYCOMMAND
    let flags = dispatch_user32(&mut engine, &mut state, "GetMenuState");
    assert_eq!(flags, 0, "Exit is enabled + unchecked → flag 0");
}

#[test]
fn test_load_menu_w_unknown_id_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // Menu id 0x999 is not in the parsed set.
    write_regs(&mut engine, image_base, 0x999, 0, 0, 0);
    let handle = dispatch_user32(&mut engine, &mut state, "LoadMenuW");
    assert_eq!(handle, 0, "unknown menu id must return NULL");
}

#[test]
fn test_load_menu_w_named_menu_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // A string-named menu (high word set) has no parsed name→template
    // mapping yet; WIE returns NULL just like an unknown id.
    write_regs(&mut engine, image_base, 0x0000_0000_4000_0000, 0, 0, 0);
    let handle = dispatch_user32(&mut engine, &mut state, "LoadMenuW");
    assert_eq!(handle, 0, "string-named menus must return NULL");
}

#[test]
fn test_load_menu_a_mirrors_w() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    write_regs(&mut engine, image_base, 0x201, 0, 0, 0);
    let handle = dispatch_user32(&mut engine, &mut state, "LoadMenuA");
    assert_ne!(handle, 0, "known menu id must return a nonzero HMENU via A");
    write_regs(&mut engine, handle, 0x0110, 0, 0, 0); // Edit → Paste
    let flags = dispatch_user32(&mut engine, &mut state, "GetMenuState");
    assert_eq!(flags, 0, "Paste is enabled + unchecked → flag 0");
}

#[test]
fn test_create_window_inherits_class_menu_name() {
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // A class whose lpszMenuName is MAKEINTRESOURCE(0x201), registered by the
    // main module (notepad's pattern: RegisterClassExW + CreateWindowExW with
    // hMenu = NULL).
    let atom = crate::user32::register_window_class(
        &mut state,
        WindowClassRecord {
            atom: 0,
            class_name: "NotepadClass".to_owned(),
            window_proc: 0x7000_0000,
            style: 0,
            instance_handle: image_base,
            icon_handle: 0,
            cursor_handle: 0,
            background_brush: 0,
            small_icon_handle: 0,
            unicode: true,
            menu_name: 0x201,
        },
    )
    .expect("register class");
    assert_ne!(atom, 0);

    let (hwnd, _, _) = crate::user32::create_window_record(
        &mut state,
        crate::user32::CreateWindowRequest {
            class_identifier: crate::user32::WindowClassIdentifier::Name("NotepadClass".to_owned()),
            title: "Untitled - Notepad".to_owned(),
            style: 0,
            extended_style: 0,
            parent_handle: 0,
            menu_handle: 0, // NULL → the class menu must be used
            instance_handle: image_base,
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        },
        true,
    )
    .expect("create window");
    assert_ne!(hwnd, 0);

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window record exists");
    assert_ne!(
        window.menu_handle, 0,
        "a window without an explicit hMenu must inherit the class menu"
    );
    assert!(
        state.window_state().menu_dirty,
        "attaching a menu must dirty"
    );
}

#[test]
fn test_register_class_ex_w_class_menu_end_to_end() {
    // The full guest path: a WNDCLASSEXW struct in guest memory whose
    // lpszMenuName is MAKEINTRESOURCEW(0x201) is read by RegisterClassExW,
    // and a CreateWindowExW with hMenu = NULL inherits the class menu.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_menu_templates(&mut state);
    let image_base = default_env().image_base;
    // Prime the guest heap control block so CreateWindowExW's CREATESTRUCT
    // allocation (a coherent LocalAlloc-style block) succeeds.
    engine
        .mem_write(0x2000, &0x2000_u64.to_le_bytes())
        .expect("guest heap bump cursor");

    // WNDCLASSEXW at 0x4000 (misc.rs layout: cbSize@0, style@4, lpfnWndProc@8,
    // cbClsExtra@0x10, cbWndExtra@0x14, hInstance@0x18, hIcon@0x20,
    // hCursor@0x28, hbrBackground@0x30, lpszMenuName@0x38, lpszClassName@0x40,
    // hIconSm@0x48).
    let wc = 0x4000_u64;
    engine
        .mem_write(wc, &80_u32.to_le_bytes())
        .expect("WNDCLASSEXW.cbSize");
    engine
        .mem_write(wc + 4, &0_u32.to_le_bytes())
        .expect("WNDCLASSEXW.style");
    engine
        .mem_write(wc + 8, &0x7000_0000_u64.to_le_bytes())
        .expect("WNDCLASSEXW.lpfnWndProc");
    engine
        .mem_write(wc + 0x10, &0_i32.to_le_bytes())
        .expect("WNDCLASSEXW.cbClsExtra");
    engine
        .mem_write(wc + 0x14, &0_i32.to_le_bytes())
        .expect("WNDCLASSEXW.cbWndExtra");
    engine
        .mem_write(wc + 0x18, &image_base.to_le_bytes())
        .expect("WNDCLASSEXW.hInstance");
    engine
        .mem_write(wc + 0x20, &0_u64.to_le_bytes())
        .expect("WNDCLASSEXW.hIcon");
    engine
        .mem_write(wc + 0x28, &0_u64.to_le_bytes())
        .expect("WNDCLASSEXW.hCursor");
    engine
        .mem_write(wc + 0x30, &0_u64.to_le_bytes())
        .expect("WNDCLASSEXW.hbrBackground");
    engine
        .mem_write(wc + 0x38, &0x201_u64.to_le_bytes())
        .expect("WNDCLASSEXW.lpszMenuName");
    write_guest_utf16(&mut engine, 0x5000, "NotepadClass");
    engine
        .mem_write(wc + 0x40, &0x5000_u64.to_le_bytes())
        .expect("WNDCLASSEXW.lpszClassName");
    engine
        .mem_write(wc + 0x48, &0_u64.to_le_bytes())
        .expect("WNDCLASSEXW.hIconSm");

    // RegisterClassExW through the full dispatch path (names.rs → dense id).
    write_regs(&mut engine, wc, 0, 0, 0, 0);
    let atom = dispatch_user32(&mut engine, &mut state, "RegisterClassExW");
    assert_ne!(atom, 0, "the class must register");

    // CreateWindowExW(0, <class atom>, L"Untitled", 0, ..., hMenu = NULL).
    // The class has a guest WndProc, so the handler returns the WM_CREATE
    // bridge; the hwnd arrives in the callback's OuterReturn.
    write_guest_utf16(&mut engine, 0x6000, "Untitled - Notepad");
    write_regs(&mut engine, 0, atom, 0x6000, 0, 0x3000);
    for (slot, bytes) in [
        (0x3028_u64, 0_i32.to_le_bytes()), // X
        (0x3030, 0_i32.to_le_bytes()),     // Y
        (0x3038, 640_i32.to_le_bytes()),   // nWidth
        (0x3040, 480_i32.to_le_bytes()),   // nHeight
    ] {
        engine
            .mem_write(slot, &bytes)
            .expect("CreateWindowExW int arg");
    }
    engine
        .mem_write(0x3048, &0_u64.to_le_bytes())
        .expect("hWndParent");
    engine
        .mem_write(0x3050, &0_u64.to_le_bytes())
        .expect("hMenu NULL");
    engine
        .mem_write(0x3058, &image_base.to_le_bytes())
        .expect("hInstance");
    engine
        .mem_write(0x3060, &0_u64.to_le_bytes())
        .expect("lpParam");
    let id = crate::resolve_winapi_id("user32.dll", "CreateWindowExW")
        .expect("CreateWindowExW must resolve to a WinApiId");
    let result = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, default_env(), &mut state),
        id,
    );
    let error = result.expect_err("CreateWindowExW with a guest WndProc requests WM_CREATE");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    let WinApiControlSignal::GuestCallbackRequested { request } = signal else {
        panic!("expected the WM_CREATE callback, got {signal:?}");
    };
    let OuterReturn::CreateWindow(hwnd) = request.outer_return else {
        panic!("expected OuterReturn::CreateWindow");
    };
    assert_ne!(hwnd, 0);

    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(hwnd))
        .expect("window record exists");
    assert_ne!(
        window.menu_handle, 0,
        "a window without an explicit hMenu must inherit the class menu"
    );
    assert!(
        state.window_state().menu_dirty,
        "attaching a menu must dirty"
    );
}
