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

/// EnumDisplaySettingsA mirrors EnumDisplaySettingsW: mode 0 (and
/// ENUM_CURRENT_SETTINGS) fills a single 1920×1080@60, 32-bpp DEVMODEA and
/// returns TRUE; any other mode is exhausted (FALSE).
#[test]
fn test_enum_display_settings_a_returns_a_single_default_mode() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();

    // mode 0 at buffer 0x4000 (device string is ignored).
    write_regs(&mut engine, 0, 0, 0x4000, 0, STACK_TOP);
    assert_return_value!(
        user32::handle_enum_display_settings_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    let read_u32 = |engine: &mut IcedCpu, addr: u64| -> u32 {
        let mut b = [0_u8; 4];
        engine.mem_read(addr, &mut b).expect("read DEVMODE u32");
        u32::from_le_bytes(b)
    };
    let read_u16 = |engine: &mut IcedCpu, addr: u64| -> u16 {
        let mut b = [0_u8; 2];
        engine.mem_read(addr, &mut b).expect("read DEVMODE u16");
        u16::from_le_bytes(b)
    };
    // Win64 DEVMODEA: CHAR name fields make every post-name offset 0x20 less
    // than DEVMODEW (dmSize @0x24 = 156, bits @0x68, width @0x6C, height @0x70,
    // frequency @0x78).
    assert_eq!(read_u16(&mut engine, 0x4024), 156, "dmSize");
    assert_eq!(read_u32(&mut engine, 0x4068), 32, "dmBitsPerPel");
    assert_eq!(read_u32(&mut engine, 0x406C), 1920, "dmPelsWidth");
    assert_eq!(read_u32(&mut engine, 0x4070), 1080, "dmPelsHeight");
    assert_eq!(read_u32(&mut engine, 0x4078), 60, "dmDisplayFrequency");

    // A mode beyond the single entry is exhausted (FALSE).
    write_regs(&mut engine, 0, 1, 0x4000, 0, STACK_TOP);
    assert_return_value!(
        user32::handle_enum_display_settings_a(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
}

/// EnumDisplayMonitors must invoke the guest `MONITORENUMPROC` exactly once,
/// forwarding `(hMonitor, hdcMonitor, lprcMonitor, dwData)` and — via
/// `Passthrough` — returning the callback's BOOL as the outer API's result.
///
/// SDL2's windows video driver depends on this: `WIN_InitModes` enumerates
/// displays through `EnumDisplayMonitors`, so a handler that never calls back
/// makes the driver see zero displays and `SDL_InitSubSystem(SDL_INIT_VIDEO)`
/// fail.
#[test]
fn test_enum_display_monitors_bridges_to_guest_callback() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Seed the guest-control bump cursor (ctrl_va 0x2000, bump offset 0) so the
    // handler's coherent heap alloc for the fake RECT returns a valid address in
    // this thin harness; the real runtime seeds the same cursor at startup.
    engine
        .mem_write(0x2000, &0x3000_u64.to_le_bytes())
        .expect("seed guest heap bump cursor");
    // RCX=hdc, RDX=clip-rect, R8=MONITORENUMPROC, R9=dwData.
    write_regs(&mut engine, 0, 0, 0x9000, 0x1234, STACK_TOP);
    let result = user32::handle_enum_display_monitors(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ));
    let error = result.expect_err("EnumDisplayMonitors must bridge to the guest callback");
    let signal = error
        .downcast_ref::<crate::WinApiControlSignal>()
        .expect("control signal");
    match signal {
        crate::WinApiControlSignal::GuestCallbackRequested { request } => {
            assert_eq!(
                request.callback_address, 0x9000,
                "the guest MONITORENUMPROC"
            );
            assert_eq!(
                request.window_handle,
                user32::FAKE_MONITOR_HANDLE,
                "hMonitor goes in RCX"
            );
            assert_eq!(
                u64::from(request.message),
                user32::FAKE_DEVICE_CONTEXT_HANDLE,
                "hdcMonitor in RDX (fits in 32 bits)"
            );
            assert_eq!(request.long_parameter, 0x1234, "dwData in R9");
            assert_eq!(request.outer_return, crate::OuterReturn::Passthrough);
            // lprcMonitor must point at a real 1920×1080 RECT (not NULL) — other
            // callers besides SDL may dereference it.
            let rect_va = request.word_parameter;
            let read_i32 = |engine: &mut IcedCpu, addr: u64| -> i32 {
                let mut b = [0_u8; 4];
                engine.mem_read(addr, &mut b).expect("read RECT i32");
                i32::from_le_bytes(b)
            };
            assert_eq!(read_i32(&mut engine, rect_va), 0, "lprcMonitor.left");
            assert_eq!(read_i32(&mut engine, rect_va + 4), 0, "lprcMonitor.top");
            assert_eq!(
                read_i32(&mut engine, rect_va + 8),
                1920,
                "lprcMonitor.right"
            );
            assert_eq!(
                read_i32(&mut engine, rect_va + 12),
                1080,
                "lprcMonitor.bottom"
            );
        }
        other => panic!("unexpected signal: {other:?}"),
    }
}

/// A NULL `MONITORENUMPROC` (invalid per Win32) bails out with success rather
/// than faulting or bridging into a null pointer.
#[test]
fn test_enum_display_monitors_null_callback_succeeds() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        user32::handle_enum_display_monitors(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
}

/// GetDCEx returns NULL (0) for an unknown window instead of a fake DC handle,
/// honoring GetDCEx's documented failure mode (GetDC's fake-handle fallback is
/// not reused here).
#[test]
fn test_get_dc_ex_returns_zero_for_an_unknown_window() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_regs(&mut engine, 0x1234, 0, 0, 0, STACK_TOP);
    assert_return_value!(
        user32::handle_get_dc_ex(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
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

/// With a non-default `DisplayMetrics` (a 1728×1117 logical-point monitor),
/// `GetMonitorInfoW` fills rcMonitor AND rcWork with that geometry: the work
/// area equals the full monitor because WIE emulates no taskbar strip.
#[test]
fn test_get_monitor_info_w_reports_custom_display_metrics() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.display = crate::DisplayMetrics::new(1728, 1117);
    // RCX=hMonitor, RDX=MONITORINFO buffer.
    write_regs(
        &mut engine,
        user32::FAKE_MONITOR_HANDLE,
        0x4000,
        0,
        0,
        STACK_TOP,
    );
    assert_return_value!(
        user32::handle_get_monitor_info_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    let read_i32 = |engine: &mut IcedCpu, addr: u64| -> i32 {
        let mut b = [0_u8; 4];
        engine.mem_read(addr, &mut b).expect("read MONITORINFO i32");
        i32::from_le_bytes(b)
    };
    // MONITORINFO at 0x4000: rcMonitor @+4..20, rcWork @+20..36, dwFlags @36.
    assert_eq!(read_i32(&mut engine, 0x4004), 0, "rcMonitor.left");
    assert_eq!(read_i32(&mut engine, 0x4008), 0, "rcMonitor.top");
    assert_eq!(read_i32(&mut engine, 0x400C), 1728, "rcMonitor.right");
    assert_eq!(read_i32(&mut engine, 0x4010), 1117, "rcMonitor.bottom");
    assert_eq!(read_i32(&mut engine, 0x4014), 0, "rcWork.left");
    assert_eq!(read_i32(&mut engine, 0x4018), 0, "rcWork.top");
    assert_eq!(read_i32(&mut engine, 0x401C), 1728, "rcWork.right");
    assert_eq!(read_i32(&mut engine, 0x4020), 1117, "rcWork.bottom");
    assert_eq!(read_i32(&mut engine, 0x4024), 1, "MONITORINFOF_PRIMARY");
}

/// Every width/height-shaped SM_* metric follows the session's
/// `DisplayMetrics`, so GetSystemMetrics cannot disagree with the monitor
/// info or the device caps on a non-default display.
#[test]
fn test_get_system_metrics_follow_custom_display_metrics() {
    for (index, expected) in [
        (0_u64, 1728_u64), // SM_CXSCREEN
        (1, 1117),         // SM_CYSCREEN
        (16, 1728),        // SM_CXFULLSCREEN
        (17, 1117),        // SM_CYFULLSCREEN (no emulated taskbar)
        (60, 1117),        // SM_CYMAXTRACK
        (62, 1117),        // SM_CYMAXIMIZED
        (78, 1728),        // SM_CXVIRTUALSCREEN
        (79, 1117),        // SM_CYVIRTUALSCREEN
    ] {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        state.display = crate::DisplayMetrics::new(1728, 1117);
        write_regs(&mut engine, index, 0, 0, 0, STACK_TOP);
        assert_return_value!(
            user32::handle_get_system_metrics(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            expected
        );
    }
}

/// `EnumDisplaySettingsW` reports the session's `DisplayMetrics` resolution
/// (not a hardcoded desktop) so SDL's mode enumeration agrees with the rest
/// of the fake surface.
#[test]
fn test_enum_display_settings_w_reports_custom_display_metrics() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    state.display = crate::DisplayMetrics::new(1728, 1117);
    // DEVMODEW at 0x5000: dmPelsWidth @+0xAC, dmPelsHeight @+0xB0.
    write_regs(&mut engine, 0, 0, 0x5000, 0, STACK_TOP);
    assert_return_value!(
        user32::handle_enum_display_settings_w(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        1
    );
    let read_u32 = |engine: &mut IcedCpu, addr: u64| -> u32 {
        let mut b = [0_u8; 4];
        engine.mem_read(addr, &mut b).expect("read DEVMODE u32");
        u32::from_le_bytes(b)
    };
    assert_eq!(read_u32(&mut engine, 0x50AC), 1728, "dmPelsWidth");
    assert_eq!(read_u32(&mut engine, 0x50B0), 1117, "dmPelsHeight");
}
