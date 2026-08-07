//! LoadAcceleratorsW / TranslateAcceleratorW tests: cached handles, WM_COMMAND posting, and table destruction.
use super::*;

// --- LoadAcceleratorsW / TranslateAcceleratorW ---

/// Seed the main-module accelerator tables with notepad-like entries.
fn push_accel_tables(state: &mut WinApiState) {
    use wie_pe::resources::{AccelEntry, AccelTemplate};
    state.process.main_module_accelerators.push(AccelTemplate {
        id: 0x0100,
        lang: 0x0409,
        entries: vec![
            // FVIRTKEY|FCONTROL, VK_N → File New (0x0100).
            AccelEntry {
                flags: 0x09,
                key: 0x4E,
                command_id: 0x0100,
            },
            // FVIRTKEY|FSHIFT, VK_O → Save As (0x0103).
            AccelEntry {
                flags: 0x05,
                key: 0x4F,
                command_id: 0x0103,
            },
            // Plain char 'a' (no VIRTKEY) → 0x0111.
            AccelEntry {
                flags: 0x00,
                key: 0x61,
                command_id: 0x0111,
            },
        ],
    });
}

#[test]
fn test_load_accelerators_w_returns_cached_handle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    // MAKEINTRESOURCEW(0x100): hinst = image base, low word = table id.
    let image_base = default_env().image_base;
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let first = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_ne!(first, 0, "known table id must return a nonzero HACCEL");
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let second = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_eq!(
        first, second,
        "the same table id must return the same HACCEL"
    );
}

#[test]
fn test_load_accelerators_w_unknown_table_returns_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    // Table id 0x200 is not in the parsed set.
    write_regs(&mut engine, image_base, 0x200, 0, 0, 0);
    let handle = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_eq!(handle, 0, "unknown table id must return NULL");
}

#[test]
fn test_translate_accelerator_w_posts_wm_command() {
    use crate::QueuedWindowMessage;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    let hwnd = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        ..Default::default()
    });
    // Load the table and note the cached HACCEL.
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let haccel = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_ne!(haccel, 0);

    // MSG struct at 0x4000: message = WM_KEYDOWN, wParam = VK_N (0x4E).
    let msg_va = 0x4000_u64;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    let mut msg = hwnd.to_le_bytes().to_vec();
    msg.extend_from_slice(&crate::user32::WM_KEYDOWN.to_le_bytes());
    msg.extend_from_slice(&[0_u8; 4]); // alignment padding
    msg.extend_from_slice(&0x4E_u64.to_le_bytes());
    msg.extend_from_slice(&0_u64.to_le_bytes()); // lParam
    engine.mem_write(msg_va, &msg).expect("write MSG struct");
    // Ctrl is down (the table entry requires FCONTROL).
    state.window_state().keyboard_state.set(0x11, 0x80);

    write_regs(&mut engine, hwnd, haccel, msg_va, 0, 0);
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorW");
    assert_eq!(translated, 1, "matching VK + Ctrl must translate");

    let queue = state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let posted = queue
        .messages
        .iter()
        .find(|m: &&QueuedWindowMessage| m.message == crate::user32::WM_COMMAND)
        .expect("WM_COMMAND must be posted");
    assert_eq!(posted.window_handle.as_u64(), hwnd);
    assert_eq!(
        posted.word_parameter, 0x0100,
        "WM_COMMAND wParam = table id"
    );
    assert_eq!(posted.long_parameter, 0);
}

#[test]
fn test_translate_accelerator_w_no_match_posts_nothing() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    let hwnd = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        ..Default::default()
    });
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let haccel = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");

    let msg_va = 0x4000_u64;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    // WM_KEYDOWN with VK_N but Ctrl NOT down: the FCONTROL entry must not match.
    let mut msg = hwnd.to_le_bytes().to_vec();
    msg.extend_from_slice(&crate::user32::WM_KEYDOWN.to_le_bytes());
    msg.extend_from_slice(&[0_u8; 4]);
    msg.extend_from_slice(&0x4E_u64.to_le_bytes());
    msg.extend_from_slice(&0_u64.to_le_bytes());
    engine.mem_write(msg_va, &msg).expect("write MSG struct");

    write_regs(&mut engine, hwnd, haccel, msg_va, 0, 0);
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorW");
    assert_eq!(translated, 0, "Ctrl-up must not translate");
    assert_eq!(
        state
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .len(),
        0,
        "no WM_COMMAND may be posted"
    );
}

#[test]
fn test_translate_accelerator_w_plain_char() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    let hwnd = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        ..Default::default()
    });
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let haccel = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");

    // A non-VIRTKEY entry matches a WM_CHAR whose wParam is the ANSI char.
    let msg_va = 0x4000_u64;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    let mut msg = hwnd.to_le_bytes().to_vec();
    msg.extend_from_slice(&crate::user32::WM_CHAR.to_le_bytes());
    msg.extend_from_slice(&[0_u8; 4]);
    msg.extend_from_slice(&0x61_u64.to_le_bytes()); // 'a'
    msg.extend_from_slice(&0_u64.to_le_bytes());
    engine.mem_write(msg_va, &msg).expect("write MSG struct");

    write_regs(&mut engine, hwnd, haccel, msg_va, 0, 0);
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorW");
    assert_eq!(translated, 1, "plain-char match must translate");
    let queue = state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let posted = queue
        .messages
        .iter()
        .find(|m: &&QueuedWindowMessage| m.message == crate::user32::WM_COMMAND)
        .expect("WM_COMMAND must be posted");
    assert_eq!(posted.word_parameter, 0x0111);
}

#[test]
fn test_translate_accelerator_a_mirrors_w() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    let hwnd = 0x6610_1000_u64;
    state.window_state().windows.push(crate::WindowRecord {
        handle: crate::handles::Hwnd::from(hwnd),
        ..Default::default()
    });
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let haccel = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsA");

    // Shift+O (0x05 = FVIRTKEY|FSHIFT = 0x01|0x04, VK_O) via the A variant.
    state.window_state().keyboard_state.set(0x10, 0x80); // VK_SHIFT
    let msg_va = 0x4000_u64;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    let mut msg = hwnd.to_le_bytes().to_vec();
    msg.extend_from_slice(&crate::user32::WM_KEYDOWN.to_le_bytes());
    msg.extend_from_slice(&[0_u8; 4]);
    msg.extend_from_slice(&0x4F_u64.to_le_bytes()); // VK_O
    msg.extend_from_slice(&0_u64.to_le_bytes());
    engine.mem_write(msg_va, &msg).expect("write MSG struct");

    write_regs(&mut engine, hwnd, haccel, msg_va, 0, 0);
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorA");
    assert_eq!(translated, 1, "Shift+O must translate via the A variant");
    let queue = state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let posted = queue
        .messages
        .iter()
        .find(|m: &&QueuedWindowMessage| m.message == crate::user32::WM_COMMAND)
        .expect("WM_COMMAND must be posted");
    assert_eq!(posted.word_parameter, 0x0103);
}

#[test]
fn test_translate_accelerator_w_unknown_haccel_is_false() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_va = 0x4000_u64;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    write_regs(
        &mut engine,
        0x6610_1000,
        0x0000_0000_6640_0005,
        msg_va,
        0,
        0,
    );
    let translated = dispatch_user32(&mut engine, &mut state, "TranslateAcceleratorW");
    assert_eq!(translated, 0, "an unallocated HACCEL must not translate");
}

#[test]
fn test_destroy_accelerator_table_frees_and_reloads_fresh() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    push_accel_tables(&mut state);
    let image_base = default_env().image_base;
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let first = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_ne!(first, 0, "known table id must return a nonzero HACCEL");

    write_regs(&mut engine, first, 0, 0, 0, 0);
    let destroyed = dispatch_user32(&mut engine, &mut state, "DestroyAcceleratorTable");
    assert_eq!(destroyed, 1, "a loaded HACCEL must destroy successfully");

    // The freed (module, id) pair must allocate a fresh handle on reload.
    write_regs(&mut engine, image_base, 0x100, 0, 0, 0);
    let reloaded = dispatch_user32(&mut engine, &mut state, "LoadAcceleratorsW");
    assert_ne!(reloaded, 0, "reload of a freed table must still succeed");
    assert_ne!(
        reloaded, first,
        "a destroyed table must not return the stale HACCEL"
    );
}

#[test]
fn test_destroy_accelerator_table_unknown_handle_is_false() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // No table was ever loaded, so any handle is unknown.
    write_regs(&mut engine, 0x0000_0000_6640_00FF, 0, 0, 0, 0);
    let destroyed = dispatch_user32(&mut engine, &mut state, "DestroyAcceleratorTable");
    assert_eq!(destroyed, 0, "an unallocated HACCEL must not destroy");
}
