//! EDIT subclass bridge tests: SetWindowLongPtr / GWLP_WNDPROC, CallWindowProc default dispatch, and guest-first bridging of WM_CHAR / WM_KEYDOWN.
use super::*;

// ─── EDIT subclass bridge (L1: CallWindowProc + GWLP_WNDPROC) ────────────
//
// notepad subclasses its EDIT: SetWindowLongPtrW(hEdit, GWLP_WNDPROC,
// EDIT_WndProc) replaces the control's proc with a guest callback that must
// see every message FIRST (it updates the title star and the status-bar
// Ln/Col), forwarding the rest through CallWindowProcW(hEdit, <original>,
// ...). The original value WIE hands back for a fresh control is 0 — the
// host's default-control-proc marker — and CallWindowProcW with that value
// must run the normal host control dispatch.

/// `GWLP_WNDPROC` (-4) as the zero-extended `u64` a Win64 `SetWindowLongPtrW`
/// index register carries it (MSVC emits `mov edx, -4`).
const GWLP_WNDPROC_RAW: u64 = 0xFFFF_FFFC;

/// A plausible guest code address for the subclass proc.
const GUEST_SUBCLASS: u64 = 0x0000_0000_1400_1000;

/// Install a guest subclass on `hwnd` through the real SetWindowLongPtrW
/// handler; returns the previous `GWLP_WNDPROC` value the handler reported.
fn subclass_edit(engine: &mut IcedCpu, state: &mut WinApiState, hwnd: u64) -> u64 {
    write_regs(engine, hwnd, GWLP_WNDPROC_RAW, GUEST_SUBCLASS, 0, 0);
    user32::handle_set_window_long_ptr_w(&mut HandlerContext::new(engine, default_env(), state))
        .expect("set subclass")
        .return_value
}

/// Call `handle_call_window_proc_w` with `prev_wndproc` / `hwnd` / `message`
/// and a zero `lParam`; returns the handler's reported return value.
fn call_window_proc_default(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    prev_wndproc: u64,
    hwnd: u64,
    message: u32,
) -> u64 {
    write_regs(engine, prev_wndproc, hwnd, u64::from(message), 0, 0);
    engine
        .mem_write(STACK_TOP + 0x28, &0_u64.to_le_bytes())
        .expect("write CallWindowProc lParam");
    user32::handle_call_window_proc_w(&mut HandlerContext::new(engine, default_env(), state))
        .expect("call window proc")
        .return_value
}

#[test]
fn test_edit_subclass_setwindowlongptr_returns_original_and_stores() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let edit = push_multiline_edit_real(&mut state);

    // First subclass: the previous GWLP_WNDPROC of a fresh control is WIE's
    // host-default marker (0) — that is what notepad's EDIT_WndProc stores
    // and passes back to CallWindowProcW.
    assert_eq!(
        subclass_edit(&mut engine, &mut state, edit),
        0,
        "the original host-default marker is returned"
    );

    // GetWindowLongPtrW now reports the subclass.
    write_regs(&mut engine, edit, GWLP_WNDPROC_RAW, 0, 0, 0);
    let read_back = user32::handle_get_window_long_ptr_w(&mut HandlerContext::new(
        &mut engine,
        default_env(),
        &mut state,
    ))
    .expect("get subclass")
    .return_value;
    assert_eq!(read_back, GUEST_SUBCLASS);

    // The record remembers the original (the marker CallWindowProcW treats
    // as "run the host default").
    let window = state
        .window_state()
        .windows
        .iter()
        .find(|w| w.handle == crate::handles::Hwnd::from(edit))
        .expect("edit record");
    assert_eq!(window.subclass_original_wndproc, 0);
}

#[test]
fn test_edit_subclass_wm_char_bridges_to_guest_subclass_first() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "hello");
    subclass_edit(&mut engine, &mut state, edit);

    // A WM_CHAR to the subclassed EDIT must bridge to the guest subclass
    // FIRST (the notepad star path) — the host default must not run, so the
    // text stays untouched.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    let error = result.expect_err("WM_CHAR must bridge to the guest subclass");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == GUEST_SUBCLASS
                    && request.window_handle == edit
                    && request.message == crate::user32::WM_CHAR
                    && request.word_parameter == u64::from(b'x')
                    && request.outer_return == crate::OuterReturn::Passthrough
        ),
        "WM_CHAR must bridge to the guest subclass, got {signal:?}"
    );
    assert_eq!(
        control_text(&state, edit),
        "hello",
        "the host default dispatch must not run while subclassed"
    );
}

#[test]
fn test_edit_subclass_wm_keydown_bridges_to_guest_subclass_first() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "hello");
    subclass_edit(&mut engine, &mut state, edit);

    // Arrow-key navigation (the status-bar Ln/Col path) also reaches the
    // subclass before the host sees it.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_KEYDOWN,
        crate::user32::VK_RIGHT,
        0,
    );
    let error = result.expect_err("WM_KEYDOWN must bridge to the guest subclass");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == GUEST_SUBCLASS
                    && request.message == crate::user32::WM_KEYDOWN
                    && request.word_parameter == crate::user32::VK_RIGHT
        ),
        "WM_KEYDOWN must bridge to the guest subclass, got {signal:?}"
    );
    let caret = control_ui(&state, edit);
    assert_eq!(caret.caret, 0, "the host caret move must not run");
}

#[test]
fn test_edit_subclass_callwindowproc_runs_host_default_dispatch() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "ab\ncd");
    let font = state
        .gdi_state()
        .alloc_font("Segoe UI".to_owned(), -16, 400, false, 0);

    // Store a font through the real control dispatch (pre-subclass).
    crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_SETFONT,
        font.as_u64(),
        0,
    )
    .expect("setfont handled")
    .expect("some result");

    subclass_edit(&mut engine, &mut state, edit);

    // The subclass's CallWindowProcW(hEdit, <original marker>, WM_GETFONT)
    // runs the HOST default control dispatch and returns its LRESULT.
    assert_eq!(
        call_window_proc_default(&mut engine, &mut state, 0, edit, crate::user32::WM_GETFONT),
        font.as_u64(),
        "CallWindowProc with the original marker must run the host default"
    );

    // EM_GETLINECOUNT through the same bridge: "ab\ncd" is 2 lines.
    assert_eq!(
        call_window_proc_default(
            &mut engine,
            &mut state,
            0,
            edit,
            crate::user32::EM_GETLINECOUNT,
        ),
        2,
        "CallWindowProc must surface the host default LRESULT"
    );

    // A foreign (non-marker, non-zero) proc is bridged to the guest — what
    // real Windows does (call that proc) — not run as the default.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    let error = result.expect_err("subclassed control still bridges");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == GUEST_SUBCLASS
        ),
        "the stored subclass is the bridge target, got {signal:?}"
    );
}

#[test]
fn test_edit_unsubclass_restores_host_dispatch() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "hello");

    assert_eq!(subclass_edit(&mut engine, &mut state, edit), 0);
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    result
        .expect_err("subclassed WM_CHAR must bridge")
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");

    // notepad's WM_DESTROY / DoCreateEditWindow "restore" passes the saved
    // original back: SetWindowLongPtrW(hEdit, GWLP_WNDPROC, 0).
    write_regs(&mut engine, edit, GWLP_WNDPROC_RAW, 0, 0, 0);
    let previous = user32::handle_set_window_long_ptr_w(&mut HandlerContext::new(
        &mut engine,
        default_env(),
        &mut state,
    ))
    .expect("restore subclass")
    .return_value;
    assert_eq!(previous, GUEST_SUBCLASS, "restoring returns the subclass");

    // With GWLP_WNDPROC back to null the control dispatch runs host-side
    // again: typing mutates the text and the change is delivered to the
    // guest-WndProc parent as EN_CHANGE — NO subclass bridge.
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    let error = result.expect_err("WM_CHAR must reach the host dispatch");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == 0x7000_0000
                    && request.message == crate::user32::WM_COMMAND
        ),
        "the restored edit delivers EN_CHANGE to its parent, got {signal:?}"
    );
    assert_eq!(control_text(&state, edit), "xhello");

    write_regs(&mut engine, edit, GWLP_WNDPROC_RAW, 0, 0, 0);
    let read_back = user32::handle_get_window_long_ptr_w(&mut HandlerContext::new(
        &mut engine,
        default_env(),
        &mut state,
    ))
    .expect("get after restore")
    .return_value;
    assert_eq!(read_back, 0, "GWLP_WNDPROC reads 0 after un-subclassing");
}

#[test]
fn test_edit_without_subclass_behaves_exactly_as_before() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let (_, edit) = push_multiline_edit(&mut state, "hello");

    // No subclass bridge: WM_CHAR mutates the text host-side and the change
    // reaches the guest-WndProc parent as EN_CHANGE (the long-standing
    // behavior — the signal targets the parent, never the control itself).
    let result = crate::user32::controls::dispatch_control_proc(
        &mut engine,
        &mut state,
        edit,
        crate::user32::WM_CHAR,
        u64::from(b'x'),
        0,
    );
    let error = result.expect_err("WM_CHAR must reach the host dispatch");
    let signal = error
        .downcast_ref::<WinApiControlSignal>()
        .expect("control signal");
    assert!(
        matches!(
            signal,
            WinApiControlSignal::GuestCallbackRequested { request }
                if request.callback_address == 0x7000_0000
                    && request.message == crate::user32::WM_COMMAND
        ),
        "an un-subclassed edit delivers EN_CHANGE to its parent, got {signal:?}"
    );
    assert_eq!(control_text(&state, edit), "xhello");

    // GWLP_WNDPROC reads 0 (nothing stored).
    write_regs(&mut engine, edit, GWLP_WNDPROC_RAW, 0, 0, 0);
    let read_back = user32::handle_get_window_long_ptr_w(&mut HandlerContext::new(
        &mut engine,
        default_env(),
        &mut state,
    ))
    .expect("get wndproc")
    .return_value;
    assert_eq!(read_back, 0);

    // CallWindowProcW(hEdit, 0, WM_GETFONT) on an un-subclassed control is
    // the conservative default path: it runs the host dispatch (0 font, no
    // crash) rather than invoking anything.
    assert_eq!(
        call_window_proc_default(&mut engine, &mut state, 0, edit, crate::user32::WM_GETFONT),
        0,
        "marker CallWindowProc on an un-subclassed control stays host-side"
    );
}
