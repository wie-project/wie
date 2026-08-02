//! GUI micro-test: checks that a PE with window creation + paint does not crash.
//!
//! Requires mingw-built gui_*.exe (run `make -C micro-exes gui_exes`).

use std::path::{Path, PathBuf};

fn micro_exe(name: &str) -> Option<PathBuf> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("micro-exes/out");
    path.push(name);
    path.is_file().then_some(path)
}

/// Serializes the 7 GUI micro-tests so they run one at a time.
///
/// Each test drives its guest synchronously on its own test thread, and the
/// guest's 50 ms WM_TIMER advances only while `run_until_stop` executes. Under
/// the harness's default parallel schedule the CPU-heavy tests (gui_blit
/// JIT-compiles its 1280x800 session for ~11 s in debug) starve the other test
/// threads' 50 ms sleep quanta, so their guests catch up in multi-tick bursts
/// the next time the thread runs — racing shift-tab's focus transitions
/// against the dialog's timer-driven auto-close and landing gui_blit's
/// resting-frame capture after the dialog has composited over the owner.
/// Both flakes reproduce only under that parallel load and pass in isolation,
/// so the suite takes a process-wide lock and runs one test at a time. The
/// wall-time cost is small: the suite is dominated by gui_blit's ~11 s either
/// way.
static GUI_SUITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the suite-wide serialization lock (see [`GUI_SUITE_LOCK`]).
///
/// Held for the whole test; the guard's `Drop` runs on unwinding too, so a
/// panicking test cannot deadlock its successors (the next `lock()` sees a
/// poisoned mutex and recovers via `PoisonError::into_inner`).
fn gui_suite_serialize() -> std::sync::MutexGuard<'static, ()> {
    GUI_SUITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drives a YieldOnIdle GUI session like the persistent GUI loop, sleeping a
/// short quantum between empty-queue yields so the host clock advances and
/// WM_TIMER / synthesized WM_PAINT actually fire.
fn drive_gui_session(path: &Path, iterations_budget: usize) -> Option<u32> {
    use std::time::Duration;
    use wie_runtime::EntryTraceTermination;
    use wie_winapi::MessageQueueIdlePolicy;

    let mut session = wie_runtime::RuntimeSession::new(path, MessageQueueIdlePolicy::YieldOnIdle)
        .expect("GUI session starts");
    // CI mode: the guest runs its scripted timer-driven self-test and
    // auto-quits. Interactive `wie-cli run --gui` sessions never set this.
    session
        .set_guest_env("WIE_SELFTEST", "1")
        .expect("inject WIE_SELFTEST");

    let mut iterations = 0;
    loop {
        let summary = session
            .run_until_stop(1_000_000)
            .expect("GUI session run_until_stop");
        iterations += 1;
        assert!(
            iterations < iterations_budget,
            "GUI session did not exit within {iterations_budget} iterations"
        );
        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => return Some(code),
            EntryTraceTermination::WaitingForMessage => {
                // Sleep a quantum so timers advance, then re-enter GetMessage.
                std::thread::sleep(Duration::from_millis(50));
            }
            other => {
                panic!("GUI session stopped unexpectedly: {other:?}");
            }
        }
    }
}

/// FNV-1a 64 over a frame's pixels (0RGB LE bytes, top-down), restricted to
/// rows `[top, bottom)`.
///
/// The gui_blit resting-frame hash only covers rows ≥ 200 — a pure-gradient
/// band with no text or child controls. Text pixels are font-dependent
/// (real macOS system fonts vary by OS version), so any hash that includes
/// them would be fragile; the gradient rows are font-independent and stable.
fn frame_hash(frame: &wie_winapi::present::SurfaceFrame, top: u32, bottom: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for row in top..bottom {
        for pixel in frame
            .pixels
            .iter()
            .skip(usize::try_from(row).unwrap_or(0) * frame.width as usize)
            .take(usize::try_from(frame.width).unwrap_or(0))
        {
            for byte in pixel.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a prime
            }
        }
    }
    hash
}

/// CI-gated hash of gui_blit's deterministic resting frame (1280x800),
/// restricted to rows 200..800.
///
/// The frame is the gradient DIB blitted under WS_CLIPCHILDREN around the
/// fixed button and static children, published before any timer tick. Text
/// (rows 8..112) and the child controls (rows 128..190) are font- or
/// layout-dependent; rows ≥ 200 are pure gradient — font-independent and
/// stable across macOS versions. Recompute with:
/// `./target/debug/wie-cli run --screenshot /tmp/g.bmp \
///   micro-exes/out/gui_blit.exe` then FNV-1a 64 over the 0RGB pixel bytes
/// of rows 200..800.
/// Recomputed when the font system was swapped from the embedded 8×16 VGA
/// bitmap font to real macOS system fonts.
const GUI_BLIT_RESTING_FRAME_HASH: u64 = 0x9FB3_7202_941F_08DA;

/// `COLOR_BTNFACE` — the classic push-button / dialog face color.
const BTNFACE_0RGB: u32 = 0x00F0_F0F0;
/// White text glyph color used by gui_blit's DIB text (SetTextColor white).
const TEXT_WHITE_0RGB: u32 = 0x00FF_FFFF;

/// D3D9 clear color from gui_d3d9's `D3DCOLOR_XRGB(200,0,0)` → 0RGB.
const D3D9_CLEAR_RED_0RGB: u32 = 0x00C8_0000;
/// D3D9 solid cyan from gui_d3d9's indexed triangle
/// (`D3DCOLOR_XRGB(0,255,255)` → 0RGB).
const D3D9_CYAN_0RGB: u32 = 0x0000_FFFF;
/// D3D9 textured-quad texel colors as rendered: MODULATE with an opaque white
/// diffuse multiplies `(255*255)>>8 = 254`, so the 2x2 checkerboard texels
/// land as 0xFE-red / 0xFE-green / 0xFE-blue / 0xFE-white.
const D3D9_QUAD_RED_0RGB: u32 = 0x00FE_0000;
const D3D9_QUAD_GREEN_0RGB: u32 = 0x0000_FE00;
const D3D9_QUAD_BLUE_0RGB: u32 = 0x0000_00FE;
const D3D9_QUAD_WHITE_0RGB: u32 = 0x00FE_FEFE;
/// P4c alpha-blend result at (30,35): blue (alpha 0x80) over red with
/// SRCALPHA/INVSRCALPHA ADD → ((src*128 + dst*127) >> 8) per channel.
const D3D9_BLEND_0RGB: u32 = 0x007E_007F;
/// P4c opaque red quad (no blending) at (80,35).
const D3D9_RED_QUAD_0RGB: u32 = 0x00FF_0000;
/// P4c far depth quad (magenta, z=0.9) where the near quad does not cover.
const D3D9_FAR_DEPTH_0RGB: u32 = 0x00FF_00FF;
/// P4c near depth quad (white, z=0.1) — wins the depth test in the overlap.
const D3D9_NEAR_DEPTH_0RGB: u32 = 0x00FF_FFFF;

/// CI-gated hash of gui_d3d9's deterministic resting frame (320×240).
///
/// The frame is the clear-red backbuffer with the gradient triangle, the cyan
/// indexed triangle, the 2x2 textured quad, the P4c alpha-blended quads, and
/// the P4c depth-tested quads (rows 200..800 of the GDI hash do not apply —
/// the D3D9 frame is fully deterministic CPU output). Recompute with
/// `./target/debug/wie-cli run --screenshot /tmp/d.bmp \
/// micro-exes/out/gui_d3d9.exe` then FNV-1a 64 over the full 0RGB pixel bytes.
/// Recomputed when P4c added the blend + depth quads.
const D3D9_RESTING_FRAME_HASH: u64 = 0x28C1_AE13_5D5C_D9D0;

/// Run gui_d3d9 end-to-end and prove the P3 D3D9 software-render slice:
/// CreateDevice → Clear(red) → BeginScene → DrawPrimitiveUP (gradient
/// triangle) → DrawIndexedPrimitiveUP (solid cyan triangle) → EndScene →
/// Present publishes a SurfaceFrame through the GDI-shared pipeline.
///
/// The exe exits 0 only if every D3D9 call's HRESULT succeeded AND
/// GetDeviceCaps honestly reported the P5a caps (ps_2_0, vs stage still 0)
/// AND the SetViewport/GetViewport round-trip matched. Pixel checks pin the
/// rendered frame: clear red outside the triangles, the blended gradient
/// triangle, and the solid cyan indexed triangle.
#[test]
fn gui_d3d9_renders_clear_and_triangle() {
    let Some(path) = micro_exe("gui_d3d9.exe") else {
        eprintln!("skip: micro-exes/out/gui_d3d9.exe not built (run make -C micro-exes gui_exes)");
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    use std::time::Duration;
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
                // The whole 320x240 frame is deterministic CPU output — gate it.
                && frame_hash(&frame, 0, frame.height) == D3D9_RESTING_FRAME_HASH
            {
                saw_d3d9_frame = true;
            }
        }

        match summary.termination {
            EntryTraceTermination::ExitProcess { code } => break Some(code),
            EntryTraceTermination::WaitingForMessage => {
                std::thread::sleep(Duration::from_millis(50));
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
         UnlockRect → SetTexture → textured DrawPrimitiveUP → EndScene → Present \
         all succeeded and caps honesty held); got {exit_code:?}"
    );
    assert!(
        saw_d3d9_frame,
        "the D3D9 frame (clear red + gradient triangle + cyan indexed \
         triangle + textured quad) was never observed in the device window's \
         published surface"
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

    use std::time::Duration;
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
                std::thread::sleep(Duration::from_millis(10));
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

/// Verify the deterministic frame's pixel content: the gradient survives the
/// WS_CLIPCHILDREN-clipped blit, the DIB text drew white glyph ink, and the
/// button (BTNFACE + border) and static (WINDOW fill) children rendered at
/// their fixed positions. The gradient formula matches gui_blit/main.c:
/// r=(x*255)/1280, g=(y*255)/800, b=((x+y)*127)/2080.
fn assert_frame_renders_gradient_text_and_controls(frame: &wie_winapi::present::SurfaceFrame) {
    let (w, h) = (frame.width, frame.height);
    assert!(
        w >= 1280 && h >= 800,
        "gui_blit frame must be at least 1280x800, got {w}x{h}"
    );
    let idx = |x: u32, y: u32| {
        usize::try_from(y).unwrap_or(0) * frame.width as usize + usize::try_from(x).unwrap_or(0)
    };

    // Gradient intact at a point clear of text and children.
    {
        let expected = (((280 * 255) / 1280) << 16)
            | (((200 * 255) / 800) << 8)
            | (((280 + 200) * 127) / 2080);
        assert_eq!(
            frame.pixels.get(idx(280, 200)).copied(),
            Some(expected),
            "gradient pixel at (280,200) changed (0x{expected:06X} expected)"
        );
    }

    // White text glyph ink inside the first DIB text line's band.
    let text_ink = (8..40).fold(0_u32, |acc, y| {
        acc + (8..152).fold(0_u32, |acc, x| {
            acc + u32::from(frame.pixels.get(idx(x, y)).copied() == Some(TEXT_WHITE_0RGB))
        })
    });
    assert!(
        text_ink > 100,
        "expected white text glyph ink in the DIB text band, found {text_ink} px"
    );

    // Button face (BTNFACE) just inside its top-left corner — clear of the
    // centered caption, whose width is font-dependent (system fonts vary).
    assert_eq!(
        frame.pixels.get(idx(108, 178)).copied(),
        Some(BTNFACE_0RGB),
        "button face (BTNFACE) missing in its face area"
    );

    // Static child fill (COLOR_BTNFACE — the label sits on the dialog face,
    // not a white box) at its center.
    assert_eq!(
        frame.pixels.get(idx(160, 138)).copied(),
        Some(BTNFACE_0RGB),
        "static child fill (COLOR_BTNFACE) missing at its center"
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
fn gui_control_child_windows_and_command() {
    let Some(path) = micro_exe("gui_control.exe") else {
        eprintln!(
            "skip: micro-exes/out/gui_control.exe not built (run make -C micro-exes gui_exes)"
        );
        return;
    };
    // Serialized suite: see GUI_SUITE_LOCK.
    let _suite = gui_suite_serialize();

    // The exe creates a BUTTON + STATIC child, self-checks the child model
    // (GetParent/GetDlgCtrlID/IsChild, SetWindowText round-trip), then after
    // TIMER_TICKS simulates a click via SendMessageA(button, WM_LBUTTONDOWN/UP).
    // The host control WndProc fires WM_COMMAND(BN_CLICKED) to the parent,
    // which destroys the window — so exit 0 proves the control dispatch path
    // (incl. control painting + WS_CLIPCHILDREN-clipped parent repaints) ran.
    let exit_code = drive_gui_session(&path, 200);

    assert_eq!(
        exit_code,
        Some(0),
        "gui_control.exe must exit 0 (proves WM_COMMAND from the button fired); got {exit_code:?}"
    );
}

/// Owner-client coordinates of the gui_dialog OK-button face (template id 100:
/// "100 DIALOG 10,20,160,60", OK at DLU (10,10,60,20) → px (20,20,120,40),
/// dialog centered in the 1280×800 owner at (480,340) → button at (500,360)).
const DIALOG_OK_BUTTON_SAMPLE: (u32, u32) = (508, 368);

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

    use std::time::Duration;
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
                std::thread::sleep(Duration::from_millis(50));
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

    use std::time::Duration;
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
                std::thread::sleep(Duration::from_millis(2));
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

    use std::time::Duration;
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
                std::thread::sleep(Duration::from_millis(50));
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

    use std::time::Duration;
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
            std::thread::sleep(Duration::from_millis(10));
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
