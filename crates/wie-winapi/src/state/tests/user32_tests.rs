//! User32 tests: keyboard state, system colors, the message loop (PeekMessage / GetMessage), and GetWindowTextLength.
use super::*;

// --- User32 ---

#[test]
fn test_get_async_key_state_default() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // VK_RETURN = 0x0D, keyboard_state starts all zero.
    write_regs(&mut engine, 0x0D, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_async_key_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_get_async_key_state_down() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // VK_RETURN high bit set — index is a compile-time constant in bounds.
    state.window_state().keyboard_state.set(0x0D, 0x80);
    write_regs(&mut engine, 0x0D, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_async_key_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0x81
    );
}

#[test]
fn test_keyboard_state_shift_update_path() {
    // Mirrors the host input seam (app.rs set_key_state): pressing the
    // Shift key sets bit 0x80 on VK_SHIFT (0x10) and releasing clears it,
    // which is exactly what IsDialogMessage's Shift+Tab and
    // GetAsyncKeyState/GetKeyState read.
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // Press: app.rs set_key_state(0x10, true).
    let held = state.window_state().keyboard_state.get(0x10) | 0x80;
    state.window_state().keyboard_state.set(0x10, held);
    write_regs(&mut engine, 0x10, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_async_key_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0x81
    );

    // Release: app.rs set_key_state(0x10, false).
    let released = state.window_state().keyboard_state.get(0x10) & !0x80;
    state.window_state().keyboard_state.set(0x10, released);
    write_regs(&mut engine, 0x10, 0, 0, 0, 0);
    assert_return_value!(
        user32::handle_get_async_key_state(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_get_sys_color_highlight_is_not_bgr_swapped() {
    // COLOR_HIGHLIGHT (13) and COLOR_ACTIVECAPTION (2) are #0078D7 stored
    // as 0RGB — the old 0xD77830 was the B/R-swapped value (rendered
    // orange).
    for index in [13_u64, 2] {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, index, 0, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_sys_color(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0x0000_78D7
        );
    }
}

#[test]
fn test_get_sys_color_3d_edge_colors() {
    // COLOR_BTNHIGHLIGHT (20) = white; COLOR_3DDKSHADOW (21) = dark gray
    // (both previously fell through to the BTNFACE fallback).
    let cases = [(20_u64, 0x00FF_FFFF_u64), (21, 0x0069_6969)];
    for (index, expected) in cases {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        write_regs(&mut engine, index, 0, 0, 0, 0);
        assert_return_value!(
            user32::handle_get_sys_color(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            expected
        );
    }
}

#[test]
fn test_peek_message_a_empty_queue() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Write a valid MSG struct address (doesn't matter since queue is empty).
    write_regs(&mut engine, 0x1000, 0, 0, 0, 0x2000);
    assert_return_value!(
        user32::handle_peek_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

#[test]
fn test_peek_message_a_with_message() {
    use crate::QueuedWindowMessage;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_va = 0x4000;
    // Map memory for the MSG struct.
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    // Push a WM_PAINT message for any window.
    state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .messages
        .push(QueuedWindowMessage {
            window_handle: crate::handles::Hwnd::from(0x100),
            message: 15, // WM_PAINT
            word_parameter: 0,
            long_parameter: 0,
            time: 1,
            point_x: 0,
            point_y: 0,
        });
    // PeekMessageA(msg_va=msg_va, hwnd=0, min=0, max=0, wRemoveMsg=1)
    // wRemoveMsg is on the stack at RSP+0x28.
    write_regs(&mut engine, msg_va, 0, 0, 0, 0x3000);
    // Write wRemoveMsg=1 (PM_REMOVE) at RSP+0x28.
    engine.mem_write(0x3028, &1_u32.to_le_bytes()).ok();
    assert_return_value!(
        user32::handle_peek_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    // WM_PAINT should have been removed from the queue.
    assert_eq!(
        state
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .len(),
        0
    );
}

#[test]
fn test_peek_message_a_noremove() {
    use crate::QueuedWindowMessage;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_va = 0x4000;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .messages
        .push(QueuedWindowMessage {
            window_handle: crate::handles::Hwnd::from(0x100),
            message: 15,
            word_parameter: 0,
            long_parameter: 0,
            time: 1,
            point_x: 0,
            point_y: 0,
        });
    write_regs(&mut engine, msg_va, 0, 0, 0, 0x3000);
    // wRemoveMsg=0 (PM_NOREMOVE) at RSP+0x28.
    engine.mem_write(0x3028, &0_u32.to_le_bytes()).ok();
    assert_return_value!(
        user32::handle_peek_message_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    // Message should still be in the queue.
    assert_eq!(
        state
            .message_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .messages
            .len(),
        1
    );
}

#[test]
fn test_get_message_wm_quit_bypasses_window_filter() {
    use crate::QueuedWindowMessage;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_va = 0x4000;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    // WM_QUIT addressed to a window that does NOT match the filter.
    state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .messages
        .push(QueuedWindowMessage {
            window_handle: crate::handles::Hwnd::from(0x1234),
            message: 0x12, // WM_QUIT
            word_parameter: 7,
            long_parameter: 0,
            time: 1,
            point_x: 0,
            point_y: 0,
        });
    // GetMessageA(msg_va, hWnd=0x5678 filter, min=0, max=0).
    write_regs(&mut engine, msg_va, 0x5678, 0, 0, 0x3000);
    let r = user32::handle_get_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetMessageA");
    // GetMessage returns 0 (FALSE) for WM_QUIT regardless of the filter.
    assert_eq!(r.return_value, 0);
    // The returned MSG carries the WM_QUIT and its wParam.
    let mut bytes = [0_u8; 4];
    engine
        .mem_read(msg_va + 8, &mut bytes)
        .expect("read MSG.message");
    assert_eq!(u32::from_le_bytes(bytes), 0x12);
    engine
        .mem_read(msg_va + 16, &mut bytes)
        .expect("read MSG.wParam");
    assert_eq!(u32::from_le_bytes(bytes), 7);
}

#[test]
fn test_get_message_dialog_filter_matches_descendants() {
    use crate::QueuedWindowMessage;
    use crate::WindowRecord;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let msg_va = 0x4000;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg struct");
    // Owner (parentless) → dialog (parent = owner) → button (parent = dialog).
    let owner = 0x6610_0001_u64;
    let dialog = 0x6610_0002_u64;
    let child = 0x6610_0003_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(owner),
        title: "Owner".to_owned(),
        width: 100,
        height: 100,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(dialog),
        parent_handle: crate::handles::Hwnd::from(owner),
        title: "Dialog".to_owned(),
        dialog_proc: 0x7000_0001,
        width: 80,
        height: 60,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(child),
        parent_handle: crate::handles::Hwnd::from(dialog),
        control_kind: Some(crate::user32::controls::ControlClassKind::Button),
        control_text: "OK".to_owned(),
        menu_handle: 1,
        visible: true,
        width: 40,
        height: 20,
        ..Default::default()
    });
    // A message addressed to the button inside the dialog.
    state
        .message_queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .messages
        .push(QueuedWindowMessage {
            window_handle: crate::handles::Hwnd::from(child),
            message: 0x0100, // WM_KEYDOWN
            word_parameter: 0x09,
            long_parameter: 0,
            time: 1,
            point_x: 0,
            point_y: 0,
        });
    // GetMessageA(msg_va, hWnd=dialog, min=0, max=0): the child's message
    // must match through the dialog's descendant rule.
    write_regs(&mut engine, msg_va, dialog, 0, 0, 0x3000);
    let r = user32::handle_get_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetMessageA");
    assert_eq!(r.return_value, 1);
    let mut bytes = [0_u8; 8];
    engine.mem_read(msg_va, &mut bytes).expect("read MSG.hwnd");
    assert_eq!(u64::from_le_bytes(bytes), child);
}

#[test]
fn test_empty_queue_yields_while_dialog_open_under_exit_on_idle() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().message_queue_idle_policy = MessageQueueIdlePolicy::ExitOnIdle;
    // An open modal dialog must never see the synthetic regression WM_QUIT.
    state.lock_message_queue().dialog_depth = 1;
    write_regs(&mut engine, 0x1000, 0, 0, 0, 0x2000);
    let result = user32::handle_get_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ));
    assert!(
        result.is_err(),
        "empty queue under an open dialog must yield"
    );
    let error = result.expect_err("expected the WaitingForMessage signal");
    assert!(
        error
            .downcast_ref::<WinApiControlSignal>()
            .is_some_and(|signal| matches!(signal, WinApiControlSignal::WaitingForMessage)),
        "expected WaitingForMessage, got {error:?}"
    );
    // No WM_QUIT was synthesized into the queue.
    assert!(
        state
            .lock_message_queue()
            .messages
            .iter()
            .all(|m| m.message != 0x12)
    );
    // With no dialog open, ExitOnIdle still synthesizes WM_QUIT (regression
    // path) and GetMessage returns 0.
    state.lock_message_queue().dialog_depth = 0;
    let r = user32::handle_get_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("GetMessageA");
    assert_eq!(r.return_value, 0);
}

#[test]
fn test_get_window_text_length_w_reports_text_length() {
    // Full dispatch path: name resolution (names.rs) → dense id → handler arm.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().window_title = "Hello".to_string();
    write_regs(&mut engine, user32::FAKE_WINDOW_HANDLE, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthW")
        .expect("GetWindowTextLengthW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthW must dispatch");
    assert_eq!(
        r.return_value, 5,
        "\"Hello\" is 5 UTF-16 units excluding the NUL"
    );
}

#[test]
fn test_get_window_text_length_w_empty_is_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, user32::FAKE_WINDOW_HANDLE, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthW")
        .expect("GetWindowTextLengthW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthW must dispatch");
    assert_eq!(r.return_value, 0, "empty title must report length 0");
}

#[test]
fn test_get_window_text_length_w_unknown_hwnd_is_zero() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x1234, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthW")
        .expect("GetWindowTextLengthW must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthW must dispatch");
    assert_eq!(r.return_value, 0, "unknown hwnd must report length 0");
}

#[test]
fn test_get_window_text_length_a_matches_ascii() {
    // ANSI mirror: for ASCII text the byte count equals the UTF-16 unit count.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().window_title = "Hello".to_string();
    write_regs(&mut engine, user32::FAKE_WINDOW_HANDLE, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthA")
        .expect("GetWindowTextLengthA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthA must dispatch");
    assert_eq!(
        r.return_value, 5,
        "ASCII \"Hello\" is 5 ANSI chars excluding the NUL"
    );
}

#[test]
fn test_get_window_text_length_a_counts_cp1252_chars() {
    // Windows ANSI length counts CP1252 characters, not UTF-8 bytes:
    // "café" is 5 UTF-8 bytes but 4 CP1252 chars (é encodes to one byte).
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.window_state().window_title = "café".to_string();
    write_regs(&mut engine, user32::FAKE_WINDOW_HANDLE, 0, 0, 0, 0);
    let id = crate::resolve_winapi_id("user32.dll", "GetWindowTextLengthA")
        .expect("GetWindowTextLengthA must resolve to a WinApiId");
    let r = crate::dispatch_winapi_id(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        id,
    )
    .expect("GetWindowTextLengthA must dispatch");
    assert_eq!(
        r.return_value, 4,
        "\"café\" is 4 CP1252 chars, not 5 UTF-8 bytes"
    );
}
