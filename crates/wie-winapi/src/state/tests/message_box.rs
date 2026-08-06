//! Host message-box bridge tests: MessageBoxA/W through the registered bridge, plus the ShellAboutW route through the same two-entry flow.
use super::*;

// --- MessageBox (host bridge) ---

/// Register a test MessageBox bridge that records every `(caption, text,
/// mb_type)` call and answers with the canned Win32 id.
fn register_message_box_bridge(
    state: &mut WinApiState,
    canned: i32,
    captured: Arc<Mutex<Vec<(String, String, u32)>>>,
) {
    state.present().message_box_bridge = Some(Box::new(move |caption, text, mb_type| {
        captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((caption.to_owned(), text.to_owned(), mb_type));
        canned
    }));
}

/// Drive a MessageBox handler with a scripted host alert bridge (the bridge
/// must already be registered).
///
/// The real flow is two entries around the bridge: the handler's first entry
/// records [`PendingNativeMessageBox`] and returns
/// [`WinApiControlSignal::MessageBoxBridgeRequested`]; the runtime runs the
/// bridge WITHOUT the shared lock and records the chosen id; the engine's
/// re-execution of the fake API re-enters the handler, which returns the id
/// to the guest. This helper simulates exactly that (the runtime is not
/// involved in unit tests).
fn dispatch_message_box_with_bridge(
    engine: &mut IcedCpu,
    state: &mut WinApiState,
    api: fn(&mut HandlerContext<'_>) -> anyhow::Result<kernel32::WinApiHandlerResult>,
) -> anyhow::Result<kernel32::WinApiHandlerResult> {
    let first = api(&mut HandlerContext::new(engine, test_environment(), state))
        .expect_err("the first entry parks the guest for the host alert");
    let signal = first
        .downcast_ref::<WinApiControlSignal>()
        .expect("a control signal");
    let WinApiControlSignal::MessageBoxBridgeRequested { request } = signal else {
        panic!("expected a message-box bridge request");
    };
    // What the runtime does between the two entries: take the bridge out,
    // run it (no shared lock), restore it, record the chosen id.
    let bridge = state
        .present()
        .message_box_bridge
        .take()
        .expect("bridge registered");
    let picked = bridge(&request.caption, &request.text, request.message_box_type);
    state.present().message_box_bridge = Some(bridge);
    state
        .window_state()
        .pending_native_message_box
        .as_mut()
        .expect("pending session recorded")
        .pick = Some(picked);
    // Re-entry: the handler returns the chosen id to the guest.
    api(&mut HandlerContext::new(engine, test_environment(), state))
}

/// `MessageBoxW` decodes UTF-16 args, forwards them to the registered bridge
/// verbatim (mb_type included), and returns the bridge's id to the guest —
/// through the two-entry bridge flow (request → runtime runs the bridge →
/// re-entry resolves the pick).
#[test]
fn test_message_box_w_calls_registered_bridge_with_decoded_args() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let captured = Arc::new(Mutex::new(Vec::<(String, String, u32)>::new()));
    register_message_box_bridge(&mut state, 6, Arc::clone(&captured)); // IDYES
    write_guest_utf16(&mut engine, 0x6000, "Save changes?");
    write_guest_utf16(&mut engine, 0x7000, "notepad");
    // MB_YESNO | MB_ICONQUESTION = 0x4 | 0x20.
    write_regs(&mut engine, 0, 0x6000, 0x7000, 0x24, 0);

    let r = dispatch_message_box_with_bridge(&mut engine, &mut state, user32::handle_message_box_w)
        .expect("MessageBoxW must dispatch");

    assert_eq!(r.return_value, 6, "the bridge's id must reach the guest");
    let calls = captured.lock().expect("bridge capture lock");
    assert_eq!(calls.len(), 1, "the bridge must be called exactly once");
    assert_eq!(calls[0].0, "notepad", "caption decoded from UTF-16");
    assert_eq!(calls[0].1, "Save changes?", "text decoded from UTF-16");
    assert_eq!(
        calls[0].2, 0x24,
        "MB_* flag bits must pass through to the bridge verbatim"
    );
}

/// `MessageBoxA` decodes ANSI/UTF-8 args and reaches the bridge the same way
/// the wide variant does.
#[test]
fn test_message_box_a_decodes_ansi_and_calls_bridge() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let captured = Arc::new(Mutex::new(Vec::<(String, String, u32)>::new()));
    register_message_box_bridge(&mut state, 2, Arc::clone(&captured)); // IDCANCEL
    write_guest_ansi(&mut engine, 0x6000, "Unsaved changes");
    write_guest_ansi(&mut engine, 0x7000, "editor");
    write_regs(&mut engine, 0, 0x6000, 0x7000, 0x1, 0); // MB_OKCANCEL

    let r = dispatch_message_box_with_bridge(&mut engine, &mut state, user32::handle_message_box_a)
        .expect("MessageBoxA must dispatch");

    assert_eq!(r.return_value, 2, "the bridge's id must reach the guest");
    let calls = captured.lock().expect("bridge capture lock");
    assert_eq!(calls.len(), 1, "the bridge must be called exactly once");
    assert_eq!(calls[0].0, "editor", "caption decoded from ANSI");
    assert_eq!(calls[0].1, "Unsaved changes", "text decoded from ANSI");
    assert_eq!(calls[0].2, 0x1, "MB_OKCANCEL must pass through");
}

/// Every MB_* button/icon set reaches the bridge unchanged and the bridge's
/// canned result (Ok/Cancel/Yes/No → the Win32 id) is what the guest sees.
#[test]
fn test_message_box_flag_sets_pass_through_and_results_map_to_ids() {
    // (mb_type, canned bridge answer, expected guest return value).
    let cases: &[(u32, i32, u64)] = &[
        (0x00, 1, user32::IDOK),     // MB_OK → bridge answers Ok → IDOK
        (0x01, 2, user32::IDCANCEL), // MB_OKCANCEL → Cancel → IDCANCEL
        (0x04, 6, user32::IDYES),    // MB_YESNO → Yes → IDYES
        (0x04, 7, user32::IDNO),     // MB_YESNO → No → IDNO
        (0x03, 6, user32::IDYES),    // MB_YESNOCANCEL → Yes → IDYES
        (0x03, 2, user32::IDCANCEL), // MB_YESNOCANCEL → Cancel → IDCANCEL
        (0x10, 1, user32::IDOK),     // MB_ICONERROR
        (0x20, 1, user32::IDOK),     // MB_ICONQUESTION
        (0x30, 1, user32::IDOK),     // MB_ICONWARNING
        (0x40, 1, user32::IDOK),     // MB_ICONINFORMATION
    ];
    for &(mb_type, canned, expected) in cases {
        let mut engine = test_engine();
        let mut state = default_winapi_state();
        let captured = Arc::new(Mutex::new(Vec::<(String, String, u32)>::new()));
        register_message_box_bridge(&mut state, canned, Arc::clone(&captured));
        write_guest_utf16(&mut engine, 0x6000, "text");
        write_guest_utf16(&mut engine, 0x7000, "caption");
        write_regs(&mut engine, 0, 0x6000, 0x7000, u64::from(mb_type), 0);

        let r =
            dispatch_message_box_with_bridge(&mut engine, &mut state, user32::handle_message_box_w)
                .expect("MessageBoxW must dispatch");

        assert_eq!(
            r.return_value, expected,
            "mb_type {mb_type:#06x} must surface the bridge's id"
        );
        let calls = captured.lock().expect("bridge capture lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].2, mb_type,
            "mb_type {mb_type:#06x} must reach the bridge verbatim"
        );
    }
}

/// No bridge registered (headless runs, `trace`): the handler echoes to the
/// host console and auto-returns IDOK so the guest never hangs.
#[test]
fn test_message_box_without_bridge_falls_back_to_idok() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_guest_utf16(&mut engine, 0x6000, "text");
    write_guest_utf16(&mut engine, 0x7000, "caption");
    write_regs(&mut engine, 0, 0x6000, 0x7000, 0x4, 0);

    let r = user32::handle_message_box_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("MessageBoxW must dispatch without a bridge");

    assert_eq!(
        r.return_value,
        user32::IDOK,
        "headless fallback returns IDOK"
    );
}

// --- ShellAboutW (the same host message-box bridge) ---

/// `ShellAboutW` (shell32) routes through the SAME two-entry message-box
/// bridge flow as `MessageBoxA/W`: the first entry records the pending box
/// and returns [`WinApiControlSignal::MessageBoxBridgeRequested`]; the
/// runtime runs the bridge WITHOUT the shared lock; the re-entry resolves the
/// pick (IDOK for the OK-style About box) and returns TRUE.
#[test]
fn test_shell_about_w_routes_through_message_box_bridge() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    let captured = Arc::new(Mutex::new(Vec::<(String, String, u32)>::new()));
    register_message_box_bridge(&mut state, 1, Arc::clone(&captured)); // IDOK
    write_guest_utf16(&mut engine, 0x6000, "Notepad Authors");
    write_guest_utf16(&mut engine, 0x7000, "Notepad");
    // ShellAboutW(hwnd, szAppName, szOtherStuff, hIcon).
    write_regs(&mut engine, 0, 0x7000, 0x6000, 0, 0);

    let r =
        dispatch_message_box_with_bridge(&mut engine, &mut state, shell32::handle_shell_about_w)
            .expect("ShellAboutW must dispatch");

    assert_eq!(r.return_value, 1, "ShellAboutW returns TRUE (IDOK)");
    let calls = captured.lock().expect("bridge capture lock");
    assert_eq!(calls.len(), 1, "the bridge must be called exactly once");
    assert_eq!(calls[0].0, "Notepad", "szAppName is the caption");
    assert_eq!(calls[0].1, "Notepad Authors", "szOtherStuff is the text");
    assert_eq!(calls[0].2, 0, "ShellAboutW is an MB_OK alert");
}

/// No bridge registered (headless runs, `trace`): ShellAboutW echoes to the
/// host console and auto-returns TRUE so the guest never hangs.
#[test]
fn test_shell_about_w_without_bridge_falls_back_to_true() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_guest_utf16(&mut engine, 0x6000, "Notepad Authors");
    write_guest_utf16(&mut engine, 0x7000, "Notepad");
    write_regs(&mut engine, 0, 0x7000, 0x6000, 0, 0);

    let r = shell32::handle_shell_about_w(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("ShellAboutW must dispatch without a bridge");

    assert_eq!(r.return_value, 1, "headless fallback returns TRUE");
    assert!(
        state.window_state().pending_native_message_box.is_none(),
        "no pending box was recorded without a bridge"
    );
}
