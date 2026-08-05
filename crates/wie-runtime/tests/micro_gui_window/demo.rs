//! The gui_*.exe micro-demo tests: end-to-end rendering (D3D9, DIB blit +
//! text), menu/timer/paint synthesis, the gui_dialog focus chain, and the
//! gui_demo click-driven dialog paths.

use crate::helpers::{
    BTNFACE_0RGB, D3D9_ALPHA_PASS_0RGB, D3D9_BLEND_0RGB, D3D9_CLEAR_RED_0RGB, D3D9_CYAN_0RGB,
    D3D9_FAR_DEPTH_0RGB, D3D9_FOGGED_0RGB, D3D9_MIP_MAGENTA_0RGB, D3D9_MIP_YELLOW_0RGB,
    D3D9_NEAR_DEPTH_0RGB, D3D9_QUAD_BLUE_0RGB, D3D9_QUAD_GREEN_0RGB, D3D9_QUAD_RED_0RGB,
    D3D9_QUAD_WHITE_0RGB, D3D9_RED_QUAD_0RGB, D3D9_RESTING_FRAME_HASH, D3D9_SCISSOR_INSIDE_0RGB,
    DIALOG_OK_BUTTON_SAMPLE, GUI_BLIT_RESTING_FRAME_HASH,
    assert_frame_renders_gradient_text_and_controls, drive_gui_session, frame_hash,
    gui_suite_serialize, micro_exe,
};

/// Run gui_d3d9 end-to-end and prove the P3 D3D9 software-render slice:
/// CreateDevice → Clear(red) → BeginScene → DrawPrimitiveUP (gradient
/// triangle) → DrawIndexedPrimitiveUP (solid cyan triangle) → the L1 vs_2_0 +
/// w-skewed quads → the L3 alpha-test/fog/scissor strip → the L4
/// renderer-completeness strip (mip-select quad, point/line primitives,
/// MinZ/MaxZ viewport-mapped depth, near-plane-clipped quad) → EndScene →
/// Present publishes a SurfaceFrame through the GDI-shared pipeline.
///
/// The exe exits 0 only if every D3D9 call's HRESULT succeeded AND
/// GetDeviceCaps honestly reported the P5a caps (ps_2_0, vs stage still 0)
/// AND the SetViewport/GetViewport round-trip matched AND the L3 state
/// surface held (D3DERR_INVALIDCALL on an out-of-range value, the raw-value
/// GetRenderState round-trip, the fog/alpha/scissor state round-trips, and
/// the GetTransform / MultiplyTransform / D3DTS_TEXTURE0 round-trips). Pixel
/// checks pin the rendered frame: clear red outside the triangles, the
/// blended gradient triangle, the solid cyan indexed triangle, and the L3
/// strip's alpha-tested / fogged / scissor-clipped colors.
#[test]
fn gui_d3d9_renders_clear_and_triangle() {
    let Some(path) = micro_exe("gui_d3d9.exe") else {
        eprintln!("skip: micro-exes/out/gui_d3d9.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    use wie_runtime::EntryTraceTermination;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("GUI session starts");
    session
        .set_guest_env("WIE_SELFTEST", "1")
        .expect("inject WIE_SELFTEST");

    let mut iterations = 0;
    let mut saw_d3d9_frame = false;
    let exit_code = loop {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("GUI session run_until_stop");
        iterations += 1;
        assert!(
            iterations < 300,
            "gui_d3d9.exe did not exit within 300 iterations"
        );

        // Presentation-model check: Present must publish the backbuffer as a
        // SurfaceFrame on the device window (320x240). The first frame is
        // deterministic: red clear + gradient triangle + cyan indexed
        // triangle + a 2x2 textured quad (red/green/blue/white checkerboard).
        if let Some(owner) = session.first_guest_window_handle()
            && let Some(frame) = session.take_frame(owner)
        {
            assert!(
                frame.width >= 320 && frame.height >= 240,
                "gui_d3d9 frame must be at least 320x240, got {}x{}",
                frame.width,
                frame.height
            );
            let idx = |x: u32, y: u32| {
                usize::try_from(y).unwrap_or(0) * frame.width as usize
                    + usize::try_from(x).unwrap_or(0)
            };
            // Clear red outside the triangles (top-left corner region).
            if frame.pixels.get(idx(5, 5)).copied() == Some(D3D9_CLEAR_RED_0RGB)
                // Blended gradient triangle interior (near the red vertex).
                && ((frame.pixels.get(idx(50, 190)).copied().unwrap_or(0) >> 16) & 0xFF) > 0xD0
                // Solid cyan indexed triangle interior.
                && frame.pixels.get(idx(160, 45)).copied() == Some(D3D9_CYAN_0RGB)
                // Textured quad: the 2x2 checkerboard fills x∈[240,310],
                // y∈[10,110]; sample each quadrant's center.
                && frame.pixels.get(idx(250, 25)).copied() == Some(D3D9_QUAD_RED_0RGB)
                && frame.pixels.get(idx(295, 25)).copied() == Some(D3D9_QUAD_GREEN_0RGB)
                && frame.pixels.get(idx(250, 85)).copied() == Some(D3D9_QUAD_BLUE_0RGB)
                && frame.pixels.get(idx(295, 85)).copied() == Some(D3D9_QUAD_WHITE_0RGB)
                // P4c alpha blend: half-alpha blue over red (x∈[10,50] region).
                && frame.pixels.get(idx(30, 35)).copied() == Some(D3D9_BLEND_0RGB)
                // P4c opaque red quad (outside the blue half).
                && frame.pixels.get(idx(80, 35)).copied() == Some(D3D9_RED_QUAD_0RGB)
                // P4c depth: far (z=0.9) magenta shows where the near quad
                // does not cover; the near (z=0.1) white wins the overlap.
                && frame.pixels.get(idx(30, 120)).copied() == Some(D3D9_FAR_DEPTH_0RGB)
                && frame.pixels.get(idx(80, 120)).copied() == Some(D3D9_NEAR_DEPTH_0RGB)
                // L1 vs_2_0 quad: the VS transform + oT0/oD0 passthrough
                // render the 2x2 checkerboard at x∈[220,290], y∈[150,220].
                && frame.pixels.get(idx(237, 202)).copied() == Some(D3D9_QUAD_RED_0RGB)
                && frame.pixels.get(idx(272, 202)).copied() == Some(D3D9_QUAD_GREEN_0RGB)
                && frame.pixels.get(idx(237, 167)).copied() == Some(D3D9_QUAD_BLUE_0RGB)
                && frame.pixels.get(idx(272, 167)).copied() == Some(D3D9_QUAD_WHITE_0RGB)
                // L1 w-skewed quad: the center pixel (165,175) samples the
                // RED texel — the perspective-correct uv (≈(0.499,0.499))
                // after the w-divide, where the affine interpolant
                // (≈(0.532,0.466)) would land on the GREEN texel.
                && frame.pixels.get(idx(165, 175)).copied() == Some(D3D9_QUAD_RED_0RGB)
                // L3 fragment stages (the y∈[220,235] strip): the alpha-test
                // quad's failing left half shows the clear red, the passing
                // right half draws white; the fog quad is red under blue
                // LINEAR fog at f=0.5 → (128,0,128); the scissor quad draws
                // green inside the rect and clear red outside it.
                && frame.pixels.get(idx(110, 227)).copied() == Some(D3D9_CLEAR_RED_0RGB)
                && frame.pixels.get(idx(140, 227)).copied() == Some(D3D9_ALPHA_PASS_0RGB)
                && frame.pixels.get(idx(170, 227)).copied() == Some(D3D9_FOGGED_0RGB)
                && frame.pixels.get(idx(197, 227)).copied() == Some(D3D9_SCISSOR_INSIDE_0RGB)
                && frame.pixels.get(idx(212, 227)).copied() == Some(D3D9_CLEAR_RED_0RGB)
                // L4 renderer-completeness strip: the mip-select quad's
                // level-1 texels (yellow at an even column, magenta at an odd
                // one — level 0's red/white texels would prove the mip chain
                // was NOT selected); the point list + line + big point (all
                // white); the MinZ/MaxZ occlusion (magenta quad A where B does
                // not cover, white quad B winning the overlap — proving the
                // viewport-mapped RHW z is what the depth test sees); and the
                // near-plane-clipped quad's magenta trapezoid (clear red above
                // it where the w≤0 half was clipped away).
                && frame.pixels.get(idx(13, 160)).copied() == Some(D3D9_MIP_MAGENTA_0RGB)
                && frame.pixels.get(idx(14, 160)).copied() == Some(D3D9_MIP_YELLOW_0RGB)
                && frame.pixels.get(idx(50, 156)).copied() == Some(0x00FF_FFFF)
                && frame.pixels.get(idx(66, 156)).copied() == Some(0x00FF_FFFF)
                && frame.pixels.get(idx(58, 166)).copied() == Some(0x00FF_FFFF)
                && frame.pixels.get(idx(74, 158)).copied() == Some(0x00FF_FFFF)
                && frame.pixels.get(idx(245, 125)).copied() == Some(D3D9_FAR_DEPTH_0RGB)
                && frame.pixels.get(idx(300, 125)).copied() == Some(0x00FF_FFFF)
                && frame.pixels.get(idx(258, 233)).copied() == Some(D3D9_FAR_DEPTH_0RGB)
                && frame.pixels.get(idx(240, 223)).copied() == Some(D3D9_CLEAR_RED_0RGB)
                // The whole 320x240 frame is deterministic CPU output — gate it.
                && frame_hash(&frame, 0, frame.height) == D3D9_RESTING_FRAME_HASH
            {
                saw_d3d9_frame = true;
            }
        }

        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => break Some(code),
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => {
                panic!("GUI session stopped unexpectedly: {other:?}");
            }
        }
    };

    assert_eq!(
        exit_code,
        Some(0),
        "gui_d3d9.exe must exit 0 (proves CreateDevice → Clear → BeginScene → \
         DrawPrimitiveUP → DrawIndexedPrimitiveUP → CreateTexture → LockRect → \
         UnlockRect → SetTexture → textured DrawPrimitiveUP → CreateVertexShader \
         (vs_2_0 gate) → SetVertexShader → vs_2_0 DrawPrimitiveUP → the \
         w-skewed-quad SetTransform → EndScene → Present all succeeded and \
         caps honesty held); got {exit_code:?}"
    );
    assert!(
        saw_d3d9_frame,
        "the D3D9 frame (clear red + gradient triangle + cyan indexed \
         triangle + textured quad + vs_2_0 quad + w-skewed quad) was never \
         observed in the device window's published surface"
    );
}

/// Run gui_blit end-to-end and prove every capability tier.
///
/// gui_blit now exercises text (TextOutA/DrawTextA into a DIB), timers,
/// menus, child controls, and a modal dialog; its WM_DESTROY only posts exit
/// code 0 when ALL of those stages ran (>= 5 timer ticks, WM_COMMAND from the
/// self-sent button click, the About command opened the dialog, the dialog
/// returned 1, and the Quit command routed). Exit 0 therefore proves the full
/// chain. The CI-gated frame hash additionally pins the deterministic
/// rendered frame (gradient + text + button/static) against rendering
/// regressions.
#[test]
fn gui_blit_comprehensive_regression() {
    let Some(path) = micro_exe("gui_blit.exe") else {
        eprintln!("skip: micro-exes/out/gui_blit.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    use wie_runtime::EntryTraceTermination;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("GUI session starts");
    // CI mode: run the scripted self-test (see drive_gui_session).
    session
        .set_guest_env("WIE_SELFTEST", "1")
        .expect("inject WIE_SELFTEST");
    // B9: turn on frame timing so `present_publish_ns_last` is measured for
    // the frame-time budget gate below (no `WIE_RUNTIME_PROFILE` env needed).
    session.enable_frame_timing();

    let mut iterations = 0;
    let mut saw_resting_frame = false;
    let exit_code = loop {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("GUI session run_until_stop");
        iterations += 1;
        assert!(
            iterations < 300,
            "gui_blit.exe did not exit within 300 iterations"
        );

        // Frame check: the deterministic resting frame (gradient + text +
        // children, no dialog, no counter text) must appear at some point.
        // The dialog / transient frames hash differently and are ignored.
        // `present_us` times the host take_frame (the B1 publish→present
        // hand-off); the FNV hash runs AFTER the timed window so the budget
        // measures the frame pipeline, not test-side verification.
        if let Some(owner) = session.first_guest_window_handle() {
            let present_t0 = std::time::Instant::now();
            let frame = session.take_frame(owner);
            let present_us = present_t0.elapsed().as_micros();
            if let Some(frame) = frame
                // Hash only the text-free gradient rows (200..800): text
                // pixels are font-dependent, the gradient is not.
                && frame_hash(&frame, 200, 800) == GUI_BLIT_RESTING_FRAME_HASH
            {
                assert_frame_renders_gradient_text_and_controls(&frame);
                // B9 regression net: publish+present must stay cheap. The
                // 10 ms ceiling is deliberately generous — it catches
                // pathological regressions (e.g. a full 4 MB clone or another
                // full copy reintroduced into publish or take_frame), not
                // tight tuning. Measured in debug builds at 1280×800.
                let publish_us = session.present_publish_ns_last() / 1_000;
                assert!(
                    publish_us + present_us < 10_000,
                    "frame publish+present budget exceeded: publish={publish_us}us \
                     present={present_us}us (10 ms ceiling)"
                );
                saw_resting_frame = true;
            }
        }

        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => break Some(code),
            EntryTraceTermination::WaitingForMessage => {
                // Poll every 10 ms so the resting frame is captured well
                // inside the pre-dialog window: the guest opens its modal
                // dialog on timer tick 4 (~200 ms), and that dialog's frame
                // composites over the owner, replacing the gradient frame
                // the hash gate needs. 50 ms polls leave only ~4 chances;
                // 10 ms gives ~20 within the same window.
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => {
                panic!("GUI session stopped unexpectedly: {other:?}");
            }
        }
    };

    assert_eq!(
        exit_code,
        Some(0),
        "gui_blit.exe must exit 0 (proves timer + menu + control + dialog \
         stages all ran); got {exit_code:?}"
    );
    assert!(
        saw_resting_frame,
        "the deterministic resting frame hash 0x{GUI_BLIT_RESTING_FRAME_HASH:016X} \
         was never observed in the owner's published surface"
    );
}

#[test]
fn gui_menu_timer_commands_and_paint() {
    let Some(path) = micro_exe("gui_menu.exe") else {
        eprintln!("skip: micro-exes/out/gui_menu.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    // The exe only calls PostQuitMessage after TIMER_TICKS WM_TIMER messages
    // (each tick also invalidates, exercising WM_PAINT synthesis), so exit
    // code 0 proves the timer synthesis path fired end-to-end.
    let exit_code = drive_gui_session(&path, 200);

    assert_eq!(
        exit_code,
        Some(0),
        "gui_menu.exe must exit 0 (proves WM_TIMER fired); got {exit_code:?}"
    );
}

#[test]
fn gui_text_renders_and_self_checks() {
    let Some(path) = micro_exe("gui_text.exe") else {
        eprintln!("skip: micro-exes/out/gui_text.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    // The exe draws text into a DIB (TextOutA/W, DrawTextA, ExtTextOut-free)
    // and self-checks the results before entering the timer loop; any failed
    // check calls ExitProcess with a distinct non-zero code. Exit 0 therefore
    // proves the text core (font scale, extents, DrawText measure) works.
    let exit_code = drive_gui_session(&path, 200);

    assert_eq!(
        exit_code,
        Some(0),
        "gui_text.exe must exit 0 (proves text rendering self-checks passed); got {exit_code:?}"
    );
}

#[test]
fn gui_dialog_modal_loop_and_end_dialog() {
    let Some(path) = micro_exe("gui_dialog.exe") else {
        eprintln!(
            "skip: micro-exes/out/gui_dialog.exe not built (run make -C micro-exes gui_exes)"
        );
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    use wie_runtime::EntryTraceTermination;

    // Drive like the persistent GUI loop (YieldOnIdle + host-clock sleeps).
    // The exe opens a modal dialog from its timer; the in-guest DialogBoxParam
    // stub runs the modal loop, the timer-driven VK_RETURN exercises
    // IsDialogMessage (Enter → WM_COMMAND(IDOK) → EndDialog(1)), and the exe
    // exits 0 only if DialogBoxParam returned 1 AND the WM_INITDIALOG sentinel
    // ran (SetDlgItemText + GetDlgItemText round-trip inside the dlgProc).
    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("GUI session starts");
    // CI mode: run the scripted self-test (see drive_gui_session).
    session
        .set_guest_env("WIE_SELFTEST", "1")
        .expect("inject WIE_SELFTEST");

    let mut iterations = 0;
    let mut saw_dialog_face = false;
    let exit_code = loop {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("GUI session run_until_stop");
        iterations += 1;
        assert!(
            iterations < 300,
            "gui_dialog.exe did not exit within 300 iterations"
        );

        // Presentation-model check: while the dialog is open it composites
        // into the owner's published surface — the OK button face (BTNFACE)
        // must appear at its centered position at some point during the run.
        if let Some(owner) = session.first_guest_window_handle()
            && let Some(frame) = session.take_frame(owner)
        {
            let (x, y) = DIALOG_OK_BUTTON_SAMPLE;
            if x < frame.width && y < frame.height {
                let idx = usize::try_from(y).unwrap_or(0) * frame.width as usize
                    + usize::try_from(x).unwrap_or(0);
                if frame.pixels.get(idx).copied() == Some(BTNFACE_0RGB) {
                    saw_dialog_face = true;
                }
            }
        }

        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => break Some(code),
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => {
                panic!("GUI session stopped unexpectedly: {other:?}");
            }
        }
    };

    assert_eq!(
        exit_code,
        Some(0),
        "gui_dialog.exe must exit 0 (proves DialogBoxParam → WM_INITDIALOG → \
         IsDialogMessage Enter → EndDialog(1) flow); got {exit_code:?}"
    );
    assert!(
        saw_dialog_face,
        "the dialog's OK-button face (BTNFACE 0xF0F0F0) was never observed in \
         the owner's published surface"
    );
}

/// Drive gui_dialog with host-posted Tab keys and assert the focus chain:
/// the dialog gives its first WS_TABSTOP child initial focus (audit item 5),
/// Shift+Tab moves focus backward (fed through the keyboard-state seam —
/// audit item 6), and plain Tab wraps it back to the first tab stop.
#[test]
fn gui_dialog_shift_tab_moves_focus() {
    let Some(path) = micro_exe("gui_dialog.exe") else {
        eprintln!(
            "skip: micro-exes/out/gui_dialog.exe not built (run make -C micro-exes gui_exes)"
        );
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    use wie_runtime::EntryTraceTermination;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("GUI session starts");
    session
        .set_guest_env("WIE_SELFTEST", "1")
        .expect("inject WIE_SELFTEST");

    // Win32 constants for the host-posted input (gui_dialog has two tab stop
    // buttons: OK then Cancel).
    const VK_SHIFT: u16 = 0x10;
    const VK_TAB: u16 = 0x09;
    const WM_KEYDOWN: u32 = 0x0100;

    let handle = session.guest_handle();
    let mut iterations = 0;
    // Stage 0: capture the dialog's initial focus, post Shift+Tab.
    // Stage 1: focus moved backward, post plain Tab.
    // Stage 2: focus wrapped back to the first tab stop.
    let mut stage = 0_u8;
    let mut first_focus = 0_u64;
    let mut reverse_focus = 0_u64;
    let exit_code = loop {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("GUI session run_until_stop");
        iterations += 1;
        assert!(
            iterations < 300,
            "gui_dialog.exe shift-tab session did not exit within 300 iterations"
        );
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => break Some(code),
            EntryTraceTermination::WaitingForMessage => {
                match stage {
                    0 => {
                        if let Some(focus) = handle.focus_window() {
                            first_focus = focus;
                            // Shift+Tab: hold Shift in the guest key state.
                            handle.set_key_state(VK_SHIFT, true);
                            let owner = session.first_guest_window_handle().unwrap_or(0);
                            handle.post_message(owner, WM_KEYDOWN, u64::from(VK_TAB), 0);
                            stage = 1;
                        }
                    }
                    1 => {
                        if let Some(focus) = handle.focus_window()
                            && focus != 0
                            && focus != first_focus
                        {
                            reverse_focus = focus;
                            handle.set_key_state(VK_SHIFT, false);
                            let owner = session.first_guest_window_handle().unwrap_or(0);
                            handle.post_message(owner, WM_KEYDOWN, u64::from(VK_TAB), 0);
                            stage = 2;
                        }
                    }
                    2 if handle.focus_window() == Some(first_focus) => {
                        stage = 3;
                    }
                    _ => {}
                }
                // Poll fast (2 ms, not 50 ms): the guest auto-closes the
                // dialog on its 5th WM_TIMER (~250 ms after open), and the
                // test must drive three focus transitions inside that
                // window. A 50 ms quantum leaves only ~2 iterations of
                // margin; 2 ms turns each tick into ~25 polls, so even a
                // test thread delayed by a couple of ticks still finishes
                // before the dialog's timer-driven VK_RETURN closes it.
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            other => {
                panic!("GUI session stopped unexpectedly: {other:?}");
            }
        }
    };

    assert_eq!(
        exit_code,
        Some(0),
        "gui_dialog.exe must still exit 0 with host-driven Tab keys; got {exit_code:?}"
    );
    assert_ne!(
        first_focus, 0,
        "the dialog must give a control initial keyboard focus (item 5)"
    );
    assert_ne!(
        reverse_focus, 0,
        "Shift+Tab must move focus backward (item 6)"
    );
    assert_eq!(
        stage, 3,
        "plain Tab must wrap focus back to the first tab stop"
    );
}

/// gui_demo's selftest opens its modal dialog through the REAL click path —
/// `BM_CLICK` on the Dialog button → host control WndProc → bridged
/// `WM_COMMAND` into the guest WndProc → `on_dialog()` → `DialogBoxParam`.
///
/// The modal loop therefore runs INSIDE a guest callback. Regression
/// (publish-model rework): the dialog's frame must be published at the
/// first message quiescence even with a guest callback in flight — pre-fix
/// the quiescent drain was skipped (`pending_callbacks` non-empty) and the
/// dialog never appeared until the callback popped (the reported "click
/// Dialog… → nothing; click Exit → dialog suddenly appears" behavior).
#[test]
fn gui_demo_dialog_opens_on_click() {
    let Some(path) = micro_exe("gui_demo.exe") else {
        eprintln!("skip: micro-exes/out/gui_demo.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    use wie_runtime::EntryTraceTermination;

    // Dialog template 100 ("100 DIALOG 10,20,180,70", DLU→px ×2) is
    // 360×140 px centered in the 640×420 owner → (140,140)-(500,280).
    // (490,150) is dialog face (DIALOG_BG = BTNFACE 0xF0F0F0), clear of
    // every dialog control; the owner behind it is plain COLOR_WINDOW
    // white, so 0xF0F0F0 there proves the dialog composited into the
    // owner's published surface.
    const DIALOG_FACE_SAMPLE: (u32, u32) = (490, 150);

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("GUI session starts");
    // CI mode: run the scripted self-test (see drive_gui_session).
    session
        .set_guest_env("WIE_SELFTEST", "1")
        .expect("inject WIE_SELFTEST");

    let mut iterations = 0;
    let mut saw_dialog_face = false;
    let mut saw_button_hit_test = false;
    let handle = session.guest_handle();
    let exit_code = loop {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("GUI session run_until_stop");
        iterations += 1;
        assert!(
            iterations < 300,
            "gui_demo.exe did not exit within 300 iterations"
        );

        // Presentation-model check: the dialog composites into the owner's
        // published surface while it is open (click path).
        if let Some(owner) = session.first_guest_window_handle()
            && let Some(frame) = session.take_frame(owner)
        {
            let (x, y) = DIALOG_FACE_SAMPLE;
            if x < frame.width && y < frame.height {
                let idx = usize::try_from(y).unwrap_or(0) * frame.width as usize
                    + usize::try_from(x).unwrap_or(0);
                if frame.pixels.get(idx).copied() == Some(BTNFACE_0RGB) {
                    saw_dialog_face = true;
                }
            }
        }

        // Host hit-testing (the click-routing regression): while the dialog
        // is open, a point inside the OK (180,228)-(300,268) or Cancel
        // (340,228)-(460,268) button must resolve to the BUTTON — its
        // 120×40 client rect — not to the 360×140 dialog. Pre-fix
        // `window_at` only tested the top-level's direct children, so a
        // dialog-button click resolved to the dialog and no BN_CLICKED fired.
        for (hx, hy) in [(240_i32, 248_i32), (400_i32, 248_i32)] {
            if let Some((hwnd, rx, ry)) = handle.window_at(hx, hy)
                && hwnd != 0
                && rx < 120
                && ry < 40
            {
                saw_button_hit_test = true;
            }
        }

        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => break Some(code),
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => {
                panic!("GUI session stopped unexpectedly: {other:?}");
            }
        }
    };

    assert_eq!(
        exit_code,
        Some(0),
        "gui_demo.exe selftest must exit 0 (click-path dialog + UTF-8 round-trip); got {exit_code:?}"
    );
    assert!(
        saw_dialog_face,
        "the dialog's face (0xF0F0F0) was never observed in the owner's published \
         surface while the dialog was open from the click path"
    );
    assert!(
        saw_button_hit_test,
        "host hit-testing never resolved a dialog-button coordinate to the button \
         (window_at must descend into the dialog's children)"
    );
}

/// gui_demo in INTERACTIVE mode (no selftest): the host drives both clicks
/// through the real host→guest posting path — `window_at` hit-test +
/// `post_message_at`, exactly what app.rs does for winit mouse events.
///
/// Regression: clicking the dialog's OK button must close the dialog. The
/// selftest covers the guest-posted click (`PostMessageA`); this covers the
/// host-posted one, which is what a real user produces. Pre-fix the host
/// hit-test only looked at the top-level's direct children, so the OK button
/// (child of the dialog) never received the mouse messages and BN_CLICKED
/// never fired.
#[test]
fn gui_demo_dialog_ok_click_closes_dialog() {
    let Some(path) = micro_exe("gui_demo.exe") else {
        eprintln!("skip: micro-exes/out/gui_demo.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    use wie_runtime::EntryTraceTermination;

    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const MK_LBUTTON: u64 = 0x0001;

    let mut session =
        wie_runtime::RuntimeSession::new(&path, wie_winapi::MessageQueueIdlePolicy::YieldOnIdle)
            .expect("GUI session starts");

    // "Dialog…" button: (256,72,90,22) in the 640×420 owner → center (301,83).
    // Dialog buttons (dialog at (140,140), DLU→px ×2): Cancel at
    // (340,228)-(460,268) → center (400,248); OK at (180,228)-(300,268) →
    // center (240,248). Both must close the dialog via host-posted clicks.
    let handle = session.guest_handle();
    for (label, bx, by) in [("Cancel", 400_i32, 248_i32), ("OK", 240_i32, 248_i32)] {
        let mut stage = 0_u8; // 0=open dialog, 1=click button, 2=assert closed
        let mut iterations = 0;
        let mut saw_face_gone = false;
        loop {
            let summary = session
                .run_until_stop(1_000_000)
                .expect("GUI session run_until_stop");
            iterations += 1;
            assert!(
                iterations < 400,
                "gui_demo.exe interactive drive stalled at {label} stage {stage}"
            );
            match summary.termination {
                EntryTraceTermination::WaitingForMessage => {}
                other => {
                    panic!("GUI session stopped unexpectedly: {other:?}");
                }
            }

            // The dialog face (DIALOG_BG 0xF0F0F0) at (490,150) must
            // disappear from the OWNER SURFACE once the dialog closes — not
            // just from the window records. EndDialog must erase the owner so
            // the class brush covers the dialog region.
            if stage == 2
                && let Some(owner) = session.first_guest_window_handle()
                && let Some(frame) = session.take_frame(owner)
            {
                let idx = 150_usize * frame.width as usize + 490_usize;
                if frame.pixels.get(idx).copied() != Some(BTNFACE_0RGB) {
                    saw_face_gone = true;
                }
            }

            match stage {
                0 => {
                    // Click the "Dialog…" button through the host path.
                    if let Some((hwnd, rx, ry)) = handle.window_at(301, 83)
                        && hwnd != 0
                        && rx < 90
                        && ry < 22
                    {
                        let lparam = u64::from(ry << 16 | rx);
                        handle.post_message_at(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam, 301, 83);
                        handle.post_message_at(hwnd, WM_LBUTTONUP, 0, lparam, 301, 83);
                        stage = 1;
                    }
                }
                1 => {
                    // The dialog should be open now: the target button is
                    // hit-testable. Click it (host path).
                    if let Some((hwnd, rx, ry)) = handle.window_at(bx, by)
                        && hwnd != 0
                        && rx < 120
                        && ry < 40
                    {
                        let lparam = u64::from(ry << 16 | rx);
                        handle.post_message_at(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lparam, bx, by);
                        handle.post_message_at(hwnd, WM_LBUTTONUP, 0, lparam, bx, by);
                        stage = 2;
                    }
                }
                _ => {
                    // The dialog must be gone: the button no longer resolves
                    // (EndDialog removed the subtree) AND the face pixels are
                    // gone from the owner surface.
                    let still_button = matches!(
                        handle.window_at(bx, by),
                        Some((hwnd, rx, ry)) if hwnd != 0 && rx < 120 && ry < 40
                    );
                    if !still_button && saw_face_gone {
                        break;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            stage, 2,
            "the dialog never closed after the host-posted {label} click"
        );
        assert!(
            saw_face_gone,
            "the dialog face (0xF0F0F0) stayed in the owner surface after the {label} \
             click — EndDialog must erase the owner"
        );
    }
}
