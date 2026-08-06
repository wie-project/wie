//! BUTTON / STATIC tests: BM_CLICK, space-key activation, BM_GETSTATE / BM_SETSTATE, track-mouse events, erase-background, IsWindow / IsDialogMessage, plus the label-control region repaint invalidation.
use super::*;

// ── Windows-fidelity batch (fix-28): BM_* / focus / erase / tracking ──

#[test]
fn test_bm_click_delivers_command_to_parent() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, button) = push_button_pair(&mut state);

    // SendMessage(button, BM_CLICK): the host control WndProc must bridge
    // WM_COMMAND(MAKEWPARAM(7, BN_CLICKED)) to the parent's WndProc.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_CLICK,
        0,
        0,
    );
    let error = result.expect_err("BM_CLICK must bridge a WM_COMMAND callback");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent
                    && request.message == 0x0111
                    && request.word_parameter == 7
        ),
        "BM_CLICK must bridge WM_COMMAND(MAKEWPARAM(7, BN_CLICKED)) to the \
             parent WndProc, got {signal:?}"
    );
}

#[test]
fn test_space_keyboard_activates_focused_button() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (parent, button) = push_button_pair(&mut state);
    state.window_state().focus_window_handle = crate::handles::Hwnd::from(button);

    // WM_KEYDOWN VK_SPACE on the focused button presses it.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_SPACE,
        0,
    )
    .expect("keydown handled")
    .expect("some result");
    assert_eq!(r, 0);
    assert!(
        state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(button))
            .is_some_and(|w| w.flags.contains(WindowFlags::PRESSED)),
        "space keydown must press the focused button"
    );

    // WM_KEYUP VK_SPACE releases and delivers BN_CLICKED to the parent.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_KEYUP,
        crate::user32::VK_SPACE,
        0,
    );
    let error = result.expect_err("space keyup must click the button");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.window_handle == parent && request.word_parameter == 7
        ),
        "space keyup must deliver WM_COMMAND(7) to the parent, got {signal:?}"
    );

    // Space on a NON-focused button is ignored (no press, no click).
    state.window_state().focus_window_handle = crate::handles::Hwnd::NULL;
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_SPACE,
        0,
    )
    .expect("non-focused keydown unhandled");
    assert!(r.is_none(), "non-focused button must not react to space");
}

#[test]
fn test_bm_get_set_state_roundtrip() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, button) = push_button_pair(&mut state);

    // Fresh button: not pressed, not focused.
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_GETSTATE,
        0,
        0,
    )
    .expect("getstate ok")
    .expect("some result");
    assert_eq!(r, 0);

    // BM_SETSTATE(TRUE) returns the previous (0) and presses.
    let prev = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_SETSTATE,
        1,
        0,
    )
    .expect("setstate ok")
    .expect("some result");
    assert_eq!(prev, 0);
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_GETSTATE,
        0,
        0,
    )
    .expect("getstate ok")
    .expect("some result");
    assert_eq!(r, crate::user32::BST_PUSHED);

    // BM_SETSTATE(FALSE) returns 1 (was pressed) and releases.
    let prev = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::BM_SETSTATE,
        0,
        0,
    )
    .expect("setstate ok")
    .expect("some result");
    assert_eq!(prev, 1);
}

#[test]
fn edit_message_on_a_button_does_not_touch_edit_state() {
    use crate::user32::controls::ControlState;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, button) = push_button_pair(&mut state);

    // EM_SETSEL is an EDIT-only message: a BUTTON must not handle it...
    let r = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::wm::WinMsg::EM_SETSEL.as_u32(),
        2,
        4,
    )
    .expect("dispatch");
    assert!(r.is_none(), "a Button must not handle EM_SETSEL");

    // ... and no EDIT-typed state may exist for the button (reading
    // `caret` on the entry would be a compile error anyway).
    let entry = state
        .window_state()
        .control_states
        .get(&crate::handles::Hwnd::from(button));
    assert!(
        matches!(entry, None | Some(ControlState::Button { .. })),
        "EM_SETSEL on a Button must not create EDIT state, got {entry:?}"
    );
}

#[test]
fn test_track_mouse_event_arms_and_cancels() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let win = 0x6610_0001_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(win),
        title: "Tracked".to_owned(),
        width: 100,
        height: 100,
        ..Default::default()
    });
    let tme = 0x5000_u64;
    engine
        .mem_map(tme, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map tme struct");
    // TRACKMOUSEEVENT: cbSize=24, dwFlags=TME_HOVER|TME_LEAVE, hwndTrack=win.
    engine
        .mem_write(tme, &24_u32.to_le_bytes())
        .expect("cbSize");
    engine
        .mem_write(tme + 4, &3_u32.to_le_bytes())
        .expect("flags");
    engine
        .mem_write(tme + 8, &win.to_le_bytes())
        .expect("hwndTrack");

    write_regs(&mut engine, tme, 0, 0, 0, 0);
    let r = user32::handle_track_mouse_event(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("TrackMouseEvent");
    assert_eq!(r.return_value, 1);
    assert!(
        state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(win))
            .expect("tracked window exists")
            .mouse_tracking,
        "TME must arm tracking on the target window"
    );

    // TME_CANCEL releases the tracking request.
    engine
        .mem_write(tme + 4, &0x8000_0000_u32.to_le_bytes())
        .expect("cancel flags");
    write_regs(&mut engine, tme, 0, 0, 0, 0);
    let r = user32::handle_track_mouse_event(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("TrackMouseEvent cancel");
    assert_eq!(r.return_value, 1);
    assert!(
        !state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(win))
            .expect("tracked window exists")
            .mouse_tracking,
        "TME_CANCEL must clear tracking"
    );
}

#[test]
fn test_erase_background_fills_class_brush_and_clears_flag() {
    let mut state = default_winapi_state();
    // Register a class with the classic (HBRUSH)(COLOR_WINDOW + 1) stock
    // brush (6 = COLOR_WINDOW + 1) — the white background.
    let atom = crate::user32::register_window_class(
        &mut state,
        WindowClassRecord {
            atom: 0,
            class_name: "EraseClass".to_owned(),
            window_proc: 0x7000_0000,
            style: 0,
            instance_handle: 0,
            icon_handle: 0,
            cursor_handle: 0,
            background_brush: 6,
            small_icon_handle: 0,
            menu_name: 0,
            unicode: false,
        },
    )
    .expect("register class");
    let win = 0x6610_0001_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(win),
        class_atom: u16::try_from(atom).unwrap_or(0),
        title: "Erase".to_owned(),
        width: 64,
        height: 32,
        invalidated: true,
        flags: WindowFlags::ERASE_BACKGROUND,
        ..Default::default()
    });

    assert!(
        crate::user32::message::erase_window_background(&mut state, win),
        "a class brush must erase the background"
    );
    assert!(
        !state
            .window_state()
            .windows
            .iter()
            .find(|w| w.handle == crate::handles::Hwnd::from(win))
            .expect("erased window exists")
            .flags
            .contains(WindowFlags::ERASE_BACKGROUND),
        "the erase must consume the pending-erase flag"
    );
    let surf = state
        .present()
        .surfaces
        .get(&crate::handles::Hwnd::from(win))
        .expect("erase surface exists");
    assert_eq!(surf.pixels.len(), 64 * 32);
    assert!(
        surf.pixels.iter().all(|&p| p == 0x00FF_FFFF),
        "COLOR_WINDOW brush must fill white (0RGB)"
    );

    // No class brush → no erase possible.
    let win2 = 0x6610_0002_u64;
    state.window_state().windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(win2),
        title: "NoBrush".to_owned(),
        width: 8,
        height: 8,
        ..Default::default()
    });
    assert!(
        !crate::user32::message::erase_window_background(&mut state, win2),
        "no class brush → nothing to erase"
    );
}

#[test]
fn test_is_window_recognizes_real_windows() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let parent = 0x6610_0001_u64;
    let child = 0x6610_0002_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(parent),
        title: "Parent".to_owned(),
        width: 100,
        height: 100,
        visible: true,
        flags: WindowFlags::ENABLED,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(child),
        parent_handle: crate::handles::Hwnd::from(parent),
        control_kind: Some(crate::user32::controls::ControlClassKind::Button),
        title: "Child".to_owned(),
        visible: false,
        width: 40,
        height: 20,
        ..Default::default()
    });

    // IsWindow: real windows (including children) are valid.
    for handle in [parent, child] {
        write_regs(&mut engine, handle, 0, 0, 0, 0);
        let r = user32::handle_is_window(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("IsWindow");
        assert_eq!(r.return_value, 1, "IsWindow({handle:#x})");
    }
    write_regs(&mut engine, 0xDEAD, 0, 0, 0, 0);
    let r = user32::handle_is_window(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindow");
    assert_eq!(r.return_value, 0, "unknown handle is not a window");

    // IsWindowVisible: parent visible, child hidden.
    write_regs(&mut engine, parent, 0, 0, 0, 0);
    let r = user32::handle_is_window_visible(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindowVisible");
    assert_eq!(r.return_value, 1);
    write_regs(&mut engine, child, 0, 0, 0, 0);
    let r = user32::handle_is_window_visible(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindowVisible");
    assert_eq!(r.return_value, 0);

    // IsWindowEnabled: child disabled, parent enabled.
    write_regs(&mut engine, child, 0, 0, 0, 0);
    let r = user32::handle_is_window_enabled(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindowEnabled");
    assert_eq!(r.return_value, 0);
    write_regs(&mut engine, parent, 0, 0, 0, 0);
    let r = user32::handle_is_window_enabled(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsWindowEnabled");
    assert_eq!(r.return_value, 1);
}

#[test]
fn test_is_dialog_message_enter_resolves_default_button() {
    use crate::user32::{VK_RETURN, WM_KEYDOWN};
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let dialog = 0x6610_0001_u64;
    let ok_button = 0x6610_0002_u64;
    let cancel_button = 0x6610_0003_u64;
    {
        let ws = state.window_state();
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(dialog),
            dialog_proc: 0x7000_0001,
            title: "Dlg".to_owned(),
            width: 160,
            height: 60,
            visible: true,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(ok_button),
            parent_handle: crate::handles::Hwnd::from(dialog),
            control_kind: Some(crate::user32::controls::ControlClassKind::Button),
            control_text: "OK".to_owned(),
            menu_handle: 1,
            visible: true,
            width: 60,
            height: 20,
            ..Default::default()
        });
        ws.windows.push(WindowRecord {
            handle: crate::handles::Hwnd::from(cancel_button),
            parent_handle: crate::handles::Hwnd::from(dialog),
            control_kind: Some(crate::user32::controls::ControlClassKind::Button),
            control_text: "Cancel".to_owned(),
            menu_handle: 2,
            visible: true,
            width: 60,
            height: 20,
            ..Default::default()
        });
    }
    // OK is the BS_DEFPUSHBUTTON default; focus sits on Cancel.
    state
        .window_state()
        .control_states
        .entry(crate::handles::Hwnd::from(ok_button))
        .or_insert_with(|| crate::user32::controls::ControlClassKind::Button.new_state())
        .set_default_push(true);
    state.window_state().focus_window_handle = crate::handles::Hwnd::from(cancel_button);

    let msg_va = 0x4000;
    engine
        .mem_map(msg_va, 0x1000, wie_cpu::RwxPerms::ALL)
        .expect("map msg");
    let dispatch = |engine: &mut IcedCpu, state: &mut WinApiState| {
        engine
            .mem_write(msg_va + 8, &WM_KEYDOWN.to_le_bytes())
            .expect("message");
        engine
            .mem_write(msg_va + 16, &VK_RETURN.to_le_bytes())
            .expect("wParam");
        write_regs(engine, dialog, msg_va, 0, 0, 0x3000);
        let result = crate::user32::handle_is_dialog_message_a(&mut HandlerContext::new(
            engine,
            test_environment(),
            state,
        ));
        let error = result.expect_err("Enter must be consumed by the dialog");
        error
            .downcast_ref::<WinApiControlSignal>()
            .expect("control signal")
            .clone()
    };

    // Enter with a default push button activates IT (id 1), not the
    // focused Cancel (id 2).
    let signal = dispatch(&mut engine, &mut state);
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.message == 0x0111 && request.word_parameter == 1
        ),
        "Enter must activate the BS_DEFPUSHBUTTON (id 1), got {signal:?}"
    );

    // Without a default button, Enter falls back to the focused button.
    state
        .window_state()
        .control_states
        .get_mut(&crate::handles::Hwnd::from(ok_button))
        .expect("ok state")
        .set_default_push(false);
    state.window_state().focus_window_handle = crate::handles::Hwnd::from(cancel_button);
    let signal = dispatch(&mut engine, &mut state);
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.word_parameter == 2
        ),
        "Enter with no default must activate the focused button (id 2), got {signal:?}"
    );

    // Neither a default nor a focused button: Enter is NOT consumed
    // (IsDialogMessage returns FALSE, caller dispatches normally).
    state.window_state().focus_window_handle = crate::handles::Hwnd::NULL;
    engine
        .mem_write(msg_va + 8, &WM_KEYDOWN.to_le_bytes())
        .expect("message");
    engine
        .mem_write(msg_va + 16, &VK_RETURN.to_le_bytes())
        .expect("wParam");
    write_regs(&mut engine, dialog, msg_va, 0, 0, 0x3000);
    let r = crate::user32::handle_is_dialog_message_a(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("IsDialogMessage completes");
    assert_eq!(
        r.return_value, 0,
        "Enter with no button target must not be consumed"
    );
}

#[test]
fn test_sys_color_mapping() {
    // COLOR_WINDOW (5) → white; COLOR_BTNFACE (15) → gray; these drive the
    // WM_ERASEBKGND class-brush fill and GetSysColor.
    assert_eq!(crate::user32::window::sys_color(5), 0x00FF_FFFF);
    assert_eq!(crate::user32::window::sys_color(15), 0x00F0_F0F0);
    assert_eq!(crate::user32::window::sys_color(0), 0x00C8_C8C8);
}

/// A top-level window with a BUTTON child (100×30 at (10,10)), for the
/// label-control region tests.
fn push_button_paint_pair(state: &mut WinApiState) -> (u64, u64) {
    let top = 0x6610_0031_u64;
    let button = 0x6610_0032_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(top),
        title: "Top".to_owned(),
        visible: true,
        width: 200,
        height: 100,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(button),
        parent_handle: crate::handles::Hwnd::from(top),
        x: 10,
        y: 10,
        width: 100,
        height: 30,
        control_kind: Some(crate::user32::controls::ControlClassKind::Button),
        control_text: "OK".to_owned(),
        visible: true,
        ..Default::default()
    });
    (top, button)
}

/// A top-level window with a STATIC child (140×20 at (10,50)), for the
/// label-control region tests.
fn push_static_paint_pair(state: &mut WinApiState) -> (u64, u64) {
    let top = 0x6610_0041_u64;
    let label = 0x6610_0042_u64;
    let ws = state.window_state();
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(top),
        title: "Top".to_owned(),
        visible: true,
        width: 200,
        height: 100,
        ..Default::default()
    });
    ws.windows.push(WindowRecord {
        handle: crate::handles::Hwnd::from(label),
        parent_handle: crate::handles::Hwnd::from(top),
        x: 10,
        y: 50,
        width: 140,
        height: 20,
        control_kind: Some(crate::user32::controls::ControlClassKind::Static),
        control_text: "Ready".to_owned(),
        visible: true,
        ..Default::default()
    });
    (top, label)
}

/// The first paint of a BUTTON covers its whole rect — the surface behind a
/// never-painted control is undefined, so the region cannot be narrowed.
#[test]
fn test_button_first_paint_reports_the_full_control_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, button) = push_button_paint_pair(&mut state);

    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame");
    assert_eq!(
        frame.region, None,
        "the first paint reports the full surface (the fresh accumulator \
         starts fully dirty and a partial mark cannot narrow it)"
    );
}

/// A BUTTON caption change (WM_SETTEXT) must report ONLY the caption band —
/// the union of the old and new caption rects — instead of the full control
/// rect, and every pixel change must land inside that region.
#[test]
fn test_button_caption_change_paints_only_the_caption_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, button) = push_button_paint_pair(&mut state);

    // First paint (full); capture the frame with the old caption "OK".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let before = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();

    // Change the caption to a longer one, then repaint.
    write_guest_ansi(&mut engine, 0x4000, "Start!");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::wm::WinMsg::WM_SETTEXT.as_u32(),
        0,
        0x4000,
    )
    .expect("settext ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint2 ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let after = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();

    let region = after.region.expect("a caption change is a partial repaint");
    let control = crate::gdi32::IRect {
        left: 10,
        top: 10,
        right: 110,
        bottom: 40,
    };
    assert!(
        region.left >= control.left
            && region.top >= control.top
            && region.right <= control.right
            && region.bottom <= control.bottom,
        "the region must stay inside the control, got {region:?}"
    );
    assert!(
        region != control,
        "a caption change must not report the full control rect, got {region:?}"
    );
    assert!(
        region.height() < control.height(),
        "the region is a single caption band, not the full face, got {region:?}"
    );

    // Every pixel diff between the two frames lies inside the region.
    let mut diffs = 0_usize;
    for y in 0..after.height {
        for x in 0..after.width {
            let idx = usize::try_from(y)
                .unwrap_or(0)
                .saturating_mul(after.width as usize)
                .saturating_add(usize::try_from(x).unwrap_or(0));
            if before.pixels.get(idx) != after.pixels.get(idx) {
                assert!(
                    i64::from(x) >= i64::from(region.left)
                        && i64::from(x) < i64::from(region.right)
                        && i64::from(y) >= i64::from(region.top)
                        && i64::from(y) < i64::from(region.bottom),
                    "a pixel diff at ({x},{y}) lies outside the reported region"
                );
                diffs = diffs.saturating_add(1);
            }
        }
    }
    assert!(diffs > 0, "the caption change must repaint pixels");
}

/// A BUTTON pressed-state change must report ONLY the face rect — the
/// interior inside the 1 px border — because the border color does not
/// change.
#[test]
fn test_button_press_reports_only_the_face_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, button) = push_button_paint_pair(&mut state);

    // First paint (full) so the border is established.
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();

    // Press the button (a click in its client area) and repaint.
    let lparam = u64::from(10_u16) | (u64::from(10_u16) << 16);
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_LBUTTONDOWN,
        0,
        lparam,
    )
    .expect("press ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        button,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint2 ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let frame = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame");

    assert_eq!(
        frame.region,
        Some(crate::gdi32::IRect {
            left: 11,
            top: 11,
            right: 109,
            bottom: 39,
        }),
        "a press repaints only the face inside the 1 px border"
    );
}

/// A STATIC caption change must report ONLY the caption band, not the whole
/// label rect.
#[test]
fn test_static_caption_change_paints_only_the_caption_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (top, label) = push_static_paint_pair(&mut state);

    // First paint (full); capture the frame with the old caption "Ready".
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        label,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let before = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();

    write_guest_ansi(&mut engine, 0x4000, "Scanning drive C");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        label,
        crate::user32::wm::WinMsg::WM_SETTEXT.as_u32(),
        0,
        0x4000,
    )
    .expect("settext ok")
    .expect("some result");
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        label,
        crate::user32::WM_PAINT,
        0,
        0,
    )
    .expect("paint2 ok")
    .expect("some result");
    state.present().drain_pending_publishes();
    let after = state
        .present()
        .published
        .get(&crate::handles::Hwnd::from(top))
        .expect("published frame")
        .clone();

    let region = after.region.expect("a caption change is a partial repaint");
    let control = crate::gdi32::IRect {
        left: 10,
        top: 50,
        right: 150,
        bottom: 70,
    };
    assert!(
        region.left >= control.left
            && region.top >= control.top
            && region.right <= control.right
            && region.bottom <= control.bottom,
        "the region must stay inside the control, got {region:?}"
    );
    assert!(
        region != control,
        "a caption change must not report the full control rect, got {region:?}"
    );
    assert!(
        region.height() < control.height(),
        "the region is a single caption band, not the whole label, got {region:?}"
    );

    let mut diffs = 0_usize;
    for y in 0..after.height {
        for x in 0..after.width {
            let idx = usize::try_from(y)
                .unwrap_or(0)
                .saturating_mul(after.width as usize)
                .saturating_add(usize::try_from(x).unwrap_or(0));
            if before.pixels.get(idx) != after.pixels.get(idx) {
                assert!(
                    i64::from(x) >= i64::from(region.left)
                        && i64::from(x) < i64::from(region.right)
                        && i64::from(y) >= i64::from(region.top)
                        && i64::from(y) < i64::from(region.bottom),
                    "a pixel diff at ({x},{y}) lies outside the reported region"
                );
                diffs = diffs.saturating_add(1);
            }
        }
    }
    assert!(diffs > 0, "the caption change must repaint pixels");
}
